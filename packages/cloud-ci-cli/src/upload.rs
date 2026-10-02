//! Orchestrates `cloud-ci upload`: `BeginRun` -> `StartJob` -> for each
//! matched report/site file (`CreateUpload` -> raw `PUT` of the bytes ->
//! `CompleteUpload` -> `SubmitReport` for reports) -> `CompleteShard`, per
//! `docs/design/byo-ci.md`'s "`IngestService` (sketch)" RPC table and
//! "Control plane and data plane are different transports".
//!
//! Single-part uploads only: files over the 32 MiB single-part limit
//! (`docs/design/byo-ci.md`'s "Upload mechanics and resumability") abort
//! with an explicit error naming the oversized file. Multipart chunking is
//! out of scope for this pass.
//!
//! A server error at any step aborts the whole upload with a message naming
//! which RPC failed; there is no partial-success path.

use std::fs;
use std::path::{Path, PathBuf};

use cloud_ci_proto::ingest::v1::{
    BeginRunRequest, BeginRunResponse, CompleteShardRequest, CompleteShardResponse,
    CompleteUploadRequest, CompleteUploadResponse, Conclusion as ProtoConclusion,
    CreateUploadRequest, CreateUploadResponse, RunKey, StartJobRequest, StartJobResponse,
    SubmitReportRequest, SubmitReportResponse, Trigger, UploadKind, UploadPart,
    submit_report_request,
};
use sha2::{Digest, Sha256};

use crate::cli::{Conclusion, UploadArgs};
use crate::connect_client::{CallError, Client, Codec};
use crate::identity::{EnvSource, resolve_run_identity};

/// Fixed single-part limit, per `docs/design/byo-ci.md`'s "Upload mechanics
/// and resumability". Multipart chunking for larger files is out of scope.
const MAX_SINGLE_PART_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub struct UploadError {
    step: String,
    message: String,
}

impl UploadError {
    fn new(step: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            step: step.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed: {}", self.step, self.message)
    }
}

impl std::error::Error for UploadError {}

/// One glob-matched file plus the monorepo scope and logical upload name it
/// carries into `CreateUpload`/`SubmitReport`.
struct MatchedFile {
    path: PathBuf,
    scope: String,
    /// `CreateUploadRequest.name` / `SubmitReportRequest.name`. For reports
    /// this is the matched path (so two scopes sharing a report's usual
    /// filename, e.g. `results.json`, stay distinct within one job/shard,
    /// since `scope` is not part of the Worker's dedup key per the
    /// Idempotency section). For sites it follows the documented
    /// `<name>` / `<name>/<scope>` rule.
    name: String,
}

pub fn run(args: &UploadArgs, env: &dyn EnvSource) -> Result<(), UploadError> {
    let identity = resolve_run_identity(&args.run_identity_flags(), env)
        .map_err(|missing| UploadError::new("resolve run identity", missing.join(", ")))?;
    let job_name = crate::identity::resolve_job_name(args.job.as_deref(), env)
        .map_err(|e| UploadError::new("resolve job name", e))?;
    let token = resolve_credential(args.token.as_deref(), env)
        .map_err(|e| UploadError::new("resolve credential", e))?;

    let client = Client::new(identity.server_url.clone(), Codec::Json, token);

    let begin: BeginRunResponse = client
        .call(
            "BeginRun",
            &BeginRunRequest {
                key: RunKey {
                    repo_id: identity.repo_id,
                    sha: identity.sha.clone(),
                    run_key: identity.run_key.clone(),
                    attempt: identity.attempt,
                    ..Default::default()
                }
                .into(),
                trigger: trigger_for(env).into(),
                ..Default::default()
            },
        )
        .map_err(|e| connect_err("BeginRun", &e))?;

    // Every later call authenticates with the run-scoped ingest token
    // BeginRun returns, per docs/design/byo-ci.md's Auth section.
    let ingest_token = begin.ingest_token;
    let client = Client::new(
        identity.server_url.clone(),
        Codec::Json,
        Some(ingest_token.clone()),
    );

    let start: StartJobResponse = client
        .call(
            "StartJob",
            &StartJobRequest {
                run_id: begin.run_id,
                job_name,
                shard_total: 1,
                check_names: args.checks.clone(),
                ..Default::default()
            },
        )
        .map_err(|e| connect_err("StartJob", &e))?;

    // `--shard` is out of scope for this pass: every upload is shard 1 of 1
    // (docs/design/byo-ci.md's default), i.e. index 0 of a 1-shard job.
    let shard_index = 0;

    for report in &args.reports {
        for file in expand_glob(&report.glob, "--report")? {
            let upload_id = upload_one(
                &client,
                &identity.server_url,
                Some(&ingest_token),
                &start.job_id,
                shard_index,
                UploadKind::UPLOAD_KIND_REPORT,
                &file,
            )?;
            client
                .call::<SubmitReportRequest, SubmitReportResponse>(
                    "SubmitReport",
                    &SubmitReportRequest {
                        job_id: start.job_id.clone(),
                        shard_index,
                        report_kind: report.kind.clone(),
                        name: file.name.clone(),
                        scope: file.scope.clone(),
                        source: Some(submit_report_request::Source::UploadId(upload_id)),
                        ..Default::default()
                    },
                )
                .map_err(|e| connect_err("SubmitReport", &e))?;
        }
    }

    for site in &args.sites {
        let matches = expand_glob(&site.glob, "--site")?;
        let multiple = matches.len() > 1;
        for mut file in matches {
            file.name = if multiple {
                format!("{}/{}", site.name, file.scope)
            } else {
                site.name.clone()
            };
            upload_one(
                &client,
                &identity.server_url,
                Some(&ingest_token),
                &start.job_id,
                shard_index,
                UploadKind::UPLOAD_KIND_SITE,
                &file,
            )?;
        }
    }

    let conclusion = args
        .conclusion
        .map(proto_conclusion)
        .unwrap_or(ProtoConclusion::CONCLUSION_UNSPECIFIED);
    client
        .call::<CompleteShardRequest, CompleteShardResponse>(
            "CompleteShard",
            &CompleteShardRequest {
                job_id: start.job_id,
                shard_index,
                conclusion: conclusion.into(),
                ..Default::default()
            },
        )
        .map_err(|e| connect_err("CompleteShard", &e))?;

    Ok(())
}

