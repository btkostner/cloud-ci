//! `NodeContainer`: the real per-`node_id` container `RunCoordinator::handle_start_node`
//! starts, closing the gap `coordinator`'s own module docs flagged as deferred ("This round
//! never starts a real container") and that `container_probe.rs`'s module docs named as
//! separate, later work ("wiring real container starts into `startNode`/`completeNode` is
//! separate, later work").
//!
//! # Why a second container-backed DO, not `ContainerProbe` reused
//!
//! `ContainerProbe` (`container_probe.rs`) is bound to a `default`-scheduling-policy
//! `[[containers]]` application: one fixed image (`container_probe/Dockerfile`), chosen once
//! in `wrangler.toml` at deploy time. A pipeline node needs a *different* image per node
//! (`ci.container(id, { image, run })`-shaped callers), which the `default` policy cannot
//! express — only the `durable_object` policy's per-call `ContainerStartupOptions.image`
//! (developers.cloudflare.com/containers/configuration/scheduling-policy/, checked
//! 2026-10-02; the patched `worker` crate's `ContainerStartupOptions::set_image`, ADR 0011)
//! can. `durable_object` and `default` are each an immutable, separate Container
//! application (same doc: "The scheduling policy is immutable. To use a different policy,
//! create a new Container application."), so this is a new DO class bound to a new
//! `durable_object`-policy `[[containers]]` entry in `wrangler.toml`, not an extension of
//! `ContainerProbe`'s existing `default`-policy one. This also means no `wrangler.toml`
//! `images` map is needed: the patched crate's `set_image` takes an arbitrary image
//! reference string at call time (a digest-pinned registry reference, or the
//! Cloudflare-managed `cloudflare/debian-trixie`), not one of a fixed, pre-declared set.
//!
//! # Addressing and idempotency
//!
//! Bound by `node_id` alone (`env.durable_object("NODE_CONTAINER")?.id_from_name(node_id)`),
//! the same one-DO-instance-per-logical-container pattern `ContainerProbe`'s `?probe_id=`
//! already uses. `RunCoordinator::handle_start_node` only ever calls `/start` once per
//! `node_id` — its own `StartNodeDecision::AlreadyStarted` guard stops a redelivered
//! `startNode` from reaching this module at all — but `/start` also checks
//! `Container::running()` itself before calling `Container::start()` (same guard
//! `container_probe.rs`'s `handle_exec` already uses), so a second `/start` for the same
//! `node_id` — however it might arrive — is still a no-op here too, never a second
//! container.
//!
//! # Synchronous start, asynchronous `exec()` (the two-phase contract)
//!
//! dynamic-pipelines.md's "### Execution model" describes `step.do("start:" + id)` as
//! returning quickly, with the real completion delivered later via
//! `step.waitForEvent("done:" + id)`. `Container::start()` itself is already non-blocking —
//! it only *initiates* startup and returns before the container is ready
//! (developers.cloudflare.com/containers/configuration/scheduling-policy/: "`ctx.container.start()`
//! initiates startup and returns before the Container is ready to accept requests") — so
//! `/start` can call it, confirm it did not throw synchronously (a real, immediate failure:
//! bad image reference, scheduling rejection), and respond right away. The subsequent
//! `exec()` — which *does* block until the command's exit code is known, potentially for a
//! node's entire multi-minute build/test run — runs inside `State::wait_until`, a real
//! capability this patched `worker` crate's `durable.rs::State` exposes (confirmed in this
//! round, not `[unverified]`: `State::wait_until<F: Future<Output = ()> + 'static>` wraps the
//! same DO `wait_until` binding `Context::wait_until` uses for ordinary Workers, keeping this
//! DO instance alive for that future after its own `fetch()` response is already sent;
//! `worker::Container` is `unsafe impl Send + Sync`, so the already-acquired handle moves
//! into that future directly — no second DO-to-DO round trip is needed to re-acquire it).
//! This is the "background task inside a DO fetch handler" shape the round's investigation
//! asked for, and it is genuinely supported — not a caveat-laden fallback to holding the
//! request open. Once `exec()` resolves, the background task calls back into the node's own
//! `RunCoordinator` instance's `/complete-node` (`coordinator::RunCoordinator`) with the real
//! exit code, exactly the role `step.waitForEvent`'s future caller will eventually trigger
//! from the coordinator side.
//!
//! **What is not built.** No `durable_object`-policy `instance` sizing (lite only — every
//! node gets the runtime default, matching `container_probe.rs`'s own "not sized per call"
//! scope note); no sidecars; no sandboxed stdin streaming (`exec()`'s own options are left at
//! their defaults, same as `container_probe.rs`). Those are `ci.container`'s richer spec
//! fields that have no caller yet (`coordinator` module docs' scope boundary: no real Dynamic
//! Workflow integration this round either).

