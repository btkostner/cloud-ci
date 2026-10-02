//! Domain model for third-party CI report parsing.
//!
//! JUnit XML and Vitest's JSON reporter are in scope so far; other report kinds named in
//! `docs/architecture.md`'s package table (Playwright, lcov, cobertura, ...) are later work and
//! intentionally have no surface here yet.

pub mod junit;
pub mod vitest;

/// A parsed `<testsuites>` document: zero or more [`TestSuite`]s.
///
/// JUnit files in the wild sometimes omit the wrapping `<testsuites>` element and use a bare
/// `<testsuite>` as the document root; [`junit::parse`] normalizes both shapes into this type,
/// with `name` left `None` when there was no wrapping element to name.
#[derive(Debug, Clone, PartialEq)]
pub struct TestSuites {
    pub name: Option<String>,
    pub suites: Vec<TestSuite>,
}

/// One `<testsuite>` element: a named group of [`TestCase`]s plus the suite-level counters and
/// captured console output JUnit writers emit alongside them.
#[derive(Debug, Clone, PartialEq)]
pub struct TestSuite {
    pub name: String,
    /// `tests` attribute, if present and parseable. Not derived from `test_cases.len()`: a
    /// writer's declared count and the number of `<testcase>` children it actually wrote can
    /// disagree, and callers may want to detect that.
    pub tests: Option<u32>,
    pub failures: Option<u32>,
    pub errors: Option<u32>,
    pub skipped: Option<u32>,
    /// `time` attribute in seconds.
    pub time: Option<f64>,
    pub system_out: Option<String>,
    pub system_err: Option<String>,
    pub test_cases: Vec<TestCase>,
}

/// One `<testcase>` element.
#[derive(Debug, Clone, PartialEq)]
pub struct TestCase {
    pub name: String,
    pub classname: Option<String>,
    /// `file` attribute: the source file the test is defined in. Emitted by writers that
    /// follow xUnit's legacy `testcase` attribute family (e.g. pytest's `--junitxml` output);
    /// not part of every writer's output. [`vitest::parse`] also populates this, from the
    /// enclosing test file's path.
    pub file: Option<String>,
    /// `line` attribute: the line in `file` the test is defined at. Same provenance as `file`.
    pub line: Option<u32>,
    /// `time` attribute in seconds.
    pub time: Option<f64>,
    pub outcome: Outcome,
    pub system_out: Option<String>,
    pub system_err: Option<String>,
}

/// What happened when a test case ran.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Passed,
    /// Child `<failure>` element: an assertion the test itself raised.
    Failed(Failure),
    /// Child `<error>` element: an unexpected error the test did not assert on (JUnit
    /// distinguishes this from a `<failure>`).
    Errored(Failure),
    /// Child `<skipped>` element, carrying its optional `message` attribute.
    Skipped(Option<String>),
}

/// The body of a `<failure>` or `<error>` element.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// `message` attribute.
    pub message: Option<String>,
    /// Element text content, typically a stack trace.
    pub stack_trace: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_variants_are_distinguishable() {
        let passed = Outcome::Passed;
        let failed = Outcome::Failed(Failure {
            message: Some("boom".to_string()),
            stack_trace: None,
        });
        let errored = Outcome::Errored(Failure {
            message: None,
            stack_trace: Some("at line 1".to_string()),
        });
        let skipped = Outcome::Skipped(Some("not implemented on this platform".to_string()));

        assert_eq!(passed, Outcome::Passed);
        assert_ne!(failed, errored);
        assert_ne!(failed, passed);

        assert!(matches!(&failed, Outcome::Failed(f) if f.message.as_deref() == Some("boom")));
        assert!(matches!(
            &skipped,
            Outcome::Skipped(Some(msg)) if msg == "not implemented on this platform"
        ));
    }

    #[test]
    fn test_suite_holds_declared_counters_independent_of_case_list() {
        let suite = TestSuite {
            name: "pkg/unit".to_string(),
            tests: Some(3),
            failures: Some(1),
            errors: Some(0),
            skipped: Some(0),
            time: Some(0.42),
            system_out: None,
            system_err: None,
            test_cases: Vec::new(),
        };

        assert_eq!(suite.tests, Some(3));
        assert_eq!(suite.test_cases.len(), 0);
        assert_eq!(suite.time, Some(0.42));
    }
}