fn connect_err(step: &str, err: &CallError) -> UploadError {
    UploadError::new(step, err.to_string())
}

fn trigger_for(env: &dyn EnvSource) -> Trigger {
    match env.var("GITHUB_EVENT_NAME").as_deref() {
        Some("pull_request") => Trigger::TRIGGER_PULL_REQUEST,
        Some("push") => Trigger::TRIGGER_PUSH,
        Some(_) => Trigger::TRIGGER_MANUAL,
        None => Trigger::TRIGGER_UNSPECIFIED,
    }
}

fn proto_conclusion(conclusion: Conclusion) -> ProtoConclusion {
    match conclusion {
        Conclusion::Success => ProtoConclusion::CONCLUSION_SUCCESS,
        Conclusion::Failure => ProtoConclusion::CONCLUSION_FAILURE,
        Conclusion::Cancelled => ProtoConclusion::CONCLUSION_CANCELLED,
    }
}

/// Expands one glob pattern to its matched files with their resolved
/// scopes. A glob that matches no files is a hard error, per
/// `docs/design/byo-ci.md`'s "Globs and scopes".
fn expand_glob(pattern: &str, flag: &str) -> Result<Vec<MatchedFile>, UploadError> {
    let step = format!("{flag} {pattern:?}");
    let entries =
        glob::glob(pattern).map_err(|e| UploadError::new(&step, format!("bad glob: {e}")))?;

    let mut matches = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| UploadError::new(&step, e.to_string()))?;
        if !path.is_file() {
            continue;
        }
        let scope = crate::scope::scope_for(&path);
        let name = path.to_string_lossy().into_owned();
        matches.push(MatchedFile { path, scope, name });
    }

    if matches.is_empty() {
        return Err(UploadError::new(&step, "glob matched no files"));
    }
    Ok(matches)
}

