# 0010: Pluggable executors behind a pull-model agent

- Status: Proposed
- Date: 2026-10-01

## Context

[ADR 0005](./0005-containers-for-execution.md) makes Cloudflare Containers the default place jobs
run, capped at 4 vCPU / 12 GiB / 20 GB disk. Some jobs need more (large builds, GPU, macOS),
some teams already pay for AWS or Kubernetes capacity, and some want jobs on their own
machines. If job execution assumes a Container (DO-driven start, in-process log pipes, Container
snapshots), every other backend becomes a rewrite.

## Decision

- **Pull/callback agent.** An executor's only job is to boot a machine that runs
  `cloud-ci agent` with a one-time bootstrap token and the deployment URL. The agent exchanges
  the bootstrap token for a job token, then pulls its job spec and sends heartbeats, logs,
  reports, and artifacts over the public ingest API ([ADR 0007](./0007-one-upload-path.md)).
  No executor needs inbound connectivity or a cloud-ci-specific control channel.
- **`Executor` trait** in the Worker: `capabilities()`, `start(job, bootstrap)`, `stop(handle)`,
  `status(handle)`. Liveness comes from agent heartbeats, not from the executor.
- **Capability descriptor** per executor, used by the coordinator to validate a node before
  starting it and by the rightsizer to choose a size:

  | Field | Meaning |
  | --- | --- |
  | `sizes` | Named instance types with vCPU, memory, disk |
  | `max_duration` | Hard wall-clock cap per job |
  | `snapshots` | Whether `snapshot:` layers can be restored natively (else cache tarballs) |
  | `sidecars` | Whether sidecar containers can run beside the job |
  | `network` | Egress model (open, allow-list, none) |

- **Selection.** Scripts pick a runner per node, e.g.
  `runner: { executor: "aws-ec2", type: "c7i.4xlarge" }`. The first release ships Containers only
  (below); other executors, and any config for choosing among several, land with the executor
  that needs them.
- **Credentials** for external executors (AWS keys, kubeconfig) live in Secrets Store and are
  only read by the Worker, never passed to scripts or jobs.

| Executor | Status | Boots agent via | Limits that matter |
| --- | --- | --- | --- |
| Cloudflare Containers | Default, built first | DO `ctx.container.start({ instance })` — as of 2026-10-02 this is only directly callable from Rust for `default`-policy (Wrangler-config-fixed image/size) containers; `durable_object`-policy per-call sizing and exit-status reads need either extending the vendored `worker`/`worker-sys` crates' container bindings (bounded wasm-bindgen glue, not a redesign) or an upstream contribution to `cloudflare/workers-rs` (Containers from Rust spike, 2026-10-02) | 4 vCPU / 12 GiB / 20 GB disk ([0005](./0005-containers-for-execution.md)) |
| AWS EC2 | Possible | `RunInstances` with user-data that starts the agent | Instance quotas and boot time `[unverified]` |
| AWS Lambda | Possible, small jobs only | Function invocation with bootstrap token in payload | 15 min timeout, ≤10,240 MB memory, `/tmp` 512–10,240 MB; no sidecars, no Docker daemon. Source: docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html, checked 2026-10-01 |
| Kubernetes Jobs | Possible | Job manifest whose pod runs the agent | Cluster-defined |
| Self-hosted | Possible | Long-running agent polls for work with a registration token | Machine-defined |

## Consequences

- The coordinator never assumes a Container: no Container-specific calls outside the
  Containers executor, and the fake executor used in tests is just "run the agent locally".
- Job specs, logs, and uploads have one path regardless of backend, so BYO CI, managed runs, and
  external executors exercise the same ingest code.
- Agent bootstrap tokens are single-use and short-lived; a leaked user-data blob is not a
  standing credential.
- Executors outside Cloudflare add egress from the deployer's account and their own bills; the
  dashboard attributes runner cost per executor.
- Only Containers ships in the managed-runs phase; other executors are a later
  [roadmap](../roadmap.md) phase.

## What would reverse this

Agent pull latency (job spec fetch, log streaming over HTTPS) making Containers jobs measurably
slower than a direct DO-to-container channel. Then Containers gets a private fast path and the
ingest API stays the contract for every other executor.
