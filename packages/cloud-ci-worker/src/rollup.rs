//! `*/15` rollup cron (docs/design/analytics.md § "Data flow"'s
//! `Cron: */15 rollup` node, "Querying for rollups uses the SQL API", and
//! "D1 rollup tables"'s `run_rollups`/`insights` schemas, migration
//! `0015_run_rollups_and_insights.sql`).
//!
//! **Scope boundary** (do not read this module as the full rollup cron
//! analytics.md describes): only the `test` Analytics Engine event kind is
//! wired this round (`coordinator::mod::write_test_events`) — `step`/
//! `sample`/`cache` are not, and are tracked as separate, later work. This
//! module's cron queries only `test`-kind rows and writes only what that
//! data honestly supports:
//!
//! - `run_rollups.run_id`/`repo_id`/`head_sha`/`started_at`/`status` are
//!   real, read directly from the existing `runs` table (the only
//!   already-written source for them — the `test` Analytics Engine row
//!   shape carries no `sha` or run-start-time field at all).
//! - `run_rollups.duration_ms`/`queue_ms`/`critical_path_ms`/
//!   `cache_hit_rate`/`cost_usd_estimate` are deliberately left `NULL` on
//!   every row this cron writes. See [`build_run_rollup`]'s doc comment
//!   for exactly why `duration_ms`/`critical_path_ms` cannot be honestly
//!   derived from `test`-kind rows even though Analytics Engine
//!   implicitly timestamps every data point. `queue_ms`/`critical_path_ms`
//!   need `step` events (analytics.md's `step` row carries `duration_ms`/
//!   `queue_ms` directly); `cache_hit_rate` needs `cache` events;
//!   `cost_usd_estimate` needs `sample` events (CPU-active-time and
//!   instance type, per analytics.md's "Cost estimation" formula). None of
//!   those three kinds are wired, so none of those columns are fabricated
//!   here.
//! - `insights` gets one new, additive kind this round, `run_test_failures`
//!   (see [`build_insight`]) — a minimal, honest starting slice, not
//!   analytics.md's full `slow_test`/`flaky_test`/`duration_regression`/
//!   `queue_regression`/`cache_degraded`/`duration_improvement`/
//!   `sizing_improvement` taxonomy, every member of which needs a
//!   historical baseline (7/30-day window, prior branch average, prior
//!   instance size) this round's single-run `test`-only query cannot
//!   support. `flaky_test` specifically is **not** this cron's job even
//!   though it is listed as a `*/15`-cron-owned kind in analytics.md:
//!   `coordinator::mod::finalize_test_stats`/`refresh_flakiness_scores`
//!   already compute `test_stats.flakiness_score` inline, per-run, on the
//!   ingest path — duplicating that computation here would be redundant
//!   and a second, possibly-inconsistent source of truth for the same
//!   number.
//!
//! Same layering as `reconcile.rs`/`test_stats.rs`: [`build_sql_query`],
//! [`parse_sql_response`], [`build_run_rollup`], and [`build_insight`] are
//! pure and unit-tested with plain `cargo test`. [`select_candidate_runs`],
//! [`query_analytics_engine`], and [`upsert_rollup`]/[`insert_insight`]
//! need the Workers runtime (D1 + an outbound `fetch` to Cloudflare's API)
//! and are only exercised by the live smoke test (`mise run
//! //packages/cloud-ci-worker:dev`'s `wrangler dev --test-scheduled`),
//! same posture as every other D1/network-touching function in this crate
//! (`reconcile.rs`'s module docs make the identical call for its own
//! cron). [`run`] is `lib.rs`'s `#[event(scheduled)]` handler's entry
//! point for the `*/15` trigger, matching `reconcile::run`'s shape for the
//! `*/20` trigger.

use std::collections::HashMap;

use serde::Deserialize;
use worker::wasm_bindgen::JsValue;
use worker::{Env, Headers, Method, Request, RequestInit};

/// One terminal run with no `run_rollups` row yet, read from D1's `runs`
/// table by [`select_candidate_runs`]. Pure shape consumed by
/// [`build_run_rollup`]/[`build_insight`] — see module docs on layering.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRun {
    pub run_id: String,
    pub repo_id: i64,
    pub head_sha: String,
    pub started_at_s: i64,
    pub status: String,
}

