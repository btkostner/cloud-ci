//! Merges multiple shards' parsed reports of the same kind into one document — the native
//! (no-container) shard-merge path for `junit`/`coverage` report types
//! (`docs/design/parallelization.md`'s "### Merge strategies per report type": junit is "Parse
//! each shard's JUnit XML from R2, concatenate `testsuite` elements into one `testsuites`
//! document"; coverage is "Parse and sum per-line/per-branch hit counts across shards").
//!
//! This module is pure (no `worker`/Queue/R2 dependency): it takes already-parsed
//! [`crate::TestSuites`]/[`crate::LcovReport`] values (produced by [`crate::junit::parse`]/
//! [`crate::lcov::parse`]) and returns the merged document's serialized text. The Worker-side
//! Queue consumer (`cloud-ci-worker`'s shard-merge module) owns reading each shard's bytes from
//! R2, parsing them, calling into this module, and writing the result back to R2 — none of that
//! I/O happens here, which is what keeps this module unit-testable with plain `cargo test`
//! against real fixture content, matching every other pure module in this crate.

use std::io::Cursor;

use quick_xml::events::BytesText;
use quick_xml::writer::Writer;

use crate::{
    BranchCoverage, FunctionCoverage, LcovReport, LineCoverage, Outcome, SourceFile, TestCase,
    TestSuite, TestSuites,
};

