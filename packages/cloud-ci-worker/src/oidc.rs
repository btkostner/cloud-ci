//! Verifies GitHub Actions OIDC JWTs presented to `BeginRun`
//! (docs/design/byo-ci.md § Auth, "GitHub Actions OIDC JWT" row): `iss` =
//! `https://token.actions.githubusercontent.com`; RS256 signature against
//! the issuer's JWKS; `aud` matched against a caller-supplied expected
//! audience; `exp`/`nbf` checked; `repository_id`/`repository_owner_id`
//! (and other) claims extracted for the caller to match against an
//! allowlist.
//!
//! Same three-layer split as `github_app.rs`:
//!
//! 1. [`decode_header`] / [`decode_claims`] / [`validate_claims`] — pure
//!    JWT parsing and claim-shape validation, no `worker`/WebCrypto
//!    dependency. Unit-testable with plain `cargo test`.
//! 2. [`verify_signature`] — the actual RS256 verification, via the
//!    Workers runtime's WebCrypto `SubtleCrypto.verify()`. Needs the
//!    `wasm32` Workers runtime to execute, same as `github_app.rs`'s
//!    `sign_rs256`; only smoke-tested under `wrangler dev`.
//! 3. [`fetch_jwks`] — fetches GitHub's JWKS over HTTP. Its URL
//!    construction is pure/unit-testable; the actual fetch is not
//!    live-verified (nothing in this environment can reach GitHub's real
//!    OIDC issuer from a `cargo test` run, and this round has no
//!    `wrangler dev` route wired to it either — see "Scope" below).
//!
//! [`verify`] composes all three into the full check. `lib.rs`'s
//! `handle_begin_run` calls it when a bearer credential is present and
//! [`looks_like_jwt`]-shaped, then matches the returned claims'
//! `repository_id`/`repository_owner_id` against the `installations`/
//! `repos` D1 tables (`installations::check_allowlist`) — this module's
//! job still ends at "this is a genuine, GitHub-issued OIDC token with
//! these claims"; the allowlist-membership check itself lives in
//! `installations.rs`, same capability-module-not-auth-decision posture
//! as `github_app.rs`.
//!
//! # Why `repository_id`/`repository_owner_id`, not `sub`
//!
//! GitHub's OIDC issuer's `claims_supported` (verified 2026-09-30 against
//! `https://token.actions.githubusercontent.com/.well-known/openid-configuration`,
//! docs/design/byo-ci.md § Auth) includes `repository_id`,
//! `repository_owner_id`, `run_id`, `run_attempt`, `sha`, `ref`, and
//! `event_name` as dedicated claims. We validate identity on
//! `repository_id`/`repository_owner_id` rather than parsing `sub`: GitHub
//! is rolling out an "immutable" `sub` format
//! (`repo:OWNER@OWNER-ID/REPO@REPO-ID:ref:...`) for repositories created
//! after 2026-07-15, while older repositories keep the pre-existing
//! format (verified 2026-09-30,
//! [GitHub Docs: OIDC reference](https://docs.github.com/en/actions/reference/security/oidc#immutable-subject-claims)).
//! Matching on the dedicated ID claims sidesteps branching on which `sub`
//! shape a given repo uses.
//!
//! # JWKS discovery and caching
//!
//! `fetch_jwks` discovers the JWKS URI from the issuer's
//! `/.well-known/openid-configuration` document (`jwks_uri` field) rather
//! than hardcoding `/.well-known/jwks`, per OIDC Discovery — the same
//! document docs/design/byo-ci.md cites for `claims_supported`, so one
//! fetch pattern covers both facts this module depends on.
//!
//! There is still no Durable Object or KV binding for a JWKS cache, but
//! none is needed: the standard Cloudflare Workers
//! [`worker::Cache`] API (`caches.default` in JS terms — see
//! `worker-0.8.7/src/cache.rs`, exported unconditionally from the crate
//! root with no feature flag or `wrangler.toml` binding, unlike D1/R2/DO)
//! is always available in a Worker and is exactly the "keyed, TTL'd HTTP
//! response cache" shape this module needs. [`fetch_jwks`] checks the
//! default cache (keyed by the real resource URL — the discovery
//! document's own URL, and separately the discovered `jwks_uri`) before
//! making either network request, and stores each response back into the
//! cache with a `Cache-Control: max-age=<JWKS_CACHE_TTL_SECONDS>` header
//! (the Cache API's documented way to express TTL — `worker::Cache::put`'s
//! docs: "The Response should include a cache-control header with max-age
//! or s-maxage directives, otherwise the Cache API will not cache the
//! response"). See [`JWKS_CACHE_TTL_SECONDS`] for the TTL value and
//! reasoning. Both the discovery document and the JWKS are cached, not
//! just the JWKS: caching only the JWKS would still leave a network round
//! trip (the discovery fetch) on every call, defeating most of the latency
//! win, for a document that changes even less often than the JWKS itself.
//!
//! The "cached, refetched on `kid` miss" behavior docs/design/byo-ci.md's
//! Auth row describes maps directly onto [`fetch_jwks`] (cache-first) and
//! [`fetch_jwks_bypassing_cache`] (network-first, cache-updating) —
//! [`verify`]'s `kid`-miss retry calls the latter, so a key rotated after
//! the cache was populated is still found on retry instead of being masked
//! by a stale cache entry for up to the full TTL.
//!
//! Cache reads/writes are best-effort: any Cache API failure, or a cached
//! body that doesn't parse, is treated as a cache miss (fall through to a
//! real fetch) or silently dropped (fall through to no caching), never
//! surfaced as a request failure — the cache is a latency optimization
//! over the network fetch, not a dependency this module's correctness
//! relies on.

