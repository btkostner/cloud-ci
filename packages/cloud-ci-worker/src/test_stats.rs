//! Read-only `test_stats` queries backing `GetTestTimings`
//! (`cloud-ci split --strategy timing`'s real data source, per
//! docs/design/parallelization.md's "`cloud-ci split`" section).
//!
//! `test_stats` (migration 0011, written by
//! `coordinator::mod::finalize_test_stats`) has one row per
//! `(repo_id, test_id)` — per-*test*, not per-file. `cloud-ci split`'s
//! granularity this round is whole files (`cloud-ci-cli::split`'s module
//! docs: `--granularity test` is not yet implemented), so a file's
//! effective historical duration is derived by **summing**
//! `duration_ewma_ms` across every test row sharing that `file_path`, not
//! `MAX`/`AVG`:
//!
//! - `cloud-ci split --strategy timing` assigns a whole file to one shard,
//!   and the test runners this strategy targets (`go test`, `cargo test`,
//!   `mocha` — docs/design/parallelization.md's "Split strategies" table)
//!   run every test in a file sequentially within that shard's process.
//!   The shard's wall-clock cost for that file is the sum of its tests'
//!   durations, not the duration of its single slowest test (`MAX`, which
//!   would silently undercount any file with more than one test) or the
//!   mean per-test duration (`AVG`, which ignores how many tests the file
//!   actually contains).
//!
//! Same `json_each(?)`-over-a-JSON-array idiom
//! `coordinator::mod::finalize_test_stats` already uses to batch an
//! unbounded list of rows into one D1 statement instead of one query per
//! item (D1's 1000-query-per-invocation cap, same doc section).
//!
//! The D1-touching [`lookup_file_timings`] needs the Workers runtime and
//! is only exercised by the live smoke test (`mise run
//! //packages/cloud-ci-worker:dev`), same posture as every other D1 reader
//! in this crate (see e.g. `installations.rs`'s module docs). Row parsing
//! is factored into the pure, `cargo test`-covered
//! [`parse_file_timing_rows`], same layering as
//! `installations::list_all_repo_ids`'s manual `serde_json::Value`
//! extraction.

use worker::Env;
use worker::wasm_bindgen::JsValue;

/// One file's aggregated historical duration, derived from `test_stats`
/// per this module's doc comment.
#[derive(Debug, Clone, PartialEq)]
pub struct FileTimingRow {
    pub file_path: String,
    pub duration_ms: u64,
}

/// Parses the raw D1 result rows from [`lookup_file_timings`]'s query
/// (`file_path`, `total_ms` columns) into typed [`FileTimingRow`]s. Pure,
/// so it is unit-tested with plain `cargo test` without the Workers
/// runtime — same layering as `installations::list_all_repo_ids`'s row
/// mapping.
pub fn parse_file_timing_rows(
    rows: Vec<serde_json::Value>,
) -> worker::Result<Vec<FileTimingRow>> {
    rows.into_iter()
        .map(|row| {
            let file_path = row
                .get("file_path")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    worker::Error::RustError("test_stats timing row missing file_path".into())
                })?;
            let duration_ms = row
                .get("total_ms")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| {
                    worker::Error::RustError("test_stats timing row missing total_ms".into())
                })?
                .round() as u64;
            Ok(FileTimingRow {
                file_path,
                duration_ms,
            })
        })
        .collect()
}

/// Sums `test_stats.duration_ewma_ms` per `file_path`, scoped to `repo_id`
/// and restricted to `file_paths` (the `--files` glob's matched set).
/// Files with no `test_stats` rows simply have no entry in the result —
/// `cloud_ci_core::split::HistoryLookup`'s "unknown duration" contract,
/// not an error. Returns an empty `Vec` without querying D1 at all when
/// `file_paths` is empty.
pub async fn lookup_file_timings(
    env: &Env,
    repo_id: u64,
    file_paths: &[String],
) -> worker::Result<Vec<FileTimingRow>> {
    if file_paths.is_empty() {
        return Ok(Vec::new());
    }

    let db = env.d1("DB")?;
    let paths_json = serde_json::to_string(file_paths)
        .map_err(|e| worker::Error::RustError(format!("encoding file_paths: {e}")))?;
    let rows: Vec<serde_json::Value> = db
        .prepare(
            "SELECT test_stats.file_path AS file_path, \
                    SUM(test_stats.duration_ewma_ms) AS total_ms \
             FROM test_stats, json_each(?2) AS requested \
             WHERE test_stats.repo_id = ?1 AND test_stats.file_path = requested.value \
             GROUP BY test_stats.file_path",
        )
        .bind(&[JsValue::from_f64(repo_id as f64), JsValue::from_str(&paths_json)])?
        .all()
        .await?
        .results()?;
    parse_file_timing_rows(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_rows() {
        let rows = vec![
            serde_json::json!({"file_path": "a_test.go", "total_ms": 1200.0}),
            serde_json::json!({"file_path": "b_test.go", "total_ms": 450.5}),
        ];
        let parsed = parse_file_timing_rows(rows).expect("rows should parse");
        assert_eq!(
            parsed,
            vec![
                FileTimingRow {
                    file_path: "a_test.go".to_string(),
                    duration_ms: 1200,
                },
                FileTimingRow {
                    file_path: "b_test.go".to_string(),
                    duration_ms: 451,
                },
            ]
        );
    }

    #[test]
    fn empty_rows_parse_to_empty_vec() {
        assert_eq!(parse_file_timing_rows(Vec::new()).unwrap(), Vec::new());
    }

    #[test]
    fn missing_file_path_is_an_error() {
        let rows = vec![serde_json::json!({"total_ms": 100.0})];
        assert!(parse_file_timing_rows(rows).is_err());
    }

    #[test]
    fn missing_total_ms_is_an_error() {
        let rows = vec![serde_json::json!({"file_path": "a_test.go"})];
        assert!(parse_file_timing_rows(rows).is_err());
    }
}
