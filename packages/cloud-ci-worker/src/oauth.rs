//! Human login via the GitHub App's own user-to-server OAuth
//! (docs/design/auth.md § "Human auth: GitHub OAuth"): browser redirected
//! to `github.com/login/oauth/authorize`, GitHub redirects back with a
//! `code`, the Worker exchanges it at `/login/oauth/access_token` using
//! `GITHUB_APP_CLIENT_ID`/`GITHUB_APP_CLIENT_SECRET` for a user access
//! token, calls `GET /user` once to get a verified `login`/`id`/`email`,
//! upserts a `users` row, creates a session ([`crate::session`]), and
//! sets the session cookie.
//!
//! Same three-layer split as `github_app.rs`/`oidc.rs`:
//!
//! 1. [`authorize_url`], [`generate_state`]/[`encode_state`],
//!    [`token_exchange_body`], [`parse_token_response`] — pure URL/body
//!    construction and response parsing, no `worker` dependency beyond
//!    `getrandom`. Unit-testable with plain `cargo test`.
//! 2. [`exchange_code`], [`fetch_github_user`] — the actual HTTP calls to
//!    `github.com`/`api.github.com`. **Not live-verified**: this
//!    environment has no real `GITHUB_APP_CLIENT_ID`/
//!    `GITHUB_APP_CLIENT_SECRET` and no way to drive a real
//!    browser-authorized GitHub `code` (same category of limitation as
//!    `github_app.rs`'s `/app/*` endpoints and `oidc.rs`'s JWKS fetch —
//!    see those modules' docs for the precedent). Their request
//!    construction and response-shape parsing are unit-tested; the network
//!    round trip itself is not.
//! 3. [`upsert_user`] — needs the Workers runtime (D1), exercised only by
//!    the live smoke test.
//!
//! `lib.rs` wires `GET /login` (redirects to GitHub, step 1 above,
//! setting a short-lived `oauth_state` cookie for CSRF) and
//! `GET /oauth/callback` (steps 2-3, then [`session::create_session`] and
//! the `__Host-cc_session` `Set-Cookie`) into `fetch`.
//!
//! # CSRF state
//!
//! The `state` query parameter GitHub round-trips is compared against a
//! value stashed in a short-lived, `HttpOnly` cookie set by `GET /login`
//! (the standard "double-submit cookie" pattern for the OAuth
//! authorization-code flow) — an attacker who can trick a victim's browser
//! into visiting `/oauth/callback?code=<attacker's code>&state=<guess>`
//! cannot also set the victim's `oauth_state` cookie to match, since that
//! cookie is only ever set by this Worker's own `/login` response. This is
//! an ordinary equality compare, not a constant-time one: unlike an HMAC
//! or session-hash comparison (`webhook.rs`, `session.rs`), a CSRF state
//! token is not a secret being checked against attacker-chosen guesses of
//! *itself* — leaking whether a guess is "close" buys an attacker nothing,
//! since the attacker cannot read the victim's cookie to begin with.

use serde::{Deserialize, Serialize};
use worker::Env;
use worker::wasm_bindgen::JsValue;

#[derive(Debug, PartialEq, Eq)]
pub struct OauthError(pub String);

impl std::fmt::Display for OauthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for OauthError {}

/// The short-lived cookie `GET /login` sets to carry the CSRF `state`
/// value to `GET /oauth/callback` (module docs "CSRF state").
pub const OAUTH_STATE_COOKIE_NAME: &str = "cc_oauth_state";

/// How long the `oauth_state` cookie lives — long enough for a human to
/// complete the GitHub authorization screen, short enough that a stale
/// unused value isn't sitting in the browser indefinitely.
pub const OAUTH_STATE_TTL_SECONDS: i64 = 600;

/// Generates a fresh, random CSRF `state` value: 32 random bytes,
/// base64url-encoded (no padding). Fails only if the platform RNG is
/// unavailable — mirrors `ulid::generate`/`session::generate_session_id`.
pub fn generate_state() -> Result<String, getrandom::Error> {
    let mut random = [0u8; 32];
    getrandom::getrandom(&mut random)?;
    Ok(encode_state(&random))
}

