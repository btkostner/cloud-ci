# 0006: Own pipeline format, not GitHub Actions syntax

- Status: Superseded by [0009](./0009-typescript-pipeline-workflows.md)
- Date: 2026-09-30

## Context

Reusing GitHub Actions YAML would ease migration, but its semantics are defined by Actions'
`uses:` ecosystem (JavaScript/composite/Docker actions, runner toolcache, expressions), which
we cannot run faithfully in a container agent.

## Decision

`.cloud-ci/pipeline.yml` was our own minimal format: jobs, `needs`, steps (shell), image,
`runner`, `parallel`, caches, reports, artifacts. Users who needed Actions kept running Actions
and sent results with `cloud-ci upload` (BYO CI). Superseded: pipelines are now TypeScript
scripts with no YAML format at all — see [0009](./0009-typescript-pipeline-workflows.md) and
[dynamic-pipelines](../design/dynamic-pipelines.md).

## Consequences

- This format never shipped; TypeScript pipeline scripts replaced it before any release.
- Alternatives considered here (Actions YAML reuse) remain rejected for the same reason under
  0009: we cannot run Actions' `uses:` ecosystem faithfully in a container agent.

## What would reverse this

A credible open-source Actions-compatible runner that can execute in Containers.
