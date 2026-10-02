//! Argument parsing for the `cloud-ci` binary.
//!
//! Scope for this pass is the `upload` subcommand only, per
//! `docs/design/byo-ci.md`'s GitHub Actions example:
//!
//! ```text
//! cloud-ci upload --job test \
//!   --report junit:reports/junit.xml \
//!   --report lcov:coverage/lcov.info \
//!   --conclusion success
//! ```

use std::fmt;
use std::str::FromStr;

use clap::{Parser, Subcommand, ValueEnum};

use crate::identity::RunIdentityFlags;

#[derive(Debug, Parser)]
#[command(
    name = "cloud-ci",
    about = "Upload CI results to a cloud-ci deployment"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Upload reports/sites for one job/shard of an external (BYO) CI run.
    Upload(UploadArgs),
}

#[derive(Debug, Parser)]
pub struct UploadArgs {
    /// Job name. Required outside GitHub Actions, or sourced from
    /// `CLOUD_CI_JOB`; defaults to `$GITHUB_JOB` on GitHub Actions.
    #[arg(long)]
    pub job: Option<String>,

    /// `<kind>:<glob>`, repeatable. For example `--report junit:'reports/*.xml'`.
    #[arg(long = "report", value_name = "KIND:GLOB")]
    pub reports: Vec<ReportArg>,

    /// `<name>=<glob>`, repeatable. For example `--site docs=site-dist`.
    #[arg(long = "site", value_name = "NAME=GLOB")]
    pub sites: Vec<SiteArg>,

    /// Named Check Run(s) this job's result attaches to, repeatable.
    #[arg(long = "check")]
    pub checks: Vec<String>,

    /// Shard conclusion. Inferred from the uploaded reports if omitted.
    #[arg(long)]
    pub conclusion: Option<Conclusion>,

    /// Commit under test. Required outside GitHub Actions (`CLOUD_CI_SHA`).
    #[arg(long)]
    pub sha: Option<String>,

    /// Groups retries of "the same" run. Required outside GitHub Actions
    /// (`CLOUD_CI_RUN_KEY`).
    #[arg(long = "run-key")]
    pub run_key: Option<String>,

    /// Distinguishes re-runs under one run key. Defaults to `1`
    /// (`CLOUD_CI_ATTEMPT`).
    #[arg(long)]
    pub attempt: Option<u32>,

    /// GitHub's numeric repository id. Required outside GitHub Actions
    /// (`CLOUD_CI_REPO_ID`).
    #[arg(long = "repo-id")]
    pub repo_id: Option<u64>,

    /// Base URL of the cloud-ci deployment to upload to (`CLOUD_CI_SERVER_URL`).
    #[arg(long = "server-url")]
    pub server_url: Option<String>,

    /// Scoped API token. GitHub Actions auto-detects an OIDC token instead
    /// when this is unset (`CLOUD_CI_TOKEN`).
    #[arg(long)]
    pub token: Option<String>,
}

impl UploadArgs {
    pub fn run_identity_flags(&self) -> RunIdentityFlags {
        RunIdentityFlags {
            sha: self.sha.clone(),
            run_key: self.run_key.clone(),
            attempt: self.attempt,
            repo_id: self.repo_id,
            server_url: self.server_url.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Conclusion {
    Success,
    Failure,
    Cancelled,
}

/// One `--report <kind>:<glob>` occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportArg {
    pub kind: String,
    pub glob: String,
}

impl FromStr for ReportArg {
    type Err = ArgParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, glob) = s.split_once(':').ok_or_else(|| ArgParseError {
            message: format!("--report {s:?}: expected <kind>:<glob>"),
        })?;
        if kind.is_empty() || glob.is_empty() {
            return Err(ArgParseError {
                message: format!("--report {s:?}: kind and glob must both be non-empty"),
            });
        }
        Ok(Self {
            kind: kind.to_string(),
            glob: glob.to_string(),
        })
    }
}

/// One `--site <name>=<glob>` occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteArg {
    pub name: String,
    pub glob: String,
}

impl FromStr for SiteArg {
    type Err = ArgParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, glob) = s.split_once('=').ok_or_else(|| ArgParseError {
            message: format!("--site {s:?}: expected <name>=<glob>"),
        })?;
        if name.is_empty() || glob.is_empty() {
            return Err(ArgParseError {
                message: format!("--site {s:?}: name and glob must both be non-empty"),
            });
        }
        Ok(Self {
            name: name.to_string(),
            glob: glob.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgParseError {
    message: String,
}

impl fmt::Display for ArgParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ArgParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_arg_splits_on_first_colon() -> Result<(), ArgParseError> {
        let arg: ReportArg = "junit:reports/*.xml".parse()?;
        assert_eq!(arg.kind, "junit");
        assert_eq!(arg.glob, "reports/*.xml");
        Ok(())
    }

    #[test]
    fn report_arg_rejects_missing_separator() {
        assert!("junit-only".parse::<ReportArg>().is_err());
    }

    #[test]
    fn site_arg_splits_on_first_equals() -> Result<(), ArgParseError> {
        let arg: SiteArg = "e2e-report=apps/*/playwright-report".parse()?;
        assert_eq!(arg.name, "e2e-report");
        assert_eq!(arg.glob, "apps/*/playwright-report");
        Ok(())
    }

    #[test]
    fn site_arg_rejects_missing_separator() {
        assert!("no-equals-here".parse::<SiteArg>().is_err());
    }

    #[test]
    fn upload_parses_from_byo_ci_example() {
        let cli = Cli::parse_from([
            "cloud-ci",
            "upload",
            "--job",
            "test",
            "--report",
            "junit:reports/junit.xml",
            "--report",
            "lcov:coverage/lcov.info",
            "--conclusion",
            "success",
        ]);
        let Command::Upload(args) = cli.command;
        assert_eq!(args.job, Some("test".to_string()));
        assert_eq!(args.reports.len(), 2);
        assert_eq!(args.conclusion, Some(Conclusion::Success));
    }
}
