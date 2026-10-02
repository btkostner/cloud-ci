//! Scoped API token verification (docs/design/auth.md § "Scoped API
//! tokens", "Data model"): the second `BeginRun` credential branch,
//! alongside GitHub Actions OIDC (`src/oidc.rs`).
//!
//! A scoped API token is an opaque, caller-presented string of the form
//! `cc_tok_<32 random bytes, base64url>`. Unlike the ingest/job token
//! format (`src/ingest_token.rs`, HMAC-signed and stateless), this family
//! is **opaque and hashed-in-D1**: only `sha256(token)` is ever stored
//! (`api_tokens.token_hash`), so a token that lives for months can be
//! revoked by deleting/flagging its row rather than rotating a shared
//! signing key for every outstanding token at once. See
//! docs/design/auth.md's "Alternatives considered" for the full
//! reasoning.
//!
//! **This module stays read-only / verification-only**, even now that a
//! real issuance surface exists. An earlier round deliberately shipped no
//! `mint`/issuance function here, in this binary, or anywhere else, with
//! the reasoning that an unrouted Rust function capable of minting a
//! usable token is a standing privilege-escalation risk (an accidental
//! future route, or a copied pattern) for as long as the real issuance
//! surface — `cloud-ci login` or the admin-only `POST /v1/tokens`
//! dashboard endpoint (docs/design/auth.md) — didn't exist yet. That
//! surface now exists: `POST /v1/tokens` is implemented in
//! `src/token_issuance.rs`, session-authenticated and admin-role-gated
//! ([`crate::session`]/[`crate::roles`]), not an informal bootstrap path.
//! It reuses [`hash_token`] from here (never duplicates the hashing
//! logic) but owns its own token generation and `INSERT`; this module
//! itself gained no new capability and still cannot mint a token. For
//! local development against a token this module alone would need to
//! verify (without driving a real session/OAuth flow), a token can still
//! be hand-crafted with `openssl`/`wrangler d1 execute` — the exact
//! recipe is documented in `migrations/0005_api_tokens.sql`'s header
//! comment.
//!
//! Same layering as `installations.rs`: [`hash_token`] and [`check_token`]
//! are pure and unit-tested with plain `cargo test`; [`lookup_by_hash`]
//! and [`touch_last_used`] need the Workers runtime (D1) and are only
//! exercised by the live smoke test.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use worker::Env;
use worker::wasm_bindgen::JsValue;

/// SHA-256 of the full presented token string (including the `cc_tok_`
/// prefix), matching the hash stored in `api_tokens.token_hash` —
/// mirrors the recipe documented in `migrations/0005_api_tokens.sql`.
/// Pure, unit-testable without the Workers runtime.
pub fn hash_token(token: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.finalize().into()
}

/// One `api_tokens` row, as needed by [`check_token`]. `id` is kept so
/// callers (e.g. [`touch_last_used`]) can update `last_used_at` without a
/// second lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiTokenRow {
    pub id: String,
    pub scopes: Vec<String>,
    /// `None` = all repos (docs/design/auth.md: "`repo_allowlist` ...
    /// null = all repos visible to `created_by`").
    pub repo_allowlist: Option<Vec<u64>>,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

/// Why a presented token failed verification
/// (docs/design/auth.md § "Scoped API tokens") — the caller
/// (`lib.rs::handle_begin_run`) maps every variant to a rejected call;
/// none of them ever fall through to success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckError {
    /// No `api_tokens` row has this hash — either the token never
    /// existed or was typo'd/truncated.
    NotFound,
    /// `revoked_at IS NOT NULL`.
    Revoked,
    /// `expires_at IS NOT NULL` and has passed `now_unix_s`.
    Expired,
    /// `required_scope` is not in the row's `scopes`.
    MissingScope,
    /// `repo_allowlist` is non-null and does not contain `repo_id`.
    RepoNotAllowed,
}

