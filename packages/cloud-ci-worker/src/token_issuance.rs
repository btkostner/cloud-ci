//! `POST /v1/tokens` issuance (docs/design/auth.md § "Scoped API tokens",
//! "Role model and permission matrix": "Issue, list, revoke scoped API
//! tokens | admin") — mints a real, caller-presentable `cc_tok_...` token
//! for an admin-role, session-authenticated caller.
//!
//! `src/api_tokens.rs`'s module doc comment explains why no earlier round
//! shipped a mint function anywhere in this crate: an unrouted Rust
//! function that can produce a usable token is a standing privilege-
//! escalation risk with no corresponding benefit, for as long as the real
//! issuance surface (`cloud-ci login` or this very endpoint) doesn't
//! exist yet. That reasoning was about the *absence* of a reviewed
//! issuance path, not about issuance being permanently out of scope —
//! this module **is** that reviewed path: role-gated
//! ([`crate::roles::resolve_role`], admin-only), session-authenticated
//! ([`crate::session::check_session`]), and the only place in the crate
//! allowed to call [`generate_token`] / produce a plaintext token.
//! `api_tokens.rs` itself is untouched except for this module reusing its
//! [`crate::api_tokens::hash_token`] — it stays the read-only
//! verification module it always was; this module is the write side.
//!
//! Same three-layer split as the rest of this crate:
//!
//! 1. [`IssueTokenRequest`]/[`validate_request`] (parsing + validation)
//!    and [`authorize`] (the per-repo-admin decision) are pure, unit-
//!    tested with plain `cargo test`.
//! 2. [`generate_token`] needs the platform RNG (same `getrandom`
//!    dependency as `session::generate_session_id`/`ulid::generate`) but
//!    not the Workers runtime; its output shape is unit-tested, the
//!    randomness itself is not.
//! 3. [`insert_token_row`] needs the Workers runtime (D1) and is only
//!    exercised by the live smoke test (`mise run
//!    //packages/cloud-ci-worker:dev`), same as every other D1 writer in
//!    this crate.
//!
//! `lib.rs::handle_issue_token` is the only caller: it authenticates the
//! session, resolves the repo ids the request needs admin on (the
//! request's own `repo_allowlist`, or — for a `null` allowlist — every
//! repo this deployment knows about, see [`authorize`]'s doc comment),
//! calls [`crate::roles::resolve_role`] for each, and only on an
//! all-admin result calls [`generate_token`] + [`insert_token_row`].

use crate::roles::Role;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use worker::Env;
use worker::wasm_bindgen::JsValue;

/// The `cc_tok_` prefix every opaque scoped API token carries
/// (docs/design/auth.md § "Scoped API tokens": `cc_tok_<32 random bytes,
/// base64url>`).
pub const TOKEN_PREFIX: &str = "cc_tok_";

/// The three scopes auth.md's "Scoped API tokens" table defines. Any
/// other value in a request's `scopes` is rejected by [`validate_request`].
pub const KNOWN_SCOPES: &[&str] = &["ingest:write", "query:read", "admin:tokens"];

/// `POST /v1/tokens`'s request body. auth.md names the semantics
/// (`scopes` array, optional `repo_allowlist`, `created_by`, `expires_at`)
/// but not a wire schema — this is this round's pick:
///
/// ```json
/// {
///   "name": "buildkite-prod",
///   "scopes": ["ingest:write"],
///   "repo_allowlist": [123, 456],
///   "expires_at": 1735689600
/// }
/// ```
///
/// `repo_allowlist` and `expires_at` are **required JSON keys** holding
/// **nullable** values, not optional/absent-defaulting ones: a plain
/// `Option<T>` field without `#[serde(default)]` still rejects a missing
/// key, it only accepts an explicit `null` value for a present one. This
/// is the deliberate guard for `repo_allowlist` specifically — a `null`
/// allowlist means "every repo this deployment knows about" (auth.md's
/// "Data model": "null = all repos visible to `created_by`, which may
/// span installations"), a strictly broader grant than any finite list,
/// so an issuer has to type the literal word `null` on purpose; a request
/// body that simply omits the field is a `400`, never a silent
/// all-repos token.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueTokenRequest {
    pub name: String,
    pub scopes: Vec<String>,
    pub repo_allowlist: Option<Vec<u64>>,
    pub expires_at: Option<i64>,
}

/// `POST /v1/tokens`'s success response: the plaintext token (shown once,
/// per auth.md — never recoverable afterward) plus the row's metadata.
/// Deliberately has no `token_hash`/internal field.
#[derive(Debug, Clone, Serialize)]
pub struct IssueTokenResponse {
    pub id: String,
    pub name: String,
    pub token: String,
    pub scopes: Vec<String>,
    pub repo_allowlist: Option<Vec<u64>>,
    pub expires_at: Option<i64>,
}

