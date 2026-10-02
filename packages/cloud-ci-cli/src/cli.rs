//! Argument parsing for the `cloud-ci` binary.
//!
//! Two subcommands: `upload`, per `docs/design/byo-ci.md`'s GitHub Actions
//! example:
//!
//! ```text
//! cloud-ci upload --job test \
//!   --report junit:reports/junit.xml \
//!   --report lcov:coverage/lcov.info \
//!   --conclusion success
//! ```
//!
//! and `split`, per `docs/design/parallelization.md`'s "`cloud-ci split`
//! (also usable from BYO CI)":
//!
//! ```text
//! cloud-ci split --strategy timing --shards 4 --index 2 \
//!   --files 'tests/**/*.spec.ts' > shard-files.txt
//! ```
//!
//! and `lint`, per `docs/design/settings.md`'s "### Validation": validates
//! `.cloud-ci/settings.yml` against passes 1 and 2 only (YAML 1.2 core
//! schema + duplicate/unknown-key errors, then enum/pattern/numeric-bound
//! semantic checks). Pass 3 — whether each `secrets.<pipeline>` name
//! actually exists under `.cloud-ci/pipelines/` — needs the live pipeline
//! tree (a GitHub API read) that this local, offline command does not have,
//! and is INTENTIONALLY NOT checked here; see `LintArgs`' doc comment,
//! which is this command's own `--help` text.
//!
//! ```text
//! cloud-ci lint --file .cloud-ci/settings.yml
//! ```
//!
//! and `agent`, per `docs/design/analytics.md`'s CLI section — **this
//! round's `agent` is a scope-limited placeholder**, see `AgentArgs`' doc
//! comment (its own `--help` text) and `crate::agent`'s module doc for the
//! full boundary:
//!
//! ```text
//! cloud-ci agent --cgroup-path /sys/fs/cgroup --duration-secs 60
//! ```

use std::fmt;
use std::path::PathBuf;
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

/// `cloud-ci setup allowed-orgs --add <login> | --remove <login> [--file
/// <path>] [--deploy]`, per `docs/design/auth.md`'s "Org allowlist
/// changes" paragraph. This is the only `setup` subcommand this round:
/// `setup github-app` (architecture.md's package table lists it alongside
/// `setup allowed-orgs`) needs a real browser + GitHub App manifest flow
/// that does not exist yet, so it is intentionally omitted rather than
/// stubbed — a `Setup` variant with only one working leaf is simpler than
/// a `Setup` variant with a leaf that errors out on use.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Upload reports/sites for one job/shard of an external (BYO) CI run.
    Upload(UploadArgs),
    /// Compute one shard's deterministic file assignment (BYO CI matrices).
    Split(SplitArgs),
    /// Validate `.cloud-ci/settings.yml` (YAML + semantic checks only).
    Lint(LintArgs),
    /// Sample cgroup v2 resource usage for a bounded duration and print the
    /// collected samples as JSON. See `AgentArgs`' own `--help` text for
    /// this round's scope limitation.
    Agent(AgentArgs),
    /// Deployment-operator configuration changes. See `SetupCommand`'s doc
    /// comment.
    #[command(subcommand)]
    Setup(SetupCommand),
}

/// `cloud-ci setup <subcommand>`. Only `allowed-orgs` exists this round —
/// see `AllowedOrgsArgs`' doc comment for why `setup github-app` is not
/// here yet.
#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// Add or remove a login from the deployed `GITHUB_ALLOWED_ORGS`
    /// allowlist in `wrangler.toml`.
    AllowedOrgs(AllowedOrgsArgs),
}

