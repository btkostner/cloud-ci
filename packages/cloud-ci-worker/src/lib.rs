pub mod connect;
pub mod coordinator;
pub mod github_app;
pub mod ingest_token;
pub mod installations;
pub mod oidc;
pub mod reconcile;
pub mod ulid;
pub mod webhook;

use cloud_ci_proto::ingest::v1::{
    BeginRunRequest, BeginRunResponse, CompleteShardRequest, CompleteShardResponse,
    CompleteUploadRequest, CompleteUploadResponse, CreateUploadRequest, CreateUploadResponse,
    GetRunRequest, GetRunResponse, StartJobRequest, StartJobResponse, SubmitReportRequest,
    SubmitReportResponse,
};
use connect::{Code, Codec, ConnectError, NegotiationError, negotiate};
use coordinator::{CoordinatorError, RunCoordinatorStore};
use worker::{Context, Date, Env, Headers, Method, Request, Response, Result, event};

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // Data plane: a plain `PUT` of upload part bytes, not a Connect RPC
    // (docs/design/byo-ci.md's "Control plane and data plane are different
    // transports").
    if req.method() == Method::Put {
        return handle_upload_part(req, &env).await;
    }
    // GitHub webhook delivery, not a Connect RPC — dispatched on path,
    // not content negotiation (docs/design/auth.md's "Webhook signature
    // verification" ordering requirement: raw body read and signature
    // verified before anything else touches the request).
    if req.method() == Method::Post && req.path() == "/webhooks/github" {
        return handle_github_webhook(req, &env).await;
    }
    if req.method() != Method::Post {
        return Response::error("method not allowed", 405);
    }
    let headers = req.headers();
    let codec = match negotiate(
        headers.get("content-type")?.as_deref(),
        headers.get("content-encoding")?.as_deref(),
    ) {
        Ok(codec) => codec,
        Err(NegotiationError::UnsupportedMediaType) => {
            return Response::error("unsupported media type", 415);
        }
        Err(NegotiationError::Connect(err)) => return connect_error(&err),
    };
    // `BeginRun`'s OIDC credential path (src/oidc.rs, handle_begin_run
    // below) is the only caller of this; every other procedure ignores
    // it. Extracted here, not inside `route`, because `req.headers()` is
    // only cheap/available before `req.bytes()` consumes the request.
    let bearer = headers
        .get("authorization")?
        .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string));
    let body = req.bytes().await?;
    match route(&req.path(), codec, &body, &env, bearer.as_deref()).await {
        Ok(bytes) => {
            let headers = Headers::new();
            headers.set("content-type", codec.content_type())?;
            Ok(Response::from_bytes(bytes)?.with_headers(headers))
        }
        Err(err) => connect_error(&err),
    }
}

/// Cron Trigger entry point (`wrangler.toml`'s `[triggers]` `crons`):
/// runs one full installation-reconcile pass ([`reconcile::run`]; see
/// that module's docs and docs/design/auth.md's "Multiple orgs and
/// installations" "Discovery" paragraph).
///
/// `worker`'s `#[event(scheduled)]` macro requires this exact
/// three-argument `(ScheduledEvent, Env, ScheduleContext)` signature
/// returning `()` — confirmed against worker-macros-0.8.7's `event.rs`
/// (`validate_event_fn(&input_fn, Scheduled, 3, true)`, and the crate's
/// own `tests/ui/scheduled-wrong-return-type.rs`/`scheduled-wrong-argument-types.rs`
/// compile-fail fixtures for a `-> String` return or a wrong parameter
/// list). A failed pass is logged, not propagated: a transient GitHub API
/// or D1 error here must not crash the Worker, and the next scheduled run
/// (same reasoning as `uninstall_disallowed_installation`'s webhook-path
/// best-effort retry) tries again.
#[event(scheduled)]
async fn scheduled(_event: worker::ScheduledEvent, env: Env, _ctx: worker::ScheduleContext) {
    if let Err(e) = reconcile::run(&env).await {
        worker::console_log!("installation reconcile pass failed: {e}");
    }
}