/// `_sample_interval`-weighted `test`-kind counts for one run, from the
/// Analytics Engine SQL API (analytics.md's "Querying for rollups uses
/// the SQL API" correctness requirement — see [`build_sql_query`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TestRunCounts {
    pub total_weighted: f64,
    pub failed_weighted: f64,
}

/// One `run_rollups` row this cron is prepared to upsert. See module docs
/// for which fields are real this round and which are deliberately
/// `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRollupRow {
    pub run_id: String,
    pub repo_id: i64,
    pub head_sha: String,
    pub started_at_s: i64,
    pub duration_ms: Option<i64>,
    pub queue_ms: Option<i64>,
    pub critical_path_ms: Option<i64>,
    pub cache_hit_rate: Option<f64>,
    pub cost_usd_estimate: Option<f64>,
    pub status: String,
}

/// One `insights` row this cron is prepared to insert.
#[derive(Debug, Clone, PartialEq)]
pub struct InsightRow {
    pub repo_id: i64,
    pub kind: String,
    pub subject_id: String,
    pub severity: String,
    pub detail_json: String,
    pub created_at_s: i64,
}

/// Terminal `runs.status` values (`coordinator::logic::RunState::as_str`)
/// this cron considers eligible for a `run_rollups` row. A run still
/// `queued`/`running`/`merging` has no finalized `test` data to roll up
/// yet, so it is simply not a candidate (not an error, not a zero-row
/// rollup) until a later tick finds it terminal.
const TERMINAL_STATUSES: [&str; 4] = ["succeeded", "failed", "cancelled", "abandoned"];

/// Raw SQL API row shape for [`build_sql_query`]'s aliased columns.
#[derive(Debug, Deserialize)]
struct SqlRow {
    run_id: String,
    total_weighted: f64,
    failed_weighted: f64,
}

#[derive(Debug, Deserialize)]
struct SqlResponse {
    #[serde(default)]
    data: Vec<SqlRow>,
}