use serde::{Deserialize, Serialize};

/// The only issuer this module accepts — docs/design/byo-ci.md § Auth.
pub const GITHUB_OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";

#[derive(Debug, PartialEq, Eq)]
pub struct OidcError(String);

impl std::fmt::Display for OidcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for OidcError {}

#[derive(Debug, Clone, Copy, Deserialize)]
struct JwtHeader<'a> {
    alg: &'a str,
    kid: &'a str,
}

/// One JSON Web Key from a JWKS document — only the fields an RSA
/// `importKey("jwk", ...)` call needs (`n`/`e`), plus `kid` to find it and
/// `kty`/`alg` to sanity-check it before handing it to WebCrypto.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwk {
    pub kid: String,
    pub kty: String,
    #[serde(default)]
    pub alg: Option<String>,
    pub n: String,
    pub e: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

/// The claims this module validates/extracts, `exp`/`nbf` already checked
/// against the caller-supplied `now_unix_s`. Only the fields
/// docs/design/byo-ci.md § Auth lists as available — not an exhaustive
/// mirror of every claim GitHub's issuer emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedOidcClaims {
    pub repository_id: u64,
    pub repository_owner_id: u64,
    pub run_id: String,
    pub run_attempt: String,
    pub sha: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub event_name: String,
}

/// Raw claims as they appear in the JWT payload, before `exp`/`nbf`/`aud`
/// validation — `repository_id`/`repository_owner_id` arrive as JSON
/// strings in real GitHub-issued tokens (documented quirk: GitHub's OIDC
/// claims are stringly-typed even for numeric IDs), so this struct accepts
/// either a string or a number for them and normalizes in
/// [`validate_claims`].
#[derive(Debug, Clone, Deserialize)]
struct RawClaims {
    iss: String,
    aud: String,
    exp: i64,
    nbf: i64,
    #[serde(deserialize_with = "string_or_u64")]
    repository_id: u64,
    #[serde(deserialize_with = "string_or_u64")]
    repository_owner_id: u64,
    run_id: String,
    run_attempt: String,
    sha: String,
    #[serde(rename = "ref")]
    git_ref: String,
    event_name: String,
}

