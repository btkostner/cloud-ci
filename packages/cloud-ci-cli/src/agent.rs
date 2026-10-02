//! Orchestrates `cloud-ci agent`: reads the per-job token, runs the 2s
//! cgroup v2 sampling loop (`cloud_ci_core::cgroup`/`cloud_ci_core::sampler`)
//! for `--duration-secs`, and prints the collected samples as JSON to
//! stdout. See `crate::cli::AgentArgs`' doc comment (this command's own
//! `--help` text) for the full scope boundary; summarized here:
//!
//! # Scope this round
//!
//! `cloud-ci agent`'s full job per `docs/architecture.md` is "pulls its job
//! spec, runs steps, streams logs, samples resource usage where available,
//! and uploads reports/artifacts through the public ingest API." This
//! module implements **only** the resource-sampling piece:
//!
//! - Pulling a job spec and running steps need a `RunCoordinator`
//!   job-spec-serving API and an `Executor`/container dispatch mechanism,
//!   neither of which exists yet (Dynamic Pipelines, Phase 2, not built).
//!   This command never attempts either.
//! - Uploading is deferred: there is no real `Report` yet to attach the
//!   samples to (that needs real step execution), so this command prints
//!   JSON to stdout instead of calling any ingest RPC.
//!
//! This is the same "capability built ahead of its full caller" pattern as
//! `cloud-ci split`'s `--strategy timing` (see `crate::split`'s module
//! doc): the sampling mechanism is genuinely self-contained and fully
//! specified independent of job-spec-pulling/step-execution, so it is
//! built now and wired into the real job runner once that exists.
//!
//! # Credential
//!
//! `RunCoordinator` mints a per-job token and injects it into the
//! container's environment as `CLOUD_CI_JOB_TOKEN`
//! (`docs/design/auth.md`'s "Per-job tokens": "never logged"). Per that
//! doc, "The in-container `cloud-ci agent` uses it exactly like a BYO-CI
//! API token against the same ingest RPCs" — so [`resolve_job_token`]
//! reads `CLOUD_CI_JOB_TOKEN` and passes it as the `explicit` credential to
//! `crate::upload::resolve_credential`, the same function `cloud-ci upload`
//! resolves its `--token`/`CLOUD_CI_TOKEN`/OIDC credential through,
//! instead of building a parallel resolution path. Resolution failing (no
//! job token, no `CLOUD_CI_TOKEN`, no GitHub Actions OIDC) is a hard error:
//! unlike `upload`, a real agent always runs with a token `RunCoordinator`
//! minted for it.

use std::fmt;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cloud_ci_core::cgroup::CgroupReader;
use cloud_ci_core::sampler::{JobSummary, Sample, Sampler};
use serde::Serialize;

use crate::cli::AgentArgs;
use crate::identity::EnvSource;
use crate::upload::resolve_credential;

/// Sampling cadence, per `docs/design/analytics.md`: "The agent samples
/// every 2 seconds".
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct AgentError {
    step: String,
    message: String,
}

impl AgentError {
    fn new(step: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            step: step.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.step, self.message)
    }
}

impl std::error::Error for AgentError {}

/// Resolves the per-job token this round's agent authenticates with — see
/// module docs' "Credential" section. `Ok(None)` means no token could be
/// resolved through any source `resolve_credential` checks.
pub(crate) fn resolve_job_token(env: &dyn EnvSource) -> Result<Option<String>, String> {
    let job_token = env.var("CLOUD_CI_JOB_TOKEN").filter(|t| !t.is_empty());
    resolve_credential(job_token.as_deref(), env)
}

/// Runs the bounded sampling loop and prints the resulting [`JobSummary`]
/// as JSON to stdout. See module docs for the full scope boundary —
/// notably, this never calls a job-spec or step-execution API, and never
/// uploads anything.
pub fn run(args: &AgentArgs, env: &dyn EnvSource) -> Result<(), AgentError> {
    let token = resolve_job_token(env).map_err(|e| AgentError::new("resolve job token", e))?;
    if token.is_none() {
        return Err(AgentError::new(
            "resolve job token",
            "no CLOUD_CI_JOB_TOKEN (or CLOUD_CI_TOKEN/GitHub Actions OIDC fallback) credential \
             found; RunCoordinator should have injected CLOUD_CI_JOB_TOKEN into this container's \
             environment",
        ));
    }
    // `token` is resolved but intentionally unused past this point: this
    // round defers uploading (see module docs' "Scope this round"), so
    // there is nothing yet to present the credential to.

    let reader = CgroupReader::new(&args.cgroup_path);
    let summary = sample_for(&reader, args.duration_secs, SAMPLE_INTERVAL, thread::sleep)?;

    let output = AgentOutput::from(summary);
    let json = serde_json::to_string_pretty(&output)
        .map_err(|e| AgentError::new("serialize samples", e.to_string()))?;
    println!("{json}");
    Ok(())
}