/// Hand-routes each `cloud_ci.ingest.v1.IngestService` procedure to its
/// request type, per ADR 0002 (Connect over `fetch`, no Tower server).
async fn route(
    path: &str,
    codec: Codec,
    body: &[u8],
    env: &Env,
    bearer: Option<&str>,
) -> std::result::Result<Vec<u8>, ConnectError> {
    match path {
        "/cloud_ci.ingest.v1.IngestService/BeginRun" => {
            handle_begin_run(codec, body, env, bearer).await
        }
        "/cloud_ci.ingest.v1.IngestService/StartJob" => handle_start_job(codec, body, env).await,
        "/cloud_ci.ingest.v1.IngestService/GetRun" => handle_get_run(codec, body, env).await,
        "/cloud_ci.ingest.v1.IngestService/CreateUpload" => {
            handle_create_upload(codec, body, env).await
        }
        "/cloud_ci.ingest.v1.IngestService/CompleteUpload" => {
            handle_complete_upload(codec, body, env).await
        }
        "/cloud_ci.ingest.v1.IngestService/SubmitReport" => {
            handle_submit_report(codec, body, env).await
        }
        "/cloud_ci.ingest.v1.IngestService/CompleteShard" => {
            handle_complete_shard(codec, body, env).await
        }
        _ => Err(ConnectError::new(
            Code::Unimplemented,
            format!("unknown procedure {path}"),
        )),
    }
}

/// # Auth (docs/design/byo-ci.md § Auth)
///
/// `BeginRun` is the only call that authenticates with something other
/// than an ingest token. Two credential shapes are valid per that doc —
/// GitHub Actions OIDC JWT, and a scoped API token for non-GitHub-Actions
/// CI systems — but only the OIDC path is implemented; the scoped-API-
/// token path has no `api_tokens` table or issuance flow yet. So the
/// credential check here is intentionally partial:
///
/// - **No `Authorization` header at all**: proceeds exactly as before
///   this round, with no identity check whatsoever. This is a real,
///   temporary gap — unauthenticated BYO-CI ingest is possible until the
///   scoped-API-token path exists — not something silently papered over.
/// - **Bearer value present and JWT-shaped** (three dot-separated
///   segments, [`oidc::looks_like_jwt`]): treated as a GitHub Actions
///   OIDC JWT and fully verified ([`oidc::verify`]: issuer/audience/
///   signature/`exp`/`nbf`), then its `repository_id`/
///   `repository_owner_id` claims are matched against the `repos`/
///   `installations` D1 tables ([`installations::check_allowlist`]). Any
///   failure at any step rejects the whole call (`Unauthenticated` for a
///   bad/forged/expired token, `PermissionDenied` for a genuine token
///   whose claims don't match an allowlisted, non-suspended
///   installation) — it never falls through to the unauthenticated path.
/// - **Bearer value present but not JWT-shaped** (e.g. a future
///   `cc_tok_...` scoped API token): falls through to the unauthenticated
///   path too, same as no header — rejecting it would be worse than
///   ignoring it, since there is no issuer yet to validate that token
///   format against. Temporary, pending the scoped-API-token
///   implementation.
async fn handle_begin_run(
    codec: Codec,
    body: &[u8],
    env: &Env,
    bearer: Option<&str>,
) -> std::result::Result<Vec<u8>, ConnectError> {
    let req: BeginRunRequest = codec.decode(body)?;

    if let Some(bearer) = bearer
        && oidc::looks_like_jwt(bearer)
    {
        verify_oidc_begin_run_credential(env, bearer, req.key.repo_id).await?;
    }
    // else: no header, or an opaque non-JWT bearer value (future scoped
    // API token) — falls through to the unauthenticated path below, see
    // doc comment above.

    // Resolved before touching the Durable Object so a misconfigured
    // deployment fails the whole call up front, rather than creating/
    // updating the run and only then discovering it cannot mint a token.
    let secret = ingest_token_secret(env)?;

    let do_name = coordinator::do_name(
        req.key.repo_id,
        &req.key.sha,
        &req.key.run_key,
        req.key.attempt,
    );
    let store = RunCoordinatorStore::new(env, &do_name).map_err(coordinator_error)?;
    let outcome = store.begin_run(&req).await.map_err(coordinator_error)?;

    let now_s = Date::now().as_millis() / 1000;
    let ingest_token = ingest_token::mint(&secret, req.key.repo_id, &outcome.run_id, now_s)
        .map_err(|e| ConnectError::new(Code::Internal, format!("cannot mint ingest token: {e}")))?;

    let resp = BeginRunResponse {
        run_id: outcome.run_id,
        status: outcome.status.into(),
        ingest_token,
        ..Default::default()
    };
    codec.encode(&resp)
}

