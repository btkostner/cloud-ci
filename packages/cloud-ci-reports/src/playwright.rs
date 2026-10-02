//! Parser for Playwright's `--reporter=json` output.
//!
//! Unlike JUnit or Vitest, Playwright's JSON report nests `suites` (recursively, one level per
//! `test.describe` block under a file-level suite) → `specs` (one per test title) → `tests` (one
//! per project a spec ran under) → `results` (one per attempt, since Playwright can retry a
//! failed test). Confirmed against `microsoft/playwright` at tag `v1.58.2`:
//! `packages/playwright/src/reporters/json.ts`
//! (<https://github.com/microsoft/playwright/blob/v1.58.2/packages/playwright/src/reporters/json.ts>)
//! for how the report is built, and `packages/playwright/types/testReporter.d.ts` for the wire
//! types (`JSONReport`, `JSONReportSuite`, `JSONReportSpec`, `JSONReportTest`,
//! `JSONReportTestResult`). Per-attempt status is `"passed" | "failed" | "timedOut" | "skipped" |
//! "interrupted"` (`TestResult.status`); the aggregate status Playwright computes across all of a
//! spec's attempts is a *different* vocabulary, `"skipped" | "expected" | "unexpected" |
//! "flaky"` (`TestCase.outcome()` / `JSONReportTest.status`) — this parser only reads the
//! per-attempt statuses and derives flakiness itself from [`TestCase::attempts`], so it does not
//! need to reconcile the two vocabularies.
//!
//! One top-level `JSONReportSuite` (Playwright emits one per test file, already merged across
//! projects — see `_mergeSuites` in the source above) becomes one [`TestSuite`]; nested
//! `describe`-block suites are flattened into that file's `test_cases`, with the describe
//! titles recorded in [`TestCase::classname`]. A spec that ran under multiple projects (e.g.
//! `chromium` and `firefox`) produces one [`TestCase`] per project, disambiguated by appending
//! the project name to `classname`, since collapsing them would silently drop which project
//! failed.
//!
//! # Retry handling
//!
//! Playwright retries are the one place this parser needs more than JUnit's `TestCase` shape
//! provides, so [`TestCase::attempts`] was added to the shared model (see its doc comment).
//! `outcome` always reflects the *last* `results[]` entry — the attempt Playwright itself treats
//! as authoritative — and `attempts` is `Some` (holding every attempt, oldest first) only when a
//! spec was actually retried (`results.len() > 1`), so the common non-retried case stays as
//! lean as JUnit/Vitest output. A `Some` whose last attempt passed but an earlier one did not is
//! a flaky test.
//!
//! Uses `serde_json`'s `#[derive(Deserialize)]` for the same reason as [`crate::vitest`]: this is
//! JSON, not XML, so [ADR 0002](../../../docs/adr/0002-rust-cloudflare-worker.md)'s streaming
//! rationale for the JUnit parser does not apply here.

use std::fmt;

use serde::Deserialize;

use crate::{Attempt, Failure, Outcome, TestCase, TestSuite, TestSuites};

/// Parses a Playwright `--reporter=json` document into [`TestSuites`].
pub fn parse(json: &[u8]) -> Result<TestSuites, ParseError> {
    let raw: RawReport = serde_json::from_slice(json)?;
    let suites = raw.suites.into_iter().map(suite_from_raw).collect();
    Ok(TestSuites { name: None, suites })
}

fn suite_from_raw(suite: RawSuite) -> TestSuite {
    let mut test_cases = Vec::new();
    collect_test_cases(&suite, &[], &mut test_cases);

    TestSuite {
        name: suite.file,
        tests: None,
        failures: None,
        errors: None,
        skipped: None,
        time: None,
        system_out: None,
        system_err: None,
        test_cases,
    }
}

/// Walks a suite tree depth-first, flattening every spec under every nested `describe` suite
/// into `out`, accumulating the chain of describe titles for [`TestCase::classname`].
fn collect_test_cases(suite: &RawSuite, ancestors: &[String], out: &mut Vec<TestCase>) {
    let mut path = ancestors.to_vec();
    path.push(suite.title.clone());

    for spec in &suite.specs {
        for test in &spec.tests {
            out.push(testcase_from_raw(spec, test, &path));
        }
    }
    for child in &suite.suites {
        collect_test_cases(child, &path, out);
    }
}

