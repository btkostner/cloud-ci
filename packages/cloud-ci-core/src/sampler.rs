//! Turns a sequence of raw cgroup v2 reads ([`crate::cgroup::RawReading`],
//! taken 2s apart per `docs/design/analytics.md`) into the batched sample
//! collection the doc describes: CPU usage as a delta between consecutive
//! `cpu.stat` reads, `memory.current` as a live per-sample gauge,
//! `memory.peak` read once at job end (not per-sample — "the agent reads
//! it once at job end instead of tracking its own max"), and OOM
//! detection (did `memory.events`' `oom_kill` counter increase between
//! job start and job end).
//!
//! All logic here is pure given its inputs — no clock, no sleeping, no
//! file I/O — so the 2s cadence and real timestamps are the caller's
//! concern (`cloud-ci-cli`'s `agent.rs`), not this module's. Tests below
//! feed synthetic, not real-time, sequences.
//!
//! # Scope
//!
//! See `crate::cgroup`'s module doc and `cloud-ci-cli`'s `agent.rs` module
//! doc: this is only the resource-sampling piece of `cloud-ci agent`'s
//! full job, built ahead of job-spec-pulling and step-execution, which do
//! not exist yet.

use crate::cgroup::RawReading;

/// One batched resource sample, matching the field names/shapes of
/// `docs/design/analytics.md`'s "Analytics Engine schema" `sample` row
/// (`cpu_usage_usec_delta`, `memory_current_bytes`) for forward
/// compatibility, even though this round never writes to Analytics
/// Engine — `cloud-ci-core` has no Worker/cloud dependency. The row's
/// other fields (`run_id`, `job_id`, `instance_type`, `memory_peak_bytes`,
/// `repo_id`) are context the real upload path attaches later; they are
/// not part of this per-sample batch (`memory_peak_bytes` in particular
/// is job-level, not per-sample — see [`JobSummary`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub timestamp_unix_ms: u64,
    pub cpu_usage_usec_delta: u64,
    pub memory_current_bytes: u64,
}

/// The full resource-usage picture for one job, ready to attach to its
/// end-of-run Report: every batched [`Sample`], the memory high-water
/// mark (`None` if the kernel never recorded one — see
/// `crate::cgroup::parse_memory_peak`), and whether an OOM kill was
/// observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSummary {
    pub samples: Vec<Sample>,
    pub memory_peak_bytes: Option<u64>,
    pub oom_detected: bool,
}

/// Accumulates [`Sample`]s from a sequence of [`RawReading`]s taken 2s
/// apart, and reports OOM status at [`Sampler::finish`].
#[derive(Debug, Clone)]
pub struct Sampler {
    last_reading: Option<RawReading>,
    oom_kill_at_start: u64,
    samples: Vec<Sample>,
}

impl Sampler {
    /// `oom_kill_at_start` is `memory.events`' `oom_kill` counter read
    /// once at job start, the baseline [`Sampler::finish`] compares
    /// against.
    pub fn new(oom_kill_at_start: u64) -> Self {
        Self {
            last_reading: None,
            oom_kill_at_start,
            samples: Vec::new(),
        }
    }

    /// Feeds one raw tick, `timestamp_unix_ms` being when it was taken.
    ///
    /// `cpu.stat`'s `usage_usec` is a monotonic cumulative counter: the
    /// delta against the previous tick is this tick's CPU usage. If the
    /// new reading is *lower* than the previous one, the counter went
    /// backwards — the cgroup was reused/recreated (e.g. a retried job
    /// attempt reusing a container) rather than genuinely using negative
    /// CPU time — so this tick's delta is discarded (no [`Sample`] is
    /// pushed for it) instead of emitting a garbage negative number. The
    /// very first tick has no previous reading to diff against and is
    /// likewise not pushed as a `Sample`, only recorded as the baseline
    /// for the next tick's delta.
    pub fn record(&mut self, timestamp_unix_ms: u64, raw: RawReading) {
        if let Some(prev) = self.last_reading
            && raw.cpu_usage_usec >= prev.cpu_usage_usec
        {
            self.samples.push(Sample {
                timestamp_unix_ms,
                cpu_usage_usec_delta: raw.cpu_usage_usec - prev.cpu_usage_usec,
                memory_current_bytes: raw.memory_current_bytes,
            });
        }
        self.last_reading = Some(raw);
    }