/// Pure "does this already-looked-up row authorize `required_scope` for
/// `repo_id`" decision, given the row [`lookup_by_hash`] fetched for the
/// presented token's hash. Kept separate from the D1 fetch so it is
/// unit-testable with plain `cargo test`, same layering as
/// `installations::check_allowlist`. `now_unix_s` is a parameter, not
/// read from a clock internally, for the same determinism/testability
/// reason as `ingest_token::verify`'s.
pub fn check_token(
    row: Option<&ApiTokenRow>,
    required_scope: &str,
    repo_id: u64,
    now_unix_s: i64,
) -> Result<(), CheckError> {
    let row = row.ok_or(CheckError::NotFound)?;
    if row.revoked_at.is_some() {
        return Err(CheckError::Revoked);
    }
    if let Some(expires_at) = row.expires_at
        && expires_at <= now_unix_s
    {
        return Err(CheckError::Expired);
    }
    if !row.scopes.iter().any(|s| s == required_scope) {
        return Err(CheckError::MissingScope);
    }
    if let Some(allowlist) = &row.repo_allowlist
        && !allowlist.contains(&repo_id)
    {
        return Err(CheckError::RepoNotAllowed);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// D1 reads/writes — needs the Workers runtime, not covered by `cargo test`
// (see module docs).
// ---------------------------------------------------------------------------

/// Raw `api_tokens` row shape as it comes back from D1 — `scopes`/
/// `repo_allowlist` are stored as JSON-encoded TEXT columns (SQLite has
/// no native array type), decoded here before handing a typed
/// [`ApiTokenRow`] to [`check_token`].
#[derive(Debug, Deserialize)]
struct RawApiTokenRow {
    id: String,
    scopes: String,
    repo_allowlist: Option<String>,
    expires_at: Option<i64>,
    revoked_at: Option<i64>,
}

/// Looks up the `api_tokens` row whose `token_hash` matches `hash`
/// (uniquely indexed, `migrations/0005_api_tokens.sql`). `None` means no
/// row has this hash — [`check_token`] turns that into
/// [`CheckError::NotFound`].
pub async fn lookup_by_hash(env: &Env, hash: &[u8; 32]) -> worker::Result<Option<ApiTokenRow>> {
    let db = env.d1("DB")?;
    let row = db
        .prepare(
            "SELECT id, scopes, repo_allowlist, expires_at, revoked_at \
             FROM api_tokens WHERE token_hash = ?1",
        )
        .bind(&[JsValue::from(worker::js_sys::Uint8Array::from(
            hash.as_slice(),
        ))])?
        .first::<RawApiTokenRow>(None)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let scopes: Vec<String> = serde_json::from_str(&row.scopes).map_err(|e| {
        worker::Error::RustError(format!("api_tokens row has invalid scopes JSON: {e}"))
    })?;
    let repo_allowlist = row
        .repo_allowlist
        .map(|raw| {
            serde_json::from_str::<Vec<u64>>(&raw).map_err(|e| {
                worker::Error::RustError(format!(
                    "api_tokens row has invalid repo_allowlist JSON: {e}"
                ))
            })
        })
        .transpose()?;
    Ok(Some(ApiTokenRow {
        id: row.id,
        scopes,
        repo_allowlist,
        expires_at: row.expires_at,
        revoked_at: row.revoked_at,
    }))
}

/// Best-effort `last_used_at` update (docs/design/auth.md's "Data model"
/// row: "updated best-effort (not every request needs a write)"). Callers
/// must not fail the request if this fails — see
/// `lib.rs::verify_scoped_api_token_begin_run_credential`, which logs and
/// ignores the error rather than propagating it.
pub async fn touch_last_used(env: &Env, token_id: &str, now_unix_s: i64) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare("UPDATE api_tokens SET last_used_at = ?1 WHERE id = ?2")
        .bind(&[
            JsValue::from_f64(now_unix_s as f64),
            JsValue::from_str(token_id),
        ])?
        .run()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        scopes: &[&str],
        repo_allowlist: Option<&[u64]>,
        expires_at: Option<i64>,
        revoked_at: Option<i64>,
    ) -> ApiTokenRow {
        ApiTokenRow {
            id: "01HQTESTTOKENID0000000000".to_string(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            repo_allowlist: repo_allowlist.map(|r| r.to_vec()),
            expires_at,
            revoked_at,
        }
    }

    #[test]
    fn hash_token_is_deterministic_sha256_of_full_string() {
        let a = hash_token("cc_tok_abc");
        let b = hash_token("cc_tok_abc");
        let c = hash_token("cc_tok_def");
        assert_eq!(a, b);
        assert_ne!(a, c);
        // Known SHA-256("cc_tok_abc") — sanity check against a plain hash,
        // not an incidental implementation detail.
        let mut hasher = Sha256::new();
        hasher.update(b"cc_tok_abc");
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(a, expected);
    }

    #[test]
    fn valid_token_with_matching_scope_and_repo_succeeds() {
        let r = row(&["ingest:write"], Some(&[1, 2, 3]), None, None);
        assert_eq!(check_token(Some(&r), "ingest:write", 2, 1_000), Ok(()));
    }

    #[test]
    fn null_allowlist_accepts_any_repo() {
        let r = row(&["ingest:write"], None, None, None);
        assert_eq!(check_token(Some(&r), "ingest:write", 999, 1_000), Ok(()));
    }

    #[test]
    fn missing_row_is_not_found() {
        assert_eq!(
            check_token(None, "ingest:write", 1, 1_000),
            Err(CheckError::NotFound)
        );
    }

    #[test]
    fn revoked_token_rejected() {
        let r = row(&["ingest:write"], None, None, Some(500));
        assert_eq!(
            check_token(Some(&r), "ingest:write", 1, 1_000),
            Err(CheckError::Revoked)
        );
    }

    #[test]
    fn expired_token_rejected() {
        let r = row(&["ingest:write"], None, Some(999), None);
        assert_eq!(
            check_token(Some(&r), "ingest:write", 1, 1_000),
            Err(CheckError::Expired)
        );
    }

    #[test]
    fn token_not_yet_expired_at_exact_boundary_is_still_rejected() {
        // `expires_at <= now_unix_s` rejects at the exact boundary second,
        // matching ingest_token::verify's `exp <= now_unix_s` convention.
        let r = row(&["ingest:write"], None, Some(1_000), None);
        assert_eq!(
            check_token(Some(&r), "ingest:write", 1, 1_000),
            Err(CheckError::Expired)
        );
    }

    #[test]
    fn unexpired_token_accepted() {
        let r = row(&["ingest:write"], None, Some(1_001), None);
        assert_eq!(check_token(Some(&r), "ingest:write", 1, 1_000), Ok(()));
    }

    #[test]
    fn wrong_scope_rejected() {
        let r = row(&["query:read"], None, None, None);
        assert_eq!(
            check_token(Some(&r), "ingest:write", 1, 1_000),
            Err(CheckError::MissingScope)
        );
    }

    #[test]
    fn repo_not_in_allowlist_rejected() {
        let r = row(&["ingest:write"], Some(&[1, 2, 3]), None, None);
        assert_eq!(
            check_token(Some(&r), "ingest:write", 42, 1_000),
            Err(CheckError::RepoNotAllowed)
        );
    }
}
