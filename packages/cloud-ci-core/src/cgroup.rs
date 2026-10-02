//! Pure parsers for the three cgroup v2 pseudofiles `cloud-ci agent` samples
//! (`docs/design/analytics.md`'s "### What is collected" table and the
//! "cgroup v2 fields used: ..." paragraph immediately after it; field
//! semantics sourced from docs.kernel.org cgroup-v2.rst's
//! `cpu.stat`/`memory.peak`/`memory.events` sections, cross-checked
//! 2026-09-30 per that doc):
//!
//! - `cpu.stat`: `key value\n` pairs, one per line; `usage_usec` is the
//!   cumulative CPU-time counter (microseconds) the agent diffs between
//!   samples to compute instantaneous CPU usage.
//! - `memory.current`: a single integer, newline-terminated — the live
//!   resident-memory gauge, sampled directly with no delta needed.
//! - `memory.peak`: a single integer, the high-water mark since the
//!   cgroup was created — OR the literal string `max` if the kernel has
//!   not recorded one yet (unset; `parse_memory_peak` returns `None` for
//!   this case). Per analytics.md, the agent reads this once at job end
//!   rather than tracking its own running max, since the container's
//!   cgroup is created fresh per job.
//! - `memory.events`: same `key value\n` format as `cpu.stat`; `oom_kill`
//!   increments every time the kernel OOM-killer fires inside this
//!   cgroup.
//!
//! Parsing (below) is pure given file *contents* as a `&str` — no I/O,
//! fully unit-testable. [`CgroupReader`] is the thin I/O wrapper that
//! reads the real pseudofiles from a cgroup directory and feeds their
//! contents through these parsers; it is what the `cloud-ci agent` CLI
//! subcommand (`cloud-ci-cli`'s `agent.rs`) actually calls.
//!
//! # Scope
//!
//! This module, and [`crate::sampler`] built on top of it, implement only
//! the "samples resource usage where available" piece of `cloud-ci
//! agent`'s job (`docs/architecture.md`). Pulling a job spec and running
//! steps are explicitly out of scope this round — see `cloud-ci-cli`'s
//! `agent.rs` module doc for the full boundary and why.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// A pseudofile's content did not match the expected `cgroup-v2.rst`
/// format (missing key, non-numeric value, etc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupParseError(String);

impl fmt::Display for CgroupParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CgroupParseError {}

/// Parses one `key value\n`-per-line pseudofile (`cpu.stat`,
/// `memory.events`) and returns the value for `key`. Unknown keys are
/// ignored — the kernel is free to add more without breaking this parser
/// — and a missing or non-numeric `key` is an error naming what went
/// wrong.
fn parse_key_value_u64(contents: &str, key: &str) -> Result<u64, CgroupParseError> {
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(found_key) = parts.next() else {
            continue;
        };
        if found_key != key {
            continue;
        }
        let value = parts
            .next()
            .ok_or_else(|| CgroupParseError(format!("key {key:?} has no value: {line:?}")))?;
        return value
            .parse::<u64>()
            .map_err(|e| CgroupParseError(format!("key {key:?} value {value:?}: {e}")));
    }
    Err(CgroupParseError(format!(
        "key {key:?} not found in: {contents:?}"
    )))
}

/// `cpu.stat`'s `usage_usec` field: cumulative CPU time, in microseconds,
/// since the cgroup was created. Monotonic absent a counter reset — see
/// [`crate::sampler`] for how deltas are computed from consecutive reads.
pub fn parse_cpu_stat_usage_usec(contents: &str) -> Result<u64, CgroupParseError> {
    parse_key_value_u64(contents, "usage_usec")
}

/// `memory.current`: a single integer, newline-terminated.
pub fn parse_memory_current(contents: &str) -> Result<u64, CgroupParseError> {
    contents
        .trim()
        .parse::<u64>()
        .map_err(|e| CgroupParseError(format!("memory.current {contents:?}: {e}")))
}