/// Full GitHub Actions OIDC validation for `BeginRun`'s `bearer` credential
/// (module doc comment on [`handle_begin_run`]): verifies the JWT itself
/// ([`oidc::verify`]) against `CLOUD_CI_INGEST_AUDIENCE`, then matches its
/// claims against the `repos`/`installations` allowlist
/// ([`installations::check_allowlist`]). Returns `Ok(())` only when both
/// steps succeed; every failure path returns an `Err` that
/// [`handle_begin_run`] propagates as a rejected call, never falling
/// through to the unauthenticated path.
async fn verify_oidc_begin_run_credential(
    env: &Env,
    jwt: &str,
    claimed_repo_id: u64,
) -> std::result::Result<(), ConnectError> {
    let audience = env
        .var("CLOUD_CI_INGEST_AUDIENCE")
        .map(|v| v.to_string())
        .unwrap_or_default();
    if audience.is_empty() {
        return Err(ConnectError::new(
            Code::Internal,
            "CLOUD_CI_INGEST_AUDIENCE is not configured",
        ));
    }

    let now_s = (Date::now().as_millis() / 1000) as i64;
    let claims = oidc::verify(jwt, &audience, now_s)
        .await
        .map_err(|e| ConnectError::new(Code::Unauthenticated, format!("invalid OIDC JWT: {e}")))?;

    if claims.repository_id != claimed_repo_id {
        return Err(ConnectError::new(
            Code::PermissionDenied,
            "OIDC token's repository_id does not match the requested run's repo",
        ));
    }

    let row = installations::lookup_repo_installation(env, claims.repository_id)
        .await
        .map_err(|e| ConnectError::new(Code::Internal, format!("allowlist lookup failed: {e}")))?;
    installations::check_allowlist(claims.repository_owner_id, row).map_err(|e| {
        let message = match e {
            installations::AllowlistError::RepoNotFound => {
                "repository is not registered with this deployment".to_string()
            }
            installations::AllowlistError::OwnerMismatch => {
                "OIDC token's repository_owner_id does not match the installation that owns this repo"
                    .to_string()
            }
            installations::AllowlistError::Suspended => {
                "the GitHub App installation that owns this repo is suspended".to_string()
            }
        };
        ConnectError::new(Code::PermissionDenied, message)
    })
}

/// Resolves the ingest-token HMAC signing key from the Worker's
/// `INGEST_TOKEN_SECRET` secret binding (`wrangler secret put` in
/// production, `.dev.vars` locally — see `.dev.vars.example`). There is no
/// fallback value: an unconfigured deployment must fail loudly here, not
/// silently mint tokens signed with a value anyone could read from source.
fn ingest_token_secret(env: &Env) -> std::result::Result<Vec<u8>, ConnectError> {
    let secret = env.secret("INGEST_TOKEN_SECRET").map_err(|e| {
        ConnectError::new(
            Code::Internal,
            format!("INGEST_TOKEN_SECRET is not configured: {e}"),
        )
    })?;
    Ok(secret.to_string().into_bytes())
}

async fn handle_start_job(
    codec: Codec,
    body: &[u8],
    env: &Env,
) -> std::result::Result<Vec<u8>, ConnectError> {
    let req: StartJobRequest = codec.decode(body)?;
    let do_name = resolve_do_name_for_run(env, &req.run_id).await?;
    let store = RunCoordinatorStore::new(env, &do_name).map_err(coordinator_error)?;
    let outcome = store.start_job(&req).await.map_err(coordinator_error)?;

    let resp = StartJobResponse {
        job_id: outcome.job_id,
        ..Default::default()
    };
    codec.encode(&resp)
}

async fn handle_get_run(
    codec: Codec,
    body: &[u8],
    env: &Env,
) -> std::result::Result<Vec<u8>, ConnectError> {
    let req: GetRunRequest = codec.decode(body)?;
    let do_name = coordinator::do_name(
        req.key.repo_id,
        &req.key.sha,
        &req.key.run_key,
        req.key.attempt,
    );
    let store = RunCoordinatorStore::new(env, &do_name).map_err(coordinator_error)?;
    let outcome = store.get_run().await.map_err(coordinator_error)?;

    let resp = GetRunResponse {
        run_id: outcome.run_id,
        status: outcome.status.into(),
        jobs: outcome.jobs,
        ..Default::default()
    };
    codec.encode(&resp)
}

