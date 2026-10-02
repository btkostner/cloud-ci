//! Parser for Vitest's `--reporter=json` output.
//!
//! Vitest's JSON reporter deliberately mirrors Jest's `--json` output "for compatibility
//! reasons" (its own source comment) rather than defining a bespoke shape: a root object with
//! per-file `testResults`, each holding per-test `assertionResults`. Confirmed against
//! `vitest-dev/vitest` `packages/vitest/src/node/reporters/json.ts` at tag `v4.1.11`
//! (<https://github.com/vitest-dev/vitest/blob/v4.1.11/packages/vitest/src/node/reporters/json.ts>).
//! Unlike JUnit, every field this parser reads is always present on the wire (Vitest always
//! serializes the full shape); the only true optionals are `duration`, `location`, and
//! `failureMessages`, which are `null`/absent for tests that never ran long enough, were never
//! located, or never failed.
//!
//! One Vitest test *file* becomes one [`TestSuite`]; there is no wrapping document name, so
//! [`TestSuites::name`] is always `None`.
//!
//! Uses `serde_json`'s `#[derive(Deserialize)]` rather than `quick-xml`'s streaming approach:
//! this is JSON, not XML, so [ADR 0002](../../../docs/adr/0002-rust-cloudflare-worker.md)'s
//! DOM-avoidance rationale for the JUnit parser does not apply, and a typed struct is the more
//! maintainable option for a dependency already pulled in across the repo.

use std::fmt;

use serde::Deserialize;

use crate::{Failure, Outcome, TestCase, TestSuite, TestSuites};

/// Parses a Vitest (or Jest-compatible) `--reporter=json` document into [`TestSuites`].
pub fn parse(json: &[u8]) -> Result<TestSuites, ParseError> {
    let raw: RawReport = serde_json::from_slice(json)?;
    let suites = raw.test_results.into_iter().map(suite_from_raw).collect();
    Ok(TestSuites { name: None, suites })
}

fn suite_from_raw(file: RawTestResult) -> TestSuite {
    let failures = file
        .assertion_results
        .iter()
        .filter(|a| a.status == RawStatus::Failed)
        .count() as u32;
    let skipped = file
        .assertion_results
        .iter()
        .filter(|a| {
            matches!(
                a.status,
                RawStatus::Skipped | RawStatus::Pending | RawStatus::Todo | RawStatus::Disabled
            )
        })
        .count() as u32;
    let tests = file.assertion_results.len() as u32;
    let time = Some((file.end_time - file.start_time) / 1000.0);

    TestSuite {
        name: file.name.clone(),
        tests: Some(tests),
        failures: Some(failures),
        errors: None,
        skipped: Some(skipped),
        time,
        system_out: None,
        system_err: None,
        test_cases: file
            .assertion_results
            .into_iter()
            .map(|assertion| testcase_from_raw(&file.name, assertion))
            .collect(),
    }
}

fn testcase_from_raw(file_name: &str, assertion: RawAssertionResult) -> TestCase {
    let classname = if assertion.ancestor_titles.is_empty() {
        None
    } else {
        Some(assertion.ancestor_titles.join(" > "))
    };

    TestCase {
        name: assertion.title,
        classname,
        file: Some(file_name.to_string()),
        line: assertion.location.map(|loc| loc.line),
        time: assertion.duration.map(|ms| ms / 1000.0),
        outcome: outcome_from_raw(assertion.status, assertion.failure_messages),
        system_out: None,
        system_err: None,
        attempts: None,
    }
}

