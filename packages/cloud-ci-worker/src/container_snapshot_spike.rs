//! `SnapshotSpike`: a standalone Durable Object proving (or disproving) docs/roadmap.md's
//! Phase-0 "Container snapshots" row — "Can a run's task containers start from a snapshot
//! taken after `setup`?" — against the real Cloudflare Containers snapshot mechanism, not a
//! Docker-level substitute (image layer caching, `docker commit`, or any other
//! Docker-specific trick would not exercise what this question is actually asking about).
//!
//! # Why this DO calls the underlying JS object directly, not just `worker::Container`
//!
//! ADR 0011's patched `worker` crate binds `ContainerStartupOptions.container_snapshot` (a
//! snapshot handle to *restore*, consumed by `start()`) but — confirmed by reading the
//! pinned fork commit's actual source, both `worker-sys/src/types/durable_object/container.rs`
//! and `worker/src/container.rs`, at
//! <https://github.com/btkostner/workers-rs/tree/df96700ab45b9e5dc400c48946f5c57951dcfd11> —
//! never binds `snapshotContainer()`, the method that *creates* a snapshot
//! (developers.cloudflare.com/containers/guides/snapshots/, checked 2026-10-02:
//! `await this.ctx.container.snapshotContainer({ name })`). The fork's own doc comment on
//! `set_container_snapshot` even says "by the id returned from `snapshotContainer()`" — but
//! that method is referenced only in that comment; `grep -rni snapshot` across the entire
//! fork's `.rs` sources at that commit turns up no extern binding, no wrapper method, no
//! test fixture use of it anywhere. So half of the real mechanism (restore) has a typed
//! Rust wrapper; the other half (create) does not exist in this patched crate at all, for
//! any target — this is not a local-dev-only gap.
//!
//! Rather than stop at "the typed wrapper is incomplete" without actually trying the real
//! JS method, this module calls `ctx.container`'s underlying JS object directly:
//! `Container: AsRef<JsValue>` exposes the live `worker_sys::Container` instance, and
//! `worker` re-exports `js_sys`/`wasm_bindgen`/`wasm_bindgen_futures`, so
//! `js_sys::Reflect::get(container.as_ref(), "snapshotContainer")` retrieves the real
//! bound-in-the-runtime JS method (if the runtime provides it at all) and calls it as a
//! plain `Function`, exactly what a hand-written `wasm_bindgen` extern block would have
//! done. This is the real Cloudflare Containers API, reached through `js_sys` reflection
//! instead of a typed wrapper — not a reimplementation, simulation, or Docker-level stand-in.
//! The resulting handle (a "plain data object" per Cloudflare's own docs) is round-tripped
//! as opaque JSON (`JSON.stringify`/`JSON.parse`) between this DO's two instances and
//! assigned straight into `ContainerStartupOptions.container_snapshot` (a public
//! `Option<JsValue>` field) on restore, preserving its exact shape — never narrowed to the
//! fork's `set_container_snapshot(id: &str)` helper, which assumes a bare string id that may
//! not match what `snapshotContainer()` actually returns.
//!
//! # Scope boundary
//!
//! Standalone proof DO, same posture as `container_probe.rs`/`node_container.rs`: no
//! `RunCoordinator`/`RepoState`/`PullRequestState` wiring. Driven during this round only by a
//! throwaway local `/internal/snapshot-spike/{id}/{setup,restore}` `lib.rs` route and matching
//! `wrangler.toml` `[[containers]]`/`durable_objects.bindings`/`[[migrations]]` entries — both
//! fully reverted after the real numbers in `/tmp/container-snapshot-spike-findings.txt` and
//! the docs/roadmap.md row were captured, so this module is **not** declared as a `pub mod` in
//! `lib.rs` and is not part of the compiled Worker; it is kept, uncompiled, as the exact
//! reviewable source this round's real local run used (re-wiring it is a one-line `pub mod
//! container_snapshot_spike;` plus the `wrangler.toml` entries this file's own git history
//! shows were added and removed). `durable_object` scheduling policy when wired, image built
//! locally from `container_snapshot_spike/Dockerfile` and passed as a Docker-tag `image`
//! string at call time — the same "already Docker-cached image, passed by tag" local-dev
//! convention the cold-start spike used (`/tmp/cold-start-spike-findings.txt`).

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use worker::js_sys::{JSON, Object, Reflect};
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::wasm_bindgen_futures::JsFuture;
use worker::{
    Container, ContainerStartupOptions, DurableObject, Env, Method, Request, Response, Result,
    State, durable_object,
};

#[durable_object]
pub struct SnapshotSpike {
    state: State,
}

impl DurableObject for SnapshotSpike {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Post, "/setup") => {
                let body: SetupRequest = req.json().await?;
                self.handle_setup(body).await
            }
            (Method::Post, "/restore") => {
                let body: RestoreRequest = req.json().await?;
                self.handle_restore(body).await
            }
            _ => Response::error("not found", 404),
        }
    }
}

#[derive(Deserialize)]
struct SetupRequest {
    image: String,
}

#[derive(Serialize)]
struct SetupResponse {
    /// Real wall-clock milliseconds for `pnpm install` inside the freshly started
    /// container — the cold path's measured cost.
    install_ms: f64,
    /// The real `snapshotContainer()` result, round-tripped as opaque JSON, when the call
    /// succeeded.
    snapshot: Option<JsonValue>,
    /// The real error `snapshotContainer()` (or the `Reflect::get` lookup for it) raised,
    /// when it did not succeed — e.g. "not a function" if the runtime never exposes this
    /// method locally, or a thrown `TypeError`/scheduling-policy error from the call itself.
    snapshot_error: Option<String>,
}

#[derive(Deserialize)]
struct RestoreRequest {
    snapshot: JsonValue,
}