/// `StartJob` only carries a `run_id` (no `RunKey`), but the Durable Object
/// is addressed by `(repo_id, sha, run_key, attempt)`
/// ([`coordinator::do_name`]). D1's `runs` table is `RunCoordinator`'s own
/// projection (ADR 0004), so reading it back here to resolve `run_id` to its
/// routing key is a read of the projection, not a second writer of run
/// state.
async fn resolve_do_name_for_run(
    env: &Env,
    run_id: &str,
) -> std::result::Result<String, ConnectError> {
    #[derive(serde::Deserialize)]
    struct RunIdentity {
        repo_id: i64,
        sha: String,
        run_key: String,
        attempt: i64,
    }

    let db = env
        .d1("DB")
        .map_err(|e| ConnectError::new(Code::Internal, format!("D1 unavailable: {e}")))?;
    let row: Option<RunIdentity> = db
        .prepare("SELECT repo_id, sha, run_key, attempt FROM runs WHERE id = ?1")
        .bind(&[run_id.into()])
        .map_err(|e| ConnectError::new(Code::Internal, format!("D1 bind failed: {e}")))?
        .first(None)
        .await
        .map_err(|e| ConnectError::new(Code::Internal, format!("D1 query failed: {e}")))?;

    match row {
        Some(r) => Ok(coordinator::do_name(
            r.repo_id as u64,
            &r.sha,
            &r.run_key,
            r.attempt as u32,
        )),
        None => Err(ConnectError::new(
            Code::NotFound,
            format!("run {run_id} not found"),
        )),
    }
}

async fn handle_create_upload(
    codec: Codec,
    body: &[u8],
    env: &Env,
) -> std::result::Result<Vec<u8>, ConnectError> {
    let req: CreateUploadRequest = codec.decode(body)?;
    let do_name = resolve_do_name_for_job(env, &req.job_id).await?;
    let store = RunCoordinatorStore::new(env, &do_name).map_err(coordinator_error)?;
    let outcome = store.create_upload(&req).await.map_err(coordinator_error)?;

    let resp = CreateUploadResponse {
        upload_id: outcome.upload_id,
        part_count: outcome.part_count,
        part_size_bytes: outcome.part_size_bytes,
        already_complete: outcome.already_complete,
        ..Default::default()
    };
    codec.encode(&resp)
}

async fn handle_complete_upload(
    codec: Codec,
    body: &[u8],
    env: &Env,
) -> std::result::Result<Vec<u8>, ConnectError> {
    let req: CompleteUploadRequest = codec.decode(body)?;
    let do_name = resolve_do_name_for_upload(env, &req.upload_id).await?;
    let store = RunCoordinatorStore::new(env, &do_name).map_err(coordinator_error)?;
    store
        .complete_upload(&req)
        .await
        .map_err(coordinator_error)?;
    codec.encode(&CompleteUploadResponse::default())
}

async fn handle_submit_report(
    codec: Codec,
    body: &[u8],
    env: &Env,
) -> std::result::Result<Vec<u8>, ConnectError> {
    let req: SubmitReportRequest = codec.decode(body)?;
    let do_name = resolve_do_name_for_job(env, &req.job_id).await?;
    let store = RunCoordinatorStore::new(env, &do_name).map_err(coordinator_error)?;
    store.submit_report(&req).await.map_err(coordinator_error)?;
    codec.encode(&SubmitReportResponse::default())
}

async fn handle_complete_shard(
    codec: Codec,
    body: &[u8],
    env: &Env,
) -> std::result::Result<Vec<u8>, ConnectError> {
    let req: CompleteShardRequest = codec.decode(body)?;
    let do_name = resolve_do_name_for_job(env, &req.job_id).await?;
    let store = RunCoordinatorStore::new(env, &do_name).map_err(coordinator_error)?;
    store
        .complete_shard(&req)
        .await
        .map_err(coordinator_error)?;
    codec.encode(&CompleteShardResponse::default())
}

