# Architecture decision records

One file per decision: `NNNN-kebab-title.md`, using the template below. A decision is never
deleted; a later ADR supersedes it and the old one's Status line says so.

| ADR | Title | Status |
| --- | --- | --- |
| [0001](./0001-monorepo-protobuf-contract.md) | Monorepo with a protobuf contract | Accepted |
| [0002](./0002-rust-cloudflare-worker.md) | Rust on Cloudflare Workers | Proposed |
| [0003](./0003-single-tenant-deployment.md) | Single-tenant, deploy-to-your-own-account | Proposed |
| [0004](./0004-storage-and-coordination.md) | Storage and coordination primitives | Proposed |
| [0005](./0005-containers-for-execution.md) | Cloudflare Containers for job execution | Proposed |
| [0006](./0006-own-pipeline-format.md) | Own pipeline format, not GitHub Actions syntax | Superseded by 0009 |
| [0007](./0007-one-upload-path.md) | One upload path for managed and external runs | Proposed |
| [0008](./0008-auth-modes.md) | GitHub OAuth for humans; OIDC and tokens for machines | Accepted |
| [0009](./0009-typescript-pipeline-workflows.md) | Pipelines as durable TypeScript workflows | Accepted |
| [0010](./0010-pluggable-executors.md) | Pluggable executors behind a pull-model agent | Proposed |
| [0011](./0011-patching-third-party-crates.md) | Patching third-party crates via a `[patch.crates-io]` fork, not a vendored copy | Accepted |

## Template

```markdown
# NNNN: Title

- Status: Proposed | Accepted | Superseded by NNNN
- Date: YYYY-MM-DD

## Context
## Decision
## Consequences
## What would reverse this
```
