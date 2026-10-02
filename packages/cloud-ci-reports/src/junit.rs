//! Streaming parser for JUnit XML test reports.
//!
//! JUnit has no single formal schema; writers disagree on whether the document root is
//! `<testsuites>` or a bare `<testsuite>`, and on which counter attributes they bother to emit.
//! This parser accepts both root shapes and treats every counter/time attribute as optional,
//! only erroring when the XML itself is malformed or an attribute that *is* present cannot be
//! parsed as the type it should have.
//!
//! Uses `quick-xml`'s pull parser (not a DOM) per [ADR 0002](../../../docs/adr/0002-rust-cloudflare-worker.md),
//! so parsing a large report never materializes a full tree before producing a result.

use std::fmt;

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::reader::Reader;

use crate::{Failure, Outcome, TestCase, TestSuite, TestSuites};

/// Parses a JUnit XML document into [`TestSuites`].
pub fn parse(xml: &[u8]) -> Result<TestSuites, ParseError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(start) => {
                let result = match local_name(&start).as_str() {
                    "testsuites" => parse_testsuites(&mut reader, &start),
                    "testsuite" => parse_testsuite(&mut reader, &start).map(|suite| TestSuites {
                        name: None,
                        suites: vec![suite],
                    }),
                    other => Err(ParseError::UnexpectedRoot(other.to_string())),
                };
                buf.clear();
                return result;
            }
            Event::Empty(start) => {
                let result = match local_name(&start).as_str() {
                    "testsuites" => Ok(TestSuites {
                        name: attr(&start, "name")?,
                        suites: Vec::new(),
                    }),
                    "testsuite" => suite_from_attrs(&start).map(|suite| TestSuites {
                        name: None,
                        suites: vec![suite],
                    }),
                    other => Err(ParseError::UnexpectedRoot(other.to_string())),
                };
                buf.clear();
                return result;
            }
            Event::Eof => return Err(ParseError::UnexpectedEof),
            Event::Decl(_)
            | Event::Comment(_)
            | Event::PI(_)
            | Event::DocType(_)
            | Event::Text(_) => {}
            Event::End(_) | Event::CData(_) => return Err(ParseError::UnexpectedEof),
        }
        buf.clear();
    }
}

fn parse_testsuites(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<TestSuites, ParseError> {
    let name = attr(start, "name")?;
    let mut suites = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(child) if local_name(&child) == "testsuite" => {
                suites.push(parse_testsuite(reader, &child)?);
            }
            Event::Empty(child) if local_name(&child) == "testsuite" => {
                suites.push(suite_from_attrs(&child)?);
            }
            Event::End(end) if local_name_qname(end.name()) == "testsuites" => break,
            Event::Eof => return Err(ParseError::UnexpectedEof),
            _ => {}
        }
        buf.clear();
    }

    Ok(TestSuites { name, suites })
}

fn parse_testsuite(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<TestSuite, ParseError> {
    let mut suite = suite_from_attrs(start)?;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(child) => match local_name(&child).as_str() {
                "testcase" => suite.test_cases.push(parse_testcase(reader, &child)?),
                "system-out" => suite.system_out = Some(read_text(reader, child.name())?),
                "system-err" => suite.system_err = Some(read_text(reader, child.name())?),
                "properties" => skip_element(reader, child.name())?,
                _ => {}
            },
            Event::Empty(child) if local_name(&child) == "testcase" => {
                suite.test_cases.push(testcase_from_attrs(&child)?);
            }
            Event::End(end) if local_name_qname(end.name()) == "testsuite" => break,
            Event::Eof => return Err(ParseError::UnexpectedEof),
            _ => {}
        }
        buf.clear();
    }

    Ok(suite)
}