fn string_or_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrU64 {
        String(String),
        U64(u64),
    }
    match StringOrU64::deserialize(deserializer)? {
        StringOrU64::String(s) => s.parse().map_err(serde::de::Error::custom),
        StringOrU64::U64(n) => Ok(n),
    }
}

/// Splits a `header.payload.signature` JWT into its three segments.
fn split_jwt(jwt: &str) -> Result<(&str, &str, &str), OidcError> {
    let mut parts = jwt.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(OidcError(
            "malformed OIDC JWT: expected exactly three dot-separated segments".to_string(),
        ));
    };
    Ok((header, payload, signature))
}

/// Structural check only — "three dot-separated segments" — not a full
/// parse: distinguishes an OIDC JWT bearer value from a future
/// `cc_tok_...`-style opaque scoped API token (docs/design/byo-ci.md §
/// Auth's other credential row, not implemented yet). Used by
/// `lib.rs::handle_begin_run` to decide which credential path a bearer
/// value is before attempting [`verify`] — deliberately cheap and
/// forgiving (an opaque token can never accidentally contain two dots
/// in practice for either token family actually in use), since a true
/// parse failure is already handled by [`verify`]'s own error path.
pub fn looks_like_jwt(bearer: &str) -> bool {
    bearer.split('.').count() == 3
}

fn base64_url_decode(segment: &str) -> Result<Vec<u8>, OidcError> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|e| OidcError(format!("cannot base64url-decode JWT segment: {e}")))
}

/// Decodes the JWT's header segment and returns its `kid` — the lookup key
/// into the fetched JWKS. Pure, unit-testable without the Workers runtime.
pub fn decode_kid(jwt: &str) -> Result<String, OidcError> {
    let (header_b64, _, _) = split_jwt(jwt)?;
    let header_bytes = base64_url_decode(header_b64)?;
    let header: JwtHeader = serde_json::from_slice(&header_bytes)
        .map_err(|e| OidcError(format!("cannot decode JWT header: {e}")))?;
    if header.alg != "RS256" {
        return Err(OidcError(format!(
            "unsupported JWT alg: expected RS256, got {}",
            header.alg
        )));
    }
    Ok(header.kid.to_string())
}

/// Decodes the JWT's payload segment into [`RawClaims`], without yet
/// checking `iss`/`aud`/`exp`/`nbf`. Pure, unit-testable without the
/// Workers runtime.
fn decode_raw_claims(jwt: &str) -> Result<RawClaims, OidcError> {
    let (_, payload_b64, _) = split_jwt(jwt)?;
    let payload_bytes = base64_url_decode(payload_b64)?;
    serde_json::from_slice(&payload_bytes)
        .map_err(|e| OidcError(format!("cannot decode JWT claims: {e}")))
}

/// Validates `iss`/`aud`/`exp`/`nbf` against `expected_audience` and
/// `now_unix_s`, and extracts the typed claims on success. Does **not**
/// check the signature — call only after [`verify_signature`] succeeds, or
/// via [`verify`] which sequences both correctly. `now_unix_s` is a
/// parameter (not read from a clock internally), same
/// determinism/testability pattern as `ingest_token`/`github_app`.
fn validate_claims(
    jwt: &str,
    expected_audience: &str,
    now_unix_s: i64,
) -> Result<VerifiedOidcClaims, OidcError> {
    let claims = decode_raw_claims(jwt)?;

    if claims.iss != GITHUB_OIDC_ISSUER {
        return Err(OidcError(format!(
            "unexpected OIDC issuer: expected {GITHUB_OIDC_ISSUER}, got {}",
            claims.iss
        )));
    }
    if claims.aud != expected_audience {
        return Err(OidcError(format!(
            "unexpected OIDC audience: expected {expected_audience}, got {}",
            claims.aud
        )));
    }
    if now_unix_s >= claims.exp {
        return Err(OidcError("OIDC JWT expired".to_string()));
    }
    if now_unix_s < claims.nbf {
        return Err(OidcError("OIDC JWT not yet valid (nbf)".to_string()));
    }

    Ok(VerifiedOidcClaims {
        repository_id: claims.repository_id,
        repository_owner_id: claims.repository_owner_id,
        run_id: claims.run_id,
        run_attempt: claims.run_attempt,
        sha: claims.sha,
        git_ref: claims.git_ref,
        event_name: claims.event_name,
    })
}