fn testcase_from_raw(spec: &RawSpec, test: &RawTest, describe_path: &[String]) -> TestCase {
    let mut classname_parts = describe_path.to_vec();
    classname_parts.push(test.project_name.clone());
    let classname = Some(classname_parts.join(" > "));

    let attempts: Vec<Attempt> = test
        .results
        .iter()
        .map(|result| Attempt {
            outcome: outcome_from_result(result, &test.annotations),
            time: Some(result.duration / 1000.0),
        })
        .collect();

    let (outcome, time) = match attempts.last() {
        Some(last) => (last.outcome.clone(), last.time),
        // Playwright should always emit at least one result per test; fall back to the
        // spec-level skip status rather than guessing at an outcome.
        None => (Outcome::Skipped(skip_reason(&test.annotations)), None),
    };

    TestCase {
        name: spec.title.clone(),
        classname,
        file: Some(spec.file.clone()),
        line: Some(spec.line),
        time,
        outcome,
        system_out: last_stdio(&test.results, |r| &r.stdout),
        system_err: last_stdio(&test.results, |r| &r.stderr),
        attempts: if attempts.len() > 1 {
            Some(attempts)
        } else {
            None
        },
    }
}

fn last_stdio(
    results: &[RawTestResult],
    select: impl Fn(&RawTestResult) -> &[RawStdio],
) -> Option<String> {
    let entries = select(results.last()?);
    let text: String = entries
        .iter()
        .filter_map(|entry| match entry {
            RawStdio::Text { text } => Some(text.as_str()),
            RawStdio::Buffer { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() { None } else { Some(text) }
}

fn skip_reason(annotations: &[RawAnnotation]) -> Option<String> {
    annotations
        .iter()
        .find(|a| a.kind == "skip" || a.kind == "fixme")
        .and_then(|a| a.description.clone())
}

/// Converts one Playwright attempt's status into the shared [`Outcome`] vocabulary. A timeout or
/// an interruption is modeled as [`Outcome::Errored`] (not [`Outcome::Failed`]): like JUnit's
/// `<error>`, these are things that went wrong around the test rather than an assertion the test
/// itself raised.
fn outcome_from_result(result: &RawTestResult, annotations: &[RawAnnotation]) -> Outcome {
    match result.status {
        Some(RawAttemptStatus::Passed) | None => Outcome::Passed,
        Some(RawAttemptStatus::Failed) => Outcome::Failed(failure_from_result(result)),
        Some(RawAttemptStatus::TimedOut) => {
            Outcome::Errored(failure_from_result_or("test timed out", result))
        }
        Some(RawAttemptStatus::Interrupted) => {
            Outcome::Errored(failure_from_result_or("test run was interrupted", result))
        }
        Some(RawAttemptStatus::Skipped) => Outcome::Skipped(skip_reason(annotations)),
    }
}

/// Builds a [`Failure`] from an attempt's `errors` (falling back to the single deprecated
/// `error` field), joining every error so a test with multiple soft-assertion failures does not
/// silently drop any of them.
fn failure_from_result(result: &RawTestResult) -> Failure {
    let errors: Vec<&RawTestError> = if result.errors.is_empty() {
        result.error.iter().collect()
    } else {
        result.errors.iter().collect()
    };

    let message = errors.first().and_then(|e| e.message.clone());
    let stack_trace = if errors.is_empty() {
        None
    } else {
        let joined = errors
            .iter()
            .filter_map(|e| e.stack.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    };

    Failure {
        message,
        stack_trace,
    }
}

/// Like [`failure_from_result`], but falls back to `default_message` when Playwright recorded no
/// error message (timeouts and interruptions often have no `error` at all).
fn failure_from_result_or(default_message: &str, result: &RawTestResult) -> Failure {
    let failure = failure_from_result(result);
    if failure.message.is_some() || failure.stack_trace.is_some() {
        failure
    } else {
        Failure {
            message: Some(default_message.to_string()),
            stack_trace: None,
        }
    }
}

#[derive(Deserialize)]
struct RawReport {
    suites: Vec<RawSuite>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSuite {
    title: String,
    file: String,
    #[serde(default)]
    specs: Vec<RawSpec>,
    #[serde(default)]
    suites: Vec<RawSuite>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSpec {
    title: String,
    file: String,
    line: u32,
    tests: Vec<RawTest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTest {
    project_name: String,
    results: Vec<RawTestResult>,
    #[serde(default)]
    annotations: Vec<RawAnnotation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTestResult {
    status: Option<RawAttemptStatus>,
    duration: f64,
    error: Option<RawTestError>,
    #[serde(default)]
    errors: Vec<RawTestError>,
    #[serde(default)]
    stdout: Vec<RawStdio>,
    #[serde(default)]
    stderr: Vec<RawStdio>,
}

#[derive(Deserialize)]
struct RawTestError {
    message: Option<String>,
    stack: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawStdio {
    Text {
        text: String,
    },
    /// Binary stdio chunks are base64-encoded and not surfaced in the shared model; this variant
    /// exists only so `serde(untagged)` can distinguish and skip them instead of erroring.
    Buffer {
        #[allow(dead_code)]
        buffer: String,
    },
}

#[derive(Deserialize)]
struct RawAnnotation {
    #[serde(rename = "type")]
    kind: String,
    description: Option<String>,
}

#[derive(Deserialize, Clone, Copy)]
enum RawAttemptStatus {
    #[serde(rename = "passed")]
    Passed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "timedOut")]
    TimedOut,
    #[serde(rename = "skipped")]
    Skipped,
    #[serde(rename = "interrupted")]
    Interrupted,
}

/// Everything that can go wrong parsing a Playwright JSON report. Never panics: malformed input
/// always produces this variant.
#[derive(Debug)]
pub enum ParseError {
    /// The input was not valid JSON, or did not match the Playwright JSON reporter's shape.
    Json(serde_json::Error),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Json(err) => write!(f, "malformed Playwright JSON report: {err}"),
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

    const MULTI_SUITE: &str = r#"{
  "config": {},
  "suites": [
    {
      "title": "math.spec.ts",
      "file": "e2e/math.spec.ts",
      "column": 0,
      "line": 0,
      "specs": [
        {
          "title": "adds two numbers",
          "ok": true,
          "tags": [],
          "id": "1",
          "file": "e2e/math.spec.ts",
          "line": 5,
          "column": 1,
          "tests": [
            {
              "timeout": 30000,
              "annotations": [],
              "expectedStatus": "passed",
              "projectName": "chromium",
              "projectId": "chromium",
              "status": "expected",
              "results": [
                {
                  "workerIndex": 0,
                  "parallelIndex": 0,
                  "status": "passed",
                  "duration": 42.0,
                  "error": null,
                  "errors": [],
                  "stdout": [{ "text": "starting math suite\n" }],
                  "stderr": [],
                  "retry": 0,
                  "startTime": "2024-01-01T00:00:00.000Z",
                  "attachments": [],
                  "annotations": []
                }
              ]
            }
          ]
        }
      ],
      "suites": [
        {
          "title": "division",
          "file": "e2e/math.spec.ts",
          "column": 1,
          "line": 10,
          "specs": [
            {
              "title": "flakes on first attempt",
              "ok": true,
              "tags": [],
              "id": "2",
              "file": "e2e/math.spec.ts",
              "line": 11,
              "column": 3,
              "tests": [
                {
                  "timeout": 30000,
                  "annotations": [],
                  "expectedStatus": "passed",
                  "projectName": "chromium",
                  "projectId": "chromium",
                  "status": "flaky",
                  "results": [
                    {
                      "workerIndex": 0,
                      "parallelIndex": 0,
                      "status": "failed",
                      "duration": 15.0,
                      "error": { "message": "expect(received).toBe(expected)", "stack": "at e2e/math.spec.ts:12:5" },
                      "errors": [{ "message": "expect(received).toBe(expected)", "stack": "at e2e/math.spec.ts:12:5" }],
                      "stdout": [],
                      "stderr": [],
                      "retry": 0,
                      "startTime": "2024-01-01T00:00:00.000Z",
                      "attachments": [],
                      "annotations": []
                    },
                    {
                      "workerIndex": 0,
                      "parallelIndex": 0,
                      "status": "passed",
                      "duration": 11.0,
                      "error": null,
                      "errors": [],
                      "stdout": [],
                      "stderr": [],
                      "retry": 1,
                      "startTime": "2024-01-01T00:00:00.100Z",
                      "attachments": [],
                      "annotations": []
                    }
                  ]
                }
              ]
            },
            {
              "title": "times out",
              "ok": false,
              "tags": [],
              "id": "3",
              "file": "e2e/math.spec.ts",
              "line": 20,
              "column": 3,
              "tests": [
                {
                  "timeout": 30000,
                  "annotations": [],
                  "expectedStatus": "passed",
                  "projectName": "chromium",
                  "projectId": "chromium",
                  "status": "unexpected",
                  "results": [
                    {
                      "workerIndex": 0,
                      "parallelIndex": 0,
                      "status": "timedOut",
                      "duration": 30000.0,
                      "error": null,
                      "errors": [],
                      "stdout": [],
                      "stderr": [{ "text": "waiting for selector\n" }],
                      "retry": 0,
                      "startTime": "2024-01-01T00:00:00.200Z",
                      "attachments": [],
                      "annotations": []
                    }
                  ]
                }
              ]
            },
            {
              "title": "not run on this platform",
              "ok": true,
              "tags": [],
              "id": "4",
              "file": "e2e/math.spec.ts",
              "line": 30,
              "column": 3,
              "tests": [
                {
                  "timeout": 30000,
                  "annotations": [{ "type": "skip", "description": "windows only" }],
                  "expectedStatus": "skipped",
                  "projectName": "chromium",
                  "projectId": "chromium",
                  "status": "skipped",
                  "results": [
                    {
                      "workerIndex": -1,
                      "parallelIndex": -1,
                      "status": "skipped",
                      "duration": 0.0,
                      "error": null,
                      "errors": [],
                      "stdout": [],
                      "stderr": [],
                      "retry": 0,
                      "startTime": "2024-01-01T00:00:00.300Z",
                      "attachments": [],
                      "annotations": [{ "type": "skip", "description": "windows only" }]
                    }
                  ]
                }
              ]
            }
          ]
        }
      ]
    }
  ],
  "errors": [],
  "stats": { "startTime": "2024-01-01T00:00:00.000Z", "duration": 100.0, "expected": 2, "unexpected": 1, "flaky": 1, "skipped": 1 }
}"#;

    #[test]
    fn parses_a_multi_suite_document_with_nested_describe_blocks() -> Result<(), ParseError> {
        let parsed = parse(MULTI_SUITE.as_bytes())?;

        assert_eq!(parsed.name, None);
        assert_eq!(parsed.suites.len(), 1);

        let file = &parsed.suites[0];
        assert_eq!(file.name, "e2e/math.spec.ts");
        // One spec at the file level, three nested under the "division" describe block.
        assert_eq!(file.test_cases.len(), 4);

        Ok(())
    }

    #[test]
    fn covers_passed_failed_timedout_and_skipped_outcomes() -> Result<(), ParseError> {
        let parsed = parse(MULTI_SUITE.as_bytes())?;
        let cases = &parsed.suites[0].test_cases;

        let passed = &cases[0];
        assert_eq!(passed.name, "adds two numbers");
        assert_eq!(passed.classname.as_deref(), Some("math.spec.ts > chromium"));
        assert_eq!(passed.file.as_deref(), Some("e2e/math.spec.ts"));
        assert_eq!(passed.line, Some(5));
        assert_eq!(passed.outcome, Outcome::Passed);
        assert_eq!(passed.system_out.as_deref(), Some("starting math suite\n"));
        assert_eq!(passed.attempts, None);

        let timed_out = &cases[2];
        assert_eq!(timed_out.name, "times out");
        assert!(matches!(&timed_out.outcome, Outcome::Errored(_)));
        if let Outcome::Errored(failure) = &timed_out.outcome {
            assert_eq!(failure.message.as_deref(), Some("test timed out"));
        }
        assert_eq!(
            timed_out.system_err.as_deref(),
            Some("waiting for selector\n")
        );

        let skipped = &cases[3];
        assert_eq!(skipped.name, "not run on this platform");
        assert_eq!(
            skipped.outcome,
            Outcome::Skipped(Some("windows only".to_string()))
        );

        Ok(())
    }

    #[test]
    fn flaky_test_retains_every_attempt() -> Result<(), ParseError> {
        let parsed = parse(MULTI_SUITE.as_bytes())?;
        let flaky = &parsed.suites[0].test_cases[1];

        assert_eq!(flaky.name, "flakes on first attempt");
        assert_eq!(
            flaky.classname.as_deref(),
            Some("math.spec.ts > division > chromium")
        );
        // Final outcome reflects the passing retry, not the first failed attempt.
        assert_eq!(flaky.outcome, Outcome::Passed);

        assert!(flaky.attempts.is_some());
        if let Some(attempts) = &flaky.attempts {
            assert_eq!(attempts.len(), 2);
            assert!(matches!(attempts[0].outcome, Outcome::Failed(_)));
            assert_eq!(attempts[1].outcome, Outcome::Passed);
            if let Outcome::Failed(failure) = &attempts[0].outcome {
                assert_eq!(
                    failure.message.as_deref(),
                    Some("expect(received).toBe(expected)")
                );
                assert_eq!(
                    failure.stack_trace.as_deref(),
                    Some("at e2e/math.spec.ts:12:5")
                );
            }
        }

        Ok(())
    }

    #[test]
    fn malformed_json_returns_a_typed_error_instead_of_panicking() {
        let err = parse(b"{ not json").err();
        assert!(matches!(err, Some(ParseError::Json(_))));
    }

    #[test]
    fn missing_required_field_returns_a_typed_error() {
        let err = parse(br#"{"suites": [{"title": "x"}]}"#).err();
        assert!(matches!(err, Some(ParseError::Json(_))));
    }
}