fn parse_testcase(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<TestCase, ParseError> {
    let mut case = testcase_from_attrs(start)?;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(child) => match local_name(&child).as_str() {
                "failure" => case.outcome = Outcome::Failed(parse_failure_body(reader, &child)?),
                "error" => case.outcome = Outcome::Errored(parse_failure_body(reader, &child)?),
                "skipped" => {
                    let message = attr(&child, "message")?;
                    case.outcome = Outcome::Skipped(message);
                    skip_element(reader, child.name())?;
                }
                "system-out" => case.system_out = Some(read_text(reader, child.name())?),
                "system-err" => case.system_err = Some(read_text(reader, child.name())?),
                _ => {}
            },
            Event::Empty(child) => match local_name(&child).as_str() {
                "failure" => {
                    case.outcome = Outcome::Failed(Failure {
                        message: attr(&child, "message")?,
                        stack_trace: None,
                    });
                }
                "error" => {
                    case.outcome = Outcome::Errored(Failure {
                        message: attr(&child, "message")?,
                        stack_trace: None,
                    });
                }
                "skipped" => case.outcome = Outcome::Skipped(attr(&child, "message")?),
                _ => {}
            },
            Event::End(end) if local_name_qname(end.name()) == "testcase" => break,
            Event::Eof => return Err(ParseError::UnexpectedEof),
            _ => {}
        }
        buf.clear();
    }

    Ok(case)
}

