//! `ContainerProbe`: a minimal, standalone Durable Object that proves the
//! patched `worker` crate's `Container::start()`/`exec()` bindings
//! ([ADR 0011](../../../docs/adr/0011-patching-third-party-crates.md)) work
//! end-to-end, callable over an ordinary service binding from
//! `packages/cloud-ci-dynamic-workflows-host`'s Dynamic Workflow step. This
//! is this round's answer to dynamic-pipelines.md's "Investigate this
//! specifically" question: a Workflow step running in a Dynamic Worker has
//! no direct JS access to `ctx.container` — that API only exists on
//! `this.ctx` inside a `DurableObject` subclass
//! (developers.cloudflare.com/containers/api/durable-object-container/,
//! checked 2026-10-02) — so the step calls back into this Rust-hosted DO
//! over a service binding instead.
//!
//! # Scope boundary (this round)
//!
//! `ContainerProbe` is deliberately **not** `RunCoordinator`'s node-start
//! path (`coordinator` module docs' "no actual container starting" gap
//! stays open for `RunCoordinator` itself): wiring real container starts
//! into `startNode`/`completeNode` is separate, later work. This DO exists
//! only to give the dynamic-workflows-host package's test script one real
//! container round trip to call, proving the chain — nothing here reads or
//! writes `RunCoordinator`/`RepoState`/`PullRequestState` state, enforces
//! policy, or tracks job/shard identity. One `default`-scheduling-policy
//! container per DO instance (`[[containers]]` in `wrangler.toml`, image
//! built from `container_probe/Dockerfile`), started lazily on first
//! `/exec` call and left running for the DO's lifetime — not snapshotted,
//! not sized per call (`durable_object`-policy sizing is the Containers
//! from Rust spike's proven-but-unwired capability, also out of scope
//! here).

use serde::{Deserialize, Serialize};
use worker::{DurableObject, Env, Method, Request, Response, Result, State, durable_object};

#[durable_object]
pub struct ContainerProbe {
    state: State,
}

impl DurableObject for ContainerProbe {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Post, "/exec") => {
                let body: ExecRequest = req.json().await?;
                self.handle_exec(body).await
            }
            _ => Response::error("not found", 404),
        }
    }
}

#[derive(Deserialize)]
struct ExecRequest {
    cmd: Vec<String>,
}

#[derive(Serialize)]
struct ExecResponse {
    exit_code: u32,
    stdout: String,
    stderr: String,
}

impl ContainerProbe {
    /// Starts the DO's `default`-policy container on first use (image and
    /// instance type come from `wrangler.toml`'s `[[containers]]` entry,
    /// not from this call — `ContainerStartupOptions` fields other than
    /// `image`/`instance`/`containerSnapshot` don't apply under the
    /// `default` policy, so `start(None)` is correct here, matching the
    /// patched fork's own `test/src/container.rs` `EchoContainer` example
    /// at the pinned commit), then `exec()`s `cmd` inside it. `exec()`
    /// itself "waits for a container that is still starting"
    /// (developers.cloudflare.com/containers/api/durable-object-container/,
    /// checked 2026-10-02), so no separate readiness poll is needed between
    /// `start()` and `exec()`.
    async fn handle_exec(&self, body: ExecRequest) -> Result<Response> {
        let Some(container) = self.state.container() else {
            return Response::error("no container configured for this Durable Object", 500);
        };
        if !container.running() {
            container.start(None)?;
        }
        let cmd: Vec<&str> = body.cmd.iter().map(String::as_str).collect();
        let process = container.exec(&cmd, None).await?;
        let output = process.output().await?;
        Response::from_json(&ExecResponse {
            exit_code: output.exit_code(),
            stdout: String::from_utf8_lossy(&output.stdout()).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr()).into_owned(),
        })
    }
}