#[derive(Serialize)]
struct RestoreResponse {
    /// Real wall-clock milliseconds from starting the new container instance to confirming
    /// `node_modules` is already present — the warm path's measured cost.
    restore_ms: f64,
    node_modules_present: bool,
}

impl SnapshotSpike {
    /// Starts a fresh container from `image`, execs a real `pnpm install` inside it, times
    /// it, then attempts the real `snapshotContainer()` call on the same container.
    async fn handle_setup(&self, body: SetupRequest) -> Result<Response> {
        let Some(container) = self.state.container() else {
            return Response::error("no container configured for this Durable Object", 500);
        };
        if !container.running() {
            let mut options = ContainerStartupOptions::new();
            options.set_image(&body.image);
            options.enable_internet(true);
            container.start(Some(options))?;
        }

        let install_start = worker::Date::now().as_millis();
        let process = container
            .exec(&["sh", "-c", "cd /app && pnpm install"], None)
            .await?;
        let output = process.output().await?;
        let install_ms = (worker::Date::now().as_millis() - install_start) as f64;
        if output.exit_code() != 0 {
            return Response::error(
                format!(
                    "pnpm install failed (exit {}): {}",
                    output.exit_code(),
                    String::from_utf8_lossy(&output.stderr())
                ),
                500,
            );
        }

        let (snapshot, snapshot_error) = match snapshot_container(&container, "spike").await {
            Ok(value) => match js_to_json(&value) {
                Ok(json) => (Some(json), None),
                Err(e) => (None, Some(format!("snapshot result was not JSON-serializable: {e}"))),
            },
            Err(e) => (None, Some(e)),
        };

        Response::from_json(&SetupResponse {
            install_ms,
            snapshot,
            snapshot_error,
        })
    }

    /// Starts a *new* container instance (this DO's own container slot, expected to be a
    /// different DO instance than the one `handle_setup` used) restoring from `snapshot`
    /// instead of a cold image, then confirms `node_modules` is already present.
    async fn handle_restore(&self, body: RestoreRequest) -> Result<Response> {
        let Some(container) = self.state.container() else {
            return Response::error("no container configured for this Durable Object", 500);
        };
        if container.running() {
            return Response::error(
                "this Durable Object instance's container is already running; use a fresh instance id for /restore",
                400,
            );
        }

        let snapshot_js = json_to_js(&body.snapshot)?;
        let restore_start = worker::Date::now().as_millis();
        let mut options = ContainerStartupOptions::new();
        // `image` and `container_snapshot` are mutually exclusive (confirmed against a
        // real `start()` call locally: "`image` and `containerSnapshot` are mutually
        // exclusive" `TypeError`) — the snapshot's own image is implied by the snapshot
        // itself (Cloudflare's own docs: "A snapshot is tied to the Container image
        // version it was created from").
        options.enable_internet(true);
        options.container_snapshot = Some(snapshot_js);
        container.start(Some(options))?;

        let process = container
            .exec(&["sh", "-c", "test -d /app/node_modules"], None)
            .await?;
        let output = process.output().await?;
        let restore_ms = (worker::Date::now().as_millis() - restore_start) as f64;

        Response::from_json(&RestoreResponse {
            restore_ms,
            node_modules_present: output.exit_code() == 0,
        })
    }
}

/// Calls the real `ctx.container.snapshotContainer({ name })` JS method via `js_sys`
/// reflection — see this module's doc comment for why no typed `worker` crate wrapper
/// exists for it. Returns the raw JS result (Cloudflare's "plain data object" snapshot
/// handle) on success.
async fn snapshot_container(container: &Container, name: &str) -> std::result::Result<JsValue, String> {
    let this: &JsValue = container.as_ref();
    let func = Reflect::get(this, &JsValue::from_str("snapshotContainer"))
        .map_err(|e| format!("looking up snapshotContainer: {}", js_error_to_string(&e)))?;
    let func: worker::js_sys::Function = func
        .dyn_into()
        .map_err(|_| "ctx.container.snapshotContainer is not a function in this runtime".to_string())?;
    let opts = Object::new();
    Reflect::set(&opts, &JsValue::from_str("name"), &JsValue::from_str(name))
        .map_err(|e| format!("building snapshotContainer() options: {}", js_error_to_string(&e)))?;
    let promise = func
        .call1(this, &opts.into())
        .map_err(|e| format!("calling snapshotContainer(): {}", js_error_to_string(&e)))?;
    let promise: worker::js_sys::Promise = promise
        .dyn_into()
        .map_err(|_| "snapshotContainer() did not return a Promise".to_string())?;
    JsFuture::from(promise)
        .await
        .map_err(|e| format!("snapshotContainer() rejected: {}", js_error_to_string(&e)))
}

fn js_error_to_string(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| JSON::stringify(value).ok().and_then(|s| s.as_string()))
        .unwrap_or_else(|| "<unprintable JS value>".to_string())
}

/// Round-trips a JS value to `serde_json::Value` via `JSON.stringify`, the only
/// representation-preserving path available for an opaque handle whose real shape this
/// repo does not control.
fn js_to_json(value: &JsValue) -> std::result::Result<JsonValue, String> {
    let text = JSON::stringify(value)
        .map_err(|e| js_error_to_string(&e))?
        .as_string()
        .ok_or_else(|| "JSON.stringify did not return a string".to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

fn json_to_js(value: &JsonValue) -> Result<JsValue> {
    let text = serde_json::to_string(value)
        .map_err(|e| worker::Error::RustError(format!("cannot encode snapshot handle: {e}")))?;
    JSON::parse(&text)
        .map_err(|e| worker::Error::RustError(format!("cannot decode snapshot handle: {}", js_error_to_string(&e))))
}