/// The issuer's OIDC Discovery document's one field this module needs:
/// `jwks_uri`. Other fields (`issuer`, `claims_supported`, etc.) are
/// ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OpenIdConfiguration {
    jwks_uri: String,
}

fn openid_configuration_url(issuer: &str) -> String {
    format!("{issuer}/.well-known/openid-configuration")
}

/// `max-age` seconds applied to cached OIDC Discovery documents and JWKS
/// responses (module docs "JWKS discovery and caching"): 15 minutes.
/// GitHub does not publish a signing-key rotation cadence for
/// `token.actions.githubusercontent.com`, so this isn't tuned against a
/// confirmed number — it's a conservative default for a key set that in
/// practice rotates on the order of weeks/months, not minutes: long enough
/// that `verify` almost never pays the network-fetch cost, short enough
/// that a rotated key is picked up well within the same working session
/// even without the `kid`-miss retry. The retry
/// ([`fetch_jwks_bypassing_cache`], wired into [`verify`]) is what actually
/// guarantees correctness across a rotation mid-TTL — this constant only
/// controls the common-case cache hit rate, not whether a rotated key is
/// ever found.
const JWKS_CACHE_TTL_SECONDS: u32 = 900;

/// Reads a cached JSON document for `url` from the Workers default cache,
/// if present. A cache miss, a Cache API failure, or a cached body that
/// fails to parse as `T` are all treated identically — `None` — per module
/// docs: the cache is a latency optimization over a real fetch, never a
/// dependency whose failure should surface to the caller.
async fn cached_json<T: serde::de::DeserializeOwned>(
    cache: &worker::Cache,
    url: &str,
) -> Option<T> {
    let mut response = cache.get(url, false).await.ok().flatten()?;
    response.json::<T>().await.ok()
}

/// Stores `value` as JSON in the Workers default cache under `url`, with a
/// `Cache-Control: max-age=JWKS_CACHE_TTL_SECONDS` header — the Cache API
/// only caches responses that carry a `max-age`/`s-maxage` directive (see
/// `worker::Cache::put` docs). Best-effort: any failure to build or store
/// the cached response is silently dropped, same reasoning as
/// [`cached_json`].
async fn cache_put_json<T: Serialize>(cache: &worker::Cache, url: &str, value: &T) {
    let Ok(response) = worker::Response::from_json(value) else {
        return;
    };
    if response
        .headers()
        .set(
            "Cache-Control",
            &format!("max-age={JWKS_CACHE_TTL_SECONDS}"),
        )
        .is_err()
    {
        return;
    }
    let _ = cache.put(url, response).await;
}

/// Fetches GitHub's JWKS, discovering the exact `jwks_uri` from the
/// issuer's `/.well-known/openid-configuration` document first (OIDC
/// Discovery) rather than hardcoding `/.well-known/jwks`. Checks the
/// Workers default cache first for both the discovery document and the
/// JWKS, only reaching the network on a cache miss — see module docs
/// "JWKS discovery and caching". Use [`fetch_jwks_bypassing_cache`] when a
/// stale cache entry must not be trusted.
pub async fn fetch_jwks(issuer: &str) -> Result<Jwks, OidcError> {
    fetch_jwks_impl(issuer, true).await
}