/// Concatenates every `<testsuite>` element across `docs` (one [`TestSuites`] per shard's parsed
/// JUnit document, in shard order) into one `<testsuites>` document — real XML writing via
/// `quick_xml::Writer`, not string concatenation, so a failure message containing `&`/`<`/`>` in
/// one shard's report is escaped correctly rather than corrupting the merged document. The
/// merged root carries no `name` attribute (shards have no single shared suite name to pick) and
/// no aggregate counters — per-suite `tests`/`failures`/`errors`/`skipped`/`time` attributes are
/// carried through unchanged from each shard's own `<testsuite>` element, since those are
/// already correct for that suite and this module does not re-derive a root-level total the
/// JUnit format does not require.
///
/// `Err` only on a genuine write failure from the underlying `Vec<u8>` buffer (`std::io::Write`
/// on a `Vec` is infallible in practice, but the type is still propagated with `?` rather than
/// `unwrap`/`expect`, per this crate's no-`unwrap`/`expect`/`panic` lints).
pub fn merge_junit(docs: &[TestSuites]) -> std::io::Result<String> {
    let mut buf = Vec::new();
    {
        let mut writer = Writer::new(Cursor::new(&mut buf));
        writer
            .create_element("testsuites")
            .write_inner_content(|writer| {
                for doc in docs {
                    for suite in &doc.suites {
                        write_testsuite(writer, suite)?;
                    }
                }
                Ok(())
            })?;
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn write_testsuite<W: std::io::Write>(
    writer: &mut Writer<W>,
    suite: &TestSuite,
) -> std::io::Result<()> {
    let mut attrs: Vec<(&str, String)> = vec![("name", suite.name.clone())];
    if let Some(v) = suite.tests {
        attrs.push(("tests", v.to_string()));
    }
    if let Some(v) = suite.failures {
        attrs.push(("failures", v.to_string()));
    }
    if let Some(v) = suite.errors {
        attrs.push(("errors", v.to_string()));
    }
    if let Some(v) = suite.skipped {
        attrs.push(("skipped", v.to_string()));
    }
    if let Some(v) = suite.time {
        attrs.push(("time", v.to_string()));
    }
    writer
        .create_element("testsuite")
        .with_attributes(attrs.iter().map(|(k, v)| (*k, v.as_str())))
        .write_inner_content(|writer| {
            for case in &suite.test_cases {
                write_testcase(writer, case)?;
            }
            if let Some(out) = &suite.system_out {
                writer
                    .create_element("system-out")
                    .write_text_content(BytesText::from_escaped(out.as_str()))?;
            }
            if let Some(err) = &suite.system_err {
                writer
                    .create_element("system-err")
                    .write_text_content(BytesText::from_escaped(err.as_str()))?;
            }
            Ok(())
        })?;
    Ok(())
}

fn write_testcase<W: std::io::Write>(
    writer: &mut Writer<W>,
    case: &TestCase,
) -> std::io::Result<()> {
    let mut attrs: Vec<(&str, String)> = vec![("name", case.name.clone())];
    if let Some(v) = &case.classname {
        attrs.push(("classname", v.clone()));
    }
    if let Some(v) = &case.file {
        attrs.push(("file", v.clone()));
    }
    if let Some(v) = case.line {
        attrs.push(("line", v.to_string()));
    }
    if let Some(v) = case.time {
        attrs.push(("time", v.to_string()));
    }
    writer
        .create_element("testcase")
        .with_attributes(attrs.iter().map(|(k, v)| (*k, v.as_str())))
        .write_inner_content(|writer| {
            match &case.outcome {
                Outcome::Passed => {}
                Outcome::Failed(failure) => write_failure_like(writer, "failure", failure)?,
                Outcome::Errored(failure) => write_failure_like(writer, "error", failure)?,
                Outcome::Skipped(message) => {
                    let mut el = writer.create_element("skipped");
                    if let Some(m) = message {
                        el = el.with_attribute(("message", m.as_str()));
                    }
                    el.write_empty()?;
                }
            }
            if let Some(out) = &case.system_out {
                writer
                    .create_element("system-out")
                    .write_text_content(BytesText::from_escaped(out.as_str()))?;
            }
            if let Some(err) = &case.system_err {
                writer
                    .create_element("system-err")
                    .write_text_content(BytesText::from_escaped(err.as_str()))?;
            }
            Ok(())
        })?;
    Ok(())
}

fn write_failure_like<W: std::io::Write>(
    writer: &mut Writer<W>,
    tag: &str,
    failure: &crate::Failure,
) -> std::io::Result<()> {
    let el = writer.create_element(tag);
    let el = match &failure.message {
        Some(m) => el.with_attribute(("message", m.as_str())),
        None => el,
    };
    match &failure.stack_trace {
        Some(text) => {
            el.write_text_content(BytesText::from_escaped(text.as_str()))?;
        }
        None => {
            el.write_empty()?;
        }
    }
    Ok(())
}

/// Sums per-line/per-branch/per-function hit counts across `reports` (one [`LcovReport`] per
/// shard's parsed tracefile), matching source files by `path` and lines/functions/branches by
/// their own identifying key — `docs/design/byo-ci.md`'s "Native: line-hit union" strategy this
/// module's doc comment and [`crate::lcov`]'s own doc comment both reference: "per-line hit
/// counts, unioned across shards by summing counts for matching `(path, line)` pairs". The same
/// summing extends naturally to `FNDA:`/`BRDA:` records, keyed by `(path, function name)` and
/// `(path, line, block, branch)` respectively — a shard's test run only executes its assigned
/// subset of tests, so two shards reporting the same source file never double-count a line a
/// single shard run would have executed once; summing is exactly how coverage recombines
/// disjoint test subsets' contributions to the same file.
///
/// Source-file order in the output follows first-occurrence order across `reports`, matching
/// this crate's established "order-preserving, not sorted" convention
/// ([`crate::junit`]'s own sibling functions make no attempt to sort either). Within a file,
/// lines/functions/branches follow the same first-occurrence order.
pub fn merge_lcov(reports: &[LcovReport]) -> LcovReport {
    let mut merged: Vec<SourceFile> = Vec::new();
    let mut file_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for report in reports {
        for file in &report.source_files {
            let idx = *file_index.entry(file.path.clone()).or_insert_with(|| {
                merged.push(SourceFile {
                    test_name: file.test_name.clone(),
                    path: file.path.clone(),
                    lines: Vec::new(),
                    functions: Vec::new(),
                    branches: Vec::new(),
                });
                merged.len() - 1
            });
            let target = &mut merged[idx];

            for line in &file.lines {
                merge_line(&mut target.lines, line);
            }
            for func in &file.functions {
                merge_function(&mut target.functions, func);
            }
            for branch in &file.branches {
                merge_branch(&mut target.branches, branch);
            }
        }
    }

    LcovReport {
        source_files: merged,
    }
}

fn merge_line(lines: &mut Vec<LineCoverage>, incoming: &LineCoverage) {
    if let Some(existing) = lines.iter_mut().find(|l| l.line == incoming.line) {
        existing.hit_count += incoming.hit_count;
        if existing.checksum.is_none() {
            existing.checksum = incoming.checksum.clone();
        }
    } else {
        lines.push(incoming.clone());
    }
}

fn merge_function(functions: &mut Vec<FunctionCoverage>, incoming: &FunctionCoverage) {
    if let Some(existing) = functions.iter_mut().find(|f| f.name == incoming.name) {
        existing.hit_count += incoming.hit_count;
    } else {
        functions.push(incoming.clone());
    }
}

fn merge_branch(branches: &mut Vec<BranchCoverage>, incoming: &BranchCoverage) {
    if let Some(existing) = branches.iter_mut().find(|b| {
        b.line == incoming.line && b.block == incoming.block && b.branch == incoming.branch
    }) {
        existing.taken = match (existing.taken, incoming.taken) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
    } else {
        branches.push(incoming.clone());
    }
}

/// Serializes a merged [`LcovReport`] back to lcov tracefile text — the inverse of
/// [`crate::lcov::parse`] for exactly the record kinds that parser reads (`TN:`, `SF:`, `DA:`,
/// `FNDA:`, `BRDA:`, `end_of_record`; see that module's doc comment's "What's parsed vs.
/// tolerated" section). Does not emit `LF:`/`LH:`/`FNF:`/`FNH:`/`BRF:`/`BRH:` summary records —
/// `crate::lcov::parse` tolerates their absence (they are reader conveniences, not required by
/// the grammar this crate's own module doc comment quotes), and computing them here would need
/// no new data: `LH`/`LF` are just `lines.iter().filter(hit_count > 0).count()`/`lines.len()`,
/// left out only because no `merge_lcov` output needs re-parsing by a tool that requires them
/// this round — `lcov::parse` itself does not.
pub fn write_lcov(report: &LcovReport) -> String {
    let mut out = String::new();
    for file in &report.source_files {
        if let Some(test_name) = &file.test_name {
            out.push_str("TN:");
            out.push_str(test_name);
            out.push('\n');
        }
        out.push_str("SF:");
        out.push_str(&file.path);
        out.push('\n');
        for func in &file.functions {
            out.push_str(&format!("FNDA:{},{}\n", func.hit_count, func.name));
        }
        for branch in &file.branches {
            let taken = branch
                .taken
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string());
            out.push_str(&format!(
                "BRDA:{},{},{},{}\n",
                branch.line, branch.block, branch.branch, taken
            ));
        }
        for line in &file.lines {
            match &line.checksum {
                Some(checksum) => out.push_str(&format!(
                    "DA:{},{},{}\n",
                    line.line, line.hit_count, checksum
                )),
                None => out.push_str(&format!("DA:{},{}\n", line.line, line.hit_count)),
            }
        }
        out.push_str("end_of_record\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Unexpected(String);

    impl std::fmt::Display for Unexpected {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for Unexpected {}

    impl From<crate::junit::ParseError> for Unexpected {
        fn from(e: crate::junit::ParseError) -> Self {
            Unexpected(e.to_string())
        }
    }

    impl From<crate::lcov::ParseError> for Unexpected {
        fn from(e: crate::lcov::ParseError) -> Self {
            Unexpected(e.to_string())
        }
    }

    impl From<std::io::Error> for Unexpected {
        fn from(e: std::io::Error) -> Self {
            Unexpected(e.to_string())
        }
    }

    type TestResult = Result<(), Unexpected>;

    fn some_or<T>(opt: Option<T>, what: &str) -> Result<T, Unexpected> {
        opt.ok_or_else(|| Unexpected(format!("expected {what} to be present")))
    }

    #[test]
    fn merge_junit_concatenates_testsuites_from_every_shard() -> TestResult {
        let shard1 = crate::junit::parse(
            br#"<testsuites name="shard1">
                  <testsuite name="pkg/a" tests="2" failures="1" time="1.5">
                    <testcase name="ok" classname="pkg.a" time="0.5"/>
                    <testcase name="bad" classname="pkg.a" time="1.0">
                      <failure message="boom">stack &amp; trace</failure>
                    </testcase>
                  </testsuite>
                </testsuites>"#,
        )?;

        let shard2 = crate::junit::parse(
            br#"<testsuites name="shard2">
                  <testsuite name="pkg/b" tests="1" skipped="1" time="0.1">
                    <testcase name="skip_me" classname="pkg.b">
                      <skipped message="not ready"/>
                    </testcase>
                  </testsuite>
                </testsuites>"#,
        )?;

        let merged_xml = merge_junit(&[shard1, shard2])?;
        let merged = crate::junit::parse(merged_xml.as_bytes())?;

        assert_eq!(merged.suites.len(), 2);
        assert_eq!(merged.suites[0].name, "pkg/a");
        assert_eq!(merged.suites[0].test_cases.len(), 2);
        assert_eq!(merged.suites[0].test_cases[0].name, "ok");
        assert_eq!(merged.suites[0].test_cases[1].name, "bad");
        match &merged.suites[0].test_cases[1].outcome {
            Outcome::Failed(failure) => {
                assert_eq!(failure.message.as_deref(), Some("boom"));
                assert_eq!(failure.stack_trace.as_deref(), Some("stack &amp; trace"));
            }
            other => {
                return Err(Unexpected(format!(
                    "expected Failed outcome, got {other:?}"
                )));
            }
        }
        assert_eq!(merged.suites[1].name, "pkg/b");
        assert_eq!(merged.suites[1].test_cases.len(), 1);
        match &merged.suites[1].test_cases[0].outcome {
            Outcome::Skipped(Some(message)) => assert_eq!(message, "not ready"),
            other => {
                return Err(Unexpected(format!(
                    "expected Skipped outcome, got {other:?}"
                )));
            }
        }
        Ok(())
    }

    #[test]
    fn merge_junit_escapes_special_characters_in_attributes_and_text() -> TestResult {
        let shard = crate::junit::parse(
            br#"<testsuites>
                  <testsuite name="a &amp; b" tests="1">
                    <testcase name="x &lt; y" classname="c">
                      <failure message="&quot;quoted&quot;">line1
line2</failure>
                    </testcase>
                  </testsuite>
                </testsuites>"#,
        )?;

        let merged_xml = merge_junit(&[shard])?;
        // The merged XML must not contain a raw, unescaped `&` from the suite name — only the
        // escaped `&amp;` form — otherwise the merged document itself would be malformed XML.
        assert!(!merged_xml.contains("a & b"));
        assert!(merged_xml.contains("a &amp; b"));

        let merged = crate::junit::parse(merged_xml.as_bytes())?;
        assert_eq!(merged.suites[0].name, "a & b");
        assert_eq!(merged.suites[0].test_cases[0].name, "x < y");
        match &merged.suites[0].test_cases[0].outcome {
            Outcome::Failed(failure) => {
                assert_eq!(failure.message.as_deref(), Some("\"quoted\""));
                assert_eq!(failure.stack_trace.as_deref(), Some("line1\nline2"));
            }
            other => {
                return Err(Unexpected(format!(
                    "expected Failed outcome, got {other:?}"
                )));
            }
        }
        Ok(())
    }

    #[test]
    fn merge_junit_empty_input_produces_empty_testsuites() -> TestResult {
        let merged_xml = merge_junit(&[])?;
        let merged = crate::junit::parse(merged_xml.as_bytes())?;
        assert!(merged.suites.is_empty());
        Ok(())
    }

    const LCOV_SHARD_1: &[u8] = b"TN:\nSF:src/lib.rs\nFNDA:3,foo\nBRDA:10,0,0,2\nBRDA:10,0,1,0\nDA:10,3\nDA:11,0\nend_of_record\n";
    const LCOV_SHARD_2: &[u8] = b"TN:\nSF:src/lib.rs\nFNDA:1,foo\nBRDA:10,0,0,1\nBRDA:10,0,1,1\nDA:10,2\nDA:12,5\nend_of_record\nSF:src/other.rs\nDA:1,1\nend_of_record\n";

    #[test]
    fn merge_lcov_sums_matching_lines_functions_and_branches() -> TestResult {
        let shard1 = crate::lcov::parse(LCOV_SHARD_1)?;
        let shard2 = crate::lcov::parse(LCOV_SHARD_2)?;

        let merged = merge_lcov(&[shard1, shard2]);

        assert_eq!(merged.source_files.len(), 2);
        let lib = &merged.source_files[0];
        assert_eq!(lib.path, "src/lib.rs");

        // DA:10 appears in both shards (3 + 2 = 5); DA:11 only in shard1; DA:12 only in shard2.
        let line10 = some_or(lib.lines.iter().find(|l| l.line == 10), "line 10")?;
        assert_eq!(line10.hit_count, 5);
        let line11 = some_or(lib.lines.iter().find(|l| l.line == 11), "line 11")?;
        assert_eq!(line11.hit_count, 0);
        let line12 = some_or(lib.lines.iter().find(|l| l.line == 12), "line 12")?;
        assert_eq!(line12.hit_count, 5);

        // FNDA:foo appears in both shards (3 + 1 = 4).
        assert_eq!(lib.functions.len(), 1);
        assert_eq!(lib.functions[0].name, "foo");
        assert_eq!(lib.functions[0].hit_count, 4);

        // BRDA:10,0,0 appears in both shards (2 + 1 = 3); BRDA:10,0,1 appears in both (0 + 1 = 1).
        assert_eq!(lib.branches.len(), 2);
        let branch0 = some_or(lib.branches.iter().find(|b| b.branch == "0"), "branch 0")?;
        assert_eq!(branch0.taken, Some(3));
        let branch1 = some_or(lib.branches.iter().find(|b| b.branch == "1"), "branch 1")?;
        assert_eq!(branch1.taken, Some(1));

        let other = &merged.source_files[1];
        assert_eq!(other.path, "src/other.rs");
        assert_eq!(other.lines.len(), 1);
        assert_eq!(other.lines[0].hit_count, 1);
        Ok(())
    }

    #[test]
    fn write_lcov_round_trips_through_parse() -> TestResult {
        let shard1 = crate::lcov::parse(LCOV_SHARD_1)?;
        let shard2 = crate::lcov::parse(LCOV_SHARD_2)?;
        let merged = merge_lcov(&[shard1, shard2]);

        let text = write_lcov(&merged);
        let reparsed = crate::lcov::parse(text.as_bytes())?;

        assert_eq!(reparsed, merged);
        Ok(())
    }

    #[test]
    fn merge_lcov_empty_input_produces_no_source_files() {
        let merged = merge_lcov(&[]);
        assert!(merged.source_files.is_empty());
    }
}