/// Reads a `<failure>`/`<error>` element's `message` attribute and its text content (the stack
/// trace), consuming through the element's end tag.
fn parse_failure_body(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<Failure, ParseError> {
    let message = attr(start, "message")?;
    let text = read_text(reader, start.name())?;
    let stack_trace = if text.is_empty() { None } else { Some(text) };
    Ok(Failure {
        message,
        stack_trace,
    })
}

/// Reads an element's text content up to its matching end tag. Works for leaf elements only
/// (`<failure>`, `<system-out>`, ...); nested child elements are not expected inside them.
fn read_text(reader: &mut Reader<&[u8]>, name: QName<'_>) -> Result<String, ParseError> {
    Ok(reader.read_text(name)?.into_owned())
}

/// Skips to an element's matching end tag, discarding its content. Used for elements whose
/// content this parser does not model (e.g. `<properties>`).
fn skip_element(reader: &mut Reader<&[u8]>, name: QName<'_>) -> Result<(), ParseError> {
    reader.read_to_end(name)?;
    Ok(())
}

fn suite_from_attrs(start: &BytesStart<'_>) -> Result<TestSuite, ParseError> {
    Ok(TestSuite {
        name: attr(start, "name")?.unwrap_or_default(),
        tests: attr_u32(start, "tests")?,
        failures: attr_u32(start, "failures")?,
        errors: attr_u32(start, "errors")?,
        skipped: attr_u32(start, "skipped")?,
        time: attr_f64(start, "time")?,
        system_out: None,
        system_err: None,
        test_cases: Vec::new(),
    })
}

fn testcase_from_attrs(start: &BytesStart<'_>) -> Result<TestCase, ParseError> {
    Ok(TestCase {
        name: attr(start, "name")?.unwrap_or_default(),
        classname: attr(start, "classname")?,
        file: attr(start, "file")?,
        line: attr_u32(start, "line")?,
        time: attr_f64(start, "time")?,
        outcome: Outcome::Passed,
        system_out: None,
        system_err: None,
    })
}

fn local_name(start: &BytesStart<'_>) -> String {
    local_name_qname(start.name())
}

fn local_name_qname(name: QName<'_>) -> String {
    String::from_utf8_lossy(name.local_name().as_ref()).into_owned()
}

fn attr(start: &BytesStart<'_>, key: &str) -> Result<Option<String>, ParseError> {
    for result in start.attributes() {
        let attribute = result?;
        if attribute.key.local_name().as_ref() == key.as_bytes() {
            let value = attribute.unescape_value()?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

fn attr_u32(start: &BytesStart<'_>, key: &str) -> Result<Option<u32>, ParseError> {
    match attr(start, key)? {
        Some(value) => value
            .parse()
            .map(Some)
            .map_err(|_| ParseError::InvalidAttribute {
                attribute: key.to_string(),
                value,
            }),
        None => Ok(None),
    }
}

fn attr_f64(start: &BytesStart<'_>, key: &str) -> Result<Option<f64>, ParseError> {
    match attr(start, key)? {
        Some(value) => value
            .parse()
            .map(Some)
            .map_err(|_| ParseError::InvalidAttribute {
                attribute: key.to_string(),
                value,
            }),
        None => Ok(None),
    }
}

/// Everything that can go wrong parsing a JUnit XML document. Never panics: malformed or
/// truncated input always produces one of these variants.
#[derive(Debug)]
pub enum ParseError {
    /// The underlying XML was not well-formed (unclosed tags, bad encoding, ...).
    Xml(quick_xml::Error),
    /// The XML was well-formed but ended before a complete document was read (e.g. a truncated
    /// upload).
    UnexpectedEof,
    /// The document's root element was neither `<testsuites>` nor `<testsuite>`.
    UnexpectedRoot(String),
    /// An attribute that should hold a number did not parse as one.
    InvalidAttribute { attribute: String, value: String },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Xml(err) => write!(f, "malformed XML: {err}"),
            ParseError::UnexpectedEof => write!(f, "unexpected end of document"),
            ParseError::UnexpectedRoot(name) => {
                write!(
                    f,
                    "expected <testsuites> or <testsuite> as the root element, found <{name}>"
                )
            }
            ParseError::InvalidAttribute { attribute, value } => {
                write!(
                    f,
                    "attribute `{attribute}` has a non-numeric value: {value:?}"
                )
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl From<quick_xml::Error> for ParseError {
    fn from(err: quick_xml::Error) -> Self {
        ParseError::Xml(err)
    }
}

impl From<quick_xml::events::attributes::AttrError> for ParseError {
    fn from(err: quick_xml::events::attributes::AttrError) -> Self {
        ParseError::Xml(quick_xml::Error::InvalidAttr(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI_SUITE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="full run" tests="4" failures="1" errors="1" skipped="1" time="1.234">
  <testsuite name="pkg.unit.MathTests" tests="2" failures="1" errors="0" skipped="0" time="0.045">
    <testcase classname="pkg.unit.MathTests" name="adds_two_numbers" file="pkg/unit/math_tests.py" line="17" time="0.012"/>
    <testcase classname="pkg.unit.MathTests" name="divides_by_zero_raises" time="0.033">
      <failure message="expected ArithmeticError, got no exception" type="AssertionError">
Traceback (most recent call last):
  File "test_math.py", line 42, in divides_by_zero_raises
    assert False, "expected exception"
AssertionError: expected exception
      </failure>
    </testcase>
    <system-out>math suite starting
math suite done</system-out>
  </testsuite>
  <testsuite name="pkg.integration.DbTests" tests="2" failures="0" errors="1" skipped="1" time="1.189">
    <testcase classname="pkg.integration.DbTests" name="connects_to_primary" time="1.150">
      <error message="connection refused" type="ConnectionError">connect() failed: ECONNREFUSED 127.0.0.1:5432</error>
      <system-err>retrying... giving up after 3 attempts</system-err>
    </testcase>
    <testcase classname="pkg.integration.DbTests" name="replica_lag_within_bounds" time="0.000">
      <skipped message="no replica configured in this environment"/>
    </testcase>
  </testsuite>
</testsuites>
"#;

    #[test]
    fn parses_a_multi_suite_document() -> Result<(), ParseError> {
        let parsed = parse(MULTI_SUITE.as_bytes())?;

        assert_eq!(parsed.name.as_deref(), Some("full run"));
        assert_eq!(parsed.suites.len(), 2);

        let math = &parsed.suites[0];
        assert_eq!(math.name, "pkg.unit.MathTests");
        assert_eq!(math.tests, Some(2));
        assert_eq!(math.time, Some(0.045));
        assert_eq!(math.test_cases.len(), 2);
        assert_eq!(
            math.system_out.as_deref(),
            Some("math suite starting\nmath suite done")
        );

        let db = &parsed.suites[1];
        assert_eq!(db.name, "pkg.integration.DbTests");
        assert_eq!(db.errors, Some(1));
        assert_eq!(db.skipped, Some(1));

        Ok(())
    }

    #[test]
    fn covers_passed_failed_errored_and_skipped_outcomes() -> Result<(), ParseError> {
        let parsed = parse(MULTI_SUITE.as_bytes())?;

        let passed = &parsed.suites[0].test_cases[0];
        assert_eq!(passed.name, "adds_two_numbers");
        assert_eq!(passed.classname.as_deref(), Some("pkg.unit.MathTests"));
        assert_eq!(passed.file.as_deref(), Some("pkg/unit/math_tests.py"));
        assert_eq!(passed.line, Some(17));
        assert_eq!(passed.time, Some(0.012));
        assert_eq!(passed.outcome, Outcome::Passed);

        let failed = &parsed.suites[0].test_cases[1];
        assert_eq!(failed.name, "divides_by_zero_raises");
        assert_eq!(failed.file, None);
        assert_eq!(failed.line, None);
        assert!(matches!(&failed.outcome, Outcome::Failed(_)));
        if let Outcome::Failed(failure) = &failed.outcome {
            assert_eq!(
                failure.message.as_deref(),
                Some("expected ArithmeticError, got no exception")
            );
            let trace = failure.stack_trace.as_deref().unwrap_or_default();
            assert!(trace.contains("AssertionError: expected exception"));
            assert!(trace.contains("line 42"));
        }

        let errored = &parsed.suites[1].test_cases[0];
        assert_eq!(errored.name, "connects_to_primary");
        assert_eq!(
            errored.system_err.as_deref(),
            Some("retrying... giving up after 3 attempts")
        );
        assert!(matches!(&errored.outcome, Outcome::Errored(_)));
        if let Outcome::Errored(failure) = &errored.outcome {
            assert_eq!(failure.message.as_deref(), Some("connection refused"));
            assert_eq!(
                failure.stack_trace.as_deref(),
                Some("connect() failed: ECONNREFUSED 127.0.0.1:5432")
            );
        }

        let skipped = &parsed.suites[1].test_cases[1];
        assert_eq!(skipped.name, "replica_lag_within_bounds");
        assert_eq!(
            skipped.outcome,
            Outcome::Skipped(Some(
                "no replica configured in this environment".to_string()
            ))
        );

        Ok(())
    }

    #[test]
    fn accepts_a_bare_testsuite_root_without_the_wrapping_element() -> Result<(), ParseError> {
        let xml = r#"<testsuite name="standalone" tests="1" failures="0" errors="0" skipped="0" time="0.5">
  <testcase classname="standalone" name="only_test" time="0.5"/>
</testsuite>"#;

        let parsed = parse(xml.as_bytes())?;

        assert_eq!(parsed.name, None);
        assert_eq!(parsed.suites.len(), 1);
        assert_eq!(parsed.suites[0].name, "standalone");
        assert_eq!(parsed.suites[0].test_cases[0].outcome, Outcome::Passed);

        Ok(())
    }

    #[test]
    fn malformed_xml_returns_a_typed_error_instead_of_panicking() {
        let truncated = r#"<testsuites name="broken">
  <testsuite name="incomplete" tests="1">
    <testcase name="never_closes""#;

        let err = parse(truncated.as_bytes()).err();
        assert!(matches!(
            err,
            Some(ParseError::Xml(_)) | Some(ParseError::UnexpectedEof)
        ));
    }

    #[test]
    fn mismatched_closing_tag_returns_a_typed_error() {
        let bad = r#"<testsuites><testsuite name="x"><testcase name="y"/></testsuite></wrongname>"#;
        let err = parse(bad.as_bytes()).err();
        assert!(matches!(err, Some(ParseError::Xml(_))));
    }

    #[test]
    fn non_numeric_counter_attribute_returns_a_typed_error() {
        let bad = r#"<testsuite name="x" tests="not-a-number"></testsuite>"#;
        let err = parse(bad.as_bytes()).err();
        assert!(matches!(&err, Some(ParseError::InvalidAttribute { .. })));
        if let Some(ParseError::InvalidAttribute { attribute, value }) = err {
            assert_eq!(attribute, "tests");
            assert_eq!(value, "not-a-number");
        }
    }

    #[test]
    fn non_numeric_testcase_line_attribute_returns_a_typed_error() {
        let bad = r#"<testsuite name="x"><testcase name="y" file="y.py" line="not-a-number"/></testsuite>"#;
        let err = parse(bad.as_bytes()).err();
        assert!(matches!(&err, Some(ParseError::InvalidAttribute { .. })));
        if let Some(ParseError::InvalidAttribute { attribute, value }) = err {
            assert_eq!(attribute, "line");
            assert_eq!(value, "not-a-number");
        }
    }

    #[test]
    fn unrecognized_root_element_returns_a_typed_error() {
        let xml = r#"<coverage version="1"><packages/></coverage>"#;
        let err = parse(xml.as_bytes()).err();
        assert!(matches!(&err, Some(ParseError::UnexpectedRoot(_))));
        if let Some(ParseError::UnexpectedRoot(name)) = err {
            assert_eq!(name, "coverage");
        }
    }
}