/// `memory.peak`: a single integer, OR the literal string `max` if the
/// kernel has not recorded a peak yet (unset — see module docs). `None`
/// means "max"/unset; `Some(bytes)` is the recorded high-water mark.
pub fn parse_memory_peak(contents: &str) -> Result<Option<u64>, CgroupParseError> {
    let trimmed = contents.trim();
    if trimmed == "max" {
        return Ok(None);
    }
    trimmed
        .parse::<u64>()
        .map(Some)
        .map_err(|e| CgroupParseError(format!("memory.peak {contents:?}: {e}")))
}

/// `memory.events`' `oom_kill` field: a monotonic counter the kernel
/// increments every time the OOM-killer fires inside this cgroup.
pub fn parse_memory_events_oom_kill(contents: &str) -> Result<u64, CgroupParseError> {
    parse_key_value_u64(contents, "oom_kill")
}

/// One raw reading of the two per-sample pseudofiles (`cpu.stat`'s
/// `usage_usec`, `memory.current`) at a point in time, before any
/// delta/peak/OOM logic is applied. [`crate::sampler::Sampler`]'s input;
/// `memory.peak` and `memory.events` are read separately, once at job
/// start/end rather than per-sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawReading {
    pub cpu_usage_usec: u64,
    pub memory_current_bytes: u64,
}

/// Failure reading or parsing one of the cgroup pseudofiles.
#[derive(Debug)]
pub struct CgroupReadError {
    pub file: &'static str,
    pub source: CgroupReadErrorSource,
}

#[derive(Debug)]
pub enum CgroupReadErrorSource {
    Io(io::Error),
    Parse(CgroupParseError),
}

impl fmt::Display for CgroupReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            CgroupReadErrorSource::Io(e) => write!(f, "reading {}: {e}", self.file),
            CgroupReadErrorSource::Parse(e) => write!(f, "parsing {}: {e}", self.file),
        }
    }
}

impl std::error::Error for CgroupReadError {}

/// Thin I/O wrapper around the pure parsers above: reads the pseudofiles
/// from a cgroup directory — the real `/sys/fs/cgroup/<path>` mount on
/// Linux with cgroup v2, or (for tests, and for development on platforms
/// without cgroup v2) a plain directory containing files of the same
/// `key value\n`/single-integer format.
#[derive(Debug, Clone)]
pub struct CgroupReader {
    base: PathBuf,
}

impl CgroupReader {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    fn read(&self, file: &'static str) -> Result<String, CgroupReadError> {
        std::fs::read_to_string(self.base.join(file)).map_err(|e| CgroupReadError {
            file,
            source: CgroupReadErrorSource::Io(e),
        })
    }

    /// Reads the two per-sample pseudofiles together: `cpu.stat`'s
    /// `usage_usec` and `memory.current`. Called every 2s by the sampling
    /// loop.
    pub fn read_raw(&self) -> Result<RawReading, CgroupReadError> {
        let cpu_stat = self.read("cpu.stat")?;
        let cpu_usage_usec = parse_cpu_stat_usage_usec(&cpu_stat).map_err(|e| CgroupReadError {
            file: "cpu.stat",
            source: CgroupReadErrorSource::Parse(e),
        })?;
        let memory_current = self.read("memory.current")?;
        let memory_current_bytes =
            parse_memory_current(&memory_current).map_err(|e| CgroupReadError {
                file: "memory.current",
                source: CgroupReadErrorSource::Parse(e),
            })?;
        Ok(RawReading {
            cpu_usage_usec,
            memory_current_bytes,
        })
    }

    /// Reads `memory.peak`. Called once at job end, not per-sample — see
    /// module docs.
    pub fn read_memory_peak(&self) -> Result<Option<u64>, CgroupReadError> {
        let contents = self.read("memory.peak")?;
        parse_memory_peak(&contents).map_err(|e| CgroupReadError {
            file: "memory.peak",
            source: CgroupReadErrorSource::Parse(e),
        })
    }

