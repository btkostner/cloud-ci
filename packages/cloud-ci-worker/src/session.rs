//! Session cookie issuance and verification (docs/design/auth.md §
//! "Human auth: GitHub OAuth", "Data model"'s `sessions` row, "Security
//! considerations" row on session cookie theft).
//!
//! The cookie (`__Host-cc_session`) holds only a random 256-bit session
//! id; the `sessions` row is looked up by the SHA-256 hex digest of that
//! id, never the plaintext — so a leaked D1 export does not yield a
//! usable session (auth.md: "session id is random and only its SHA-256
//! hash is stored, so a D1 leak alone does not yield a usable session").
//!
//! Same layering as `api_tokens.rs`/`installations.rs`: [`hash_session_id`],
//! [`generate_session_id`], [`build_set_cookie_header`],
//! [`parse_cookie_header`], and [`check_session`] are pure and unit-tested
//! with plain `cargo test`; [`create_session`], [`lookup_session`], and
//! [`touch_last_seen`] need the Workers runtime (D1) and are only
//! exercised by the live smoke test (`mise run
//! //packages/cloud-ci-worker:dev`).
//!
//! **There is deliberately no function here that mints a *usable* session
//! for a caller who hasn't gone through real GitHub OAuth** — the only
//! caller of [`create_session`] is `oauth.rs`'s callback handler, after
//! GitHub has verified the user. Same reasoning as `api_tokens.rs`'s
//! module doc: session creation is a privilege-granting operation. For
//! local development, a session row is hand-inserted directly via
//! `wrangler d1 execute` — see this module's live smoke test notes in the
//! round's report, not a Rust helper.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use worker::Env;
use worker::wasm_bindgen::JsValue;

/// How long a freshly created session is valid for. auth.md does not
/// pin an exact value, only that sessions are D1 rows with an
/// `expires_at` column; 30 days is a conservative default for a human
/// browser session, consistent with GitHub's own OAuth app access-token
/// lifetimes being far longer than the 8-hour window called out for the
/// *installation*-scoped role lookup. [INFERENCE — not specified by
/// auth.md].
pub const SESSION_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

/// The cookie name — auth.md's `Set-Cookie` sketch uses `__Host-cc_session`.
pub const SESSION_COOKIE_NAME: &str = "__Host-cc_session";

