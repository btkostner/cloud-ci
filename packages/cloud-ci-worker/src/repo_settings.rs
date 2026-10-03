//! Fetches, validates, and caches a repo's `.cloud-ci/settings.yml`
//! (`docs/design/settings.md`). `cloud_ci_core::settings::parse` already
//! exists; this module is the real fetch+cache path to hand it bytes.
//!
//! **Now settings.md-compliant end to end**: `BeginRun` (`coordinator/mod.rs`'s "Settings
//! SHA (frozen at admission)" module doc section) resolves and durably stores the default
//! branch's HEAD *at event time* on each run's `runs.settings_sha`. Every delayed consumer
//! (`lib.rs`'s `handle_analysis_requested` today; rightsizing later) reads that stored sha
//! and calls [`settings_for_sha`] with it — never resolves live HEAD itself.
//!
//! A `404` from the Contents API alone cannot distinguish "file absent"
//! from "sha unreadable", so [`settings_for_sha`] always verifies the
//! frozen sha via [`verify_frozen_commit_sha`] first — which also checks
//! GitHub's response `sha` matches exactly what was requested, since an
//! abbreviated/ambiguous ref can resolve to a different real commit.
//! Raw `settings.yml` text (not a serialized `Settings`) is cached once
//! a sha is immutable, regardless of parse outcome, and re-parsed on
//! every read rather than giving `cloud-ci-core` a serde dependency.
//! **Negative cache entries are permanent too**: `file_present = 0` for
//! `(repo_id, head_sha)` is cached exactly like a real file, and the
//! same `ON CONFLICT (repo_id, head_sha) DO NOTHING` immutability
//! applies — there is no TTL, no re-check, nothing short of a manual D1
//! row delete ever converts a cached "absent" back to "present" for the
//! same sha. `[unverified]` risk: [`verify_frozen_commit_sha`] confirms
//! the sha itself is readable before the Contents fetch, but if GitHub's
//! Contents API is served from a lagging read replica relative to the
//! commit-lookup API that answered `verify_frozen_commit_sha`, a
//! genuinely-present file could still 404 on the Contents call — which
//! this module cannot distinguish from "file really doesn't exist at
//! this sha" and would cache as `file_present = 0` forever.

use cloud_ci_core::settings::{self, Settings};
use percent_encoding::utf8_percent_encode as pct;
use serde::Deserialize;
use worker::wasm_bindgen::JsValue;
use worker::{Env, Headers, Method, Request, RequestInit};

use crate::roles::{self, RepoOwnerRow};

const GITHUB_API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "cloud-ci-worker";
const SETTINGS_PATH: &str = ".cloud-ci/settings.yml";
const GITHUB_API_BASE: &str = "https://api.github.com";

#[derive(Debug)]
pub enum RepoSettingsError {
    /// Repo lookup or installation-token exchange failed.
    Auth(String),
    /// A GitHub REST call failed, or returned non-2xx/404.
    GitHub(String),
    /// `settings.yml` exists but failed to parse. Never silently
    /// defaulted — see module docs.
    Invalid(Vec<settings::Diagnostic>),
    /// D1 cache read/write failed.
    Cache(String),
}

impl std::fmt::Display for RepoSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(e) => write!(f, "settings auth: {e}"),
            Self::GitHub(e) => write!(f, "settings GitHub API: {e}"),
            Self::Invalid(diags) => {
                write!(f, "settings.yml invalid: {} diagnostic(s)", diags.len())
            }
            Self::Cache(e) => write!(f, "settings cache: {e}"),
        }
    }
}

impl std::error::Error for RepoSettingsError {}