use crate::coordinator::{self, CompleteNodeRequest};
use serde::{Deserialize, Serialize};
use worker::wasm_bindgen::JsValue;
use worker::{
    Container, ContainerStartupOptions, DurableObject, Env, Method, Request, RequestInit, Response,
    State, durable_object,
};

/// Durable Object binding name; must match `durable_objects.bindings[].name` in
/// `wrangler.toml`.
pub const NODE_CONTAINER_BINDING: &str = "NODE_CONTAINER";

#[durable_object]
pub struct NodeContainer {
    state: State,
    env: Env,
}

impl DurableObject for NodeContainer {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> worker::Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Post, "/start") => {
                let body: StartRequest = req.json().await?;
                self.handle_start(body)
            }
            (Method::Post, "/stop") => self.handle_stop().await,
            (Method::Get, "/status") => self.handle_status(),
            _ => Response::error("not found", 404),
        }
    }
}

#[derive(Deserialize)]
struct StartRequest {
    /// `coordinator::do_name(...)`'s output for the node's run — lets the background
    /// `exec()` callback address the right `RunCoordinator` instance without this DO
    /// needing to know `(repo_id, sha, run_key, attempt)` itself.
    run_do_name: String,
    node_id: String,
    image: String,
    command: Vec<String>,
}

#[derive(Serialize)]
struct StartResponse {
    started: bool,
}

#[derive(Serialize)]
struct StopResponse {
    stopped: bool,
}

#[derive(Serialize, Deserialize)]
struct StatusResponse {
    running: bool,
}

impl NodeContainer {
    /// Starts this node's container if not already running, then hands the blocking
    /// `exec()` + completion callback off to `State::wait_until` so this call itself
    /// returns as soon as `Container::start()` either succeeds or throws synchronously —
    /// matching `step.do("start:" + id)`'s "returns quickly" contract (module docs).
    ///
    /// Returns a non-2xx `Response` only when `Container::start()` itself threw — a real,
    /// immediate failure (bad image reference, scheduling rejection) — which
    /// `RunCoordinator::handle_start_node`'s caller maps straight to the node's own
    /// `failed` status (module docs: "a real failure ... never an unhandled 500").
    fn handle_start(&self, body: StartRequest) -> worker::Result<Response> {
        let Some(container) = self.state.container() else {
            return Response::error("no container configured for this Durable Object", 500);
        };
        if container.running() {
            // Idempotent no-op (module docs' "Addressing and idempotency"): this
            // node_id's container is already running from an earlier `/start`.
            return Response::from_json(&StartResponse { started: false });
        }

        let mut options = ContainerStartupOptions::new();
        options.set_image(&body.image);
        if let Err(e) = container.start(Some(options)) {
            return Response::error(format!("container start failed: {e}"), 500);
        }

        let env = self.env.clone();
        self.state.wait_until(async move {
            run_and_report(
                container,
                &env,
                &body.run_do_name,
                &body.node_id,
                &body.command,
            )
            .await;
        });

        Response::from_json(&StartResponse { started: true })
    }

    /// Stops this node's container — `RunCoordinator::handle_cancel_run`'s real-stop hook
    /// (module docs: cancellation must not "just flip a DB flag while a real container
    /// keeps running unsupervised"). A container that never started (never got a `/start`
    /// call, or already exited on its own) is a no-op, not an error: cancelling a run
    /// whose node already finished, or whose container start never actually reached this
    /// DO, is an ordinary redelivery/race, not a failure.
    async fn handle_stop(&self) -> worker::Result<Response> {
        let Some(container) = self.state.container() else {
            return Response::from_json(&StopResponse { stopped: false });
        };
        if !container.running() {
            return Response::from_json(&StopResponse { stopped: false });
        }
        container.destroy(None).await?;
        Response::from_json(&StopResponse { stopped: true })
    }