/// Why an [`IssueTokenRequest`] fails validation, before any role check
/// or D1 write happens. Pure, unit-tested with plain `cargo test`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    EmptyName,
    EmptyScopes,
    UnknownScope(String),
    EmptyRepoAllowlist,
    AlreadyExpired,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::EmptyName => write!(f, "name must not be empty"),
            RequestError::EmptyScopes => write!(f, "scopes must not be empty"),
            RequestError::UnknownScope(s) => write!(f, "unknown scope: {s}"),
            RequestError::EmptyRepoAllowlist => write!(
                f,
                "repo_allowlist must not be an empty list (use null for all repos)"
            ),
            RequestError::AlreadyExpired => write!(f, "expires_at is already in the past"),
        }
    }
}

impl std::error::Error for RequestError {}

/// Pure request-shape validation — no role checks, no D1. `now_unix_s` is
/// a parameter, not read internally, same determinism reasoning as every
/// other `now_unix_s`-taking pure function in this crate.
pub fn validate_request(req: &IssueTokenRequest, now_unix_s: i64) -> Result<(), RequestError> {
    if req.name.trim().is_empty() {
        return Err(RequestError::EmptyName);
    }
    if req.scopes.is_empty() {
        return Err(RequestError::EmptyScopes);
    }
    for scope in &req.scopes {
        if !KNOWN_SCOPES.contains(&scope.as_str()) {
            return Err(RequestError::UnknownScope(scope.clone()));
        }
    }
    if let Some(allowlist) = &req.repo_allowlist
        && allowlist.is_empty()
    {
        return Err(RequestError::EmptyRepoAllowlist);
    }
    if let Some(expires_at) = req.expires_at
        && expires_at <= now_unix_s
    {
        return Err(RequestError::AlreadyExpired);
    }
    Ok(())
}

/// Why the caller may not mint the requested token: the per-repo-admin
/// decision failed for this repo id. auth.md's permission matrix: "Issue,
/// list, revoke scoped API tokens | admin", evaluated per repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotAdminOnRepo(pub u64);

/// Pure "does this set of already-resolved per-repo roles authorize this
/// issuance request" decision — kept separate from the
/// `roles::resolve_role` D1/GitHub calls so it is unit-testable with
/// plain `cargo test`, same layering as `installations::check_allowlist`
/// / `api_tokens::check_token`.
///
/// `checked` is `(repo_id, role)` for every repo id the request needs
/// admin on:
///
/// - **Finite `repo_allowlist`**: every id in that list.
/// - **`null` `repo_allowlist`**: every `repo_id` this deployment knows
///   about (`lib.rs::handle_issue_token` builds this via
///   `installations::list_all_repo_ids`), not just the repos the issuer
///   happens to use. auth.md defines a `null` allowlist as "all repos
///   visible to `created_by`" — a strictly broader grant than any finite
///   list the issuer could name — so minting one is a *more* privileged
///   operation than minting a scoped one, and requires proof the issuer
///   is admin everywhere, not merely "admin on whatever repos they
///   thought to list." An empty `checked` (a deployment with zero
///   registered repos requesting a null allowlist) vacuously authorizes —
///   there is nothing to be admin on, and nothing the resulting token
///   could reach either.
pub fn authorize(checked: &[(u64, Role)]) -> Result<(), NotAdminOnRepo> {
    for (repo_id, role) in checked {
        if *role != Role::Admin {
            return Err(NotAdminOnRepo(*repo_id));
        }
    }
    Ok(())
}

/// Generates a fresh scoped API token's plaintext value (auth.md §
/// "Scoped API tokens": `cc_tok_<32 random bytes, base64url>`). Mirrors
/// `session::generate_session_id`/`oauth::generate_state`'s RNG-then-
/// encode shape. **This is the only function in the crate allowed to
/// produce a usable token plaintext** — see the module doc comment for
/// why that was deliberately not true before this round.
pub fn generate_token() -> Result<String, getrandom::Error> {
    let mut random = [0u8; 32];
    getrandom::getrandom(&mut random)?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
    Ok(format!("{TOKEN_PREFIX}{encoded}"))
}

// ---------------------------------------------------------------------------
// D1 writes — needs the Workers runtime, not covered by `cargo test` (see
// module docs).
// ---------------------------------------------------------------------------

