# 0009: Pipelines as durable TypeScript workflows

- Status: Proposed (would supersede [0006](./0006-own-pipeline-format.md) once accepted)
- Date: 2026-10-01

## Context

ADR 0006 chose a static YAML format. Monorepos using Turborepo or mise already encode a task graph
that CI should follow node by node, and teams want control over steps, sidecars, retries, and
which work reports GitHub checks. Each of those becomes an engine feature and a config key in a
static format. An earlier draft of this ADR proposed a TypeScript program that returns a static
plan expanded by adapters; that kept the same problem one level up.

## Decision

- Pipelines are TypeScript scripts in `.cloud-ci/pipelines/<name>.ts` that orchestrate the run
  imperatively through a `ci` API: start containers, await results, branch, loop.
- Scripts run as Cloudflare **Dynamic Workflows**. Every side effect is a durable step, so an
  isolate recycle or redeploy replays the script without repeating finished work.
- `RunCoordinator` remains the single writer of run state and enforces policy the script cannot
  override (secrets, runner bounds, concurrency, node caps).
- Turborepo and mise support ships as library helpers (`turbo.plan`, `turbo.execute`) in
  `@cloud-ci/pipeline`, not as engine features.
- GitHub checks are created by name from the script; nodes attach to them. The aggregate
  `cloud-ci` check is always reported.
- `pipeline.yml` remains, executed by a built-in script.

Details: [dynamic-pipelines](../design/dynamic-pipelines.md).

## Why TypeScript

| Language | For | Against |
| --- | --- | --- |
| TypeScript | Runs in Dynamic Workflows with durable steps; typed SDK; familiar to Turborepo users | Not deterministic by construction; replay needs lint and divergence detection |
| Lua | Small, embeddable | We would host and sandbox the interpreter and build durable execution ourselves |
| Starlark | Deterministic by design | Same hosting problem; unfamiliar outside Bazel shops |

## Consequences

- The host side of script execution is likely TypeScript (`@cloudflare/dynamic-workflows` is a JS
  library), alongside the Rust Worker. The boundary is the coordinator RPC.
- `cloud-ci-proto-typescript` and the `@cloud-ci/pipeline` SDK move to Phase 2.
- Script authors must keep code between steps deterministic.
- Managed runs depend on Dynamic Workers and Workflows, both newer Cloudflare products.
- The graph is not known up front, so the dashboard renders it progressively, and checks must be
  created by name early to satisfy branch protection.

## What would reverse this

Dynamic Workflows not isolating PR-controlled code well enough, or step costs and limits making
large graphs impractical. The fallback is the static-plan design or YAML only.
