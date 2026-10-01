# 0006: Own pipeline format, not GitHub Actions syntax

- Status: Proposed; would be superseded by [0009](./0009-typescript-pipeline-programs.md) if that is accepted
- Date: 2026-09-30

## Context

Reusing GitHub Actions YAML would ease migration, but its semantics are defined by Actions'
`uses:` ecosystem (JavaScript/composite/Docker actions, runner toolcache, expressions), which
we cannot run faithfully in a container agent.

## Decision

`.cloud-ci/pipeline.yml` is our own minimal format: jobs, `needs`, steps (shell), image,
`runner`, `parallel`, caches, reports, artifacts. Users who need Actions keep running Actions
and send results with `cloud-ci upload` (BYO CI). See [pipeline-config](../design/pipeline-config.md).

## Consequences

- Clear semantics we fully control, including first-class `parallel:` and `reports:`.
- Migration is manual; the docs ship side-by-side translations of common Actions workflows.

## What would reverse this

A credible open-source Actions-compatible runner that can execute in Containers.