/// Pure base64url encoding of raw state bytes — split out from
/// [`generate_state`] so the encoding itself is unit-testable without the
/// platform RNG, same pattern as `ulid::encode`/`ulid::generate`.
pub fn encode_state(random: &[u8; 32]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}

/// Percent-encodes a query-string value per RFC 3986 "unreserved"
/// characters (`A-Za-z0-9-_.~` pass through verbatim, everything else is
/// `%XX`-escaped) — a hand-rolled encoder (same posture `webhook.rs`
/// takes for hex) rather than pulling in the `url` crate: it is not
/// already a direct dependency of this crate (`worker` depends on it
/// internally but doesn't expose a convenient way to build an arbitrary
/// query string outside its own `Request`/`Response` types), for what is
/// otherwise a ~10-line function.
fn percent_encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Encodes a value for an `application/x-www-form-urlencoded` body: same
/// unreserved-character set as [`percent_encode_query_value`], but a
/// literal space encodes as `+` rather than `%20`, per the
/// `application/x-www-form-urlencoded` serialization algorithm (WHATWG
/// URL Standard § application/x-www-form-urlencoded serializing).
fn form_url_encode_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Builds the `github.com/login/oauth/authorize` redirect URL (auth.md §
/// "Human auth: GitHub OAuth"). Pure string construction, unit-testable
/// without the Workers runtime. `client_id` is `GITHUB_APP_CLIENT_ID`
/// (a Worker `var`, not a secret — auth.md's "Secret storage" table);
/// `redirect_uri` is this deployment's own `/oauth/callback` URL;
/// `state` is [`generate_state`]'s output.
pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&state={}",
        percent_encode_query_value(client_id),
        percent_encode_query_value(redirect_uri),
        percent_encode_query_value(state),
    )
}

/// Builds the `application/x-www-form-urlencoded` request body for
/// `POST https://github.com/login/oauth/access_token`
/// (docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps,
/// accessed 2026-10-02: GitHub App user-to-server OAuth uses the same
/// code-exchange endpoint/parameters as a classic OAuth App). Pure string
/// construction, unit-testable without the Workers runtime.
pub fn token_exchange_body(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> String {
    format!(
        "client_id={}&client_secret={}&code={}&redirect_uri={}",
        form_url_encode_value(client_id),
        form_url_encode_value(client_secret),
        form_url_encode_value(code),
        form_url_encode_value(redirect_uri),
    )
}

/// A successful `/login/oauth/access_token` response (requested as JSON
/// via `Accept: application/json` — GitHub's default is
/// form-urlencoded, but it supports JSON on request, same doc as above).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub scope: String,
    pub token_type: String,
}

/// GitHub's error shape for a failed code exchange (e.g. `bad_verification_code`),
/// returned with HTTP 200 and an `error`/`error_description` JSON body
/// rather than a non-2xx status (documented GitHub OAuth behavior).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct TokenErrorResponse {
    error: String,
    error_description: Option<String>,
}

/// Parses `/login/oauth/access_token`'s JSON response body, handling both
/// the success shape ([`TokenResponse`]) and GitHub's `{error, ...}` shape
/// for a rejected code. Pure, unit-testable without the Workers runtime.
pub fn parse_token_response(body: &str) -> Result<TokenResponse, OauthError> {
    if let Ok(error) = serde_json::from_str::<TokenErrorResponse>(body)
        && !error.error.is_empty()
    {
        return Err(OauthError(format!(
            "GitHub rejected the OAuth code: {} ({})",
            error.error,
            error.error_description.unwrap_or_default()
        )));
    }
    serde_json::from_str::<TokenResponse>(body)
        .map_err(|e| OauthError(format!("cannot decode token exchange response: {e}")))
}

/// `GET /user`'s response shape, trimmed to the fields auth.md names:
/// "calls `GET /user` once to get a verified `login`/`id`/`email`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubUser {
    pub id: u64,
    pub login: String,
    pub email: Option<String>,
}

