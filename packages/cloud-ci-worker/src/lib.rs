pub mod connect;
pub mod coordinator;
pub mod ingest_token;
pub mod ulid;

use cloud_ci_proto::ingest::v1::{
    BeginRunRequest, BeginRunResponse, CompleteShardRequest, CompleteUploadRequest,
    CreateUploadRequest, GetRunRequest, GetRunResponse, StartJobRequest, StartJobResponse,
    SubmitReportRequest,
};
use connect::{Code, Codec, ConnectError, NegotiationError, negotiate};
use coordinator::{CoordinatorError, RunCoordinatorStore};
use worker::{Context, Date, Env, Headers, Method, Request, Response, Result, event};

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
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
    let body = req.bytes().await?;
    match route(&req.path(), codec, &body, &env).await {
        Ok(bytes) => {
            let headers = Headers::new();
            headers.set("content-type", codec.content_type())?;
            Ok(Response::from_bytes(bytes)?.with_headers(headers))
        }
        Err(err) => connect_error(&err),
    }
}

/// Hand-routes each `cloud_ci.ingest.v1.IngestService` procedure to its
/// request type, per ADR 0002 (Connect over `fetch`, no Tower server).
/// `BeginRun`, `StartJob`, and `GetRun` forward to the run's
/// `RunCoordinator` Durable Object; the remaining four need R2 upload
/// plumbing (a separate, later piece of work) and still just decode their
/// request before reporting `unimplemented`.
async fn route(
    path: &str,
    codec: Codec,
    body: &[u8],
    env: &Env,
) -> std::result::Result<Vec<u8>, ConnectError> {
    match path {
        "/cloud_ci.ingest.v1.IngestService/BeginRun" => handle_begin_run(codec, body, env).await,
        "/cloud_ci.ingest.v1.IngestService/StartJob" => handle_start_job(codec, body, env).await,
        "/cloud_ci.ingest.v1.IngestService/GetRun" => handle_get_run(codec, body, env).await,
        "/cloud_ci.ingest.v1.IngestService/CreateUpload" => Err(decode_then_unimplemented::<
            CreateUploadRequest,
        >(
            "CreateUpload", codec, body
        )),
        "/cloud_ci.ingest.v1.IngestService/CompleteUpload" => Err(decode_then_unimplemented::<
            CompleteUploadRequest,
        >(
            "CompleteUpload", codec, body
        )),
        "/cloud_ci.ingest.v1.IngestService/SubmitReport" => Err(decode_then_unimplemented::<
            SubmitReportRequest,
        >(
            "SubmitReport", codec, body
        )),
        "/cloud_ci.ingest.v1.IngestService/CompleteShard" => Err(decode_then_unimplemented::<
            CompleteShardRequest,
        >(
            "CompleteShard", codec, body
        )),
        _ => Err(ConnectError::new(
            Code::Unimplemented,
            format!("unknown procedure {path}"),
        )),
    }
}

async fn handle_begin_run(
    codec: Codec,
    body: &[u8],
    env: &Env,
) -> std::result::Result<Vec<u8>, ConnectError> {
    let req: BeginRunRequest = codec.decode(body)?;

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

fn coordinator_error(err: CoordinatorError) -> ConnectError {
    match err {
        CoordinatorError::NotFound => ConnectError::new(Code::NotFound, "run not found"),
        CoordinatorError::Conflict(msg) => ConnectError::new(Code::FailedPrecondition, msg),
        CoordinatorError::Internal(msg) => ConnectError::new(Code::Internal, msg),
    }
}

fn decode_then_unimplemented<M: buffa::Message + serde::de::DeserializeOwned>(
    name: &str,
    codec: Codec,
    body: &[u8],
) -> ConnectError {
    match codec.decode::<M>(body) {
        Ok(_) => ConnectError::new(Code::Unimplemented, format!("{name} is not implemented")),
        Err(err) => err,
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
    use cloud_ci_proto::ingest::v1::{CreateUploadRequest, RunKey};

    #[test]
    fn still_unimplemented_procedure_decodes_both_codecs_before_reporting_unimplemented()
    -> std::result::Result<(), ConnectError> {
        let req = CreateUploadRequest::default();
        for codec in [Codec::Proto, Codec::Json] {
            let body = codec.encode(&req)?;
            let err =
                decode_then_unimplemented::<CreateUploadRequest>("CreateUpload", codec, &body);
            assert_eq!(err.code, Code::Unimplemented, "{codec:?}");
        }
        Ok(())
    }

    #[test]
    fn still_unimplemented_procedure_with_malformed_body_is_invalid_argument() {
        let err = decode_then_unimplemented::<CreateUploadRequest>(
            "CreateUpload",
            Codec::Proto,
            &[0xff, 0xff],
        );
        assert_eq!(err.code, Code::InvalidArgument);
    }

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
}