/// `CreateUpload`/`SubmitReport`/`CompleteShard` only carry a `job_id`;
/// resolves it to the owning run's Durable Object name the same way
/// [`resolve_do_name_for_run`] does for a `run_id`.
async fn resolve_do_name_for_job(
    env: &Env,
    job_id: &str,
) -> std::result::Result<String, ConnectError> {
    #[derive(serde::Deserialize)]
    struct RunIdentity {
        repo_id: i64,
        sha: String,
        run_key: String,
        attempt: i64,
    }

    let db = env
        .d1("DB")
        .map_err(|e| ConnectError::new(Code::Internal, format!("D1 unavailable: {e}")))?;
    let row: Option<RunIdentity> = db
        .prepare(
            "SELECT runs.repo_id as repo_id, runs.sha as sha, runs.run_key as run_key, runs.attempt as attempt \
             FROM jobs JOIN runs ON jobs.run_id = runs.id WHERE jobs.id = ?1",
        )
        .bind(&[job_id.into()])
        .map_err(|e| ConnectError::new(Code::Internal, format!("D1 bind failed: {e}")))?
        .first(None)
        .await
        .map_err(|e| ConnectError::new(Code::Internal, format!("D1 query failed: {e}")))?;

    match row {
        Some(r) => Ok(coordinator::do_name(
            r.repo_id as u64,
            &r.sha,
            &r.run_key,
            r.attempt as u32,
        )),
        None => Err(ConnectError::new(
            Code::NotFound,
            format!("job {job_id} not found"),
        )),
    }
}

/// `CompleteUpload` only carries an `upload_id`; resolves it to the owning
/// run's Durable Object name via the `uploads` -> `jobs` -> `runs`
/// projection join.
async fn resolve_do_name_for_upload(
    env: &Env,
    upload_id: &str,
) -> std::result::Result<String, ConnectError> {
    #[derive(serde::Deserialize)]
    struct RunIdentity {
        repo_id: i64,
        sha: String,
        run_key: String,
        attempt: i64,
    }

    let db = env
        .d1("DB")
        .map_err(|e| ConnectError::new(Code::Internal, format!("D1 unavailable: {e}")))?;
    let row: Option<RunIdentity> = db
        .prepare(
            "SELECT runs.repo_id as repo_id, runs.sha as sha, runs.run_key as run_key, runs.attempt as attempt \
             FROM uploads JOIN jobs ON uploads.job_id = jobs.id JOIN runs ON jobs.run_id = runs.id \
             WHERE uploads.id = ?1",
        )
        .bind(&[upload_id.into()])
        .map_err(|e| ConnectError::new(Code::Internal, format!("D1 bind failed: {e}")))?
        .first(None)
        .await
        .map_err(|e| ConnectError::new(Code::Internal, format!("D1 query failed: {e}")))?;

    match row {
        Some(r) => Ok(coordinator::do_name(
            r.repo_id as u64,
            &r.sha,
            &r.run_key,
            r.attempt as u32,
        )),
        None => Err(ConnectError::new(
            Code::NotFound,
            format!("upload {upload_id} not found"),
        )),
    }
}

/// Parses `/ingest/v1/uploads/{upload_id}/parts/{n}` into its two path
/// segments. Pure, so it is unit-tested directly without a live `Request`.
fn parse_upload_part_path(path: &str) -> Option<(&str, u32)> {
    let rest = path.strip_prefix("/ingest/v1/uploads/")?;
    let (upload_id, rest) = rest.split_once("/parts/")?;
    if upload_id.is_empty() {
        return None;
    }
    let part_number: u32 = rest.parse().ok()?;
    Some((upload_id, part_number))
}

struct UploadForPart {
    repo_id: u64,
    r2_key: String,
    sha256: String,
}

async fn lookup_upload_for_part(env: &Env, upload_id: &str) -> Result<Option<UploadForPart>> {
    #[derive(serde::Deserialize)]
    struct Row {
        repo_id: i64,
        r2_key: String,
        sha256: String,
    }
    let db = env.d1("DB")?;
    let row: Option<Row> = db
        .prepare(
            "SELECT runs.repo_id as repo_id, uploads.r2_key as r2_key, uploads.sha256 as sha256 \
             FROM uploads JOIN jobs ON uploads.job_id = jobs.id JOIN runs ON jobs.run_id = runs.id \
             WHERE uploads.id = ?1",
        )
        .bind(&[upload_id.into()])?
        .first(None)
        .await?;
    Ok(row.map(|r| UploadForPart {
        repo_id: r.repo_id as u64,
        r2_key: r.r2_key,
        sha256: r.sha256,
    }))
}