/// Builds the Analytics Engine SQL API query text for a batch of
/// candidate run ids (analytics.md's `cloud_ci_metrics` dataset, `test`
/// kind in `blob1`, `run_id` in `blob2`, pass(1)/fail(0)/skip(-1) in
/// `double2` — `coordinator::logic::TestOutcomeKind::as_test_double`).
///
/// `_sample_interval`-weighted per analytics.md's explicit correctness
/// requirement: `SUM(_sample_interval)` instead of `COUNT()` for
/// `total_weighted`, and a sample-interval-weighted fail count for
/// `failed_weighted` (verified 2026-10-02,
/// developers.cloudflare.com/analytics/analytics-engine/sql-api/
//  "Sampling": `SUM(_sample_interval * double1)` is the documented
/// pattern for a weighted conditional sum). A plain `COUNT()`/un-weighted
/// `SUM()` would silently understate both for any repo busy enough to be
/// sampled — analytics.md treats that as the common case to design for,
/// not a rare edge case to special-case away, even though real sampling
/// is expected to be rare for this single-tenant deployment.
///
/// Bounded to the last 24 hours: `test` events are written by
/// `coordinator::mod::write_test_events` at (or within one
/// `flush_test_event_overflow` alarm tick of) a run's finalization, so a
/// terminal run's rows always land within normal ingest latency of
/// becoming terminal; 24h generously covers that without scanning
/// Analytics Engine's full 3-month retention on every 15-minute tick. A
/// run that stays terminal for over 24h with no AE rows (e.g. a
/// `writeDataPoints` failure per analytics.md's Failure modes table) still
/// gets a `run_rollups` row from [`build_run_rollup`] with no test counts
/// — [`select_candidate_runs`] only gates on "no `run_rollups` row yet",
/// not on AE data existing, so a run is never blocked from ever getting a
/// rollup row just because its test telemetry never arrived.
///
/// `run_ids` are this deployment's own ULIDs (`ulid.rs`), alphanumeric by
/// construction; `'` is still doubled defensively before splicing into
/// the query text, since the SQL API takes raw SQL as the request body
/// with no parameterized-query form (verified 2026-10-02, same source).
pub fn build_sql_query(run_ids: &[String]) -> String {
    let escaped = run_ids
        .iter()
        .map(|id| format!("'{}'", id.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT blob2 AS run_id, \
                SUM(_sample_interval) AS total_weighted, \
                SUM(_sample_interval * CASE WHEN double2 = 0 THEN 1 ELSE 0 END) AS failed_weighted \
         FROM cloud_ci_metrics \
         WHERE blob1 = 'test' AND blob2 IN ({escaped}) \
           AND timestamp > NOW() - INTERVAL '1' DAY \
         GROUP BY blob2"
    )
}

/// Parses the SQL API's JSON response body (`{"data": [...], "rows": N}`,
/// verified 2026-10-02, developers.cloudflare.com/analytics/analytics-engine/sql-api/
/// doesn't show this exact shape but cloudflare/skills'
/// `analytics-engine/api.md` "Response Format" section does, and it
/// matches the plain `{column: value}` object-per-row shape the "Example
/// queries" on the official page imply for `SELECT ... AS alias`) into a
/// `run_id -> counts` map. Pure — no network/D1, see module docs.
pub fn parse_sql_response(body: &str) -> Result<HashMap<String, TestRunCounts>, String> {
    let parsed: SqlResponse = serde_json::from_str(body)
        .map_err(|e| format!("cannot parse Analytics Engine SQL API response: {e}"))?;
    Ok(parsed
        .data
        .into_iter()
        .map(|row| {
            (
                row.run_id,
                TestRunCounts {
                    total_weighted: row.total_weighted,
                    failed_weighted: row.failed_weighted,
                },
            )
        })
        .collect())
}

/// Builds the `run_rollups` row for `run`. `counts` is `None` when the
/// Analytics Engine SQL API returned no `test`-kind rows for this run yet
/// (see [`build_sql_query`]'s docs) — that only affects whether
/// [`build_insight`] has anything to report, not this row, since none of
/// `run_rollups`' own numeric columns are derived from `test` counts
/// (`run_rollups` has no "tests failed" column — that is `insights`'
/// job).
///
/// **Why `duration_ms`/`critical_path_ms` are `None`, not computed from
/// Analytics Engine's implicit per-row `timestamp`.** Every Analytics
/// Engine data point is auto-timestamped at write time (verified
/// 2026-10-02, developers.cloudflare.com/analytics/analytics-engine/sql-api/
/// "Table structure": "timestamp ... The timestamp at which the event was
/// logged in your worker"), and analytics.md's `test` row shape
/// (`blob1..blob4`/`double1`/`double2`) carries no separate, explicit
/// event-time field. A tempting shortcut is `MAX(timestamp) -
/// MIN(timestamp)` across a run's `test` rows as a `duration_ms` proxy —
/// rejected: `coordinator::mod::write_test_events` writes one report
/// upload's entire outcome batch in a single call near job end, so for
/// the common case (one shard, one upload) every `test` row for a run
/// shares nearly the same write-time timestamp, and the span would read
/// as near-zero — not an imprecise estimate of run duration, a
/// *wrong* one. Multi-shard runs would fare no better: the span would
/// reflect "time between the first and last shard's upload", which omits
/// queue time and any post-test-upload work entirely, and still doesn't
/// answer "how long did this run take". Real duration needs `step`
/// events, which analytics.md's schema gives an explicit `duration_ms`/
/// `queue_ms` pair per step — not wired this round. `critical_path_ms` is
/// a DAG computation over step durations, so it has the same
/// prerequisite. `queue_ms` is a `step`-row field outright.
/// `cache_hit_rate` needs `cache` events; `cost_usd_estimate` needs
/// `sample` events (CPU-active-time + instance type). None of `step`/
/// `cache`/`sample` are wired, so all five stay `None`.
pub fn build_run_rollup(run: &CandidateRun, _counts: Option<TestRunCounts>) -> RunRollupRow {
    RunRollupRow {
        run_id: run.run_id.clone(),
        repo_id: run.repo_id,
        head_sha: run.head_sha.clone(),
        started_at_s: run.started_at_s,
        duration_ms: None,
        queue_ms: None,
        critical_path_ms: None,
        cache_hit_rate: None,
        cost_usd_estimate: None,
        status: run.status.clone(),
    }
}

/// Builds a `run_test_failures` insight for `run` when its weighted
/// `test`-kind counts show at least one failure, rounded to the nearest
/// whole test (`_sample_interval` weighting means the raw sum can be
/// fractional under real sampling — analytics.md's correctness
/// requirement is about not *understating* the count, not about
/// presenting a fractional test count to a reader). Returns `None` for a
/// clean run (zero failures) or when Analytics Engine has no `test` rows
/// for this run yet — a clean/no-data run gets no `insights` row at all,
/// rather than a zero-severity row that would just be dashboard noise.
///
/// See module docs for why this is a new, additive `insights.kind` value
/// beyond analytics.md's `slow_test`/`flaky_test`/`duration_regression`/
/// `queue_regression`/`cache_degraded`/`duration_improvement`/
/// `sizing_improvement` table, and why `flaky_test` specifically is not
/// this function's job.
pub fn build_insight(run: &CandidateRun, counts: TestRunCounts, now_s: i64) -> Option<InsightRow> {
    let failed = counts.failed_weighted.round() as i64;
    if failed <= 0 {
        return None;
    }
    let total = counts.total_weighted.round() as i64;
    let severity = if total > 0 && failed * 2 >= total {
        "critical"
    } else {
        "warn"
    };
    Some(InsightRow {
        repo_id: run.repo_id,
        kind: "run_test_failures".to_string(),
        subject_id: run.run_id.clone(),
        severity: severity.to_string(),
        detail_json: format!(
            r#"{{"run_id":"{}","failed":{failed},"total":{total}}}"#,
            run.run_id
        ),
        created_at_s: now_s,
    })
}

/// Reads up to `limit` terminal runs (see [`TERMINAL_STATUSES`]) that
/// have no `run_rollups` row yet, oldest-terminal-first — this cron's
/// idempotency gate (analytics.md's "Rollup cron fails mid-batch" failure
/// mode: a retried tick safely reprocesses the same candidates because
/// `upsert_rollup` is `ON CONFLICT DO NOTHING`, and a *successful* tick's
/// candidates never reappear here once their row exists). Needs the
/// Workers runtime — see module docs.
async fn select_candidate_runs(env: &Env, limit: u32) -> worker::Result<Vec<CandidateRun>> {
    let db = env.d1("DB")?;
    let placeholders = TERMINAL_STATUSES
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!(
        "SELECT runs.id AS run_id, runs.repo_id, runs.sha, runs.created_at, runs.status \
         FROM runs \
         LEFT JOIN run_rollups ON run_rollups.run_id = runs.id \
         WHERE runs.status IN ({placeholders}) AND run_rollups.run_id IS NULL \
         ORDER BY runs.created_at ASC \
         LIMIT ?{}",
        TERMINAL_STATUSES.len() + 1
    );
    let mut binds: Vec<JsValue> = TERMINAL_STATUSES
        .iter()
        .map(|s| JsValue::from_str(s))
        .collect();
    binds.push(JsValue::from_f64(limit as f64));
    let rows: Vec<serde_json::Value> = db.prepare(&query).bind(&binds)?.all().await?.results()?;
    rows.into_iter()
        .map(|row| {
            let run_id = row
                .get("run_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| worker::Error::RustError("candidate run row missing run_id".into()))?
                .to_string();
            let repo_id = row.get("repo_id").and_then(|v| v.as_i64()).ok_or_else(|| {
                worker::Error::RustError("candidate run row missing repo_id".into())
            })?;
            let head_sha = row
                .get("sha")
                .and_then(|v| v.as_str())
                .ok_or_else(|| worker::Error::RustError("candidate run row missing sha".into()))?
                .to_string();
            let started_at_s = row
                .get("created_at")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| {
                    worker::Error::RustError("candidate run row missing created_at".into())
                })?;
            let status = row
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| worker::Error::RustError("candidate run row missing status".into()))?
                .to_string();
            Ok(CandidateRun {
                run_id,
                repo_id,
                head_sha,
                started_at_s,
                status,
            })
        })
        .collect()
}