/// `User-Agent` GitHub requires on every REST API request — same
/// reasoning/value as `github_app.rs::USER_AGENT`; duplicated locally
/// rather than made `pub` cross-module, matching this codebase's existing
/// per-module header-builder convention (each of `github_app.rs`,
/// `oidc.rs` owns its own).
const USER_AGENT: &str = "cloud-ci-worker";

const GITHUB_API_VERSION: &str = "2022-11-28";

/// Exchanges an authorization `code` for a user access token:
/// `POST https://github.com/login/oauth/access_token`. **Not
/// live-verified** — see module docs.
pub async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, OauthError> {
    let body = token_exchange_body(client_id, client_secret, code, redirect_uri);

    let headers = worker::Headers::new();
    headers
        .set("content-type", "application/x-www-form-urlencoded")
        .map_err(|e| OauthError(format!("cannot set content-type header: {e}")))?;
    headers
        .set("accept", "application/json")
        .map_err(|e| OauthError(format!("cannot set accept header: {e}")))?;
    headers
        .set("user-agent", USER_AGENT)
        .map_err(|e| OauthError(format!("cannot set user-agent header: {e}")))?;

    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Post);
    init.with_headers(headers);
    init.with_body(Some(JsValue::from_str(&body)));

    let request =
        worker::Request::new_with_init("https://github.com/login/oauth/access_token", &init)
            .map_err(|e| OauthError(format!("cannot build token exchange request: {e}")))?;

    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| OauthError(format!("token exchange request failed: {e}")))?;

    let status = response.status_code();
    let text = response
        .text()
        .await
        .map_err(|e| OauthError(format!("cannot read token exchange response body: {e}")))?;
    if status != 200 {
        return Err(OauthError(format!(
            "token exchange failed: {status} {text}"
        )));
    }
    parse_token_response(&text)
}

/// Calls `GET /user` with the user access token from [`exchange_code`] to
/// get a verified `login`/`id`/`email` (auth.md § "Human auth: GitHub
/// OAuth"). **Not live-verified** — see module docs.
pub async fn fetch_github_user(access_token: &str) -> Result<GithubUser, OauthError> {
    let headers = worker::Headers::new();
    headers
        .set("authorization", &format!("Bearer {access_token}"))
        .map_err(|e| OauthError(format!("cannot set authorization header: {e}")))?;
    headers
        .set("accept", "application/vnd.github+json")
        .map_err(|e| OauthError(format!("cannot set accept header: {e}")))?;
    headers
        .set("x-github-api-version", GITHUB_API_VERSION)
        .map_err(|e| OauthError(format!("cannot set api-version header: {e}")))?;
    headers
        .set("user-agent", USER_AGENT)
        .map_err(|e| OauthError(format!("cannot set user-agent header: {e}")))?;

    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Get);
    init.with_headers(headers);

    let request = worker::Request::new_with_init("https://api.github.com/user", &init)
        .map_err(|e| OauthError(format!("cannot build GET /user request: {e}")))?;

    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| OauthError(format!("GET /user request failed: {e}")))?;

    if response.status_code() != 200 {
        let body = response.text().await.unwrap_or_default();
        return Err(OauthError(format!(
            "GET /user failed: {} {body}",
            response.status_code()
        )));
    }

    response
        .json::<GithubUser>()
        .await
        .map_err(|e| OauthError(format!("cannot decode GET /user response: {e}")))
}

// ---------------------------------------------------------------------------
// D1 reads/writes — needs the Workers runtime, not covered by `cargo test`
// (see module docs).
// ---------------------------------------------------------------------------