fn installation_token_headers(installation_token: &str) -> [(&'static str, String); 4] {
    [
        ("authorization", format!("Bearer {installation_token}")),
        ("accept", "application/vnd.github+json".to_string()),
        ("x-github-api-version", GITHUB_API_VERSION.to_string()),
        ("user-agent", USER_AGENT.to_string()),
    ]
}

fn build_headers(installation_token: &str) -> worker::Result<Headers> {
    let headers = Headers::new();
    for (name, value) in installation_token_headers(installation_token) {
        headers.set(name, &value)?;
    }
    Ok(headers)
}

/// Escapes everything outside RFC 3986's unreserved set plus `/` (git
/// refs legitimately contain `/`, e.g. `feature/foo`) — notably escapes
/// `#`, which would otherwise truncate the request at a URL fragment
/// boundary before it ever reaches GitHub.
const PATH_SAFE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// [`PATH_SAFE`] plus `/` escaped: a query value has no reason to carry
/// an unescaped path separator.
const QUERY_VALUE_SAFE: &percent_encoding::AsciiSet = &PATH_SAFE.add(b'/');

/// `base_url` parameterized for the unit tests below (`worker::Fetch`
/// needs the Workers runtime, so the real network calls have no plain
/// `cargo test` path — only these pure builders do).
fn repo_url(base_url: &str, owner: &str, repo: &str) -> String {
    format!(
        "{base_url}/repos/{}/{}",
        pct(owner, PATH_SAFE),
        pct(repo, PATH_SAFE)
    )
}

fn commit_url(base_url: &str, owner: &str, repo: &str, git_ref: &str) -> String {
    format!(
        "{base_url}/repos/{}/{}/commits/{}",
        pct(owner, PATH_SAFE),
        pct(repo, PATH_SAFE),
        pct(git_ref, PATH_SAFE)
    )
}

fn contents_url(base_url: &str, owner: &str, repo: &str, path: &str, sha: &str) -> String {
    format!(
        "{base_url}/repos/{}/{}/contents/{}?ref={}",
        pct(owner, PATH_SAFE),
        pct(repo, PATH_SAFE),
        pct(path, PATH_SAFE),
        pct(sha, QUERY_VALUE_SAFE)
    )
}

#[derive(Debug, Deserialize)]
struct RepoResponse {
    default_branch: String,
}

#[derive(Debug, Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct ContentsResponse {
    content: String,
    encoding: String,
}

/// GitHub's Contents API returns `encoding: "none"` with an empty `content` for a file too
/// large to return inline (undocumented exactly where the threshold sits, but real and
/// observed around 1MB) — indistinguishable from a genuinely empty/absent file without
/// checking this field explicitly. Decoding an empty `content` unconditionally would produce
/// empty bytes, which [`settings_for_sha`] would then cache *permanently* as
/// `file_present = 1, raw_text = ""` for this sha — a wrong answer no retry can ever correct.
/// Pure so it is unit-testable with plain `cargo test`; [`fetch_settings_yml_bytes`] calls
/// this before ever attempting to decode or cache anything.
fn check_contents_encoding(encoding: &str) -> Result<(), RepoSettingsError> {
    if encoding != "base64" {
        return Err(RepoSettingsError::GitHub(format!(
            "contents response has encoding {encoding:?}, expected \"base64\" (an oversized \
             file returns encoding \"none\" with empty content — never treat that as an \
             absent/empty settings.yml)"
        )));
    }
    Ok(())
}

/// `GET .../commits/{ref}` -> that ref's commit `sha`. `ref` may be a
/// branch name or a sha.
async fn fetch_commit(
    installation_token: &str,
    owner: &str,
    repo: &str,
    git_ref: &str,
) -> Result<CommitResponse, RepoSettingsError> {
    let headers = build_headers(installation_token)
        .map_err(|e| RepoSettingsError::GitHub(format!("cannot build headers: {e}")))?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    init.with_headers(headers);
    let request = Request::new_with_init(&commit_url(GITHUB_API_BASE, owner, repo, git_ref), &init)
        .map_err(|e| RepoSettingsError::GitHub(format!("cannot build commit request: {e}")))?;
    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| RepoSettingsError::GitHub(format!("commit lookup request failed: {e}")))?;
    if response.status_code() != 200 {
        return Err(RepoSettingsError::GitHub(format!(
            "commit lookup for {git_ref} returned {}",
            response.status_code()
        )));
    }
    response
        .json()
        .await
        .map_err(|e| RepoSettingsError::GitHub(format!("cannot parse commit response: {e}")))
}