/// `POST`s `query` to the Analytics Engine SQL API
/// (`https://api.cloudflare.com/client/v4/accounts/<account_id>/analytics_engine/sql`,
/// bearer-token-authenticated with `ANALYTICS_SQL_TOKEN`, an *Account
/// Analytics Read*-scoped Worker secret — analytics.md's "Analytics
/// Engine schema" § "Querying for rollups uses the SQL API" and
/// "Security considerations": never exposed to the dashboard or any
/// end-user-facing API, only read here). `CLOUDFLARE_ACCOUNT_ID` is a
/// plain `[vars]` entry, not a secret, matching `GITHUB_APP_ID`'s
/// not-secret public-identifier category in `wrangler.toml`. Needs the
/// Workers runtime — see module docs.
async fn query_analytics_engine(env: &Env, query: &str) -> worker::Result<String> {
    let account_id = env
        .var("CLOUDFLARE_ACCOUNT_ID")
        .map_err(|e| {
            worker::Error::RustError(format!("CLOUDFLARE_ACCOUNT_ID is not configured: {e}"))
        })?
        .to_string();
    let token = env.secret("ANALYTICS_SQL_TOKEN").map_err(|e| {
        worker::Error::RustError(format!("ANALYTICS_SQL_TOKEN is not configured: {e}"))
    })?;
    let url =
        format!("https://api.cloudflare.com/client/v4/accounts/{account_id}/analytics_engine/sql");

    let headers = Headers::new();
    headers.set("authorization", &format!("Bearer {token}"))?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    init.with_headers(headers);
    init.with_body(Some(JsValue::from_str(query)));

    let request = Request::new_with_init(&url, &init)?;
    let mut response = worker::Fetch::Request(request).send().await?;

    if response.status_code() != 200 {
        let body = response.text().await.unwrap_or_default();
        return Err(worker::Error::RustError(format!(
            "Analytics Engine SQL API request failed: {} {body}",
            response.status_code()
        )));
    }
    response.text().await
}