/// The sampling loop itself, factored out of [`run`] so tests can inject a
/// fast, non-blocking `sleep` (real wall-clock sleeping, exercised by
/// `run`, is the only thing this split skips — the rest, including every
/// `reader` call, is the real code path). `interval` is
/// [`SAMPLE_INTERVAL`] in production; tests may pass a different value to
/// pair with fixture files that simulate multiple ticks.
fn sample_for(
    reader: &CgroupReader,
    duration_secs: u64,
    interval: Duration,
    mut sleep: impl FnMut(Duration),
) -> Result<JobSummary, AgentError> {
    let oom_kill_at_start = reader
        .read_oom_kill()
        .map_err(|e| AgentError::new("read memory.events (start)", e.to_string()))?;
    let mut sampler = Sampler::new(oom_kill_at_start);

    // Baseline reading, taken immediately rather than after the first
    // sleep, so every one of the `ticks` sleeps below has a prior reading
    // to diff against and therefore emits a Sample (Sampler::record never
    // emits one for the very first reading it ever sees — see
    // cloud_ci_core::sampler's docs).
    let baseline = reader
        .read_raw()
        .map_err(|e| AgentError::new("read cgroup sample (baseline)", e.to_string()))?;
    sampler.record(now_unix_ms(), baseline);

    let interval_secs = interval.as_secs().max(1);
    let ticks = duration_secs / interval_secs;
    for _ in 0..ticks {
        sleep(interval);
        let raw = reader
            .read_raw()
            .map_err(|e| AgentError::new("read cgroup sample", e.to_string()))?;
        sampler.record(now_unix_ms(), raw);
    }

    let memory_peak_bytes = reader
        .read_memory_peak()
        .map_err(|e| AgentError::new("read memory.peak", e.to_string()))?;
    let oom_kill_at_end = reader
        .read_oom_kill()
        .map_err(|e| AgentError::new("read memory.events (end)", e.to_string()))?;

    Ok(sampler.finish(memory_peak_bytes, oom_kill_at_end))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Serializable mirror of [`JobSummary`]/[`Sample`] — kept as a thin
/// `From` wrapper here rather than deriving `Serialize` directly on the
/// `cloud-ci-core` types, since JSON-shape stability for this printed
/// output is this CLI's concern, not `cloud-ci-core`'s.
#[derive(Debug, Serialize)]
struct AgentOutput {
    samples: Vec<SampleOutput>,
    memory_peak_bytes: Option<u64>,
    oom_detected: bool,
}

#[derive(Debug, Serialize)]
struct SampleOutput {
    timestamp_unix_ms: u64,
    cpu_usage_usec_delta: u64,
    memory_current_bytes: u64,
}

impl From<Sample> for SampleOutput {
    fn from(s: Sample) -> Self {
        Self {
            timestamp_unix_ms: s.timestamp_unix_ms,
            cpu_usage_usec_delta: s.cpu_usage_usec_delta,
            memory_current_bytes: s.memory_current_bytes,
        }
    }
}

impl From<JobSummary> for AgentOutput {
    fn from(summary: JobSummary) -> Self {
        Self {
            samples: summary
                .samples
                .into_iter()
                .map(SampleOutput::from)
                .collect(),
            memory_peak_bytes: summary.memory_peak_bytes,
            oom_detected: summary.oom_detected,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command};
    use crate::identity::MapEnv;

    fn parse(argv: &[&str]) -> AgentArgs {
        let cli = Cli::parse_from(argv);
        let Command::Agent(args) = cli.command else {
            unreachable!("expected Command::Agent");
        };
        args
    }

    #[test]
    fn parses_documented_shape() {
        let args = parse(&[
            "cloud-ci",
            "agent",
            "--cgroup-path",
            "/sys/fs/cgroup/job-123",
            "--duration-secs",
            "30",
        ]);
        assert_eq!(
            args.cgroup_path,
            std::path::PathBuf::from("/sys/fs/cgroup/job-123")
        );
        assert_eq!(args.duration_secs, 30);
    }

    #[test]
    fn defaults_to_real_cgroupfs_mount_and_sixty_seconds() {
        let args = parse(&["cloud-ci", "agent"]);
        assert_eq!(args.cgroup_path, std::path::PathBuf::from("/sys/fs/cgroup"));
        assert_eq!(args.duration_secs, 60);
    }

    #[test]
    fn resolve_job_token_reads_cloud_ci_job_token() -> Result<(), String> {
        let env = MapEnv::new(&[("CLOUD_CI_JOB_TOKEN", "job-token-abc")]);
        assert_eq!(resolve_job_token(&env)?, Some("job-token-abc".to_string()));
        Ok(())
    }

    #[test]
    fn resolve_job_token_falls_back_to_cloud_ci_token_via_resolve_credential() -> Result<(), String>
    {
        // No CLOUD_CI_JOB_TOKEN set: resolve_job_token delegates entirely to
        // resolve_credential's own precedence (CLOUD_CI_TOKEN here), proving
        // it is reused rather than reimplemented.
        let env = MapEnv::new(&[("CLOUD_CI_TOKEN", "fallback-token")]);
        assert_eq!(resolve_job_token(&env)?, Some("fallback-token".to_string()));
        Ok(())
    }

    #[test]
    fn resolve_job_token_prefers_job_token_over_cloud_ci_token() -> Result<(), String> {
        let env = MapEnv::new(&[
            ("CLOUD_CI_JOB_TOKEN", "job-token"),
            ("CLOUD_CI_TOKEN", "should-not-be-used"),
        ]);
        assert_eq!(resolve_job_token(&env)?, Some("job-token".to_string()));
        Ok(())
    }

    #[test]
    fn resolve_job_token_none_when_nothing_resolves() -> Result<(), String> {
        let env = MapEnv::new(&[]);
        assert_eq!(resolve_job_token(&env)?, None);
        Ok(())
    }

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cloud-ci-cli-agent-test-{label}-{}",
            std::process::id()
        ))
    }

    fn write_fixture(dir: &std::path::Path, cpu_usage_usec: u64, memory_current: u64) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(
            dir.join("cpu.stat"),
            format!("usage_usec {cpu_usage_usec}\n"),
        );
        let _ = std::fs::write(dir.join("memory.current"), format!("{memory_current}\n"));
    }

    /// Live-ish test, per this codebase's convention of substituting
    /// synthetic fixture files for the real `/sys/fs/cgroup` pseudofiles on
    /// platforms without cgroup v2 (this dev environment is macOS, which
    /// has neither `/sys/fs/cgroup` nor cgroup v2 — see this module's
    /// commit message / task report for the platform check). `sleep` is
    /// injected as a no-op that instead mutates the fixture files between
    /// ticks, simulating 2s-apart reads without a real 2s wall-clock wait
    /// per tick.
    #[test]
    fn sample_for_drives_the_real_file_reading_path_against_synthetic_fixtures()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = scratch_dir("sample-loop");
        write_fixture(&dir, 1_000_000, 1024);
        std::fs::write(dir.join("memory.peak"), "max\n")?;
        std::fs::write(dir.join("memory.events"), "oom_kill 0\n")?;

        // Each injected "sleep" call advances the fixture to the next
        // tick's readings before sample_for's next reader.read_raw() call,
        // in order.
        let dir_for_sleep = dir.clone();
        let mut ordered = vec![(1_300_000u64, 2048u64), (1_900_000u64, 1536u64)].into_iter();
        let sleep = move |_: Duration| {
            if let Some((cpu, mem)) = ordered.next() {
                write_fixture(&dir_for_sleep, cpu, mem);
            }
        };

        let reader = CgroupReader::new(&dir);
        let summary = sample_for(&reader, 4, Duration::from_secs(2), sleep)?;

        assert_eq!(summary.samples.len(), 2);
        assert_eq!(summary.samples[0].cpu_usage_usec_delta, 300_000);
        assert_eq!(summary.samples[0].memory_current_bytes, 2048);
        assert_eq!(summary.samples[1].cpu_usage_usec_delta, 600_000);
        assert_eq!(summary.samples[1].memory_current_bytes, 1536);
        assert_eq!(summary.memory_peak_bytes, None);
        assert!(!summary.oom_detected);

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn sample_for_detects_oom_between_start_and_end() -> Result<(), Box<dyn std::error::Error>> {
        let dir = scratch_dir("sample-oom");
        write_fixture(&dir, 0, 0);
        std::fs::write(dir.join("memory.peak"), "4096\n")?;
        std::fs::write(dir.join("memory.events"), "oom_kill 0\n")?;

        let dir_for_sleep = dir.clone();
        let sleep = move |_: Duration| {
            // Bump the OOM counter mid-run, as the real kernel would after a
            // step gets OOM-killed.
            let _ = std::fs::write(dir_for_sleep.join("memory.events"), "oom_kill 1\n");
        };

        let reader = CgroupReader::new(&dir);
        let summary = sample_for(&reader, 2, Duration::from_secs(2), sleep)?;

        assert!(summary.oom_detected);
        assert_eq!(summary.memory_peak_bytes, Some(4096));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn run_errors_when_no_credential_resolves() {
        let dir = scratch_dir("run-no-token");
        write_fixture(&dir, 0, 0);
        let _ = std::fs::write(dir.join("memory.peak"), "max\n");
        let _ = std::fs::write(dir.join("memory.events"), "oom_kill 0\n");

        let args = AgentArgs {
            cgroup_path: dir.clone(),
            duration_secs: 0,
        };
        let env = MapEnv::new(&[]);
        let result = run(&args, &env);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