/// Raw `PUT /ingest/v1/uploads/{upload_id}/parts/{n}` — the data plane
/// (docs/design/byo-ci.md's "Control plane and data plane are different
/// transports"). Single-part uploads only this round (`coordinator` module
/// docs): `n` must be `1`.
async fn handle_upload_part(mut req: Request, env: &Env) -> Result<Response> {
    let path = req.path();
    let Some((upload_id, part_number)) = parse_upload_part_path(&path) else {
        return Response::error("not found", 404);
    };
    let upload_id = upload_id.to_string();
    if part_number != 1 {
        return Response::error(
            "multipart uploads are out of scope this round; part number must be 1",
            400,
        );
    }

    // Security consideration: the 32 MiB part-size cap is enforced from the
    // declared `Content-Length` before touching R2, so an oversized or
    // hostile upload never reaches the R2 binding.
    let content_length = req
        .headers()
        .get("content-length")?
        .and_then(|v| v.parse::<u64>().ok());
    if content_length.is_none_or(|len| len > coordinator::logic::MAX_SINGLE_PART_BYTES) {
        return Response::error("part exceeds the 32 MiB single-part limit", 413);
    }

    let Some(token) = req
        .headers()
        .get("authorization")?
        .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
    else {
        return Response::error("missing bearer ingest token", 401);
    };
    let secret = match env.secret("INGEST_TOKEN_SECRET") {
        Ok(secret) => secret.to_string().into_bytes(),
        Err(_) => return Response::error("INGEST_TOKEN_SECRET is not configured", 500),
    };
    let now_s = Date::now().as_millis() / 1000;
    let claims = match ingest_token::verify(&secret, &token, now_s) {
        Ok(claims) => claims,
        Err(_) => return Response::error("invalid or expired ingest token", 401),
    };

    // Security consideration: a part PUT without a valid token for the
    // owning run's `repo_id` is rejected before touching R2.
    let Some(info) = lookup_upload_for_part(env, &upload_id).await? else {
        return Response::error("upload not found", 404);
    };
    if info.repo_id != claims.repo_id {
        return Response::error("ingest token does not match the upload's repo", 403);
    }

    let bytes = req.bytes().await?;
    if bytes.len() as u64 > coordinator::logic::MAX_SINGLE_PART_BYTES {
        return Response::error("part exceeds the 32 MiB single-part limit", 413);
    }

    let bucket = env.bucket("ASSETS")?;
    bucket.put(&info.r2_key, bytes).execute().await?;

    let headers = Headers::new();
    headers.set("etag", &info.sha256)?;
    Ok(Response::empty()?.with_headers(headers))
}

/// `POST /webhooks/github` (docs/design/auth.md § "Webhook signature
/// verification"). Reads the raw body first, verifies
/// `X-Hub-Signature-256` via [`webhook::verify_signature`], and rejects
/// with 401 *before* parsing anything or touching D1 on any failure —
/// the doc's explicit ordering requirement. Only `installation` and
/// `installation_repositories` are handled this round (see
/// `src/installations.rs`); any other `X-GitHub-Event` gets a 200
/// "not handled" ack so GitHub does not retry-storm an event type this
/// deployment doesn't act on yet
/// (docs.github.com/en/webhooks/using-webhooks/handling-webhook-deliveries,
/// accessed 2026-10-02: a non-2xx response causes GitHub to retry
/// delivery).
async fn handle_github_webhook(mut req: Request, env: &Env) -> Result<Response> {
    let raw_body = req.bytes().await?;

    let secret = match env.secret("GITHUB_WEBHOOK_SECRET") {
        Ok(secret) => secret.to_string(),
        Err(_) => return Response::error("GITHUB_WEBHOOK_SECRET is not configured", 500),
    };
    let Some(signature_header) = req.headers().get("x-hub-signature-256")? else {
        return Response::error("missing X-Hub-Signature-256", 401);
    };
    if webhook::verify_signature(secret.as_bytes(), &signature_header, &raw_body).is_err() {
        return Response::error("invalid webhook signature", 401);
    }

    match req.headers().get("x-github-event")?.as_deref() {
        Some("installation") => handle_installation_event(&raw_body, env).await,
        Some("installation_repositories") => {
            handle_installation_repositories_event(&raw_body, env).await
        }
        _ => Response::ok("event not handled"),
    }
}