/// SHA-256 of the session id (the cookie's plaintext value), hex-encoded
/// — matches the `sessions.id` storage format (migration 0006). Pure,
/// unit-testable without the Workers runtime.
pub fn hash_session_id(session_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Generates a fresh 256-bit random session id, base64url-encoded (no
/// padding) — matches auth.md's cookie sketch:
/// `base64url(32 random bytes)`. Fails only if the platform RNG is
/// unavailable.
pub fn generate_session_id() -> Result<String, getrandom::Error> {
    let mut random = [0u8; 32];
    getrandom::getrandom(&mut random)?;
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
}

/// Builds the exact `Set-Cookie` header value auth.md specifies:
/// `__Host-cc_session=<id>; Secure; HttpOnly; SameSite=Lax; Path=/`.
/// `max_age_s` sets `Max-Age` to the session's remaining TTL so the
/// browser does not keep offering an already-expired cookie past
/// [`SESSION_TTL_SECONDS`]. Pure string construction.
pub fn build_set_cookie_header(session_id: &str, max_age_s: i64) -> String {
    format!(
        "{SESSION_COOKIE_NAME}={session_id}; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age_s}"
    )
}

/// Builds a `Set-Cookie` header value that immediately expires the
/// session cookie (logout / failed-login cleanup) — same attributes as
/// [`build_set_cookie_header`] but `Max-Age=0` and an empty value, the
/// standard way to tell a browser to drop a cookie.
pub fn build_expired_cookie_header() -> String {
    format!("{SESSION_COOKIE_NAME}=; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

/// Extracts the value of cookie `name` from a raw `Cookie` request header
/// (`"a=1; b=2; c=3"`). Pure, unit-testable without the Workers runtime.
/// Returns `None` if the header doesn't contain `name`.
pub fn parse_cookie_header(cookie_header: &str, name: &str) -> Option<String> {
    cookie_header.split(';').find_map(|pair| {
        let pair = pair.trim();
        let (key, value) = pair.split_once('=')?;
        if key.trim() == name {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

/// A `sessions` row joined with its owning `users` row — the two fields
/// [`check_session`]'s caller (the OAuth-gated surfaces this round's scope
/// excludes) needs, per the task brief's "return the associated
/// user_id/github_login".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub user_id: String,
    pub github_login: String,
    pub expires_at: i64,
}

/// Why a presented session cookie failed verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// No `sessions` row has this id hash.
    NotFound,
    /// `expires_at` has passed `now_unix_s`.
    Expired,
}

/// Verified session identity — the caller-facing result of
/// [`check_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSession {
    pub user_id: String,
    pub github_login: String,
}

/// Pure "is this already-looked-up row a live session" decision, given
/// the row [`lookup_session`] fetched for the presented cookie's hash.
/// Kept separate from the D1 fetch so it is unit-testable with plain
/// `cargo test`, same layering as `api_tokens::check_token`. `now_unix_s`
/// is a parameter, not read from a clock internally, for the same
/// determinism/testability reason.
pub fn check_session(
    row: Option<&SessionRow>,
    now_unix_s: i64,
) -> Result<VerifiedSession, SessionError> {
    let row = row.ok_or(SessionError::NotFound)?;
    if row.expires_at <= now_unix_s {
        return Err(SessionError::Expired);
    }
    Ok(VerifiedSession {
        user_id: row.user_id.clone(),
        github_login: row.github_login.clone(),
    })
}

// ---------------------------------------------------------------------------
// D1 reads/writes — needs the Workers runtime, not covered by `cargo test`
// (see module docs).
// ---------------------------------------------------------------------------

/// Inserts a new `sessions` row for `user_id`, keyed by `id_hash`
/// (hex-encoded SHA-256 of the cookie's plaintext session id —
/// [`hash_session_id`]). The only caller is `oauth.rs`'s callback handler,
/// after GitHub has verified the user (see module docs on why there is no
/// other minting path).
pub async fn create_session(
    env: &Env,
    id_hash: &str,
    user_id: &str,
    created_at_s: i64,
    expires_at_s: i64,
) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare(
        "INSERT INTO sessions (id, user_id, created_at, expires_at, last_seen_at) \
         VALUES (?1, ?2, ?3, ?4, ?3)",
    )
    .bind(&[
        JsValue::from_str(id_hash),
        JsValue::from_str(user_id),
        JsValue::from_f64(created_at_s as f64),
        JsValue::from_f64(expires_at_s as f64),
    ])?
    .run()
    .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RawSessionRow {
    user_id: String,
    github_login: String,
    expires_at: i64,
}

/// Looks up the `sessions` row (joined with `users` for `github_login`)
/// whose `id` matches `id_hash`. `None` means no row has this hash —
/// [`check_session`] turns that into [`SessionError::NotFound`].
pub async fn lookup_session(env: &Env, id_hash: &str) -> worker::Result<Option<SessionRow>> {
    let db = env.d1("DB")?;
    let row = db
        .prepare(
            "SELECT sessions.user_id, users.github_login, sessions.expires_at \
             FROM sessions JOIN users ON sessions.user_id = users.id \
             WHERE sessions.id = ?1",
        )
        .bind(&[JsValue::from_str(id_hash)])?
        .first::<RawSessionRow>(None)
        .await?;
    Ok(row.map(|r| SessionRow {
        user_id: r.user_id,
        github_login: r.github_login,
        expires_at: r.expires_at,
    }))
}

/// Best-effort `last_seen_at` update — same reasoning as
/// `api_tokens::touch_last_used`: callers must not fail the request if
/// this fails.
pub async fn touch_last_seen(env: &Env, id_hash: &str, now_unix_s: i64) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare("UPDATE sessions SET last_seen_at = ?1 WHERE id = ?2")
        .bind(&[
            JsValue::from_f64(now_unix_s as f64),
            JsValue::from_str(id_hash),
        ])?
        .run()
        .await?;
    Ok(())
}

/// Deletes every session row for `user_id` — logout-all / admin-triggered
/// revoke (auth.md's `sessions` row: "deleted on logout or
/// admin-triggered revoke"). Not wired to a route this round (no
/// dashboard yet), but kept alongside the rest of this module's D1
/// surface since it needs no additional design.
pub async fn delete_sessions_for_user(env: &Env, user_id: &str) -> worker::Result<()> {
    let db = env.d1("DB")?;
    db.prepare("DELETE FROM sessions WHERE user_id = ?1")
        .bind(&[JsValue::from_str(user_id)])?
        .run()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_session_id_is_64_char_lowercase_hex() {
        let hash = hash_session_id("some-session-id");
        assert_eq!(hash.len(), 64);
        assert!(
            hash.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn hash_session_id_is_deterministic_and_distinguishes_input() {
        assert_eq!(hash_session_id("a"), hash_session_id("a"));
        assert_ne!(hash_session_id("a"), hash_session_id("b"));
    }

    #[test]
    fn generate_session_id_produces_distinct_ids() -> Result<(), getrandom::Error> {
        let a = generate_session_id()?;
        let b = generate_session_id()?;
        assert_ne!(a, b);
        // 32 random bytes, base64url no-pad -> 43 characters.
        assert_eq!(a.len(), 43);
        Ok(())
    }

    #[test]
    fn build_set_cookie_header_matches_auth_md_sketch() {
        let header = build_set_cookie_header("abc123", 3600);
        assert_eq!(
            header,
            "__Host-cc_session=abc123; Secure; HttpOnly; SameSite=Lax; Path=/; Max-Age=3600"
        );
    }

    #[test]
    fn build_expired_cookie_header_has_zero_max_age() {
        let header = build_expired_cookie_header();
        assert!(header.contains("Max-Age=0"));
        assert!(header.starts_with("__Host-cc_session=;"));
    }

    #[test]
    fn parse_cookie_header_finds_named_cookie_among_several() {
        let header = "a=1; __Host-cc_session=the-session-id; b=2";
        assert_eq!(
            parse_cookie_header(header, "__Host-cc_session"),
            Some("the-session-id".to_string())
        );
    }

    #[test]
    fn parse_cookie_header_returns_none_when_absent() {
        let header = "a=1; b=2";
        assert_eq!(parse_cookie_header(header, "__Host-cc_session"), None);
    }

    #[test]
    fn parse_cookie_header_handles_single_cookie_no_semicolons() {
        let header = "__Host-cc_session=only";
        assert_eq!(
            parse_cookie_header(header, "__Host-cc_session"),
            Some("only".to_string())
        );
    }

    #[test]
    fn check_session_rejects_missing_row() {
        assert_eq!(check_session(None, 1_000), Err(SessionError::NotFound));
    }

    #[test]
    fn check_session_rejects_expired_row() {
        let row = SessionRow {
            user_id: "u1".into(),
            github_login: "octocat".into(),
            expires_at: 1_000,
        };
        assert_eq!(check_session(Some(&row), 1_000), Err(SessionError::Expired));
        assert_eq!(check_session(Some(&row), 1_001), Err(SessionError::Expired));
    }

    #[test]
    fn check_session_accepts_live_row() -> Result<(), SessionError> {
        let row = SessionRow {
            user_id: "u1".into(),
            github_login: "octocat".into(),
            expires_at: 2_000,
        };
        let verified = check_session(Some(&row), 1_000)?;
        assert_eq!(verified.user_id, "u1");
        assert_eq!(verified.github_login, "octocat");
        Ok(())
    }
}