/// `cloud-ci setup allowed-orgs (--add <login> | --remove <login>)
/// [--file <path>] [--deploy]`, per `docs/design/auth.md`'s "Org allowlist
/// changes" paragraph: "`cloud-ci setup allowed-orgs --add <login>` (or
/// `--remove`) is the real write path: it edits `GITHUB_ALLOWED_ORGS` in
/// `wrangler.toml` with the operator's own Cloudflare credentials and runs
/// `wrangler deploy`".
///
/// # One login per invocation
///
/// The doc's own CLI sketch shows a single `--add <login>` (or
/// `--remove <login>`); nothing in auth.md suggests either flag repeats.
/// `--add` and `--remove` are mutually exclusive (clap's `conflicts_with`)
/// and exactly one is required per invocation — the simpler, defensible
/// reading, and it keeps the printed before/after diff (see `run`'s doc
/// comment) unambiguous: one login added or removed, one line of diff.
///
/// # `--deploy` is opt-in, diverging from the doc's literal wording
///
/// auth.md's prose describes this command as running `wrangler deploy`
/// unconditionally. This CLI instead edits the file, prints the old/new
/// `GITHUB_ALLOWED_ORGS` diff, and only invokes `wrangler deploy` itself
/// when `--deploy` is passed — otherwise it prints the file change and
/// tells the operator to run `wrangler deploy` themselves. A command that
/// silently pushes a real, mutating deploy to a real Cloudflare account by
/// default is too easy to trigger by accident (e.g. retrying after a typo
/// in `--add`); an explicit opt-in flag matches how this codebase treats
/// other deploy-time-only operations it cannot safely exercise outside a
/// real Cloudflare account (see `run`'s module doc for the corresponding
/// test boundary).
#[derive(Debug, Parser)]
#[command(group(clap::ArgGroup::new("allowed_orgs_op").required(true).args(["add", "remove"])))]
pub struct AllowedOrgsArgs {
    /// Add this login to `GITHUB_ALLOWED_ORGS`. Mutually exclusive with
    /// `--remove`.
    #[arg(long, conflicts_with = "remove")]
    pub add: Option<String>,

    /// Remove this login from `GITHUB_ALLOWED_ORGS`. Mutually exclusive
    /// with `--add`.
    #[arg(long)]
    pub remove: Option<String>,

    /// Path to the `wrangler.toml` to edit. Defaults to the deployed
    /// worker's own config file, `packages/cloud-ci-worker/wrangler.toml`
    /// — the file `GITHUB_ALLOWED_ORGS` actually lives in — rather than a
    /// bare `wrangler.toml` in the current directory, since this command
    /// is meant to be run from the repo root (or anywhere), not only from
    /// inside `packages/cloud-ci-worker`.
    #[arg(long, default_value = "packages/cloud-ci-worker/wrangler.toml")]
    pub file: std::path::PathBuf,

    /// Run `wrangler deploy --config <file>` (without changing the
    /// process's own working directory — `wrangler.toml`'s `[build]`
    /// comment documents that its `cwd` is repo-root-relative and expects
    /// every invoker to already be running from the repo root, same as
    /// `--file`'s own default below) after a successful file edit. Off
    /// by default; see `AllowedOrgsArgs`' doc comment for why.
    #[arg(long)]
    pub deploy: bool,
}

/// `cloud-ci lint [--file <path>]`: validates `settings.yml` against
/// `docs/design/settings.md`'s content-dependent passes only —
///
/// 1. YAML 1.2 core schema parsing (duplicate-map-key and
///    unknown-key-with-suggestion errors), and
/// 2. semantic checks (enum values, name patterns, numeric bounds).
///
/// **Does NOT check `secrets.<pipeline>` against `.cloud-ci/pipelines/`**
/// (settings.md's validation pass 3): that check needs the live pipeline
/// file tree from GitHub, which this local, offline, no-network command
/// has no access to. A `secrets:` entry naming a pipeline that doesn't
/// exist will lint clean here; it is only caught by the Worker's
/// `cloud-ci / config` check against the repo's actual default branch.
///
/// Exits non-zero only if validation found errors; warnings (deployment
/// -wide-bound clamping, which this command does not perform — it has no
/// deployment context — see `cloud_ci_core::settings`' module docs) never
/// fail the command.
#[derive(Debug, Parser)]
pub struct LintArgs {
    /// Path to the settings file to validate.
    #[arg(long, default_value = ".cloud-ci/settings.yml")]
    pub file: std::path::PathBuf,
}