/// `installation.created`/`deleted`/`suspend`/`unsuspend`
/// (docs/design/auth.md § "Multiple orgs and installations").
async fn handle_installation_event(raw_body: &[u8], env: &Env) -> Result<Response> {
    let event: installations::InstallationEvent = match serde_json::from_slice(raw_body) {
        Ok(event) => event,
        Err(_) => return Response::error("malformed installation payload", 400),
    };
    let installation_id = event.installation.id;
    let now_s = (Date::now().as_millis() / 1000) as i64;

    match event.action.as_str() {
        "created" => {
            let allowed_orgs = env
                .var("GITHUB_ALLOWED_ORGS")
                .map(|v| v.to_string())
                .unwrap_or_default();
            if installations::is_allowed_org(&event.installation.account.login, &allowed_orgs) {
                installations::upsert_installation(
                    env,
                    installation_id,
                    &event.installation.account.login,
                    &event.installation.account.account_type,
                    event.installation.account.id,
                    now_s,
                )
                .await?;
            } else {
                // No row is ever created for a disallowed account
                // (auth.md: "never creates rows for it") — that is the
                // security-critical invariant and it holds unconditionally
                // here, regardless of whether the uninstall call below
                // succeeds. The uninstall call itself is best-effort and
                // intentionally NOT the sole enforcement mechanism: against
                // a synthetic or already-removed installation it 404s, and
                // more generally it can fail for reasons unrelated to
                // this delivery (bad/rotated credentials, GitHub outage).
                // auth.md's Failure modes table names the durable backstop
                // for exactly this case — "The periodic `GET
                // /app/installations` reconcile job ... catches it on its
                // next pass and uninstalls it then, rather than depending
                // on the webhook alone" — which is not built yet (separate,
                // later infrastructure, same as everything else deferred
                // this round). Until it exists, a failed uninstall here
                // means the disallowed account stays installed on GitHub's
                // side with no usable row on ours, which is an acceptable
                // gap this round, not a silent security hole: no data is
                // ever created for it.
                //
                // Returning a non-200 to force a GitHub webhook retry
                // would be the wrong fix: webhook retry exists for
                // delivery failures, not as a substitute for the reconcile
                // job, and retrying the same delivery against a
                // persistently-failing uninstall (e.g. bad credentials)
                // would not help — it would just retry-storm this
                // deployment for an error retrying can't fix. The ack
                // stays 200; the uninstall attempt is logged on failure
                // for operator visibility, nothing more.
                if let Err(e) = uninstall_disallowed_installation(env, installation_id).await {
                    worker::console_log!(
                        "uninstall of disallowed installation {installation_id} failed: {e}"
                    );
                }
            }
        }
        "deleted" => {
            installations::delete_installation_row(env, installation_id).await?;
        }
        "suspend" => {
            installations::set_suspended(env, installation_id, Some(now_s)).await?;
        }
        "unsuspend" => {
            installations::set_suspended(env, installation_id, None).await?;
        }
        // Other documented `installation` actions (e.g.
        // `new_permissions_accepted`) carry nothing this round acts on.
        _ => {}
    }
    Response::ok("ok")
}

/// Mints a fresh App-level JWT ([`github_app::mint_app_jwt`]) and calls
/// `DELETE /app/installations/{installation_id}`
/// ([`github_app::delete_installation`]) — per auth.md, App-authenticated,
/// never an installation token, since the installation being removed
/// cannot be trusted to mint its own token for the call.
async fn uninstall_disallowed_installation(
    env: &Env,
    installation_id: u64,
) -> std::result::Result<(), String> {
    let private_key_pem = env
        .secret("GITHUB_APP_PRIVATE_KEY")
        .map_err(|e| format!("GITHUB_APP_PRIVATE_KEY is not configured: {e}"))?
        .to_string();
    let app_id: u64 = env
        .var("GITHUB_APP_ID")
        .map_err(|e| format!("GITHUB_APP_ID is not configured: {e}"))?
        .to_string()
        .parse()
        .map_err(|e| format!("GITHUB_APP_ID is not numeric: {e}"))?;
    let now_s = (Date::now().as_millis() / 1000) as i64;
    let app_jwt = github_app::mint_app_jwt(&private_key_pem, app_id, now_s)
        .await
        .map_err(|e| e.to_string())?;
    github_app::delete_installation(&app_jwt, installation_id)
        .await
        .map_err(|e| e.to_string())
}

