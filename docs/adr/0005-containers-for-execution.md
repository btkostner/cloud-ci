# 0005: Cloudflare Containers for job execution

- Status: Proposed
- Date: 2026-09-30

## Context

Workers cannot run arbitrary build tools. Cloudflare Containers can, and with the
`durable_object` scheduling policy the instance size can be chosen per start
(`ctx.container.start({ instance })`), from `lite` (1/16 vCPU, 256 MiB) to `standard-4`
(4 vCPU, 12 GiB, 20 GB disk), plus custom 1–4 vCPU shapes. Source:
developers.cloudflare.com/containers/platform/limits, checked 2026-09-30.

## Decision

- Cloudflare Containers is the default `Executor` implementation
  ([ADR 0010](./0010-pluggable-executors.md)); the coordinator talks to it only through the
  `Executor` trait, so this ADR covers Containers specifically and 0010 covers the boundary.
- Each job or shard runs in one container started by its `RunCoordinator`.
- The image is `cloud-ci-runner-image` (Linux, `cloud-ci agent` as entrypoint) or a user image
  with the agent injected; the agent pulls its job spec with a job-scoped token over the public
  ingest API, same as every other executor.
- Runtime instance-size selection is the mechanism for `runner: auto`
  ([analytics](../design/analytics.md)).
- If workers-rs cannot drive the container API, a minimal TypeScript Durable Object handles
  only container start/stop/size; all scheduling decisions stay in Rust. This is the first
  spike on the roadmap.

## Consequences

- Max job size is 4 vCPU / 12 GiB / 20 GB disk without an account-team limit increase; jobs
  that need more must shard or use BYO CI.
- No Docker-in-Docker assumption; jobs needing a Docker daemon are out of scope initially.
- Snapshots (≤20 GB, 30-day retention) are a candidate for warm dependency caches.

## What would reverse this

Containers' cold-start time or concurrency limits making typical CI slower than GitHub-hosted
runners; then managed execution shrinks to "dispatch to self-hosted runners" and BYO CI becomes
the primary mode.