    /// Reads `memory.events`' `oom_kill` counter. Called once at job
    /// start (baseline) and once at job end (to detect an increase).
    pub fn read_oom_kill(&self) -> Result<u64, CgroupReadError> {
        let contents = self.read("memory.events")?;
        parse_memory_events_oom_kill(&contents).map_err(|e| CgroupReadError {
            file: "memory.events",
            source: CgroupReadErrorSource::Parse(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_stat_usage_usec() {
        let contents = "usage_usec 1234567\nuser_usec 1000000\nsystem_usec 234567\n";
        assert_eq!(parse_cpu_stat_usage_usec(contents), Ok(1234567));
    }

    #[test]
    fn cpu_stat_missing_key_is_an_error() {
        let contents = "user_usec 1000000\nsystem_usec 234567\n";
        assert!(parse_cpu_stat_usage_usec(contents).is_err());
    }

    #[test]
    fn cpu_stat_malformed_value_is_an_error() {
        let contents = "usage_usec not-a-number\n";
        assert!(parse_cpu_stat_usage_usec(contents).is_err());
    }

    #[test]
    fn cpu_stat_empty_content_is_an_error() {
        assert!(parse_cpu_stat_usage_usec("").is_err());
    }

    #[test]
    fn parses_memory_current() {
        assert_eq!(parse_memory_current("104857600\n"), Ok(104857600));
    }

    #[test]
    fn memory_current_malformed_is_an_error() {
        assert!(parse_memory_current("not-a-number\n").is_err());
    }

    #[test]
    fn parses_memory_peak_integer() {
        assert_eq!(parse_memory_peak("209715200\n"), Ok(Some(209715200)));
    }

    #[test]
    fn memory_peak_max_is_unset() {
        assert_eq!(parse_memory_peak("max\n"), Ok(None));
    }

    #[test]
    fn memory_peak_malformed_is_an_error() {
        assert!(parse_memory_peak("not-max-or-a-number\n").is_err());
    }

    #[test]
    fn parses_memory_events_oom_kill() {
        let contents = "low 0\nhigh 0\nmax 0\noom 0\noom_kill 2\noom_group_kill 0\n";
        assert_eq!(parse_memory_events_oom_kill(contents), Ok(2));
    }

    #[test]
    fn memory_events_missing_oom_kill_is_an_error() {
        let contents = "low 0\nhigh 0\n";
        assert!(parse_memory_events_oom_kill(contents).is_err());
    }

    #[test]
    fn reader_reads_synthetic_fixture_files() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "cloud-ci-cgroup-test-{}-{}",
            std::process::id(),
            "reader-reads-synthetic-fixture-files"
        ));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("cpu.stat"), "usage_usec 500\n")?;
        std::fs::write(dir.join("memory.current"), "1024\n")?;
        std::fs::write(dir.join("memory.peak"), "max\n")?;
        std::fs::write(dir.join("memory.events"), "oom_kill 0\n")?;

        let reader = CgroupReader::new(&dir);
        let raw = reader.read_raw()?;
        assert_eq!(raw.cpu_usage_usec, 500);
        assert_eq!(raw.memory_current_bytes, 1024);
        assert_eq!(reader.read_memory_peak()?, None);
        assert_eq!(reader.read_oom_kill()?, 0);

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn reader_surfaces_io_error_for_missing_file() {
        let dir = std::env::temp_dir().join(format!(
            "cloud-ci-cgroup-test-{}-{}",
            std::process::id(),
            "reader-surfaces-io-error"
        ));
        let reader = CgroupReader::new(&dir);
        let err = reader.read_raw();
        assert!(matches!(
            err,
            Err(CgroupReadError {
                file: "cpu.stat",
                source: CgroupReadErrorSource::Io(_)
            })
        ));
    }
}
