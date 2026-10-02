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
                job_name: args.job.clone(),
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
}
