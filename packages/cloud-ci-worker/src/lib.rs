pub mod api_tokens;
pub mod connect;
pub mod coordinator;
pub mod github_app;
pub mod github_checks;
pub mod ingest_token;
pub mod installations;
pub mod oauth;
pub mod oidc;
pub mod pr_comment;
pub mod pull_request_state;
pub mod pull_request_webhook;
pub mod reconcile;
pub mod roles;
pub mod session;
pub mod template_spike;
pub mod token_issuance;
pub mod ulid;
pub mod webhook;
pub mod workflow_run;

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
    // Human login (docs/design/auth.md § "Human auth: GitHub OAuth"), not
    // a Connect RPC — plain browser-navigated GET requests.
    if req.method() == Method::Get && req.path() == "/login" {
        return handle_login(&req, &env).await;
    }
    if req.method() == Method::Get && req.path() == "/oauth/callback" {
        return handle_oauth_callback(&req, &env).await;
    }
    // Admin-only token issuance (docs/design/auth.md § "Scoped API
    // tokens", "Role model and permission matrix"), not a Connect RPC —
    // a plain JSON POST gated by session cookie + per-repo admin role
    // rather than a bearer credential.
    if req.method() == Method::Post && req.path() == "/v1/tokens" {
        return handle_issue_token(&mut req, &env).await;
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

/// # Auth (docs/design/auth.md § "Machine auth", "Scoped API tokens")
///
/// `BeginRun` is the only call that authenticates with something other
/// than an ingest token. Two credential shapes are valid — a GitHub
/// Actions OIDC JWT, or a scoped API token for non-GitHub-Actions CI
/// systems — and presenting one of them is now **mandatory**:
///
/// - **No `Authorization` header at all**: rejected with
///   `Code::Unauthenticated` before anything else runs — no Durable
///   Object is touched, no run is created, no token is minted. Earlier
///   rounds proceeded unauthenticated here as a documented temporary
///   gap; now that both credential paths exist (OIDC above, scoped API
///   tokens via [`api_tokens`]), that gap is closed for real.
/// - **Bearer value present and JWT-shaped** (three dot-separated
///   segments, [`oidc::looks_like_jwt`]): treated as a GitHub Actions
///   OIDC JWT and fully verified ([`oidc::verify`]: issuer/audience/
///   signature/`exp`/`nbf`), then its `repository_id`/
///   `repository_owner_id` claims are matched against the `repos`/
///   `installations` D1 tables ([`installations::check_allowlist`]). Any
///   failure at any step rejects the whole call (`Unauthenticated` for a
///   bad/forged/expired token, `PermissionDenied` for a genuine token
///   whose claims don't match an allowlisted, non-suspended
///   installation).
/// - **Bearer value present but not JWT-shaped** (a `cc_tok_...`-style
///   opaque scoped API token, or anything else): hashed and looked up
///   against the `api_tokens` table ([`api_tokens::lookup_by_hash`]),
///   then checked for revocation/expiry/`ingest:write` scope/repo
///   allowlist membership ([`api_tokens::check_token`]). Same
///   `Unauthenticated`-vs-`PermissionDenied` split as the OIDC path: an
///   unknown/revoked/expired token is `Unauthenticated` (it never
///   identified a real, live credential), a real token whose scope or
///   allowlist doesn't cover this call is `PermissionDenied`.
///
/// Either branch's failure rejects the whole call — neither ever falls
/// through to an unauthenticated success.
async fn handle_begin_run(
    codec: Codec,
    body: &[u8],
    env: &Env,
    bearer: Option<&str>,
) -> std::result::Result<Vec<u8>, ConnectError> {
    let req: BeginRunRequest = codec.decode(body)?;

    match bearer {
        Some(bearer) if oidc::looks_like_jwt(bearer) => {
            verify_oidc_begin_run_credential(env, bearer, req.key.repo_id).await?;
        }
        Some(bearer) => {
            verify_scoped_api_token_begin_run_credential(env, bearer, req.key.repo_id).await?;
        }
        None => {
            return Err(ConnectError::new(
                Code::Unauthenticated,
                "BeginRun requires a credential: GitHub Actions OIDC JWT or a scoped API token",
            ));
        }
    }
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

/// Full scoped-API-token validation for `BeginRun`'s `bearer` credential
/// (module doc comment on [`handle_begin_run`]): hashes the presented
/// token ([`api_tokens::hash_token`]), looks up the matching
/// `api_tokens` row ([`api_tokens::lookup_by_hash`]), and checks
/// revocation/expiry/`ingest:write` scope/repo allowlist membership
/// ([`api_tokens::check_token`]). Returns `Ok(())` only when the token is
/// found and every check passes; every failure path returns an `Err`
/// that [`handle_begin_run`] propagates as a rejected call, never
/// falling through to the unauthenticated path. On success, best-effort
/// updates `last_used_at` (docs/design/auth.md: "not every request needs
/// a write" — a failure here must not fail the request, so it is logged
/// and ignored, not propagated).
async fn verify_scoped_api_token_begin_run_credential(
    env: &Env,
    presented_token: &str,
    repo_id: u64,
) -> std::result::Result<(), ConnectError> {
    let hash = api_tokens::hash_token(presented_token);
    let row = api_tokens::lookup_by_hash(env, &hash)
        .await
        .map_err(|e| ConnectError::new(Code::Internal, format!("api token lookup failed: {e}")))?;

    let now_s = (Date::now().as_millis() / 1000) as i64;
    api_tokens::check_token(row.as_ref(), "ingest:write", repo_id, now_s).map_err(|e| match e {
        api_tokens::CheckError::NotFound
        | api_tokens::CheckError::Revoked
        | api_tokens::CheckError::Expired => ConnectError::new(
            Code::Unauthenticated,
            "invalid, revoked, or expired scoped API token",
        ),
        api_tokens::CheckError::MissingScope => ConnectError::new(
            Code::PermissionDenied,
            "scoped API token does not have the ingest:write scope",
        ),
        api_tokens::CheckError::RepoNotAllowed => ConnectError::new(
            Code::PermissionDenied,
            "scoped API token's repo allowlist does not include this repo",
        ),
    })?;

    // Best-effort: see doc comment above.
    if let Some(row) = row
        && let Err(e) = api_tokens::touch_last_used(env, &row.id, now_s).await
    {
        worker::console_log!(
            "failed to update api_tokens.last_used_at for {}: {e}",
            row.id
        );
    }

    Ok(())
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

/// `GET /login` (docs/design/auth.md § "Human auth: GitHub OAuth"):
/// builds the `github.com/login/oauth/authorize` redirect URL
/// ([`oauth::authorize_url`]) with a fresh CSRF `state`
/// ([`oauth::generate_state`]), stashes that state in a short-lived
/// `cc_oauth_state` cookie, and 302-redirects the browser to GitHub.
/// Fails closed (503) if `GITHUB_APP_CLIENT_ID` isn't configured yet —
/// same posture as the webhook/OIDC paths before `cloud-ci setup
/// github-app` has run (auth.md § "GitHub App setup": "the deployed
/// Worker's webhook handler, OAuth callback, and OIDC/ingest paths all
/// fail closed (503, 'not yet configured')").
async fn handle_login(req: &Request, env: &Env) -> Result<Response> {
    let client_id = match env.var("GITHUB_APP_CLIENT_ID") {
        Ok(v) => v.to_string(),
        Err(_) => return Response::error("GITHUB_APP_CLIENT_ID is not configured", 503),
    };
    let redirect_uri = match oauth_callback_url(req) {
        Ok(url) => url,
        Err(e) => return Response::error(format!("cannot build redirect_uri: {e}"), 500),
    };
    let state = match oauth::generate_state() {
        Ok(s) => s,
        Err(e) => return Response::error(format!("cannot generate oauth state: {e}"), 500),
    };
    let authorize_url = oauth::authorize_url(&client_id, &redirect_uri, &state);
    // `Response::redirect` builds on `web_sys::Response::redirect()`, whose
    // Fetch-spec "guard" is `immutable` — any attempt to add a header
    // (e.g. `Set-Cookie`) afterward throws `TypeError: Can't modify
    // immutable headers` (confirmed live under `wrangler dev`). Building
    // the 302 from `Response::empty()` + a fresh, mutable `Headers`
    // instead avoids that guard entirely.
    let response_headers = Headers::new();
    response_headers.set("location", &authorize_url)?;
    response_headers.set(
        "set-cookie",
        &format!(
            "{}={state}; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
            oauth::OAUTH_STATE_COOKIE_NAME,
            oauth::OAUTH_STATE_TTL_SECONDS
        ),
    )?;
    Ok(Response::empty()?
        .with_status(302)
        .with_headers(response_headers))
}

/// `GET /oauth/callback` (docs/design/auth.md § "Human auth: GitHub
/// OAuth"): verifies the `state` query parameter against the
/// `cc_oauth_state` cookie [`handle_login`] set (CSRF — see
/// `oauth.rs`'s module docs), exchanges the `code` for a user access
/// token ([`oauth::exchange_code`]), calls `GET /user`
/// ([`oauth::fetch_github_user`]), upserts the `users` row
/// ([`oauth::upsert_user`]), creates a session
/// ([`session::create_session`]), and sets the `__Host-cc_session`
/// cookie exactly as auth.md specifies
/// ([`session::build_set_cookie_header`]). Every failure path (missing/
/// mismatched state, missing `code`, a rejected code exchange, a failed
/// `GET /user` call) fails the request before any session is created —
/// mirrors `handle_begin_run`'s "every failure path rejects the whole
/// call" posture for machine auth.
async fn handle_oauth_callback(req: &Request, env: &Env) -> Result<Response> {
    let url = match req.url() {
        Ok(u) => u,
        Err(e) => return Response::error(format!("cannot parse request url: {e}"), 400),
    };
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            _ => {}
        }
    }
    let Some(code) = code else {
        return Response::error("missing code query parameter", 400);
    };
    let Some(state) = state else {
        return Response::error("missing state query parameter", 400);
    };

    let cookie_header = req.headers().get("cookie")?.unwrap_or_default();
    let cookie_state = session::parse_cookie_header(&cookie_header, oauth::OAUTH_STATE_COOKIE_NAME);
    // Ordinary equality compare, not constant-time — see oauth.rs's
    // module docs "CSRF state" for why that's the right call here.
    if cookie_state.as_deref() != Some(state.as_str()) {
        return Response::error("oauth state mismatch", 400);
    }

    let client_id = match env.var("GITHUB_APP_CLIENT_ID") {
        Ok(v) => v.to_string(),
        Err(_) => return Response::error("GITHUB_APP_CLIENT_ID is not configured", 503),
    };
    let client_secret = match env.secret("GITHUB_APP_CLIENT_SECRET") {
        Ok(v) => v.to_string(),
        Err(_) => return Response::error("GITHUB_APP_CLIENT_SECRET is not configured", 503),
    };
    let redirect_uri = match oauth_callback_url(req) {
        Ok(url) => url,
        Err(e) => return Response::error(format!("cannot build redirect_uri: {e}"), 500),
    };

    let token = match oauth::exchange_code(&client_id, &client_secret, &code, &redirect_uri).await {
        Ok(t) => t,
        Err(e) => return Response::error(format!("oauth code exchange failed: {e}"), 400),
    };
    let github_user = match oauth::fetch_github_user(&token.access_token).await {
        Ok(u) => u,
        Err(e) => return Response::error(format!("GET /user failed: {e}"), 400),
    };

    let now_s = (Date::now().as_millis() / 1000) as i64;
    let user_id = oauth::upsert_user(
        env,
        github_user.id,
        &github_user.login,
        github_user.email.as_deref(),
        now_s,
    )
    .await?;

    let session_id = match session::generate_session_id() {
        Ok(id) => id,
        Err(e) => return Response::error(format!("cannot generate session id: {e}"), 500),
    };
    let id_hash = session::hash_session_id(&session_id);
    let expires_at = now_s + session::SESSION_TTL_SECONDS;
    session::create_session(env, &id_hash, &user_id, now_s, expires_at).await?;

    let mut response = Response::ok(format!("Logged in as {}", github_user.login))?;
    response.headers_mut().append(
        "set-cookie",
        &session::build_set_cookie_header(&session_id, session::SESSION_TTL_SECONDS),
    )?;
    response.headers_mut().append(
        "set-cookie",
        &format!(
            "{}=; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
            oauth::OAUTH_STATE_COOKIE_NAME
        ),
    )?;
    Ok(response)
}

/// This deployment's own `/oauth/callback` URL, derived from the
/// incoming request — must exactly match what [`handle_login`] passed as
/// `redirect_uri` to GitHub, since GitHub validates the callback's
/// `redirect_uri` against the one used to start the flow.
fn oauth_callback_url(req: &Request) -> std::result::Result<String, String> {
    let url = req.url().map_err(|e| e.to_string())?;
    let mut callback = url.clone();
    callback.set_path("/oauth/callback");
    callback.set_query(None);
    Ok(callback.to_string())
}

/// `POST /v1/tokens` (docs/design/auth.md § "Scoped API tokens", "Role
/// model and permission matrix": "Issue, list, revoke scoped API tokens |
/// admin"). The first real issuance path for scoped API tokens — see
/// `src/token_issuance.rs`'s module doc comment for why earlier rounds
/// deliberately shipped no mint function anywhere in this crate, and why
/// this round is different.
///
/// # Auth
///
/// - **Authentication**: a valid `__Host-cc_session` cookie
///   ([`session::check_session`]) — missing/invalid/expired is `401`.
///   No separate CSRF token is checked; see `token_issuance.rs`'s "# CSRF"
///   module-doc section for why the cookie's `SameSite=Lax` attribute
///   alone is sufficient for this `POST`-only, non-navigable endpoint.
/// - **Authorization**: `admin` role ([`roles::resolve_role`]) on every
///   repo id the request needs it for:
///   - A finite `repo_allowlist`: admin on every id in that list.
///   - A `null` `repo_allowlist` ("all repos visible to `created_by`,
///     which may span installations" — auth.md's "Data model", a
///     strictly broader grant than any finite list): admin on *every*
///     repo this deployment knows about
///     ([`installations::list_all_repo_ids`]), not just the repos the
///     issuer happens to use. Any repo failing this check is `403`
///     ([`token_issuance::authorize`]).
///
/// On success, mints the token ([`token_issuance::generate_token`]),
/// hashes it ([`api_tokens::hash_token`] — reused, never duplicated), and
/// inserts the row with `created_by` set to the caller's real
/// `users.id` ([`token_issuance::insert_token_row`]). The plaintext
/// token is returned once in the response body and never stored.
async fn handle_issue_token(req: &mut Request, env: &Env) -> Result<Response> {
    let cookie_header = req.headers().get("cookie")?.unwrap_or_default();
    let Some(session_id) =
        session::parse_cookie_header(&cookie_header, session::SESSION_COOKIE_NAME)
    else {
        return json_error(401, "missing session cookie");
    };
    let id_hash = session::hash_session_id(&session_id);
    let now_s = (Date::now().as_millis() / 1000) as i64;
    let session_row = session::lookup_session(env, &id_hash).await?;
    let verified = match session::check_session(session_row.as_ref(), now_s) {
        Ok(v) => v,
        Err(_) => return json_error(401, "invalid or expired session"),
    };

    let body = req.bytes().await?;
    let request = match token_issuance::parse_request(&body) {
        Ok(r) => r,
        Err(e) => return json_error(400, &format!("{e}")),
    };
    if let Err(e) = token_issuance::validate_request(&request, now_s) {
        return json_error(400, &format!("{e}"));
    }

    // Which repo ids the issuer must be admin on — see
    // `token_issuance::authorize`'s doc comment for why a `null`
    // allowlist means "every repo this deployment knows about", not
    // merely the repos the issuer happens to use.
    let repo_ids: Vec<u64> = match &request.repo_allowlist {
        Some(ids) => ids.clone(),
        None => installations::list_all_repo_ids(env).await?,
    };

    let mut checked = Vec::with_capacity(repo_ids.len());
    for repo_id in &repo_ids {
        let role = roles::resolve_role(
            env,
            &verified.user_id,
            *repo_id,
            &verified.github_login,
            now_s,
        )
        .await?;
        checked.push((*repo_id, role));
    }
    if let Err(token_issuance::NotAdminOnRepo(repo_id)) = token_issuance::authorize(&checked) {
        return json_error(
            403,
            &format!("admin role required on repo {repo_id} to issue this token"),
        );
    }

    let plaintext = match token_issuance::generate_token() {
        Ok(t) => t,
        Err(e) => return json_error(500, &format!("cannot generate token: {e}")),
    };
    let hash = api_tokens::hash_token(&plaintext);
    let id = match ulid::generate(Date::now().as_millis()) {
        Ok(id) => id,
        Err(e) => return json_error(500, &format!("cannot generate token id: {e}")),
    };
    token_issuance::insert_token_row(
        env,
        &id,
        &hash,
        &request.name,
        &request.scopes,
        request.repo_allowlist.as_deref(),
        &verified.user_id,
        now_s,
        request.expires_at,
    )
    .await?;

    Response::from_json(&token_issuance::IssueTokenResponse {
        id,
        name: request.name,
        token: plaintext,
        scopes: request.scopes,
        repo_allowlist: request.repo_allowlist,
        expires_at: request.expires_at,
    })
}

/// Builds a `{"error": message}` JSON response with `status` — this
/// endpoint's uniform failure shape, since it is a plain JSON POST, not a
/// Connect RPC (`connect_error` is that family's equivalent).
fn json_error(status: u16, message: &str) -> Result<Response> {
    Ok(Response::from_json(&serde_json::json!({ "error": message }))?.with_status(status))
}

/// `POST /webhooks/github` (docs/design/auth.md § "Webhook signature
/// verification"). Reads the raw body first, verifies
/// `X-Hub-Signature-256` via [`webhook::verify_signature`], and rejects
/// with 401 *before* parsing anything or touching D1 on any failure —
/// the doc's explicit ordering requirement. `installation`,
/// `installation_repositories` (see `src/installations.rs`),
/// `workflow_run` (see `src/workflow_run.rs` — only its `completed`
/// action does anything, per docs/design/byo-ci.md's "Completion
/// semantics"), and `pull_request` (see `src/pull_request_webhook.rs` —
/// `opened`/`synchronize`/`closed` are handled, per pr-comment.md's
/// `PullRequestState` section) are handled this round; any other
/// `X-GitHub-Event` gets a 200 "not handled" ack so GitHub does not
/// retry-storm an event type this deployment doesn't act on yet
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
        Some("workflow_run") => handle_workflow_run_event(&raw_body, env).await,
        Some("pull_request") => handle_pull_request_event(&raw_body, env).await,
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

/// `workflow_run` `completed` (docs/design/byo-ci.md § "Completion
/// semantics"; see `src/workflow_run.rs` module docs for the full
/// correlation strategy and why `requested`/`in_progress` are no-ops
/// here). Looks the `(repo_id, external_url)` pair up against the
/// `runs` D1 projection; a miss, or a match that is already terminal, is
/// a silent 200 ack — the webhook isn't for a cloud-ci-tracked run, is
/// for a managed run, or the run already closed by another signal/this
/// same redelivery. Only a genuine, non-terminal match reaches
/// `RunCoordinator::close_run`.
async fn handle_workflow_run_event(raw_body: &[u8], env: &Env) -> Result<Response> {
    let event: workflow_run::WorkflowRunEvent = match serde_json::from_slice(raw_body) {
        Ok(event) => event,
        Err(_) => return Response::error("malformed workflow_run payload", 400),
    };

    let matched = workflow_run::find_run_by_external_url(
        env,
        event.repository.id,
        &event.workflow_run.html_url,
    )
    .await?;
    let matched_state = match &matched {
        Some(r) => Some(
            coordinator::logic::RunState::from_db_str(&r.status).ok_or_else(|| {
                worker::Error::RustError(format!("unknown run status {}", r.status))
            })?,
        ),
        None => None,
    };

    let decision = workflow_run::decide_close(&event.action, matched_state);
    let Some(matched) = matched.filter(|_| decision == workflow_run::CloseDecision::Close) else {
        return Response::ok("ok");
    };

    let do_name = coordinator::do_name(
        event.repository.id,
        &matched.sha,
        &matched.run_key,
        matched.attempt as u32,
    );
    let store = RunCoordinatorStore::new(env, &do_name)
        .map_err(|e| worker::Error::RustError(format!("run coordinator unavailable: {e}")))?;
    store
        .close_run()
        .await
        .map_err(|e| worker::Error::RustError(format!("close_run failed: {e}")))?;

    Response::ok("ok")
}

/// `pull_request` `opened`/`synchronize`/`closed` (docs/design/pr-comment.md
/// § "`PullRequestState`: one writer per PR"; see
/// `src/pull_request_webhook.rs` module docs for the full action-dispatch
/// decision, the `closed`-no-op rationale, and why a redelivered `opened`
/// is already idempotent). A malformed body is a 400; a DO-call failure
/// propagates as a `worker::Error` so the Worker's default error response
/// surfaces it — same posture as `handle_workflow_run_event`'s own `?`
/// propagation on `RunCoordinatorStore` failures. Every other outcome
/// (including the documented `closed`/unhandled-action no-ops) acks 200.
async fn handle_pull_request_event(raw_body: &[u8], env: &Env) -> Result<Response> {
    match pull_request_webhook::handle_pull_request_event(env, raw_body).await {
        Ok(()) => Response::ok("ok"),
        Err(pull_request_webhook::PullRequestWebhookError::MalformedPayload(msg)) => {
            Response::error(format!("malformed pull_request payload: {msg}"), 400)
        }
        Err(e @ pull_request_webhook::PullRequestWebhookError::DoCall(_)) => {
            Err(worker::Error::RustError(e.to_string()))
        }
    }
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