/// Upserts the `users` row for a verified GitHub identity (auth.md:
/// "upserts a `users` row"). Returns the row's `id` (ULID), minted fresh
/// only on first login — an existing row keeps its original `id` via
/// `ON CONFLICT ... DO UPDATE`, only refreshing `github_login`/`email`/
/// `last_login_at` (a login can change their GitHub username; the row
/// stays the same identity, keyed on the immutable `github_user_id`).
pub async fn upsert_user(
    env: &Env,
    github_user_id: u64,
    github_login: &str,
    email: Option<&str>,
    now_s: i64,
) -> worker::Result<String> {
    let db = env.d1("DB")?;
    let new_id = crate::ulid::generate(now_s as u64 * 1000)
        .map_err(|e| worker::Error::RustError(format!("cannot generate user id: {e}")))?;
    db.prepare(
        "INSERT INTO users (id, github_user_id, github_login, email, created_at, last_login_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
         ON CONFLICT (github_user_id) DO UPDATE SET \
             github_login = excluded.github_login, \
             email = excluded.email, \
             last_login_at = excluded.last_login_at",
    )
    .bind(&[
        JsValue::from_str(&new_id),
        JsValue::from_f64(github_user_id as f64),
        JsValue::from_str(github_login),
        email.map_or(JsValue::NULL, JsValue::from_str),
        JsValue::from_f64(now_s as f64),
    ])?
    .run()
    .await?;

    // The row's real `id` is `new_id` only on first insert; on conflict it
    // keeps the original. Read it back rather than trust `new_id` blindly.
    let row = db
        .prepare("SELECT id FROM users WHERE github_user_id = ?1")
        .bind(&[JsValue::from_f64(github_user_id as f64)])?
        .first::<serde_json::Value>(None)
        .await?
        .ok_or_else(|| worker::Error::RustError("users row missing after upsert".into()))?;
    row.get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| worker::Error::RustError("users row missing id after upsert".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_state_is_deterministic_and_distinguishes_input() {
        assert_eq!(encode_state(&[0u8; 32]), encode_state(&[0u8; 32]));
        assert_ne!(encode_state(&[0u8; 32]), encode_state(&[1u8; 32]));
    }

    #[test]
    fn generate_state_produces_distinct_values() -> Result<(), getrandom::Error> {
        let a = generate_state()?;
        let b = generate_state()?;
        assert_ne!(a, b);
        assert_eq!(a.len(), 43); // 32 bytes, base64url no-pad
        Ok(())
    }

    #[test]
    fn authorize_url_contains_client_id_redirect_uri_and_state() {
        let url = authorize_url(
            "Iv1.abc123",
            "https://ci.acme.example/oauth/callback",
            "the-state-value",
        );
        assert!(url.starts_with("https://github.com/login/oauth/authorize?"));
        assert!(url.contains("client_id=Iv1.abc123"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fci.acme.example%2Foauth%2Fcallback"));
        assert!(url.contains("state=the-state-value"));
    }

    #[test]
    fn token_exchange_body_url_encodes_all_four_fields() {
        let body = token_exchange_body(
            "client-id",
            "client secret with spaces",
            "the-code",
            "https://ci.acme.example/oauth/callback",
        );
        assert!(body.contains("client_id=client-id"));
        assert!(body.contains("client_secret=client+secret+with+spaces"));
        assert!(body.contains("code=the-code"));
        assert!(body.contains("redirect_uri=https%3A%2F%2Fci.acme.example%2Foauth%2Fcallback"));
    }

    #[test]
    fn parse_token_response_decodes_success_shape() -> Result<(), OauthError> {
        let body = r#"{"access_token":"gho_abc123","scope":"","token_type":"bearer"}"#;
        let resp = parse_token_response(body)?;
        assert_eq!(resp.access_token, "gho_abc123");
        assert_eq!(resp.token_type, "bearer");
        Ok(())
    }

    #[test]
    fn parse_token_response_surfaces_github_error_shape() -> Result<(), String> {
        let body = r#"{"error":"bad_verification_code","error_description":"The code passed is incorrect or expired."}"#;
        match parse_token_response(body) {
            Ok(_) => Err("expected an error for GitHub's error response shape".to_string()),
            Err(e) => {
                assert!(e.0.contains("bad_verification_code"));
                Ok(())
            }
        }
    }

    #[test]
    fn parse_token_response_rejects_malformed_json() {
        assert!(parse_token_response("not json").is_err());
    }

    #[test]
    fn github_user_deserializes_without_email() -> Result<(), serde_json::Error> {
        let json = r#"{"id":583231,"login":"octocat","email":null}"#;
        let user: GithubUser = serde_json::from_str(json)?;
        assert_eq!(user.id, 583231);
        assert_eq!(user.login, "octocat");
        assert_eq!(user.email, None);
        Ok(())
    }
}
