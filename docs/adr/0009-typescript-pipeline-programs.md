# 0009: TypeScript pipeline programs with discovered task graphs

- Status: Proposed (supersedes [0006](./0006-own-pipeline-format.md))
- Date: 2026-10-01

## Context

ADR 0006 chose a static YAML format. Monorepos using Turborepo or mise already encode their task
graph. Running each node in its own container needs config that can read that graph at runtime
and make decisions about it (runner size, grouping, which nodes report GitHub checks). YAML
cannot express that without growing its own expression language.

## Decision

- Pipelines may be TypeScript programs (`.cloud-ci/pipeline.ts`) that return a **plan**
  (`cloud_ci.v1.Plan`). Programs do not execute tasks.
- Plans can contain **expansion nodes**, resolved at runtime by adapters (`turbo`, `mise`, generic
  graph JSON) in a discovery container. Expansion is append-only and recorded, so reruns replay
  it.
- Programs are evaluated in Cloudflare Dynamic Workers, sandboxed with egress blocked, never
  inside `cloud-ci-worker`.
- cloud-ci serves Turborepo's Remote Cache API from R2 for cache-hit pruning and for moving
  outputs between containers.
- YAML (`pipeline.yml`) remains as a static shorthand compiling to the same plan.
- GitHub checks are per selector (stable name, glob over nodes), opt-in per node, plus the
  always-on aggregate `cloud-ci` check.

Details: [dynamic-pipelines](../design/dynamic-pipelines.md).

## Why TypeScript, not Lua or Starlark

| Language | For | Against |
| --- | --- | --- |
| TypeScript | Runs natively in Dynamic Workers (sandbox, ms startup); typed SDK generated from the proto contract; the target users (turbo shops) already write it | Not deterministic by construction; needs bundling |
| Lua | Small, embeddable | We would host the interpreter in our wasm Worker and own sandboxing and CPU limits; no types; unfamiliar to most JS monorepo teams |
| Starlark | Deterministic and hermetic by design; pure-Rust implementation exists | Unfamiliar outside Bazel shops; weaker editor tooling |

## Consequences

- `cloud-ci-proto-typescript` and a published `@cloud-ci/pipeline` SDK move from Phase 4 to
  Phase 2.
- Two config surfaces (TS and YAML) must stay equivalent. YAML is defined as a subset of
  the plan, so equivalence is checked by compiling both to proto in tests.
- Managed runs depend on a beta Cloudflare product (Dynamic Workers).

## What would reverse this

Dynamic Workers being unavailable from our deployment model or too costly. Then programs would
evaluate in the discovery container, accepting a container cold start per run. If sandboxing JS
proves unworkable, Starlark (hosted in Rust) is the fallback.
