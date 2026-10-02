//! Parser for `lcov` tracefiles (coverage data, not test pass/fail results).
//!
//! An lcov tracefile is a plain-text, line-oriented format: a sequence of `SF:`-delimited
//! sections, each describing one source file's coverage, built from records like `DA:` (line hit
//! counts), `FN:`/`FNDA:` (function declarations/hit counts), and `BRDA:` (branch hit counts).
//! Confirmed against `linux-test-project/lcov` `docs/man/geninfo.rst` at the `master` branch's
//! "TRACEFILE FORMAT" section
//! (<https://github.com/linux-test-project/lcov/blob/master/docs/man/geninfo.rst>), which is the
//! canonical description the project's own man pages point to (`lcov(1)`: "The lcov tracefile
//! (\".info\" file) format is described in man geninfo(1)").
//!
//! This is fundamentally different data from [`crate::TestSuites`]: a tracefile reports which
//! lines of which source files *executed*, not which tests passed or failed, so it gets its own
//! domain model ([`LcovReport`]) rather than being forced into [`crate::TestCase`]/
//! [`crate::Outcome`]. [`SourceFile::lines`] is the data `docs/design/byo-ci.md`'s "Native:
//! line-hit union" merge strategy needs: per-line hit counts, unioned across shards by summing
//! counts for matching `(path, line)` pairs.
//!
//! # Grammar
//!
//! Per the source above:
//!
//! ```text
//! tracefile := ( comment | blank )*  section*
//! section   := 'TN:'?  'SF:'  ( coverpoint | 'VER:' | comment | blank )*
//!              'end_of_record'
//! ```
//!
//! A section begins with `SF:` (`KF:` is an accepted synonym) and ends with `end_of_record`; an
//! optional `TN:` record naming the testcase precedes it. Coverpoint records (`DA:`, `FN:`,
//! `FNDA:`, `BRDA:`, and others this parser does not model) only mean something inside a section.
//! `#...` lines are comments and blank lines are ignored anywhere.
//!
//! # What's parsed vs. tolerated
//!
//! - **`TN:`**, **`SF:`**/**`KF:`**, **`DA:`**, **`end_of_record`** — the required minimum: test
//!   name, source file path, and per-line hit counts.
//! - **`FNDA:<count>,<name>`** — per-function hit counts, into [`SourceFile::functions`]. The
//!   companion `FN:` record (function *declaration*: start line, no hit count) carries no data
//!   this report needs and is tolerated like any other unhandled record.
//! - **`BRDA:<line>,<block>,<branch>,<taken>`** — per-branch hit counts, into
//!   [`SourceFile::branches`]. `<block>` may carry a leading `e`/`f`/`U` flag (exception /
//!   fallthrough / unreachable); this parser strips it and keeps only the numeric block index.
//!   `<branch>` is an arbitrary string that may itself contain commas, so it is read as
//!   everything between `<block>` and the final (`<taken>`) field rather than split naively.
//! - Everything else (`VER:`, `FN:`, `FNL:`/`FNA:` (the LCOV 2.2+ function format), `FNF:`/
//!   `FNH:`, `BRF:`/`BRH:`, `MCDC:`, `LF:`/`LH:`, comments, blank lines) is recognized as
//!   belonging to the grammar but not modeled: it is skipped without error, per lcov's own
//!   tolerance for readers that only care about some record types.
//!
//! Structural violations — a coverpoint before any `SF:`, a second `SF:` with no intervening
//! `end_of_record`, an `end_of_record` with no section open, or end of input with a section still
//! open — are reported as [`ParseError`] variants (the source above calls these `ERROR_FORMAT`).
//! A `DA:`/`FNDA:`/`BRDA:` record whose fields don't parse as the format's `<line>,<count>[,...]`
//! shape is also a typed error, never a panic.

use std::fmt;
use std::str;

use crate::{BranchCoverage, FunctionCoverage, LcovReport, LineCoverage, SourceFile};