/// `installation_repositories.added`/`removed`
/// (docs/design/auth.md § "Multiple orgs and installations").
async fn handle_installation_repositories_event(raw_body: &[u8], env: &Env) -> Result<Response> {
    let event: installations::InstallationRepositoriesEvent = match serde_json::from_slice(raw_body)
    {
        Ok(event) => event,
        Err(_) => return Response::error("malformed installation_repositories payload", 400),
    };
    let installation_id = event.installation.id;

    match event.action.as_str() {
        "added" => {
            for repo in &event.repositories_added {
                installations::upsert_repo(env, repo.id, installation_id, &repo.name).await?;
            }
        }
        "removed" => {
            for repo in &event.repositories_removed {
                installations::delete_repo(env, repo.id).await?;
            }
        }
        _ => {}
    }
    Response::ok("ok")
}

fn coordinator_error(err: CoordinatorError) -> ConnectError {
    match err {
        CoordinatorError::NotFound => ConnectError::new(Code::NotFound, "run not found"),
        CoordinatorError::Conflict(msg) => ConnectError::new(Code::FailedPrecondition, msg),
        CoordinatorError::Internal(msg) => ConnectError::new(Code::Internal, msg),
    }
}

fn connect_error(err: &ConnectError) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::from_bytes(err.body())?
        .with_status(err.code.http_status())
        .with_headers(headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_ci_proto::ingest::v1::RunKey;

    #[test]
    fn coordinator_errors_map_to_expected_connect_codes() {
        assert_eq!(
            coordinator_error(CoordinatorError::NotFound).code,
            Code::NotFound
        );
        assert_eq!(
            coordinator_error(CoordinatorError::Conflict("x".into())).code,
            Code::FailedPrecondition
        );
        assert_eq!(
            coordinator_error(CoordinatorError::Internal("x".into())).code,
            Code::Internal
        );
    }

    #[test]
    fn begin_run_request_round_trips_run_key_fields_used_for_do_naming()
    -> std::result::Result<(), ConnectError> {
        // Exercises the exact field accesses `handle_begin_run`/`handle_get_run`
        // use to derive the Durable Object name, without needing a live `Env`.
        // Compares against a direct `do_name` call with the same literal
        // values rather than pinning the hash's incidental output, since
        // what matters here is that the field order/values match, not the
        // specific digest.
        let req = BeginRunRequest {
            key: RunKey {
                repo_id: 1_296_269,
                sha: "6dcb09b5b57875f334f61aebed695e2e4193db5e".into(),
                run_key: "gha/42".into(),
                attempt: 1,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let name = coordinator::do_name(
            req.key.repo_id,
            &req.key.sha,
            &req.key.run_key,
            req.key.attempt,
        );
        let expected = coordinator::do_name(
            1_296_269,
            "6dcb09b5b57875f334f61aebed695e2e4193db5e",
            "gha/42",
            1,
        );
        assert_eq!(name, expected);
        Ok(())
    }

    #[test]
    fn parse_upload_part_path_extracts_upload_id_and_part_number() {
        assert_eq!(
            parse_upload_part_path("/ingest/v1/uploads/01ARZ3NDEKTSV4RRFFQ69G5FAV/parts/1"),
            Some(("01ARZ3NDEKTSV4RRFFQ69G5FAV", 1))
        );
    }

    #[test]
    fn parse_upload_part_path_rejects_non_numeric_part() {
        assert_eq!(
            parse_upload_part_path("/ingest/v1/uploads/abc/parts/one"),
            None
        );
    }

    #[test]
    fn parse_upload_part_path_rejects_empty_upload_id() {
        assert_eq!(parse_upload_part_path("/ingest/v1/uploads//parts/1"), None);
    }

    #[test]
    fn parse_upload_part_path_rejects_wrong_prefix() {
        assert_eq!(parse_upload_part_path("/other/path"), None);
    }
}