/// Verifies `frozen_sha` is a real, readable commit whose own returned
/// `sha` matches exactly what was requested — an abbreviated/ambiguous
/// ref can resolve to a different commit, so a bare `200` alone is not
/// proof.
async fn verify_frozen_commit_sha(
    installation_token: &str,
    owner: &str,
    repo: &str,
    frozen_sha: &str,
) -> Result<(), RepoSettingsError> {
    let commit = fetch_commit(installation_token, owner, repo, frozen_sha).await?;
    if !commit.sha.eq_ignore_ascii_case(frozen_sha) {
        return Err(RepoSettingsError::GitHub(format!(
            "commit lookup for {frozen_sha} resolved to a different sha {}",
            commit.sha
        )));
    }
    Ok(())
}

pub async fn resolve_default_branch_head_sha(
    installation_token: &str,
    owner: &str,
    repo: &str,
) -> Result<(String, String), RepoSettingsError> {
    let headers = build_headers(installation_token)
        .map_err(|e| RepoSettingsError::GitHub(format!("cannot build headers: {e}")))?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    init.with_headers(headers);
    let request = Request::new_with_init(&repo_url(GITHUB_API_BASE, owner, repo), &init)
        .map_err(|e| RepoSettingsError::GitHub(format!("cannot build repo request: {e}")))?;
    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| RepoSettingsError::GitHub(format!("repo lookup request failed: {e}")))?;
    if response.status_code() != 200 {
        return Err(RepoSettingsError::GitHub(format!(
            "repo lookup returned {}",
            response.status_code()
        )));
    }
    let repo_body: RepoResponse = response
        .json()
        .await
        .map_err(|e| RepoSettingsError::GitHub(format!("cannot parse repo response: {e}")))?;
    let commit = fetch_commit(installation_token, owner, repo, &repo_body.default_branch).await?;
    Ok((repo_body.default_branch, commit.sha))
}

/// `Ok(None)` only for a real `404` — callers must confirm `sha` is a
/// real, readable commit first ([`verify_frozen_commit_sha`]), or a
/// `404` from an unreadable commit is indistinguishable from "file
/// absent".
async fn fetch_settings_yml_bytes(
    installation_token: &str,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Result<Option<Vec<u8>>, RepoSettingsError> {
    let headers = build_headers(installation_token)
        .map_err(|e| RepoSettingsError::GitHub(format!("cannot build headers: {e}")))?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    init.with_headers(headers);
    let request = Request::new_with_init(
        &contents_url(GITHUB_API_BASE, owner, repo, SETTINGS_PATH, sha),
        &init,
    )
    .map_err(|e| RepoSettingsError::GitHub(format!("cannot build contents request: {e}")))?;
    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| RepoSettingsError::GitHub(format!("contents request failed: {e}")))?;
    if response.status_code() == 404 {
        return Ok(None);
    }
    if response.status_code() != 200 {
        return Err(RepoSettingsError::GitHub(format!(
            "contents fetch for {SETTINGS_PATH}@{sha} returned {}",
            response.status_code()
        )));
    }
    let body: ContentsResponse = response
        .json()
        .await
        .map_err(|e| RepoSettingsError::GitHub(format!("cannot parse contents response: {e}")))?;
    check_contents_encoding(&body.encoding)?;
    let cleaned: String = body
        .content
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|e| RepoSettingsError::GitHub(format!("cannot decode base64 content: {e}")))?;
    Ok(Some(decoded))
}

#[derive(Debug, Deserialize)]
struct CachedSettingsRow {
    file_present: i64,
    raw_text: Option<String>,
}

async fn read_cached_settings(
    env: &Env,
    repo_id: i64,
    head_sha: &str,
) -> Result<Option<Result<Settings, RepoSettingsError>>, RepoSettingsError> {
    let db = env
        .d1("DB")
        .map_err(|e| RepoSettingsError::Cache(format!("D1 binding unavailable: {e}")))?;
    let row = db
        .prepare(
            "SELECT file_present, raw_text FROM repo_settings_cache \
             WHERE repo_id = ?1 AND head_sha = ?2",
        )
        .bind(&[
            JsValue::from_f64(repo_id as f64),
            JsValue::from_str(head_sha),
        ])
        .map_err(|e| RepoSettingsError::Cache(format!("cannot bind cache lookup: {e}")))?
        .first::<CachedSettingsRow>(None)
        .await
        .map_err(|e| RepoSettingsError::Cache(format!("cache lookup failed: {e}")))?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.file_present == 0 {
        return Ok(Some(Ok(Settings::default())));
    }
    let raw_text = row
        .raw_text
        .ok_or_else(|| RepoSettingsError::Cache("file_present=1 but no raw_text".into()))?;
    Ok(Some(parse_or_invalid(raw_text.as_bytes())))
}

