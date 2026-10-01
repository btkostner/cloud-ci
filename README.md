# cloud-ci

CI that runs on **your own Cloudflare account**, with first-class GitHub support. Written in
Rust as a Cloudflare Worker.

> Status: design phase. Nothing is implemented yet; start with
> [docs/architecture.md](docs/architecture.md).

## Features (planned)

| Feature | What it does | Design |
| --- | --- | --- |
| Managed CI | Runs `.cloud-ci/pipeline.yml` on Cloudflare Containers | [pipeline-config](docs/design/pipeline-config.md) |
| Dynamic pipelines | TypeScript pipeline programs; runs each Turborepo/mise task in its own container, skipping cached ones | [dynamic-pipelines](docs/design/dynamic-pipelines.md) |
| Single PR comment | One optional, continuously updated comment summarizing every run on a PR | [pr-comment](docs/design/pr-comment.md) |
| Bring your own CI | Upload results from GitHub Actions or any CI for the same comment and analytics | [byo-ci](docs/design/byo-ci.md) |
| Parallelization | Timing-based test splitting and merging results back together | [parallelization](docs/design/parallelization.md) |
| Analytics & autoscaling | Slow/flaky/regressing spots and automatic runner sizing | [analytics](docs/design/analytics.md) |
| AI | Failure summaries, improvement suggestions, opt-in autofix via Workers AI | [ai](docs/design/ai.md) |
| Auth | Cloudflare Access or GitHub, with per-repo roles | [auth](docs/design/auth.md) |
| Asset hosting | Browsable Vitest/Playwright/coverage HTML reports and artifacts on R2 | [assets](docs/design/assets.md) |

## Shape of the system

| Choice | Decision |
| --- | --- |
| Runtime | Rust Worker (workers-rs) — [ADR 0002](docs/adr/0002-rust-cloudflare-worker.md) |
| Tenancy | One deployment per org, in the deployer's account — [ADR 0003](docs/adr/0003-single-tenant-deployment.md) |
| State | Durable Objects, D1, R2, Analytics Engine, Queues — [ADR 0004](docs/adr/0004-storage-and-coordination.md) |
| Execution | Cloudflare Containers, size chosen per job — [ADR 0005](docs/adr/0005-containers-for-execution.md) |
| Contract | Protobuf in `packages/cloud-ci-proto` — [ADR 0001](docs/adr/0001-monorepo-protobuf-contract.md) |

## Development

Requires [mise](https://mise.jdx.dev).

```sh
mise install        # tools + git hooks
mise run check      # all checks
mise run fix        # auto-fix formatting
```

Packages live under `packages/`; see [docs/roadmap.md](docs/roadmap.md) for the planned set.