/// Inserts the freshly minted `api_tokens` row. `token_hash` is
/// [`crate::api_tokens::hash_token`]'s output for the plaintext
/// [`generate_token`] returned — the plaintext is never stored, only
/// returned once in the `POST /v1/tokens` response. `created_by` is the
/// issuing admin's real `users.id`, now that a session-authenticated
/// caller exists (`migrations/0005_api_tokens.sql`'s header comment
/// describes the earlier free-text/NULL placeholder this replaces).
#[allow(clippy::too_many_arguments)]
pub async fn insert_token_row(
    env: &Env,
    id: &str,
    token_hash: &[u8; 32],
    name: &str,
    scopes: &[String],
    repo_allowlist: Option<&[u64]>,
    created_by: &str,
    created_at_s: i64,
    expires_at: Option<i64>,
) -> worker::Result<()> {
    let db = env.d1("DB")?;
    let scopes_json = serde_json::to_string(scopes)
        .map_err(|e| worker::Error::RustError(format!("cannot encode scopes: {e}")))?;
    let repo_allowlist_json = repo_allowlist
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| worker::Error::RustError(format!("cannot encode repo_allowlist: {e}")))?;
    db.prepare(
        "INSERT INTO api_tokens \
         (id, token_hash, name, scopes, repo_allowlist, created_by, created_at, expires_at, last_used_at, revoked_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)",
    )
    .bind(&[
        JsValue::from_str(id),
        JsValue::from(worker::js_sys::Uint8Array::from(token_hash.as_slice())),
        JsValue::from_str(name),
        JsValue::from_str(&scopes_json),
        repo_allowlist_json
            .map(|j| JsValue::from_str(&j))
            .unwrap_or(JsValue::NULL),
        JsValue::from_str(created_by),
        JsValue::from_f64(created_at_s as f64),
        expires_at
            .map(|e| JsValue::from_f64(e as f64))
            .unwrap_or(JsValue::NULL),
    ])?
    .run()
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_request() -> IssueTokenRequest {
        IssueTokenRequest {
            name: "buildkite-prod".to_string(),
            scopes: vec!["ingest:write".to_string()],
            repo_allowlist: Some(vec![123, 456]),
            expires_at: None,
        }
    }

    #[test]
    fn valid_request_passes() {
        assert_eq!(validate_request(&valid_request(), 1_000), Ok(()));
    }

    #[test]
    fn null_repo_allowlist_passes() {
        let mut req = valid_request();
        req.repo_allowlist = None;
        assert_eq!(validate_request(&req, 1_000), Ok(()));
    }

    #[test]
    fn empty_name_rejected() {
        let mut req = valid_request();
        req.name = "   ".to_string();
        assert_eq!(validate_request(&req, 1_000), Err(RequestError::EmptyName));
    }

    #[test]
    fn empty_scopes_rejected() {
        let mut req = valid_request();
        req.scopes = vec![];
        assert_eq!(
            validate_request(&req, 1_000),
            Err(RequestError::EmptyScopes)
        );
    }

    #[test]
    fn unknown_scope_rejected() {
        let mut req = valid_request();
        req.scopes = vec!["sudo:everything".to_string()];
        assert_eq!(
            validate_request(&req, 1_000),
            Err(RequestError::UnknownScope("sudo:everything".to_string()))
        );
    }

    #[test]
    fn empty_repo_allowlist_list_rejected() {
        let mut req = valid_request();
        req.repo_allowlist = Some(vec![]);
        assert_eq!(
            validate_request(&req, 1_000),
            Err(RequestError::EmptyRepoAllowlist)
        );
    }

    #[test]
    fn already_expired_rejected() {
        let mut req = valid_request();
        req.expires_at = Some(999);
        assert_eq!(
            validate_request(&req, 1_000),
            Err(RequestError::AlreadyExpired)
        );
    }

    #[test]
    fn expires_at_in_future_accepted() {
        let mut req = valid_request();
        req.expires_at = Some(1_001);
        assert_eq!(validate_request(&req, 1_000), Ok(()));
    }

    #[test]
    fn expires_at_at_exact_boundary_is_rejected() {
        // Matches `api_tokens::check_token`'s `expires_at <= now_unix_s`
        // boundary convention.
        let mut req = valid_request();
        req.expires_at = Some(1_000);
        assert_eq!(
            validate_request(&req, 1_000),
            Err(RequestError::AlreadyExpired)
        );
    }

    #[test]
    fn all_admin_authorizes() {
        let checked = [(1, Role::Admin), (2, Role::Admin)];
        assert_eq!(authorize(&checked), Ok(()));
    }

    #[test]
    fn one_non_admin_rejects_with_its_repo_id() {
        let checked = [(1, Role::Admin), (2, Role::Operator)];
        assert_eq!(authorize(&checked), Err(NotAdminOnRepo(2)));
    }

    #[test]
    fn viewer_rejected_too() {
        let checked = [(1, Role::Viewer)];
        assert_eq!(authorize(&checked), Err(NotAdminOnRepo(1)));
    }

    #[test]
    fn empty_checked_list_vacuously_authorizes() {
        assert_eq!(authorize(&[]), Ok(()));
    }

    #[test]
    fn generated_token_has_expected_prefix_and_length() -> Result<(), getrandom::Error> {
        let token = generate_token()?;
        assert!(token.starts_with(TOKEN_PREFIX));
        // 32 random bytes, base64url (no padding) = 43 chars.
        assert_eq!(token.len(), TOKEN_PREFIX.len() + 43);
        Ok(())
    }

    #[test]
    fn generated_tokens_are_distinct() -> Result<(), getrandom::Error> {
        let a = generate_token()?;
        let b = generate_token()?;
        assert_ne!(a, b);
        Ok(())
    }
}