    /// Samples recorded so far, in record order.
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    /// Finalizes the job: `memory_peak_bytes` is `memory.peak` read once
    /// at job end, and `oom_kill_at_end` is `memory.events`' `oom_kill`
    /// counter read at the same time — an OOM is detected if it increased
    /// since [`Sampler::new`]'s baseline.
    pub fn finish(self, memory_peak_bytes: Option<u64>, oom_kill_at_end: u64) -> JobSummary {
        JobSummary {
            samples: self.samples,
            memory_peak_bytes,
            oom_detected: oom_kill_at_end > self.oom_kill_at_start,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(cpu_usage_usec: u64, memory_current_bytes: u64) -> RawReading {
        RawReading {
            cpu_usage_usec,
            memory_current_bytes,
        }
    }

    #[test]
    fn first_tick_has_no_delta_and_is_not_sampled() {
        let mut sampler = Sampler::new(0);
        sampler.record(2000, raw(1_000_000, 1024));
        assert_eq!(sampler.samples(), &[]);
    }

    #[test]
    fn second_tick_emits_the_delta_since_the_first() {
        let mut sampler = Sampler::new(0);
        sampler.record(2000, raw(1_000_000, 1024));
        sampler.record(4000, raw(1_300_000, 2048));
        assert_eq!(
            sampler.samples(),
            &[Sample {
                timestamp_unix_ms: 4000,
                cpu_usage_usec_delta: 300_000,
                memory_current_bytes: 2048,
            }]
        );
    }

    #[test]
    fn multiple_ticks_each_emit_their_own_delta() {
        let mut sampler = Sampler::new(0);
        sampler.record(2000, raw(0, 100));
        sampler.record(4000, raw(500, 200));
        sampler.record(6000, raw(1_500, 150));
        assert_eq!(
            sampler.samples(),
            &[
                Sample {
                    timestamp_unix_ms: 4000,
                    cpu_usage_usec_delta: 500,
                    memory_current_bytes: 200,
                },
                Sample {
                    timestamp_unix_ms: 6000,
                    cpu_usage_usec_delta: 1_000,
                    memory_current_bytes: 150,
                },
            ]
        );
    }

    #[test]
    fn counter_reset_discards_that_ticks_delta_without_going_negative() {
        let mut sampler = Sampler::new(0);
        sampler.record(2000, raw(5_000_000, 1024));
        // Counter dropped: cgroup reused/recreated, not real CPU usage.
        sampler.record(4000, raw(100, 512));
        // Normal delta resumes from the post-reset baseline.
        sampler.record(6000, raw(400, 256));
        assert_eq!(
            sampler.samples(),
            &[Sample {
                timestamp_unix_ms: 6000,
                cpu_usage_usec_delta: 300,
                memory_current_bytes: 256,
            }]
        );
    }

    #[test]
    fn zero_delta_is_a_valid_sample_not_a_reset() {
        let mut sampler = Sampler::new(0);
        sampler.record(2000, raw(1_000, 10));
        sampler.record(4000, raw(1_000, 10));
        assert_eq!(
            sampler.samples(),
            &[Sample {
                timestamp_unix_ms: 4000,
                cpu_usage_usec_delta: 0,
                memory_current_bytes: 10,
            }]
        );
    }

    #[test]
    fn finish_reports_memory_peak_and_no_oom_when_counter_unchanged() {
        let mut sampler = Sampler::new(3);
        sampler.record(2000, raw(0, 10));
        sampler.record(4000, raw(100, 20));
        let summary = sampler.finish(Some(2048), 3);
        assert_eq!(summary.memory_peak_bytes, Some(2048));
        assert!(!summary.oom_detected);
        assert_eq!(summary.samples.len(), 1);
    }

    #[test]
    fn finish_detects_oom_when_counter_increased() {
        let sampler = Sampler::new(0);
        let summary = sampler.finish(Some(1024), 1);
        assert!(summary.oom_detected);
    }

    #[test]
    fn finish_preserves_unset_memory_peak() {
        let sampler = Sampler::new(0);
        let summary = sampler.finish(None, 0);
        assert_eq!(summary.memory_peak_bytes, None);
    }
}