/// Parses an lcov tracefile into an [`LcovReport`].
pub fn parse(tracefile: &[u8]) -> Result<LcovReport, ParseError> {
    let text = str::from_utf8(tracefile)?;

    let mut source_files = Vec::new();
    let mut pending_test_name: Option<String> = None;
    let mut current: Option<SourceFile> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (record, rest) = match line.split_once(':') {
            Some((record, rest)) => (record, rest),
            None => (line, ""),
        };

        match record {
            "TN" => pending_test_name = Some(rest.to_string()),
            "SF" | "KF" => {
                if let Some(file) = current.take() {
                    return Err(ParseError::UnterminatedSection { path: file.path });
                }
                current = Some(SourceFile {
                    test_name: pending_test_name.take(),
                    path: rest.to_string(),
                    lines: Vec::new(),
                    functions: Vec::new(),
                    branches: Vec::new(),
                });
            }
            "DA" => {
                let file = current
                    .as_mut()
                    .ok_or_else(|| ParseError::RecordOutsideSection {
                        record: line.to_string(),
                    })?;
                file.lines.push(parse_da(rest)?);
            }
            "FNDA" => {
                let file = current
                    .as_mut()
                    .ok_or_else(|| ParseError::RecordOutsideSection {
                        record: line.to_string(),
                    })?;
                file.functions.push(parse_fnda(rest)?);
            }
            "BRDA" => {
                let file = current
                    .as_mut()
                    .ok_or_else(|| ParseError::RecordOutsideSection {
                        record: line.to_string(),
                    })?;
                file.branches.push(parse_brda(rest)?);
            }
            "end_of_record" => {
                let file = current.take().ok_or(ParseError::UnmatchedEndOfRecord)?;
                source_files.push(file);
            }
            // VER, FN, FNL/FNA, FNF/FNH, BRF/BRH, MCDC, LF/LH, and anything else this parser
            // does not model: part of the grammar, tolerated without error (see module docs).
            _ => {}
        }
    }

    if let Some(file) = current {
        return Err(ParseError::UnexpectedEof { path: file.path });
    }

    Ok(LcovReport { source_files })
}

/// Parses a `DA:` record's fields: `<line number>,<execution count>[,<checksum>]`.
fn parse_da(rest: &str) -> Result<LineCoverage, ParseError> {
    let fields: Vec<&str> = rest.splitn(3, ',').collect();
    let [line_field, count_field] = [
        *fields.first().unwrap_or(&""),
        *fields.get(1).unwrap_or(&""),
    ];
    if fields.len() < 2 {
        return Err(ParseError::InvalidDa {
            line: rest.to_string(),
        });
    }
    let line = line_field.parse().map_err(|_| ParseError::InvalidDa {
        line: rest.to_string(),
    })?;
    let hit_count = count_field.parse().map_err(|_| ParseError::InvalidDa {
        line: rest.to_string(),
    })?;
    let checksum = fields.get(2).map(|s| s.to_string());

    Ok(LineCoverage {
        line,
        hit_count,
        checksum,
    })
}

/// Parses an `FNDA:` record's fields: `<execution count>,<function name>`. The function name is
/// everything after the first comma, since it may itself contain commas (templated names).
fn parse_fnda(rest: &str) -> Result<FunctionCoverage, ParseError> {
    let (count_field, name) = rest
        .split_once(',')
        .ok_or_else(|| ParseError::InvalidFnda {
            line: rest.to_string(),
        })?;
    let hit_count = count_field.parse().map_err(|_| ParseError::InvalidFnda {
        line: rest.to_string(),
    })?;

    Ok(FunctionCoverage {
        name: name.to_string(),
        hit_count,
    })
}

/// Parses a `BRDA:` record's fields: `<line_number>,[efU]<block>,<branch>,<taken>`. `<branch>`
/// may contain commas, so it is read as everything between `<block>` and the final `<taken>`
/// field rather than assumed to be exactly one comma-separated token.
fn parse_brda(rest: &str) -> Result<BranchCoverage, ParseError> {
    let invalid = || ParseError::InvalidBrda {
        line: rest.to_string(),
    };

    let mut fields = rest.splitn(3, ',');
    let line_field = fields.next().ok_or_else(invalid)?;
    let block_field = fields.next().ok_or_else(invalid)?;
    let remainder = fields.next().ok_or_else(invalid)?;

    let line = line_field.parse().map_err(|_| invalid())?;
    let block = block_field
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .parse()
        .map_err(|_| invalid())?;

    let split_at = remainder.rfind(',').ok_or_else(invalid)?;
    let branch = remainder[..split_at].to_string();
    let taken_field = &remainder[split_at + 1..];
    let taken = if taken_field == "-" {
        None
    } else {
        Some(taken_field.parse().map_err(|_| invalid())?)
    };

    Ok(BranchCoverage {
        line,
        block,
        branch,
        taken,
    })
}