/// `INSERT ... ON CONFLICT (run_id) DO NOTHING` — idempotent per
/// analytics.md's "Rollup cron fails mid-batch" failure mode: a run
/// already rolled up by an earlier (possibly concurrently-running, though
/// crons here never overlap in practice) tick is left untouched rather
/// than overwritten, since [`select_candidate_runs`] never re-selects a
/// run once its row exists anyway — this is a second, defense-in-depth
/// guard, not the primary gate. Needs the Workers runtime — see module
/// docs.
async fn upsert_rollup(env: &Env, row: &RunRollupRow, now_s: i64) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare(
        "INSERT INTO run_rollups \
            (run_id, repo_id, head_sha, started_at, duration_ms, queue_ms, \
             critical_path_ms, cache_hit_rate, cost_usd_estimate, status, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
         ON CONFLICT (run_id) DO NOTHING",
    )
    .bind(&[
        JsValue::from_str(&row.run_id),
        JsValue::from_f64(row.repo_id as f64),
        JsValue::from_str(&row.head_sha),
        JsValue::from_f64(row.started_at_s as f64),
        row.duration_ms
            .map_or(JsValue::NULL, |v| JsValue::from_f64(v as f64)),
        row.queue_ms
            .map_or(JsValue::NULL, |v| JsValue::from_f64(v as f64)),
        row.critical_path_ms
            .map_or(JsValue::NULL, |v| JsValue::from_f64(v as f64)),
        row.cache_hit_rate.map_or(JsValue::NULL, JsValue::from_f64),
        row.cost_usd_estimate
            .map_or(JsValue::NULL, JsValue::from_f64),
        JsValue::from_str(&row.status),
        JsValue::from_f64(now_s as f64),
    ])?
    .run()
    .await?;
    Ok(())
}