    /// Reads this node's container running/not state — the `Executor` trait's `status`
    /// conformer (`crate::executor::ContainersExecutor::status`), with no other caller yet
    /// (module docs: liveness is agent heartbeats, not executor polling, so nothing in
    /// `RunCoordinator` calls this on a timer).
    fn handle_status(&self) -> worker::Result<Response> {
        let Some(container) = self.state.container() else {
            return Response::from_json(&StatusResponse { running: false });
        };
        Response::from_json(&StatusResponse {
            running: container.running(),
        })
    }
}

// ---------------------------------------------------------------------------
// Free functions — the real DO-to-DO calls, shared by `RunCoordinator::start_node_container`/
// `stop_node_container` and `crate::executor::ContainersExecutor` so neither duplicates this
// logic. Extracted from what were previously private `RunCoordinator` methods; the HTTP calls
// themselves are unchanged.
// ---------------------------------------------------------------------------

/// Starts `node_id`'s real container via a DO-to-DO call into this module's own `NodeContainer`
/// DO's `/start`. `Err` means the container itself failed to *start* (bad image, Docker/runtime
/// error) — callers map that straight to the node's own `failed` status, never an unhandled 500
/// (`coordinator`'s own doc comment on this call covers why).
pub async fn start_container(
    env: &Env,
    run_do_name: &str,
    node_id: &str,
    image: &str,
    command: &[String],
) -> Result<(), String> {
    let namespace = env
        .durable_object(NODE_CONTAINER_BINDING)
        .map_err(|e| format!("node container namespace unavailable: {e}"))?;
    let id = namespace
        .id_from_name(node_id)
        .map_err(|e| format!("node container id error: {e}"))?;
    let stub = id
        .get_stub()
        .map_err(|e| format!("node container stub error: {e}"))?;

    let encoded = serde_json::to_string(&serde_json::json!({
        "run_do_name": run_do_name,
        "node_id": node_id,
        "image": image,
        "command": command,
    }))
    .map_err(|e| format!("cannot encode node-container start request: {e}"))?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(JsValue::from_str(&encoded)));
    let request = Request::new_with_init("https://node-container.cloud-ci.internal/start", &init)
        .map_err(|e| format!("cannot build node-container start request: {e}"))?;
    let mut response = stub
        .fetch_with_request(request)
        .await
        .map_err(|e| format!("node container fetch failed: {e}"))?;
    match response.status_code() {
        200..=299 => Ok(()),
        status => {
            let detail = response.text().await.unwrap_or_default();
            Err(format!("node container rejected start: {status} {detail}"))
        }
    }
}

/// Stops `node_id`'s real container via a DO-to-DO call into `/stop`. Best-effort is the
/// caller's posture, not this function's: it returns the real `worker::Result`, and
/// `RunCoordinator::stop_node_container` is the one that logs-and-swallows a failure.
pub async fn stop_container(env: &Env, node_id: &str) -> worker::Result<()> {
    let namespace = env.durable_object(NODE_CONTAINER_BINDING)?;
    let id = namespace.id_from_name(node_id)?;
    let stub = id.get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    let request = Request::new_with_init("https://node-container.cloud-ci.internal/stop", &init)?;
    stub.fetch_with_request(request).await?;
    Ok(())
}

