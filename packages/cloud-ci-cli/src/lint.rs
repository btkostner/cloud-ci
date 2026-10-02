//! Orchestrates `cloud-ci lint`: reads `--file` (default
//! `.cloud-ci/settings.yml`) off local disk and runs it through
//! `cloud_ci_core::settings::parse`, which is itself "a pure module with no
//! I/O" (`docs/design/settings.md`'s "### Validation"). This command's own
//! I/O is limited to that one local file read — no network, no GitHub API,
//! no D1 — so it is genuinely local and offline. Per this codebase's test
//! conventions (see `crate::split`'s `#[cfg(test)]` module), that makes this
//! a `cargo test`-only surface: there is no live service to smoke-test
//! against, and the fixture-file tests below exercise the real binary
//! entrypoint (`run`) end to end against real files on disk.
//!
//! See `crate::cli::LintArgs`' doc comment (this command's own `--help`
//! text) for the explicit, intentional pass-3 gap: `secrets.<pipeline>`
//! references are not checked against `.cloud-ci/pipelines/` here.

use std::fmt;
use std::fs;

use cloud_ci_core::settings::{self, Severity};

use crate::cli::LintArgs;

#[derive(Debug)]
pub struct LintError {
    message: String,
}

impl LintError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LintError {}

/// Reads and validates `args.file`, printing one line per diagnostic in
/// `path:line:col: severity: message` form (matching `rustc`/`cargo`'s own
/// `file:line:col: level: message` convention). Returns `Ok(true)` when
/// validation found zero errors (warnings do not fail the command, per
/// settings.md: clamping "only ever produces warnings, never errors" — this
/// command does not perform clamping itself, see `LintArgs`' doc comment,
/// but the same never-fails-on-warnings contract holds for any warning a
/// future pass adds), `Ok(false)` when it found at least one error, and
/// `Err` only for a problem reading the file itself (not found, permission
/// denied, ...).
pub fn run(args: &LintArgs) -> Result<bool, LintError> {
    let bytes = fs::read(&args.file)
        .map_err(|err| LintError::new(format!("could not read {}: {err}", args.file.display())))?;

    let outcome = settings::parse(&bytes);
    for diagnostic in &outcome.diagnostics {
        let level = match diagnostic.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        eprintln!(
            "{}:{}:{}: {}: {}",
            args.file.display(),
            diagnostic.line,
            diagnostic.col,
            level,
            diagnostic.message
        );
    }

    if outcome.has_errors() {
        Ok(false)
    } else {
        if outcome.diagnostics.is_empty() {
            println!("{}: ok", args.file.display());
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command};

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cloud-ci-cli-lint-test-{label}-{}",
            std::process::id()
        ))
    }

    fn parse(argv: &[&str]) -> LintArgs {
        let cli = Cli::parse_from(argv);
        let Command::Lint(args) = cli.command else {
            unreachable!("expected Command::Lint");
        };
        args
    }

    #[test]
    fn defaults_to_dot_cloud_ci_settings_yml() {
        let args = parse(&["cloud-ci", "lint"]);
        assert_eq!(args.file.to_string_lossy(), ".cloud-ci/settings.yml");
    }

    #[test]
    fn parses_explicit_file_flag() {
        let args = parse(&["cloud-ci", "lint", "--file", "custom/settings.yml"]);
        assert_eq!(args.file.to_string_lossy(), "custom/settings.yml");
    }

    #[test]
    fn valid_settings_file_exits_zero() {
        let tmp = scratch_dir("valid");
        let _ = fs::create_dir_all(&tmp);
        let file = tmp.join("settings.yml");
        let _ = fs::write(&file, "version: 1\n");

        let args = LintArgs { file: file.clone() };
        let result = run(&args);

        let _ = fs::remove_dir_all(&tmp);
        match result {
            Ok(passed) => assert!(passed, "expected a minimal valid file to pass"),
            Err(err) => unreachable!("expected Ok, got {err}"),
        }
    }

    #[test]
    fn invalid_settings_file_exits_nonzero_without_erroring() {
        let tmp = scratch_dir("invalid");
        let _ = fs::create_dir_all(&tmp);
        let file = tmp.join("settings.yml");
        let _ = fs::write(&file, "version: 2\n");

        let args = LintArgs { file: file.clone() };
        let result = run(&args);

        let _ = fs::remove_dir_all(&tmp);
        match result {
            Ok(passed) => assert!(!passed, "expected an invalid version to fail"),
            Err(err) => unreachable!("expected Ok(false), got {err}"),
        }
    }

    #[test]
    fn missing_file_is_a_run_error() {
        let tmp = scratch_dir("missing");
        let args = LintArgs {
            file: tmp.join("does-not-exist.yml"),
        };
        match run(&args) {
            Err(err) => assert!(err.to_string().contains("could not read")),
            Ok(_) => unreachable!("expected a read error for a missing file"),
        }
    }
}