/// Same as [`fetch_jwks`], but skips the cache read for both the discovery
/// document and the JWKS, always fetching fresh from the network — the
/// network-first half of the "cached, refetched on `kid` miss" behavior
/// (module docs). The fresh result is still written back to the cache
/// afterward, healing a stale entry for the next cache-first call.
pub async fn fetch_jwks_bypassing_cache(issuer: &str) -> Result<Jwks, OidcError> {
    fetch_jwks_impl(issuer, false).await
}

async fn fetch_jwks_impl(issuer: &str, use_cache: bool) -> Result<Jwks, OidcError> {
    let cache = worker::Cache::default();
    let config_url = openid_configuration_url(issuer);

    let cached_config = if use_cache {
        cached_json::<OpenIdConfiguration>(&cache, &config_url).await
    } else {
        None
    };
    let config = match cached_config {
        Some(config) => config,
        None => {
            let config_request = worker::Request::new(&config_url, worker::Method::Get)
                .map_err(|e| OidcError(format!("cannot build OIDC discovery request: {e}")))?;
            let mut config_response = worker::Fetch::Request(config_request)
                .send()
                .await
                .map_err(|e| OidcError(format!("OIDC discovery request failed: {e}")))?;
            if config_response.status_code() != 200 {
                return Err(OidcError(format!(
                    "OIDC discovery request failed: status {}",
                    config_response.status_code()
                )));
            }
            let config: OpenIdConfiguration = config_response
                .json()
                .await
                .map_err(|e| OidcError(format!("cannot decode OIDC discovery document: {e}")))?;
            cache_put_json(&cache, &config_url, &config).await;
            config
        }
    };

    if use_cache && let Some(jwks) = cached_json::<Jwks>(&cache, &config.jwks_uri).await {
        return Ok(jwks);
    }

    let jwks_request = worker::Request::new(&config.jwks_uri, worker::Method::Get)
        .map_err(|e| OidcError(format!("cannot build JWKS request: {e}")))?;
    let mut jwks_response = worker::Fetch::Request(jwks_request)
        .send()
        .await
        .map_err(|e| OidcError(format!("JWKS request failed: {e}")))?;
    if jwks_response.status_code() != 200 {
        return Err(OidcError(format!(
            "JWKS request failed: status {}",
            jwks_response.status_code()
        )));
    }
    let jwks: Jwks = jwks_response
        .json()
        .await
        .map_err(|e| OidcError(format!("cannot decode JWKS: {e}")))?;
    cache_put_json(&cache, &config.jwks_uri, &jwks).await;
    Ok(jwks)
}