/// Everything that can go wrong parsing an lcov tracefile. Never panics: malformed input always
/// produces one of these variants.
#[derive(Debug)]
pub enum ParseError {
    /// The tracefile was not valid UTF-8.
    InvalidUtf8(str::Utf8Error),
    /// A `DA:` record's fields were not `<line number>,<execution count>[,<checksum>]`.
    InvalidDa { line: String },
    /// An `FNDA:` record's fields were not `<execution count>,<function name>`.
    InvalidFnda { line: String },
    /// A `BRDA:` record's fields were not `<line>,<block>,<branch>,<taken>`.
    InvalidBrda { line: String },
    /// A `DA:`/`FNDA:`/`BRDA:` record appeared before any `SF:`/`KF:` opened a section.
    RecordOutsideSection { record: String },
    /// A second `SF:`/`KF:` opened a section before the previous one's `end_of_record`.
    UnterminatedSection { path: String },
    /// `end_of_record` appeared with no section open.
    UnmatchedEndOfRecord,
    /// The tracefile ended while a section (opened by `SF:`/`KF:`) was still open.
    UnexpectedEof { path: String },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::InvalidUtf8(err) => write!(f, "tracefile is not valid UTF-8: {err}"),
            ParseError::InvalidDa { line } => write!(f, "malformed DA record: {line:?}"),
            ParseError::InvalidFnda { line } => write!(f, "malformed FNDA record: {line:?}"),
            ParseError::InvalidBrda { line } => write!(f, "malformed BRDA record: {line:?}"),
            ParseError::RecordOutsideSection { record } => {
                write!(
                    f,
                    "record {record:?} appeared before any SF: opened a section"
                )
            }
            ParseError::UnterminatedSection { path } => {
                write!(
                    f,
                    "section for {path:?} was not closed with end_of_record before the next SF:"
                )
            }
            ParseError::UnmatchedEndOfRecord => {
                write!(f, "end_of_record appeared with no section open")
            }
            ParseError::UnexpectedEof { path } => {
                write!(
                    f,
                    "tracefile ended while the section for {path:?} was still open"
                )
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl From<str::Utf8Error> for ParseError {
    fn from(err: str::Utf8Error) -> Self {
        ParseError::InvalidUtf8(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI_FILE: &str = "TN:unit
SF:src/math.rs
FN:3,add
FNDA:5,add
FN:8,subtract
FNDA:0,subtract
DA:3,5
DA:4,5
DA:8,0
DA:9,0
BRDA:3,0,enable,5
BRDA:3,0,!enable,0
LF:4
LH:2
end_of_record
TN:unit
SF:src/util.rs
DA:1,1
DA:2,1
DA:5,0
# a comment explaining the next block
VER:deadbeef
LF:3
LH:2
end_of_record
";

    #[test]
    fn parses_a_multi_file_tracefile() -> Result<(), ParseError> {
        let report = parse(MULTI_FILE.as_bytes())?;

        assert_eq!(report.source_files.len(), 2);

        let math = &report.source_files[0];
        assert_eq!(math.test_name.as_deref(), Some("unit"));
        assert_eq!(math.path, "src/math.rs");
        assert_eq!(math.lines.len(), 4);

        Ok(())
    }

    #[test]
    fn captures_per_line_hit_counts_for_line_hit_union() -> Result<(), ParseError> {
        let report = parse(MULTI_FILE.as_bytes())?;
        let math = &report.source_files[0];

        assert_eq!(
            math.lines[0],
            LineCoverage {
                line: 3,
                hit_count: 5,
                checksum: None
            }
        );
        assert_eq!(
            math.lines[2],
            LineCoverage {
                line: 8,
                hit_count: 0,
                checksum: None
            }
        );

        let util = &report.source_files[1];
        assert_eq!(util.path, "src/util.rs");
        assert_eq!(
            util.lines[2],
            LineCoverage {
                line: 5,
                hit_count: 0,
                checksum: None
            }
        );
        assert!(util.lines.iter().any(|l| l.hit_count > 0));

        Ok(())
    }

    #[test]
    fn captures_function_hit_counts_from_fnda() -> Result<(), ParseError> {
        let report = parse(MULTI_FILE.as_bytes())?;
        let math = &report.source_files[0];

        assert_eq!(math.functions.len(), 2);
        assert_eq!(
            math.functions[0],
            FunctionCoverage {
                name: "add".to_string(),
                hit_count: 5
            }
        );
        assert_eq!(
            math.functions[1],
            FunctionCoverage {
                name: "subtract".to_string(),
                hit_count: 0
            }
        );

        Ok(())
    }

    #[test]
    fn captures_branch_hit_counts_including_string_branch_ids() -> Result<(), ParseError> {
        let report = parse(MULTI_FILE.as_bytes())?;
        let math = &report.source_files[0];

        assert_eq!(math.branches.len(), 2);
        assert_eq!(
            math.branches[0],
            BranchCoverage {
                line: 3,
                block: 0,
                branch: "enable".to_string(),
                taken: Some(5)
            }
        );
        assert_eq!(
            math.branches[1],
            BranchCoverage {
                line: 3,
                block: 0,
                branch: "!enable".to_string(),
                taken: Some(0)
            }
        );

        Ok(())
    }

    #[test]
    fn branch_taken_dash_means_never_evaluated() -> Result<(), ParseError> {
        let tracefile = "SF:src/cond.rs
DA:10,0
BRDA:10,0,x,-
end_of_record
";
        let report = parse(tracefile.as_bytes())?;

        assert_eq!(report.source_files[0].branches[0].taken, None);

        Ok(())
    }

    #[test]
    fn unhandled_record_types_are_tolerated_not_errors() -> Result<(), ParseError> {
        // MULTI_FILE already has VER:, FN:, LF:, LH:, and a comment interleaved with the
        // records this parser does model; a successful parse above already proves tolerance.
        // This test adds a record type not used anywhere else in the fixtures: FNL/FNA (the
        // LCOV 2.2+ function format) and MCDC.
        let tracefile = "SF:src/cond.rs
FNL:0,10,12
FNA:0,3,guarded
MCDC:10,2,f,0,0,enable
DA:10,3
end_of_record
";
        let report = parse(tracefile.as_bytes())?;

        assert_eq!(report.source_files.len(), 1);
        assert_eq!(report.source_files[0].lines.len(), 1);

        Ok(())
    }

    #[test]
    fn malformed_da_line_returns_a_typed_error_instead_of_panicking() {
        let tracefile = "SF:src/broken.rs
DA:not-a-line,5
end_of_record
";
        let err = parse(tracefile.as_bytes()).err();

        assert!(matches!(&err, Some(ParseError::InvalidDa { .. })));
        if let Some(ParseError::InvalidDa { line }) = err {
            assert_eq!(line, "not-a-line,5");
        }
    }

    #[test]
    fn da_line_missing_the_hit_count_field_is_a_typed_error() {
        let tracefile = "SF:src/broken.rs
DA:10
end_of_record
";
        let err = parse(tracefile.as_bytes()).err();

        assert!(matches!(&err, Some(ParseError::InvalidDa { .. })));
    }

    #[test]
    fn da_before_any_sf_is_a_typed_error() {
        let tracefile = "DA:1,1
";
        let err = parse(tracefile.as_bytes()).err();

        assert!(matches!(
            &err,
            Some(ParseError::RecordOutsideSection { .. })
        ));
    }

    #[test]
    fn unterminated_section_at_eof_is_a_typed_error() {
        let tracefile = "SF:src/unclosed.rs
DA:1,1
";
        let err = parse(tracefile.as_bytes()).err();

        assert!(matches!(&err, Some(ParseError::UnexpectedEof { .. })));
    }

    #[test]
    fn unmatched_end_of_record_is_a_typed_error() {
        let tracefile = "end_of_record
";
        let err = parse(tracefile.as_bytes()).err();

        assert!(matches!(err, Some(ParseError::UnmatchedEndOfRecord)));
    }

    #[test]
    fn kf_is_accepted_as_a_synonym_for_sf() -> Result<(), ParseError> {
        let tracefile = "KF:src/legacy.rs
DA:1,1
end_of_record
";
        let report = parse(tracefile.as_bytes())?;

        assert_eq!(report.source_files[0].path, "src/legacy.rs");

        Ok(())
    }
}
