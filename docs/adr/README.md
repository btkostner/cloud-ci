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
| [0006](./0006-own-pipeline-format.md) | Own pipeline format, not GitHub Actions syntax | Proposed (0009 would supersede) |
| [0007](./0007-one-upload-path.md) | One upload path for managed and external runs | Proposed |
| [0008](./0008-auth-modes.md) | Access or GitHub OAuth for humans; OIDC for machines | Proposed |
| [0009](./0009-typescript-pipeline-programs.md) | TypeScript pipeline programs with discovered task graphs | Proposed |

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