/// Verifies `jwt`'s RS256 signature against `jwk` (an RSA public key, `n`
/// base64url-encoded modulus / `e` base64url-encoded exponent, as found in
/// a JWKS) via the Workers runtime's WebCrypto `SubtleCrypto.verify()` —
/// the public-key counterpart of `github_app.rs::sign_rs256`'s private-key
/// `SubtleCrypto.sign()`. Reaches `SubtleCrypto` the same way
/// `github_app.rs` does (see that module's doc comment for the
/// investigation); imports the key via `importKey("jwk", ...)` rather than
/// `"pkcs8"`, since a JWKS hands back a JWK (modulus/exponent), not a DER
/// key.
pub async fn verify_signature(jwt: &str, jwk: &Jwk) -> Result<(), OidcError> {
    use wasm_bindgen::JsCast;
    use worker::wasm_bindgen_futures::JsFuture;
    use worker::web_sys;

    if jwk.kty != "RSA" {
        return Err(OidcError(format!(
            "unsupported JWK key type: expected RSA, got {}",
            jwk.kty
        )));
    }

    let (header_b64, payload_b64, signature_b64) = split_jwt(jwt)?;
    let signing_input = format!("{header_b64}.{payload_b64}");
    let signature = base64_url_decode(signature_b64)?;

    let global: web_sys::WorkerGlobalScope = worker::js_sys::global().unchecked_into();
    let crypto = global
        .crypto()
        .map_err(|e| OidcError(format!("WebCrypto unavailable: {e:?}")))?;
    let subtle = crypto.subtle();

    let jwk_obj = worker::js_sys::Object::new();
    set_prop(&jwk_obj, "kty", &jwk.kty)?;
    set_prop(&jwk_obj, "n", &jwk.n)?;
    set_prop(&jwk_obj, "e", &jwk.e)?;
    set_prop(&jwk_obj, "alg", "RS256")?;
    set_prop(&jwk_obj, "ext", "true")?;
    let usages_for_key = worker::js_sys::Array::new();
    usages_for_key.push(&wasm_bindgen::JsValue::from_str("verify"));
    worker::js_sys::Reflect::set(
        &jwk_obj,
        &wasm_bindgen::JsValue::from_str("key_ops"),
        &usages_for_key,
    )
    .map_err(|e| OidcError(format!("cannot build JWK object: {e:?}")))?;

    let algorithm = worker::js_sys::Object::new();
    set_prop(&algorithm, "name", "RSASSA-PKCS1-v1_5")?;
    set_prop(&algorithm, "hash", "SHA-256")?;

    let usages = worker::js_sys::Array::of1(&wasm_bindgen::JsValue::from_str("verify"));

    let import_promise = subtle
        .import_key_with_object("jwk", jwk_obj.as_ref(), &algorithm, false, &usages)
        .map_err(|e| OidcError(format!("importKey call failed: {e:?}")))?;
    let key: web_sys::CryptoKey = JsFuture::from(import_promise)
        .await
        .map_err(|e| OidcError(format!("importKey rejected: {e:?}")))?
        .unchecked_into();

    let verify_promise = subtle
        .verify_with_str_and_u8_array_and_u8_array(
            "RSASSA-PKCS1-v1_5",
            &key,
            &signature,
            signing_input.as_bytes(),
        )
        .map_err(|e| OidcError(format!("verify call failed: {e:?}")))?;
    let valid: bool = JsFuture::from(verify_promise)
        .await
        .map_err(|e| OidcError(format!("verify rejected: {e:?}")))?
        .as_bool()
        .unwrap_or(false);

    if !valid {
        return Err(OidcError(
            "OIDC JWT signature verification failed".to_string(),
        ));
    }
    Ok(())
}

fn set_prop(obj: &worker::js_sys::Object, key: &str, value: &str) -> Result<(), OidcError> {
    worker::js_sys::Reflect::set(
        obj,
        &wasm_bindgen::JsValue::from_str(key),
        &wasm_bindgen::JsValue::from_str(value),
    )
    .map(|_| ())
    .map_err(|e| OidcError(format!("cannot build WebCrypto JWK object: {e:?}")))
}