/// Plain `INSERT` — `insights`' PRIMARY KEY is
/// `(repo_id, kind, subject_id, created_at)`, and this cron always derives
/// `created_at` from `worker::Date::now()` at insert time within a single
/// tick's run, so a redelivered/retried tick for the same run lands on a
/// different `created_at` and would otherwise duplicate the row; in
/// practice this never fires twice for the same run because
/// [`select_candidate_runs`] stops returning a run the instant its
/// `run_rollups` row exists (written in the same tick, just before this
/// call — see [`run`]). Needs the Workers runtime — see module docs.
async fn insert_insight(env: &Env, row: &InsightRow) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare(
        "INSERT INTO insights (repo_id, kind, subject_id, severity, detail_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&[
        JsValue::from_f64(row.repo_id as f64),
        JsValue::from_str(&row.kind),
        JsValue::from_str(&row.subject_id),
        JsValue::from_str(&row.severity),
        JsValue::from_str(&row.detail_json),
        JsValue::from_f64(row.created_at_s as f64),
    ])?
    .run()
    .await?;
    Ok(())
}

/// Max candidate runs processed per tick — bounds both the D1 read and
/// the Analytics Engine SQL API's `IN (...)` list size to something that
/// stays well under the SQL API's own request-size/complexity limits. At
/// a 15-minute cadence, any backlog beyond this drains over the next few
/// ticks rather than needing to fit in one.
const BATCH_LIMIT: u32 = 100;