/// `cloud-ci agent --cgroup-path <path> --duration-secs <n>`.
///
/// # This round is a scope-limited placeholder, not a real job runner
///
/// `cloud-ci agent`'s full job per `docs/architecture.md` is "pulls its
/// job spec, runs steps, streams logs, samples resource usage where
/// available, and uploads reports/artifacts through the public ingest
/// API." Only the **resource-sampling** piece exists this round:
///
/// - **"pulls its job spec"**: NOT implemented. `RunCoordinator` has no
///   job-spec-serving API yet — Dynamic Pipelines, the thing that would
///   define what steps to run, is explicitly Phase 2 and not built.
/// - **"runs steps"**: NOT implemented, for the same reason (no
///   `Executor`/container dispatch mechanism exists to run steps against).
/// - **"samples resource usage"**: implemented — this command reads the
///   cgroup v2 pseudofiles under `--cgroup-path` every 2s for
///   `--duration-secs`, via `cloud_ci_core::cgroup`/`cloud_ci_core::sampler`
///   (`docs/design/analytics.md`'s "What is collected" table).
/// - **"uploads reports/artifacts"**: deferred. Uploading needs a real
///   `Report` to attach the collected samples to (`docs/design/analytics.md`:
///   samples are emitted "as part of the job's end-of-run Report"), which
///   needs real step execution to produce. This command instead prints the
///   collected samples as JSON to stdout on exit, so the sampling +
///   credential-reading mechanism is provably exercised without inventing
///   fake job execution.
///
/// The per-job token `RunCoordinator` mints and injects as `CLOUD_CI_JOB_TOKEN`
/// (`docs/design/auth.md`'s "Per-job tokens") is read and resolved through
/// the same `resolve_credential` path `cloud-ci upload` uses, proving the
/// credential-reading mechanism works — but, per the deferred-upload note
/// above, nothing is sent over the wire with it yet.
#[derive(Debug, Parser)]
pub struct AgentArgs {
    /// Root of the cgroup v2 hierarchy to sample from. Defaults to the
    /// real cgroupfs mount; override for testing against a directory of
    /// synthetic fixture files in the same `cpu.stat`/`memory.current`/
    /// `memory.peak`/`memory.events` format.
    #[arg(long, default_value = "/sys/fs/cgroup")]
    pub cgroup_path: PathBuf,

    /// How long to run the 2s sampling loop before printing results and
    /// exiting. Stands in for the real job's lifetime, since there is no
    /// real step execution to bound this on yet — see this struct's own
    /// doc comment.
    #[arg(long, default_value_t = 60)]
    pub duration_secs: u64,
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

/// `cloud-ci split --strategy timing|file|count --shards <N> --index <1..N>
/// --files <glob> [--granularity file|test]`, per
/// `docs/design/parallelization.md`'s "`cloud-ci split` (also usable from
/// BYO CI)". Shard count here is always a fixed integer (`1..64`, clamped
/// by `cloud_ci_core::split::resolve_shard_count`) — the `{min, max,
/// target}` auto-sizing spec is part of `ci.shard`'s script-level API, not
/// this CLI's documented surface.
#[derive(Debug, Parser)]
pub struct SplitArgs {
    /// Split strategy. `timing` currently degrades to `file` order for
    /// every run — see `crate::split`'s module docs for why.
    #[arg(long)]
    pub strategy: SplitStrategy,

    /// Total shard count, `1..64` (values outside that range are clamped).
    #[arg(long)]
    pub shards: u32,

    /// 1-based shard index to print the file list for.
    #[arg(long)]
    pub index: u32,

    /// Glob matched against the universe of files to divide. A glob
    /// matching no files is an error.
    #[arg(long)]
    pub files: String,

    /// Item granularity. `test` is not yet implemented (see `crate::split`'s
    /// module docs).
    #[arg(long, value_enum, default_value = "file")]
    pub granularity: Granularity,

    /// Scoped API token. GitHub Actions auto-detects an OIDC token instead
    /// when this is unset (`CLOUD_CI_TOKEN`). Accepted and resolved for a
    /// future `test_stats` lookup; unused by this round's `timing` fallback
    /// — see `crate::split`'s module docs.
    #[arg(long)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum SplitStrategy {
    Timing,
    File,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Granularity {
    File,
    Test,
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
        let Command::Upload(args) = cli.command else {
            unreachable!("expected Command::Upload");
        };
        assert_eq!(args.job, Some("test".to_string()));
        assert_eq!(args.reports.len(), 2);
        assert_eq!(args.conclusion, Some(Conclusion::Success));
    }
}