/// Full verification: fetches the issuer's JWKS (cache-first, module
/// docs), finds the key named by the JWT's `kid`, and — "cached, refetched
/// on `kid` miss" per docs/design/byo-ci.md § Auth — refetches once via
/// [`fetch_jwks_bypassing_cache`] if the cached/first fetch's JWKS doesn't
/// have it, so a key rotated after the cache was populated is still found.
/// Then verifies the RS256 signature and validates
/// `iss`/`aud`/`exp`/`nbf`. Returns the typed claims on success — the
/// caller is responsible for the final allowlist-membership check (module
/// docs).
pub async fn verify(
    jwt: &str,
    expected_audience: &str,
    now_unix_s: i64,
) -> Result<VerifiedOidcClaims, OidcError> {
    let kid = decode_kid(jwt)?;

    let mut jwks = fetch_jwks(GITHUB_OIDC_ISSUER).await?;
    let mut jwk = jwks.keys.iter().find(|k| k.kid == kid);
    if jwk.is_none() {
        jwks = fetch_jwks_bypassing_cache(GITHUB_OIDC_ISSUER).await?;
        jwk = jwks.keys.iter().find(|k| k.kid == kid);
    }
    let jwk = jwk.ok_or_else(|| OidcError(format!("no JWKS key found for kid {kid}")))?;

    verify_signature(jwt, jwk).await?;
    validate_claims(jwt, expected_audience, now_unix_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(header_json: &str, payload_json: &str) -> String {
        use base64::Engine as _;
        let header_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(header_json.as_bytes());
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        format!("{header_b64}.{payload_b64}.fake-signature")
    }

    fn valid_payload() -> String {
        r#"{
            "iss": "https://token.actions.githubusercontent.com",
            "aud": "https://ci.example.com/exchange",
            "exp": 2000,
            "nbf": 500,
            "repository_id": "12345",
            "repository_owner_id": "67890",
            "run_id": "999",
            "run_attempt": "1",
            "sha": "abcdef0123456789",
            "ref": "refs/heads/main",
            "event_name": "push"
        }"#
        .to_string()
    }

    #[test]
    fn split_jwt_requires_exactly_three_segments() {
        assert!(split_jwt("a.b.c").is_ok());
        assert!(split_jwt("a.b").is_err());
        assert!(split_jwt("a.b.c.d").is_err());
        assert!(split_jwt("noseparators").is_err());
    }

    #[test]
    fn decode_kid_reads_header_kid_for_rs256() -> Result<(), OidcError> {
        let jwt = make_jwt(r#"{"alg":"RS256","typ":"JWT","kid":"test-kid-1"}"#, "{}");
        assert_eq!(decode_kid(&jwt)?, "test-kid-1");
        Ok(())
    }

    #[test]
    fn decode_kid_rejects_non_rs256_alg() {
        let jwt = make_jwt(r#"{"alg":"HS256","typ":"JWT","kid":"x"}"#, "{}");
        assert!(decode_kid(&jwt).is_err());
    }

    #[test]
    fn decode_kid_rejects_malformed_jwt() {
        assert!(decode_kid("not-a-jwt").is_err());
    }

    #[test]
    fn validate_claims_accepts_a_well_formed_token_within_its_window() -> Result<(), OidcError> {
        let jwt = make_jwt(r#"{"alg":"RS256","typ":"JWT","kid":"k"}"#, &valid_payload());
        let claims = validate_claims(&jwt, "https://ci.example.com/exchange", 1000)?;
        assert_eq!(claims.repository_id, 12345);
        assert_eq!(claims.repository_owner_id, 67890);
        assert_eq!(claims.run_id, "999");
        assert_eq!(claims.run_attempt, "1");
        assert_eq!(claims.sha, "abcdef0123456789");
        assert_eq!(claims.git_ref, "refs/heads/main");
        assert_eq!(claims.event_name, "push");
        Ok(())
    }

    #[test]
    fn validate_claims_accepts_numeric_repository_ids_too() -> Result<(), OidcError> {
        let payload = r#"{
            "iss": "https://token.actions.githubusercontent.com",
            "aud": "aud",
            "exp": 2000,
            "nbf": 500,
            "repository_id": 12345,
            "repository_owner_id": 67890,
            "run_id": "999",
            "run_attempt": "1",
            "sha": "abc",
            "ref": "refs/heads/main",
            "event_name": "push"
        }"#;
        let jwt = make_jwt(r#"{"alg":"RS256","typ":"JWT","kid":"k"}"#, payload);
        let claims = validate_claims(&jwt, "aud", 1000)?;
        assert_eq!(claims.repository_id, 12345);
        Ok(())
    }

    #[test]
    fn validate_claims_rejects_wrong_issuer() {
        let payload = valid_payload().replace(
            "https://token.actions.githubusercontent.com",
            "https://evil.example.com",
        );
        let jwt = make_jwt(r#"{"alg":"RS256","typ":"JWT","kid":"k"}"#, &payload);
        let result = validate_claims(&jwt, "https://ci.example.com/exchange", 1000);
        assert!(result.is_err());
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("issuer"));
    }

    #[test]
    fn validate_claims_rejects_wrong_audience() {
        let jwt = make_jwt(r#"{"alg":"RS256","typ":"JWT","kid":"k"}"#, &valid_payload());
        let result = validate_claims(&jwt, "https://other.example.com/exchange", 1000);
        assert!(result.is_err());
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("audience"));
    }

    #[test]
    fn validate_claims_rejects_expired_token() {
        let jwt = make_jwt(r#"{"alg":"RS256","typ":"JWT","kid":"k"}"#, &valid_payload());
        let result = validate_claims(&jwt, "https://ci.example.com/exchange", 2000);
        assert!(result.is_err());
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("expired"));
    }

    #[test]
    fn validate_claims_rejects_token_not_yet_valid() {
        let jwt = make_jwt(r#"{"alg":"RS256","typ":"JWT","kid":"k"}"#, &valid_payload());
        let result = validate_claims(&jwt, "https://ci.example.com/exchange", 10);
        assert!(result.is_err());
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("nbf"));
    }

    #[test]
    fn validate_claims_rejects_token_exactly_at_exp() {
        // exp is an instant of expiry, not inclusive — `now_unix_s == exp`
        // must already be rejected (matches `ingest_token::verify`'s
        // `claims.exp <= now_unix_s` semantics).
        let jwt = make_jwt(r#"{"alg":"RS256","typ":"JWT","kid":"k"}"#, &valid_payload());
        assert!(validate_claims(&jwt, "https://ci.example.com/exchange", 2000).is_err());
    }

    #[test]
    fn openid_configuration_url_appends_well_known_path() {
        assert_eq!(
            openid_configuration_url("https://token.actions.githubusercontent.com"),
            "https://token.actions.githubusercontent.com/.well-known/openid-configuration"
        );
    }

    #[test]
    fn jwks_response_parses_documented_shape() -> Result<(), serde_json::Error> {
        let body = r#"{
            "keys": [
                {
                    "kty": "RSA",
                    "use": "sig",
                    "kid": "abc123",
                    "alg": "RS256",
                    "n": "modulus-base64url",
                    "e": "AQAB"
                }
            ]
        }"#;
        let jwks: Jwks = serde_json::from_str(body)?;
        assert_eq!(jwks.keys.len(), 1);
        assert_eq!(jwks.keys[0].kid, "abc123");
        assert_eq!(jwks.keys[0].n, "modulus-base64url");
        assert_eq!(jwks.keys[0].e, "AQAB");
        Ok(())
    }

    #[test]
    fn openid_configuration_response_parses_jwks_uri() -> Result<(), serde_json::Error> {
        let body = r#"{
            "issuer": "https://token.actions.githubusercontent.com",
            "jwks_uri": "https://token.actions.githubusercontent.com/.well-known/jwks",
            "claims_supported": ["repository_id", "repository_owner_id"]
        }"#;
        let config: OpenIdConfiguration = serde_json::from_str(body)?;
        assert_eq!(
            config.jwks_uri,
            "https://token.actions.githubusercontent.com/.well-known/jwks"
        );
        Ok(())
    }

    #[test]
    fn looks_like_jwt_accepts_three_segments() {
        assert!(looks_like_jwt("header.payload.signature"));
    }

    #[test]
    fn looks_like_jwt_rejects_opaque_scoped_api_token() {
        assert!(!looks_like_jwt("cc_tok_abcdef1234567890"));
    }

    #[test]
    fn looks_like_jwt_rejects_wrong_segment_counts() {
        assert!(!looks_like_jwt("a.b"));
        assert!(!looks_like_jwt("a.b.c.d"));
        assert!(!looks_like_jwt("noseparators"));
    }
}