/// Shared by the fresh-fetch and cache-hit-reparse paths so both produce
/// identical results for the same bytes.
fn parse_or_invalid(bytes: &[u8]) -> Result<Settings, RepoSettingsError> {
    let outcome = settings::parse(bytes);
    if outcome.has_errors() {
        return Err(RepoSettingsError::Invalid(outcome.diagnostics));
    }
    outcome.settings.ok_or_else(|| {
        RepoSettingsError::Cache("zero error diagnostics but ParseOutcome::settings is None".into())
    })
}

async fn write_cached_settings(
    env: &Env,
    repo_id: i64,
    head_sha: &str,
    file_present: bool,
    raw_text: Option<&str>,
    now_s: i64,
) -> Result<(), RepoSettingsError> {
    let db = env
        .d1("DB")
        .map_err(|e| RepoSettingsError::Cache(format!("D1 binding unavailable: {e}")))?;
    db.prepare(
        "INSERT INTO repo_settings_cache \
         (repo_id, head_sha, file_present, raw_text, cached_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (repo_id, head_sha) DO NOTHING",
    )
    .bind(&[
        JsValue::from_f64(repo_id as f64),
        JsValue::from_str(head_sha),
        JsValue::from_f64(if file_present { 1.0 } else { 0.0 }),
        raw_text.map(JsValue::from_str).unwrap_or(JsValue::NULL),
        JsValue::from_f64(now_s as f64),
    ])
    .map_err(|e| RepoSettingsError::Cache(format!("cannot bind cache write: {e}")))?
    .run()
    .await
    .map_err(|e| RepoSettingsError::Cache(format!("cache write failed: {e}")))?;
    Ok(())
}

pub async fn settings_for_sha(
    env: &Env,
    repo_id: i64,
    head_sha: &str,
) -> Result<Settings, RepoSettingsError> {
    if let Some(cached) = read_cached_settings(env, repo_id, head_sha).await? {
        return cached;
    }

    let owner_row: RepoOwnerRow = roles::lookup_repo_owner(env, repo_id as u64)
        .await
        .map_err(|e| RepoSettingsError::Auth(e.to_string()))?
        .ok_or_else(|| RepoSettingsError::Auth(format!("repo {repo_id} not registered")))?;
    let now_s = (worker::Date::now().as_millis() / 1000) as i64;
    let installation_token = roles::installation_token_for_repo(env, &owner_row, now_s)
        .await
        .map_err(|e| RepoSettingsError::Auth(e.to_string()))?;

    verify_frozen_commit_sha(
        &installation_token.token,
        &owner_row.owner_login,
        &owner_row.repo_name,
        head_sha,
    )
    .await?;

    let bytes = fetch_settings_yml_bytes(
        &installation_token.token,
        &owner_row.owner_login,
        &owner_row.repo_name,
        head_sha,
    )
    .await?;

    let Some(bytes) = bytes else {
        write_cached_settings(env, repo_id, head_sha, false, None, now_s).await?;
        return Ok(Settings::default());
    };

    let raw_text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            // settings::parse's own UTF-8 check produces the same diagnostic a caller would
            // see for any other malformed settings.yml (precise line/col, `Invalid` not a
            // one-off `GitHub`-layer error) — reused rather than reinvented here. Never
            // cached: there is no `raw_text` to cache for bytes that were never valid UTF-8
            // in the first place, unlike a genuinely malformed-but-valid-UTF-8 settings.yml
            // (cached below, raw_text and all, once it passes this check).
            return parse_or_invalid(&e.into_bytes());
        }
    };
    write_cached_settings(env, repo_id, head_sha, true, Some(&raw_text), now_s).await?;
    parse_or_invalid(raw_text.as_bytes())
}