/// Converts a Vitest assertion result's status into the shared [`Outcome`] vocabulary.
///
/// Vitest's `failureMessages` are already-formatted strings (each one is the failed assertion's
/// `error.stack || error.message`, per the reporter source), not a separate short-message plus
/// stack-trace pair the way JUnit's `<failure message="...">...</failure>` is. We take the first
/// line of the first message as [`Failure::message`] (a short summary, matching JUnit's
/// attribute) and every message joined by blank lines as [`Failure::stack_trace`] (the full
/// detail, matching JUnit's element text) so a test with multiple failed assertions does not
/// silently drop any of them.
fn outcome_from_raw(status: RawStatus, failure_messages: Option<Vec<String>>) -> Outcome {
    match status {
        RawStatus::Passed => Outcome::Passed,
        RawStatus::Failed => {
            let messages = failure_messages.unwrap_or_default();
            let message = messages
                .first()
                .and_then(|m| m.lines().next())
                .map(str::to_string);
            let stack_trace = if messages.is_empty() {
                None
            } else {
                Some(messages.join("\n\n"))
            };
            Outcome::Failed(Failure {
                message,
                stack_trace,
            })
        }
        RawStatus::Skipped | RawStatus::Pending | RawStatus::Disabled => Outcome::Skipped(None),
        // Vitest's `todo` status has no JUnit equivalent; it is a declared-but-unimplemented
        // test, closest in spirit to a skip with a known reason.
        RawStatus::Todo => Outcome::Skipped(Some("todo".to_string())),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReport {
    test_results: Vec<RawTestResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTestResult {
    name: String,
    start_time: f64,
    end_time: f64,
    assertion_results: Vec<RawAssertionResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAssertionResult {
    #[serde(default)]
    ancestor_titles: Vec<String>,
    status: RawStatus,
    title: String,
    duration: Option<f64>,
    #[serde(default)]
    failure_messages: Option<Vec<String>>,
    location: Option<RawLocation>,
}

#[derive(Deserialize)]
struct RawLocation {
    line: u32,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RawStatus {
    Passed,
    Failed,
    Skipped,
    Pending,
    Todo,
    Disabled,
}

/// Everything that can go wrong parsing a Vitest JSON report. Never panics: malformed input
/// always produces this variant.
#[derive(Debug)]
pub enum ParseError {
    /// The input was not valid JSON, or did not match the Vitest JSON reporter's shape.
    Json(serde_json::Error),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Json(err) => write!(f, "malformed Vitest JSON report: {err}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<serde_json::Error> for ParseError {
    fn from(err: serde_json::Error) -> Self {
        ParseError::Json(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI_FILE: &str = r#"{
  "numTotalTestSuites": 2,
  "numPassedTestSuites": 1,
  "numFailedTestSuites": 1,
  "numPendingTestSuites": 0,
  "numTotalTests": 4,
  "numPassedTests": 2,
  "numFailedTests": 1,
  "numPendingTests": 1,
  "numTodoTests": 0,
  "startTime": 1700000000000,
  "success": false,
  "testResults": [
    {
      "name": "src/math.test.ts",
      "status": "failed",
      "message": "",
      "startTime": 1700000000000,
      "endTime": 1700000000120,
      "assertionResults": [
        {
          "ancestorTitles": ["math", "addition"],
          "fullName": "math addition adds two numbers",
          "status": "passed",
          "title": "adds two numbers",
          "duration": 3.2,
          "failureMessages": [],
          "location": { "line": 12, "column": 3 },
          "meta": {},
          "tags": []
        },
        {
          "ancestorTitles": ["math", "division"],
          "fullName": "math division divides by zero",
          "status": "failed",
          "title": "divides by zero",
          "duration": 5.1,
          "failureMessages": [
            "AssertionError: expected Infinity to equal NaN\n    at src/math.test.ts:20:18"
          ],
          "location": { "line": 20, "column": 18 },
          "meta": {},
          "tags": []
        }
      ]
    },
    {
      "name": "src/string.test.ts",
      "status": "passed",
      "message": "",
      "startTime": 1700000000200,
      "endTime": 1700000000260,
      "assertionResults": [
        {
          "ancestorTitles": ["string"],
          "fullName": "string trims whitespace",
          "status": "passed",
          "title": "trims whitespace",
          "duration": 1.0,
          "failureMessages": [],
          "location": null,
          "meta": {},
          "tags": []
        },
        {
          "ancestorTitles": ["string"],
          "fullName": "string not yet implemented",
          "status": "pending",
          "title": "not yet implemented",
          "duration": null,
          "failureMessages": [],
          "location": null,
          "meta": {},
          "tags": []
        }
      ]
    }
  ],
  "snapshot": {
    "added": 0, "failure": false, "filesAdded": 0, "filesRemoved": 0, "filesRemovedList": [],
    "filesUnmatched": 0, "filesUpdated": 0, "matched": 0, "total": 0, "unchecked": 0,
    "uncheckedKeysByFile": [], "unmatched": 0, "updated": 0, "didUpdate": false
  }
}"#;

    #[test]
    fn parses_a_multi_file_document() -> Result<(), ParseError> {
        let parsed = parse(MULTI_FILE.as_bytes())?;

        assert_eq!(parsed.name, None);
        assert_eq!(parsed.suites.len(), 2);

        let math = &parsed.suites[0];
        assert_eq!(math.name, "src/math.test.ts");
        assert_eq!(math.tests, Some(2));
        assert_eq!(math.failures, Some(1));
        assert_eq!(math.skipped, Some(0));
        assert_eq!(math.time, Some(0.12));
        assert_eq!(math.test_cases.len(), 2);

        let strings = &parsed.suites[1];
        assert_eq!(strings.name, "src/string.test.ts");
        assert_eq!(strings.tests, Some(2));
        assert_eq!(strings.failures, Some(0));
        assert_eq!(strings.skipped, Some(1));

        Ok(())
    }

    #[test]
    fn covers_passed_failed_and_skipped_outcomes() -> Result<(), ParseError> {
        let parsed = parse(MULTI_FILE.as_bytes())?;

        let passed = &parsed.suites[0].test_cases[0];
        assert_eq!(passed.name, "adds two numbers");
        assert_eq!(passed.classname.as_deref(), Some("math > addition"));
        assert_eq!(passed.file.as_deref(), Some("src/math.test.ts"));
        assert_eq!(passed.line, Some(12));
        assert_eq!(passed.time, Some(0.0032));
        assert_eq!(passed.outcome, Outcome::Passed);
        assert_eq!(passed.attempts, None);

        let failed = &parsed.suites[0].test_cases[1];
        assert_eq!(failed.name, "divides by zero");
        assert!(matches!(&failed.outcome, Outcome::Failed(_)));
        if let Outcome::Failed(failure) = &failed.outcome {
            assert_eq!(
                failure.message.as_deref(),
                Some("AssertionError: expected Infinity to equal NaN")
            );
            assert!(
                failure
                    .stack_trace
                    .as_deref()
                    .unwrap_or_default()
                    .contains("src/math.test.ts:20:18")
            );
        }

        let skipped = &parsed.suites[1].test_cases[1];
        assert_eq!(skipped.name, "not yet implemented");
        assert_eq!(skipped.outcome, Outcome::Skipped(None));
        assert_eq!(skipped.file.as_deref(), Some("src/string.test.ts"));

        Ok(())
    }

    #[test]
    fn todo_status_maps_to_skipped_with_a_todo_message() {
        let outcome = outcome_from_raw(RawStatus::Todo, None);
        assert_eq!(outcome, Outcome::Skipped(Some("todo".to_string())));
    }

    #[test]
    fn malformed_json_returns_a_typed_error_instead_of_panicking() {
        let err = parse(b"{ not json").err();
        assert!(matches!(err, Some(ParseError::Json(_))));
    }

    #[test]
    fn missing_required_field_returns_a_typed_error() {
        let err = parse(br#"{"testResults": [{"name": "x.test.ts"}]}"#).err();
        assert!(matches!(err, Some(ParseError::Json(_))));
    }
}