#[allow(clippy::too_many_arguments)]
fn upload_one(
    client: &Client,
    server_url: &str,
    token: Option<&str>,
    job_id: &str,
    shard_index: u32,
    kind: UploadKind,
    file: &MatchedFile,
) -> Result<String, UploadError> {
    let step = format!("CreateUpload {}", file.path.display());
    let bytes = fs::read(&file.path)
        .map_err(|e| UploadError::new(&step, format!("reading {}: {e}", file.path.display())))?;
    let size = bytes.len() as u64;
    if size > MAX_SINGLE_PART_BYTES {
        return Err(UploadError::new(
            &step,
            format!(
                "{} is {size} bytes, over the 32 MiB single-part limit; multipart chunking for \
                 larger files is out of scope for this pass",
                file.path.display()
            ),
        ));
    }

    let create: CreateUploadResponse = client
        .call(
            "CreateUpload",
            &CreateUploadRequest {
                job_id: job_id.to_string(),
                shard_index,
                kind: kind.into(),
                name: file.name.clone(),
                scope: file.scope.clone(),
                content_type: guess_content_type(&file.path),
                size_bytes: size,
                sha256: hex_sha256(&bytes),
                ..Default::default()
            },
        )
        .map_err(|e| connect_err("CreateUpload", &e))?;

    if create.already_complete {
        return Ok(create.upload_id);
    }

    let etag = put_part(server_url, &create.upload_id, 1, &bytes, token)
        .map_err(|e| UploadError::new("PUT upload part", e))?;

    client
        .call::<CompleteUploadRequest, CompleteUploadResponse>(
            "CompleteUpload",
            &CompleteUploadRequest {
                upload_id: create.upload_id.clone(),
                parts: vec![UploadPart {
                    number: 1,
                    etag,
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .map_err(|e| connect_err("CompleteUpload", &e))?;

    Ok(create.upload_id)
}

/// Raw `PUT {server_url}/ingest/v1/uploads/{upload_id}/parts/{n}`, per
/// `docs/design/byo-ci.md`'s "Control plane and data plane are different
/// transports" (data plane is plain HTTP, not a Connect RPC). Returns the
/// part's `ETag` for `CompleteUpload`.
fn put_part(
    server_url: &str,
    upload_id: &str,
    part: u32,
    bytes: &[u8],
    token: Option<&str>,
) -> Result<String, String> {
    let url = format!(
        "{}/ingest/v1/uploads/{upload_id}/parts/{part}",
        server_url.trim_end_matches('/'),
    );
    let mut builder = ureq::put(&url);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = builder.send(bytes).map_err(|e| e.to_string())?;
    Ok(response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn guess_content_type(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some("xml") => "application/xml",
        Some("json") => "application/json",
        Some("html" | "htm") => "text/html",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Resolves the bearer credential for every call after (and including)
/// `BeginRun`: explicit `--token`/`CLOUD_CI_TOKEN`, else a GitHub Actions
/// OIDC token fetched from `ACTIONS_ID_TOKEN_REQUEST_URL` when running in
/// GitHub Actions, else none (the server will reject the call). Exchanging
/// the OIDC JWT for a run-scoped ingest token is `BeginRun`'s job, not the
/// CLI's; this only acquires the raw JWT to send as the `BeginRun` bearer.
fn resolve_credential(
    explicit: Option<&str>,
    env: &dyn EnvSource,
) -> Result<Option<String>, String> {
    if let Some(token) = explicit {
        return Ok(Some(token.to_string()));
    }
    if let Some(token) = env.var("CLOUD_CI_TOKEN").filter(|t| !t.is_empty()) {
        return Ok(Some(token));
    }
    if env.var("GITHUB_ACTIONS").as_deref() == Some("true")
        && let (Some(url), Some(bearer)) = (
            env.var("ACTIONS_ID_TOKEN_REQUEST_URL"),
            env.var("ACTIONS_ID_TOKEN_REQUEST_TOKEN"),
        )
    {
        return fetch_github_actions_oidc_token(&url, &bearer).map(Some);
    }
    Ok(None)
}

fn fetch_github_actions_oidc_token(request_url: &str, bearer: &str) -> Result<String, String> {
    let mut response = ureq::get(request_url)
        .header("authorization", format!("Bearer {bearer}"))
        .call()
        .map_err(|e| format!("fetching GitHub Actions OIDC token: {e}"))?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("reading GitHub Actions OIDC token response: {e}"))?;
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("parsing GitHub Actions OIDC token response: {e}"))?;
    json.get("value")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "GitHub Actions OIDC token response had no \"value\" field".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ReportArg;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    #[test]
    fn expand_glob_errors_when_nothing_matches() {
        let err = expand_glob("/no/such/path/*.does-not-exist", "--report")
            .err()
            .map(|e| e.to_string());
        assert_eq!(
            err,
            Some(
                "--report \"/no/such/path/*.does-not-exist\" failed: glob matched no files"
                    .to_string()
            )
        );
    }

    #[test]
    fn resolve_credential_prefers_explicit_flag_over_env() {
        let env = crate::identity::MapEnv::new(&[
            ("CLOUD_CI_TOKEN", "env-token"),
            ("GITHUB_ACTIONS", "true"),
        ]);
        let token = resolve_credential(Some("flag-token"), &env);
        assert_eq!(token, Ok(Some("flag-token".to_string())));
    }

    #[test]
    fn resolve_credential_falls_back_to_cloud_ci_token_env() {
        let env = crate::identity::MapEnv::new(&[("CLOUD_CI_TOKEN", "env-token")]);
        let token = resolve_credential(None, &env);
        assert_eq!(token, Ok(Some("env-token".to_string())));
    }

    #[test]
    fn resolve_credential_is_none_outside_github_actions_with_nothing_set() {
        let env = crate::identity::MapEnv::new(&[]);
        let token = resolve_credential(None, &env);
        assert_eq!(token, Ok(None));
    }

    const FIXTURE_ETAG: &str = "etag-abc123";

    struct CapturedRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Reads one HTTP/1.1 request off a real socket: request line, headers,
    /// and a `content-length` body. Minimal by design, matching
    /// `connect_client.rs`'s `serve_once` approach rather than pulling in a
    /// test-only HTTP server dependency.
    fn read_request(stream: &mut TcpStream) -> std::io::Result<CapturedRequest> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                break buf.len();
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
        };

        let header_text = String::from_utf8_lossy(&buf[..header_end.min(buf.len())]).into_owned();
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap_or_default().to_string();
        let path = request_parts.next().unwrap_or_default().to_string();

        let mut headers = HashMap::new();
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }

        let content_length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let mut body = buf[header_end.min(buf.len())..].to_vec();
        while body.len() < content_length {
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(content_length);

        Ok(CapturedRequest {
            method,
            path,
            headers,
            body,
        })
    }

    fn write_response(
        stream: &mut TcpStream,
        status: u16,
        extra_headers: &[(&str, String)],
        body: &[u8],
    ) -> std::io::Result<()> {
        let mut response = format!(
            "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        for (key, value) in extra_headers {
            response.push_str(&format!("{key}: {value}\r\n"));
        }
        response.push_str("\r\n");
        stream.write_all(response.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()
    }

    /// Canned responses for the full `BeginRun` -> `CompleteShard` sequence
    /// one `--report` upload produces. Response bodies are built by
    /// serializing the real generated proto response types (not hand-written
    /// JSON strings), so the shape is guaranteed to match what the real
    /// client decodes.
    fn fixture_response(request: &CapturedRequest) -> (u16, Vec<(&'static str, String)>, Vec<u8>) {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/cloud_ci.ingest.v1.IngestService/BeginRun") => (
                200,
                vec![],
                serde_json::to_vec(&BeginRunResponse {
                    run_id: "run-1".into(),
                    ingest_token: "ingest-token-1".into(),
                    ..Default::default()
                })
                .unwrap_or_default(),
            ),
            ("POST", "/cloud_ci.ingest.v1.IngestService/StartJob") => (
                200,
                vec![],
                serde_json::to_vec(&StartJobResponse {
                    job_id: "job-1".into(),
                    ..Default::default()
                })
                .unwrap_or_default(),
            ),
            ("POST", "/cloud_ci.ingest.v1.IngestService/CreateUpload") => (
                200,
                vec![],
                serde_json::to_vec(&CreateUploadResponse {
                    upload_id: "upload-1".into(),
                    part_count: 1,
                    part_size_bytes: MAX_SINGLE_PART_BYTES,
                    already_complete: false,
                    ..Default::default()
                })
                .unwrap_or_default(),
            ),
            ("PUT", "/ingest/v1/uploads/upload-1/parts/1") => {
                (200, vec![("etag", FIXTURE_ETAG.to_string())], Vec::new())
            }
            ("POST", "/cloud_ci.ingest.v1.IngestService/CompleteUpload") => (
                200,
                vec![],
                serde_json::to_vec(&CompleteUploadResponse::default()).unwrap_or_default(),
            ),
            ("POST", "/cloud_ci.ingest.v1.IngestService/SubmitReport") => (
                200,
                vec![],
                serde_json::to_vec(&SubmitReportResponse::default()).unwrap_or_default(),
            ),
            ("POST", "/cloud_ci.ingest.v1.IngestService/CompleteShard") => (
                200,
                vec![],
                serde_json::to_vec(&CompleteShardResponse::default()).unwrap_or_default(),
            ),
            _ => (
                404,
                vec![],
                br#"{"code":"not_found","message":"unhandled fixture path"}"#.to_vec(),
            ),
        }
    }

    /// Starts a background server that accepts exactly `request_count` real
    /// socket connections, parses each request, replies from
    /// `fixture_response`, and records every request it saw for later
    /// assertions.
    fn start_fixture_server(
        request_count: usize,
    ) -> std::io::Result<(String, Arc<Mutex<Vec<CapturedRequest>>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_thread = Arc::clone(&captured);

        std::thread::spawn(move || {
            for _ in 0..request_count {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let Ok(request) = read_request(&mut stream) else {
                    continue;
                };
                let (status, headers, body) = fixture_response(&request);
                let _ = write_response(&mut stream, status, &headers, &body);
                if let Ok(mut log) = captured_for_thread.lock() {
                    log.push(request);
                }
            }
        });

        Ok((format!("http://{addr}"), captured))
    }

    /// Proves the whole `BeginRun` -> `StartJob` -> `CreateUpload` -> `PUT`
    /// -> `CompleteUpload` -> `SubmitReport` -> `CompleteShard` sequence
    /// actually executes over real sockets with the real glob/scope
    /// resolution and a real file on disk, not just that the calls are
    /// wired in the right order by inspection.
    #[test]
    fn full_upload_sequence_executes_against_a_real_http_fixture() -> Result<(), String> {
        let (base_url, captured) = start_fixture_server(7).map_err(|e| e.to_string())?;

        let tmp = std::env::temp_dir().join(format!(
            "cloud-ci-cli-upload-fixture-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&tmp);
        let report_path = tmp.join("junit.xml");
        std::fs::write(
            &report_path,
            b"<testsuite name=\"smoke\" tests=\"1\" failures=\"0\">\
              <testcase classname=\"smoke\" name=\"it_works\"/></testsuite>",
        )
        .map_err(|e| e.to_string())?;

        let args = UploadArgs {
            job: Some("smoke".to_string()),
            reports: vec![ReportArg {
                kind: "junit".to_string(),
                glob: report_path.to_string_lossy().into_owned(),
            }],
            sites: vec![],
            checks: vec![],
            conclusion: Some(Conclusion::Success),
            sha: Some("deadbeef".to_string()),
            run_key: Some("manual/fixture".to_string()),
            attempt: Some(1),
            repo_id: Some(1),
            server_url: Some(base_url),
            token: Some("initial-token".to_string()),
        };
        let env = crate::identity::MapEnv::new(&[]);

        let result = run(&args, &env);
        let _ = std::fs::remove_dir_all(&tmp);
        result.map_err(|e| format!("expected Ok(()), got Err: {e}"))?;

        let log = captured
            .lock()
            .map_err(|_| "captured request log poisoned".to_string())?;
        assert_eq!(log.len(), 7, "expected 7 requests, got {}", log.len());

        // BeginRun authenticates with the original --token; every later call
        // must switch to the run-scoped ingest_token BeginRun returned.
        let put = log
            .iter()
            .find(|r| r.method == "PUT")
            .ok_or_else(|| "no PUT request captured".to_string())?;
        assert_eq!(
            put.headers.get("authorization").map(String::as_str),
            Some("Bearer ingest-token-1"),
            "PUT must carry the BeginRun-issued ingest token, not the original --token"
        );
        assert!(
            put.body.starts_with(b"<testsuite"),
            "PUT body should be the report file's own bytes"
        );

        let begin_run = log
            .iter()
            .find(|r| r.path == "/cloud_ci.ingest.v1.IngestService/BeginRun")
            .ok_or_else(|| "no BeginRun request captured".to_string())?;
        assert_eq!(
            begin_run.headers.get("authorization").map(String::as_str),
            Some("Bearer initial-token"),
            "BeginRun must carry the original --token"
        );

        // The ETag the PUT fixture response returned must flow, unmodified,
        // into CompleteUpload's parts[0].etag.
        let complete_upload = log
            .iter()
            .find(|r| r.path == "/cloud_ci.ingest.v1.IngestService/CompleteUpload")
            .ok_or_else(|| "no CompleteUpload request captured".to_string())?;
        let complete_upload_body: serde_json::Value =
            serde_json::from_slice(&complete_upload.body).map_err(|e| e.to_string())?;
        let parts = complete_upload_body
            .get("parts")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "CompleteUpload had no parts array".to_string())?;
        assert_eq!(parts.len(), 1);
        assert_eq!(
            parts[0].get("number").and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(
            parts[0].get("etag").and_then(|v| v.as_str()),
            Some(FIXTURE_ETAG),
            "CompleteUpload must forward the exact ETag the PUT response returned, not a hardcoded placeholder"
        );

        let complete_shard = log
            .iter()
            .find(|r| r.path == "/cloud_ci.ingest.v1.IngestService/CompleteShard")
            .ok_or_else(|| "no CompleteShard request captured".to_string())?;
        let complete_shard_body: serde_json::Value =
            serde_json::from_slice(&complete_shard.body).map_err(|e| e.to_string())?;
        assert_eq!(
            complete_shard_body
                .get("conclusion")
                .and_then(|v| v.as_str()),
            Some("CONCLUSION_SUCCESS")
        );

        Ok(())
    }
}
