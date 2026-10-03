# cloud-ci

CI that runs on **your own Cloudflare account**, with first-class GitHub support. Written in
Rust as a Cloudflare Worker.

> Status: under active development, not yet released. Several packages have real, tested code
> (protobuf contract, Rust Worker ingest, core splitter/reports, CLI, pipeline SDK, dynamic
> workflow host); most product surface is still design. See [docs/roadmap.md](docs/roadmap.md)
> for what each phase has actually proven, and [docs/architecture.md](docs/architecture.md) for
> the intended shape. Browsable docs (this tree plus search): `packages/cloud-ci-docs` — not
> deployed anywhere public yet.

## Features (planned)

| Feature | What it does | Design |
| --- | --- | --- |
| Managed CI | TypeScript pipeline scripts (`.cloud-ci/pipelines/*.ts`) run as durable workflows on Cloudflare Containers by default, each Turborepo/mise task in its own container, skipping cached ones | [dynamic-pipelines](docs/design/dynamic-pipelines.md) |
| Single PR comment | One optional, continuously updated comment summarizing every run on a PR | [pr-comment](docs/design/pr-comment.md) |
| Bring your own CI | Upload results from GitHub Actions or any CI for the same comment and analytics | [byo-ci](docs/design/byo-ci.md) |
| Parallelization | Timing-based test splitting and merging results back together | [parallelization](docs/design/parallelization.md) |
| Analytics & autoscaling | Slow/flaky/regressing spots and automatic runner sizing | [analytics](docs/design/analytics.md) |
| AI | Failure summaries, improvement suggestions, opt-in autofix via Workers AI | [ai](docs/design/ai.md) |
| Auth | GitHub OAuth for humans, per-repo roles; OIDC/tokens for machines | [auth](docs/design/auth.md) |
| Asset hosting | Browsable Vitest/Playwright/coverage HTML reports and artifacts on R2 | [assets](docs/design/assets.md) |
| Settings | Static repo config: PR comment, check names, concurrency, runner bounds, retention | [settings](docs/design/settings.md) |

## Shape of the system

| Choice | Decision |
| --- | --- |
| Runtime | Rust Worker (workers-rs) — [ADR 0002](docs/adr/0002-rust-cloudflare-worker.md) |
| Tenancy | One deployment per company (one or more GitHub orgs), in the deployer's account — [ADR 0003](docs/adr/0003-single-tenant-deployment.md) |
| State | Durable Objects, D1, R2, Analytics Engine, Queues — [ADR 0004](docs/adr/0004-storage-and-coordination.md) |
| Execution | Pluggable executors, Cloudflare Containers by default — [ADR 0005](docs/adr/0005-containers-for-execution.md), [ADR 0010](docs/adr/0010-pluggable-executors.md) |
| Contract | Protobuf in `packages/cloud-ci-proto` — [ADR 0001](docs/adr/0001-monorepo-protobuf-contract.md) |

## Development

Requires [mise](https://mise.jdx.dev).

```sh
mise install        # tools + git hooks
mise run check      # all checks
mise run fix        # auto-fix formatting
```

Packages live under `packages/`; see [docs/roadmap.md](docs/roadmap.md) for the planned set.