/// `lib.rs`'s `#[event(scheduled)]` handler's entry point for the `*/15`
/// trigger (`wrangler.toml`'s `[triggers]` `crons`). A failed pass is
/// logged, not propagated — same reasoning as `reconcile::run`'s doc
/// comment: a transient D1 or Analytics Engine SQL API error here must
/// not crash the Worker, and the next scheduled tick (15 minutes later)
/// tries again against the same still-pending candidates
/// ([`select_candidate_runs`]'s idempotent gate).
pub async fn run(env: &Env) -> Result<(), String> {
    let candidates = select_candidate_runs(env, BATCH_LIMIT)
        .await
        .map_err(|e| format!("cannot select candidate runs: {e}"))?;
    if candidates.is_empty() {
        return Ok(());
    }

    let run_ids: Vec<String> = candidates.iter().map(|c| c.run_id.clone()).collect();
    let query = build_sql_query(&run_ids);
    let counts_by_run = match query_analytics_engine(env, &query).await {
        Ok(body) => match parse_sql_response(&body) {
            Ok(map) => map,
            Err(e) => {
                worker::console_log!("rollup: {e}");
                HashMap::new()
            }
        },
        Err(e) => {
            worker::console_log!("rollup: Analytics Engine SQL API query failed: {e}");
            HashMap::new()
        }
    };

    let now_s = (worker::Date::now().as_millis() / 1000) as i64;
    for run in &candidates {
        let counts = counts_by_run.get(&run.run_id).copied();
        let rollup = build_run_rollup(run, counts);
        if let Err(e) = upsert_rollup(env, &rollup, now_s).await {
            worker::console_log!(
                "rollup: upsert of run_rollups for {} failed: {e}",
                run.run_id
            );
            continue;
        }
        if let Some(insight) = counts.and_then(|counts| build_insight(run, counts, now_s))
            && let Err(e) = insert_insight(env, &insight).await
        {
            worker::console_log!("rollup: insert of insight for {} failed: {e}", run.run_id);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(run_id: &str) -> CandidateRun {
        CandidateRun {
            run_id: run_id.to_string(),
            repo_id: 42,
            head_sha: "deadbeef".to_string(),
            started_at_s: 1_700_000_000,
            status: "succeeded".to_string(),
        }
    }

    #[test]
    fn sql_query_weights_by_sample_interval_and_escapes_run_ids() {
        let query = build_sql_query(&["r1".to_string(), "o'brien".to_string()]);
        assert!(query.contains("SUM(_sample_interval)"));
        assert!(query.contains("SUM(_sample_interval * CASE WHEN double2 = 0 THEN 1 ELSE 0 END)"));
        assert!(query.contains("'r1'"));
        assert!(query.contains("'o''brien'"));
        assert!(query.contains("blob1 = 'test'"));
    }

    #[test]
    fn sql_query_with_no_run_ids_has_empty_in_list() {
        let query = build_sql_query(&[]);
        assert!(query.contains("blob2 IN ()"));
    }

    #[test]
    fn parses_sql_api_response_rows() -> Result<(), String> {
        let body =
            r#"{"data":[{"run_id":"r1","total_weighted":10.0,"failed_weighted":2.0}],"rows":1}"#;
        let parsed = parse_sql_response(body)?;
        assert_eq!(
            parsed.get("r1").copied(),
            Some(TestRunCounts {
                total_weighted: 10.0,
                failed_weighted: 2.0
            })
        );
        Ok(())
    }

    #[test]
    fn parses_sql_api_response_with_no_rows() -> Result<(), String> {
        let body = r#"{"data":[],"rows":0}"#;
        let parsed = parse_sql_response(body)?;
        assert!(parsed.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_malformed_sql_api_response() {
        assert!(parse_sql_response("not json").is_err());
    }

    #[test]
    fn run_rollup_carries_real_identity_fields_and_leaves_rest_null() {
        let run = candidate("r1");
        let row = build_run_rollup(
            &run,
            Some(TestRunCounts {
                total_weighted: 5.0,
                failed_weighted: 1.0,
            }),
        );
        assert_eq!(row.run_id, "r1");
        assert_eq!(row.repo_id, 42);
        assert_eq!(row.head_sha, "deadbeef");
        assert_eq!(row.started_at_s, 1_700_000_000);
        assert_eq!(row.status, "succeeded");
        assert_eq!(row.duration_ms, None);
        assert_eq!(row.queue_ms, None);
        assert_eq!(row.critical_path_ms, None);
        assert_eq!(row.cache_hit_rate, None);
        assert_eq!(row.cost_usd_estimate, None);
    }

    #[test]
    fn run_rollup_identical_regardless_of_counts() {
        let run = candidate("r1");
        let with_counts = build_run_rollup(
            &run,
            Some(TestRunCounts {
                total_weighted: 5.0,
                failed_weighted: 1.0,
            }),
        );
        let without_counts = build_run_rollup(&run, None);
        assert_eq!(with_counts, without_counts);
    }

    #[test]
    fn insight_skipped_for_clean_run() {
        let run = candidate("r1");
        let counts = TestRunCounts {
            total_weighted: 10.0,
            failed_weighted: 0.0,
        };
        assert_eq!(build_insight(&run, counts, 1_000), None);
    }

    #[test]
    fn insight_warns_for_minority_failures() {
        let run = candidate("r1");
        let counts = TestRunCounts {
            total_weighted: 10.0,
            failed_weighted: 1.0,
        };
        let Some(insight) = build_insight(&run, counts, 1_000) else {
            unreachable!("build_insight must return Some for a run with failures");
        };
        assert_eq!(insight.kind, "run_test_failures");
        assert_eq!(insight.severity, "warn");
        assert_eq!(insight.repo_id, 42);
        assert_eq!(insight.subject_id, "r1");
        assert_eq!(insight.created_at_s, 1_000);
        assert!(insight.detail_json.contains("\"failed\":1"));
        assert!(insight.detail_json.contains("\"total\":10"));
    }

    #[test]
    fn insight_critical_for_majority_failures() {
        let run = candidate("r1");
        let counts = TestRunCounts {
            total_weighted: 10.0,
            failed_weighted: 6.0,
        };
        let Some(insight) = build_insight(&run, counts, 1_000) else {
            unreachable!("build_insight must return Some for a run with failures");
        };
        assert_eq!(insight.severity, "critical");
    }

    #[test]
    fn insight_rounds_fractional_sample_weighted_counts() {
        let run = candidate("r1");
        // Under real sampling, _sample_interval weighting can produce
        // fractional sums; rounding keeps the insight's reported counts
        // whole numbers rather than a confusing fraction of a test.
        let counts = TestRunCounts {
            total_weighted: 9.6,
            failed_weighted: 1.4,
        };
        let Some(insight) = build_insight(&run, counts, 1_000) else {
            unreachable!("build_insight must return Some for a run with failures");
        };
        assert!(insight.detail_json.contains("\"failed\":1"));
        assert!(insight.detail_json.contains("\"total\":10"));
    }
}