/// Reads `node_id`'s container running/not state via `/status` —
/// `crate::executor::ContainersExecutor::status`'s only caller.
pub async fn container_status(env: &Env, node_id: &str) -> Result<bool, String> {
    let namespace = env
        .durable_object(NODE_CONTAINER_BINDING)
        .map_err(|e| format!("node container namespace unavailable: {e}"))?;
    let id = namespace
        .id_from_name(node_id)
        .map_err(|e| format!("node container id error: {e}"))?;
    let stub = id
        .get_stub()
        .map_err(|e| format!("node container stub error: {e}"))?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    let request = Request::new_with_init("https://node-container.cloud-ci.internal/status", &init)
        .map_err(|e| format!("cannot build node-container status request: {e}"))?;
    let mut response = stub
        .fetch_with_request(request)
        .await
        .map_err(|e| format!("node container fetch failed: {e}"))?;
    match response.status_code() {
        200..=299 => {
            let body: StatusResponse = response
                .json()
                .await
                .map_err(|e| format!("cannot decode node-container status response: {e}"))?;
            Ok(body.running)
        }
        status => {
            let detail = response.text().await.unwrap_or_default();
            Err(format!("node container rejected status: {status} {detail}"))
        }
    }
}

/// Runs `command` inside `container` (already started by `handle_start`) and reports the
/// real exit code back to `RunCoordinator::handle_complete_node` — the background half of
/// `handle_start`'s two-phase contract (module docs). Never returns an error itself: every
/// failure mode (container crash, `exec()` error, the callback fetch itself failing) is
/// folded into either a `failed` completion report or, if even that callback cannot be
/// sent, a logged, swallowed error — there is no caller left to propagate a `Result` to
/// once `handle_start`'s own response has already gone out.
async fn run_and_report(
    container: Container,
    env: &Env,
    run_do_name: &str,
    node_id: &str,
    command: &[String],
) {
    let (status, result_json) = match exec_in_container(&container, command).await {
        Ok((exit_code, stdout, stderr)) => {
            let status = coordinator::logic::node_status_for_exit_code(exit_code).as_db_str();
            let result = serde_json::json!({
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
            });
            (status, result.to_string())
        }
        Err(e) => {
            let result = serde_json::json!({ "error": e });
            ("failed", result.to_string())
        }
    };

    if let Err(e) = report_completion(env, run_do_name, node_id, status, &result_json).await {
        worker::console_log!("node_container: completion callback for node {node_id} failed: {e}");
    }
}

/// Runs `command` to completion inside `container` and reads back its real exit code —
/// the patched `worker` crate's `Container::exec()`/`ExecOutput` (ADR 0011), the same
/// bindings `container_probe.rs`'s `handle_exec` already proved end-to-end.
async fn exec_in_container(
    container: &Container,
    command: &[String],
) -> Result<(u32, String, String), String> {
    let cmd: Vec<&str> = command.iter().map(String::as_str).collect();
    let process = container
        .exec(&cmd, None)
        .await
        .map_err(|e| format!("exec failed: {e}"))?;
    let output = process
        .output()
        .await
        .map_err(|e| format!("reading exec output failed: {e}"))?;
    Ok((
        output.exit_code(),
        String::from_utf8_lossy(&output.stdout()).into_owned(),
        String::from_utf8_lossy(&output.stderr()).into_owned(),
    ))
}

/// Posts the real completion to the node's `RunCoordinator` instance's `/complete-node`
/// (`coordinator::CompleteNodeRequest`) — the same wire shape a caller-supplied completion
/// would use (`coordinator` module docs: `completeNode`'s `result` is "the opaque,
/// caller-supplied result payload ... stored but never interpreted by this round"), so
/// `handle_complete_node` needs no change to accept this DO's callback.
async fn report_completion(
    env: &Env,
    run_do_name: &str,
    node_id: &str,
    status: &str,
    result_json: &str,
) -> worker::Result<()> {
    let namespace = env.durable_object(coordinator::RUN_COORDINATOR_BINDING)?;
    let id = namespace.id_from_name(run_do_name)?;
    let stub = id.get_stub()?;

    let req = CompleteNodeRequest {
        node_id: node_id.to_string(),
        status: status.to_string(),
        result: Some(result_json.to_string()),
    };
    let encoded = serde_json::to_string(&req)
        .map_err(|e| worker::Error::RustError(format!("cannot encode complete-node body: {e}")))?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(JsValue::from_str(&encoded)));
    let request = Request::new_with_init(
        "https://run-coordinator.cloud-ci.internal/complete-node",
        &init,
    )?;
    stub.fetch_with_request(request).await?;
    Ok(())
}
