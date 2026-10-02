# 0004: Storage and coordination primitives

- Status: Proposed
- Date: 2026-09-30

## Context

CI generates relational metadata (runs, jobs, tests), large blobs (logs, artifacts, HTML
reports, caches), high-volume time series (resource samples), and needs strongly consistent
coordination (DAG progress, shard barriers, comment debouncing).

## Decision

| Need | Primitive | Why |
| --- | --- | --- |
| Run coordination | Durable Object `RunCoordinator`, one per run, SQLite-backed | Single-writer consistency; alarms for timeouts and debounce |
| Repo-level scheduling | Durable Object `RepoState`, one per repo | Cancel-superseded and concurrency limits need one serialized view |
| PR comment | Durable Object `PullRequestState`, one per (repo, PR number) | The comment aggregates many runs; one writer per PR keeps edits ordered and debounced |
| Queryable metadata, history, rollups | D1 | SQL for the dashboard and for timing-based splitting |
| Logs, artifacts, sites | R2 bucket `cloud-ci-assets` | No egress fees; multipart for large uploads; lifecycle rules for retention |
| Job caches | R2 bucket `cloud-ci-cache` | Never browser-served; separate lifecycle and eviction |
| GitHub App key, webhook secret, token root key | Secrets Store | Account-level, write-only after creation |
| Raw metric samples | Workers Analytics Engine | Cheap high-cardinality writes; rolled up into D1 by cron |
| Decoupling webhooks and post-run work | Queues | Fast webhook ack; retries with backoff |

The `RunCoordinator` is the only writer of run state; D1 rows are its projection.

## Consequences

- D1 has per-database size limits [unverified for current figure]; test-case history is the
  largest table and gets a retention window and rollups from day one.
- Analytics Engine queries go through its SQL API, which needs an API token stored as a
  Worker secret; the dashboard reads D1 rollups instead so it never depends on that path.

## What would reverse this

D1 size limits binding for large monorepos with many tests — the escape hatch is sharding
test history per repo into DO SQLite, which the coordinator pattern already accommodates.