/// Resolves the live default-branch HEAD sha only — no `settings.yml`
/// fetch, parse, or cache. For [`crate::coordinator`]'s admission-time
/// freeze: `RunCoordinator::handle_begin_run` (coordinator/mod.rs) calls
/// this once per run, before the run row is ever durably created, and
/// stores the result as `runs.settings_sha` so every delayed consumer of
/// that run reads this frozen sha (via [`settings_for_sha`]) instead of
/// ever resolving live HEAD itself — see coordinator/mod.rs's "Settings
/// SHA" module doc section for the full admission/race contract.
pub async fn resolve_default_branch_sha(
    env: &Env,
    repo_id: i64,
) -> Result<String, RepoSettingsError> {
    let owner_row: RepoOwnerRow = roles::lookup_repo_owner(env, repo_id as u64)
        .await
        .map_err(|e| RepoSettingsError::Auth(e.to_string()))?
        .ok_or_else(|| RepoSettingsError::Auth(format!("repo {repo_id} not registered")))?;
    let now_s = (worker::Date::now().as_millis() / 1000) as i64;
    let installation_token = roles::installation_token_for_repo(env, &owner_row, now_s)
        .await
        .map_err(|e| RepoSettingsError::Auth(e.to_string()))?;
    let (_default_branch, head_sha) = resolve_default_branch_head_sha(
        &installation_token.token,
        &owner_row.owner_login,
        &owner_row.repo_name,
    )
    .await?;
    Ok(head_sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_contents_encoding_accepts_base64() {
        assert!(check_contents_encoding("base64").is_ok());
    }

    #[test]
    fn check_contents_encoding_rejects_none_an_oversized_file_returns() {
        // GitHub returns `encoding: "none"` with empty `content` for a file too large for
        // the Contents API — this must be rejected explicitly, never silently decoded as an
        // empty file and cached as `file_present = 1, raw_text = ""` forever.
        assert!(matches!(
            check_contents_encoding("none"),
            Err(RepoSettingsError::GitHub(_))
        ));
    }

    #[test]
    fn check_contents_encoding_rejects_any_other_unexpected_value() {
        assert!(matches!(
            check_contents_encoding("utf-8"),
            Err(RepoSettingsError::GitHub(_))
        ));
    }

    #[test]
    fn repo_commit_contents_urls_match_the_documented_github_rest_shape() {
        assert_eq!(
            repo_url("https://api.github.com", "acme", "widgets"),
            "https://api.github.com/repos/acme/widgets"
        );
        assert_eq!(
            commit_url("https://api.github.com", "acme", "widgets", "main"),
            "https://api.github.com/repos/acme/widgets/commits/main"
        );
        assert_eq!(
            contents_url(
                "https://api.github.com",
                "acme",
                "widgets",
                ".cloud-ci/settings.yml",
                "deadbeef"
            ),
            "https://api.github.com/repos/acme/widgets/contents/.cloud-ci/settings.yml?ref=deadbeef"
        );
    }

    #[test]
    fn commit_url_percent_encodes_a_ref_containing_a_hash() {
        // `#` must never reach the URL unescaped: it would truncate the
        // request path at a fragment boundary before the HTTP client
        // even sends it.
        assert_eq!(
            commit_url("https://api.github.com", "acme", "widgets", "rel#1"),
            "https://api.github.com/repos/acme/widgets/commits/rel%231"
        );
    }

    #[test]
    fn commit_url_preserves_a_slash_in_a_branch_name() {
        assert_eq!(
            commit_url("https://api.github.com", "acme", "widgets", "feature/foo"),
            "https://api.github.com/repos/acme/widgets/commits/feature/foo"
        );
    }

    #[test]
    fn contents_url_percent_encodes_a_hash_in_the_ref_query_value() {
        assert_eq!(
            contents_url(
                "https://api.github.com",
                "acme",
                "widgets",
                ".cloud-ci/settings.yml",
                "rel#1"
            ),
            "https://api.github.com/repos/acme/widgets/contents/.cloud-ci/settings.yml?ref=rel%231"
        );
    }
}
