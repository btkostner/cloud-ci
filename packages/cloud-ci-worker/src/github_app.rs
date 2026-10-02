//! GitHub App JWT minting, RS256 signing, and installation access token
//! exchange (docs/roadmap.md Phase 0 "GitHub App JWT" spike;
//! docs/design/auth.md § "GitHub App setup" "App-level authentication"
//! paragraph).
//!
//! This module has three layers, in increasing order of "needs the
//! Workers runtime to actually run":
//!
//! 1. [`build_claims`] / [`signing_input`] — pure claim construction and
//!    the unsigned `header.payload` string. No `worker`/WebCrypto
//!    dependency; unit-testable with plain `cargo test`, same as
//!    `ingest_token.rs`.
//! 2. [`sign_rs256`] — the actual RS256 signature, via the Workers
//!    runtime's WebCrypto `SubtleCrypto.sign()`. This one genuinely needs
//!    the `wasm32` Workers runtime to execute (see "Why WebCrypto" below)
//!    and is only smoke-tested under `wrangler dev`, not `cargo test`.
//! 3. [`fetch_installation_token`] — exchanges a signed App JWT for an
//!    installation access token
//!    (docs.github.com/en/rest/apps/apps#create-an-installation-access-token-for-an-app,
//!    accessed 2026-10-01). Its URL/header construction
//!    ([`installation_access_token_url`]/[`installation_access_token_headers`])
//!    and [`InstallationToken`] response parsing are pure enough to unit
//!    test; the actual HTTP call is **not** live-verified against
//!    GitHub, since no real GitHub App installation exists yet (full
//!    `cloud-ci setup github-app` is a later round's work — this module
//!    is a capability check, not the setup flow).
//!
//! Nothing in `lib.rs` calls [`fetch_installation_token`] yet — webhook
//! handling and Check-Run posting, its real callers, don't exist yet. It
//! is intentionally dead code from the router's perspective this round.
//!
//! # Claim shape and TTL
//!
//! Per GitHub's docs
//! (docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app,
//! accessed 2026-10-01): `alg: RS256`; `iat` set 60 seconds in the past
//! ("we recommend that you set this 60 seconds in the past" to tolerate
//! clock drift); `exp` "no more than 10 minutes into the future"; `iss` =
//! "the client ID or application ID of your GitHub App" ("Use of the
//! client ID is recommended", but the numeric application ID is
//! explicitly documented as valid too). This module takes the numeric App
//! ID as a `u64` and signs it in `iss` as a JSON number — GitHub accepts
//! both the numeric ID and the client ID string for `iss`.
//!
//! # Why WebCrypto, not a Rust RSA crate
//!
//! GitHub App JWTs must be signed with RS256 (RSASSA-PKCS1-v1_5 /
//! SHA-256 over an RSA key) — categorically different from the
//! HMAC-SHA256 `ingest_token.rs` already does, which only needs the
//! `hmac`/`sha2` crates (pure Rust, no platform crypto API). An RSA
//! signing crate would work the same way, but this repo's Phase 0 spike
//! (docs/roadmap.md) specifically asks whether the *platform's* WebCrypto
//! exposes RS256 signing from Rust, since that is the path every other
//! crypto operation in this codebase already uses (see
//! docs/design/auth.md's webhook signature verification, which uses
//! `crypto.subtle.verify`) — staying on one crypto primitive path avoids
//! a second, divergent dependency just for this one operation.
//!
//! Investigation: `workers-rs` 0.8.7's own `worker::crypto` module
//! (`worker-0.8.7/src/crypto.rs`) only wraps the non-standard
//! `DigestStream` API (SHA digests of a stream) — it has no
//! `SubtleCrypto`/`CryptoKey` wrapper at all. `web-sys` 0.3.106 (already
//! a transitive dependency of `worker` via `worker/Cargo.toml`'s
//! `[dependencies.web-sys]`) *does* generate full bindings for
//! `SubtleCrypto::sign`/`importKey`
//! (`web-sys-0.3.106/src/features/gen_SubtleCrypto.rs`), gated behind the
//! `SubtleCrypto`/`CryptoKey`/`Crypto` web-sys feature flags, which
//! `worker`'s own `Cargo.toml` does not request (it only requests the
//! features it needs for its own wrappers). Cargo feature unification
//! means this crate's own `[dependencies.web-sys]` entry (see
//! `Cargo.toml`) turns those features on for the one `web-sys` crate
//! instance shared with `worker`, without needing to fork or patch
//! `worker` itself. The global `SubtleCrypto` instance is reached the
//! same way `worker` itself reaches other globals (`cache.rs`,
//! `delay.rs`, `global.rs`, `websocket.rs` all do
//! `js_sys::global().unchecked_into::<web_sys::WorkerGlobalScope>()`):
//! `.crypto()?.subtle()`.
//!
//! `importKey` only accepts a PKCS#8 DER key for `"pkcs8"` format RSA
//! private keys — it does **not** accept the older PKCS#1 PEM format
//! (`RSA PRIVATE KEY` label), which is exactly the format GitHub hands
//! back from the App manifest flow and from `openssl genrsa` (this
//! repo's throwaway test keypair used the same format). Rather than
//! require operators to re-encode their App's private key before
//! pasting it into a secret, [`sign_rs256`] detects PKCS#1 PEM input and
//! wraps it in the fixed `PrivateKeyInfo` ASN.1 envelope PKCS#8 requires
//! (a `rsaEncryption` `AlgorithmIdentifier` plus the untouched PKCS#1
//! bytes as the `privateKey` `OCTET STRING`) before calling `importKey`.
//! PKCS#8 PEM input (`PRIVATE KEY` label) is accepted as-is.

use serde::{Deserialize, Serialize};

/// GitHub's documented maximum: "The time must be no more than 10 minutes
/// into the future."
const JWT_TTL_SECONDS: i64 = 600;

/// GitHub's documented recommendation: "we recommend that you set this 60
/// seconds in the past" to tolerate clock drift between this Worker and
/// GitHub's verifier.
const JWT_CLOCK_DRIFT_TOLERANCE_SECONDS: i64 = 60;

#[derive(Debug, PartialEq, Eq)]
pub struct GithubAppError(String);

impl std::fmt::Display for GithubAppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for GithubAppError {}

#[derive(Debug, Clone, Copy, Serialize)]
struct JwtHeader {
    alg: &'static str,
    typ: &'static str,
}

/// The three claims GitHub requires for App-level JWTs — see module docs
/// for the exact shape/TTL citation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppClaims {
    /// Issued-at, Unix seconds, already shifted back by
    /// [`JWT_CLOCK_DRIFT_TOLERANCE_SECONDS`].
    pub iat: i64,
    /// Expiry, Unix seconds, at most [`JWT_TTL_SECONDS`] after `iat`'s
    /// unshifted `now`.
    pub exp: i64,
    /// The GitHub App's numeric ID.
    pub iss: u64,
}

/// Builds the JWT claims for App `app_id`, anchored at `now_unix_s`.
/// `now_unix_s` is a parameter (not read from a clock internally) for the
/// same determinism/testability reason as `ingest_token::mint`'s.
pub fn build_claims(app_id: u64, now_unix_s: i64) -> AppClaims {
    AppClaims {
        iat: now_unix_s - JWT_CLOCK_DRIFT_TOLERANCE_SECONDS,
        exp: now_unix_s + JWT_TTL_SECONDS,
        iss: app_id,
    }
}

/// Produces the unsigned `header.payload` string ready for RS256 signing:
/// `base64url(header_json) + "." + base64url(claims_json)`. Separate from
/// the actual signing call (added in a later commit) so this half can be
/// unit-tested without the Workers runtime.
pub fn signing_input(claims: &AppClaims) -> Result<String, GithubAppError> {
    let header = JwtHeader {
        alg: "RS256",
        typ: "JWT",
    };
    let header_json = serde_json::to_vec(&header)
        .map_err(|e| GithubAppError(format!("cannot encode JWT header: {e}")))?;
    let claims_json = serde_json::to_vec(claims)
        .map_err(|e| GithubAppError(format!("cannot encode JWT claims: {e}")))?;
    Ok(format!(
        "{}.{}",
        base64_url_encode(&header_json),
        base64_url_encode(&claims_json)
    ))
}

/// Signs `signing_input` (from [`signing_input`]) with RS256 using the
/// Workers runtime's WebCrypto `SubtleCrypto`, returning the complete
/// `header.payload.signature` JWT. `private_key_pem` is a PEM-formatted
/// RSA private key, PKCS#1 (`RSA PRIVATE KEY` PEM label) or PKCS#8
/// (`PRIVATE KEY` PEM label) — see module docs for why PKCS#1 needs
/// re-wrapping here.
///
/// There is no hardcoded or default key anywhere in this module — the
/// caller is required to resolve `private_key_pem` from the Worker's
/// `GITHUB_APP_PRIVATE_KEY` secret binding (`env.secret("GITHUB_APP_PRIVATE_KEY")`),
/// failing the request if it isn't configured. A hardcoded fallback here
/// would mean anyone who reads this source (this repository is public)
/// could forge a JWT that impersonates the GitHub App against any
/// deployment using it. Local dev sets this via `.dev.vars` (see
/// `.dev.vars.example`); production sets it via Secrets Store once
/// `cloud-ci setup github-app` exists (docs/design/auth.md § "Secret
/// storage").
pub async fn sign_rs256(
    private_key_pem: &str,
    signing_input: &str,
) -> Result<String, GithubAppError> {
    use wasm_bindgen::JsCast;
    use worker::wasm_bindgen_futures::JsFuture;
    use worker::web_sys;

    let pkcs8_der = pkcs8_der_from_pem(private_key_pem)?;

    let global: web_sys::WorkerGlobalScope = worker::js_sys::global().unchecked_into();
    let crypto = global
        .crypto()
        .map_err(|e| GithubAppError(format!("WebCrypto unavailable: {e:?}")))?;
    let subtle = crypto.subtle();

    let algorithm = worker::js_sys::Object::new();
    set_prop(&algorithm, "name", "RSASSA-PKCS1-v1_5")?;
    set_prop(&algorithm, "hash", "SHA-256")?;

    let key_data = worker::js_sys::Uint8Array::from(pkcs8_der.as_slice());
    let usages = worker::js_sys::Array::of1(&wasm_bindgen::JsValue::from_str("sign"));

    let import_promise = subtle
        .import_key_with_object("pkcs8", key_data.as_ref(), &algorithm, false, &usages)
        .map_err(|e| GithubAppError(format!("importKey call failed: {e:?}")))?;
    let key: web_sys::CryptoKey = JsFuture::from(import_promise)
        .await
        .map_err(|e| GithubAppError(format!("importKey rejected: {e:?}")))?
        .unchecked_into();

    let sign_promise = subtle
        .sign_with_str_and_u8_array("RSASSA-PKCS1-v1_5", &key, signing_input.as_bytes())
        .map_err(|e| GithubAppError(format!("sign call failed: {e:?}")))?;
    let signature_buffer: worker::js_sys::ArrayBuffer = JsFuture::from(sign_promise)
        .await
        .map_err(|e| GithubAppError(format!("sign rejected: {e:?}")))?
        .unchecked_into();
    let signature = worker::js_sys::Uint8Array::new(&signature_buffer).to_vec();

    Ok(format!("{signing_input}.{}", base64_url_encode(&signature)))
}

fn set_prop(obj: &worker::js_sys::Object, key: &str, value: &str) -> Result<(), GithubAppError> {
    worker::js_sys::Reflect::set(
        obj,
        &wasm_bindgen::JsValue::from_str(key),
        &wasm_bindgen::JsValue::from_str(value),
    )
    .map(|_| ())
    .map_err(|e| GithubAppError(format!("cannot build WebCrypto algorithm object: {e:?}")))
}

/// `rsaEncryption` `AlgorithmIdentifier` DER, fixed per RFC 8017 Appendix
/// A.1 / RFC 5280: `SEQUENCE { OID 1.2.840.113549.1.1.1, NULL }`.
const RSA_ENCRYPTION_ALGORITHM_IDENTIFIER: &[u8] = &[
    0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
];

/// Decodes a PEM-formatted RSA private key into a PKCS#8 `PrivateKeyInfo`
/// DER byte string, suitable for WebCrypto `importKey("pkcs8", ...)`. PEM
/// already in PKCS#8 form is decoded and returned as-is; PKCS#1 form is
/// wrapped in the `PrivateKeyInfo` envelope first — see module docs.
fn pkcs8_der_from_pem(pem: &str) -> Result<Vec<u8>, GithubAppError> {
    let (label, der) = decode_pem(pem)?;
    match label {
        "PRIVATE KEY" => Ok(der),
        "RSA PRIVATE KEY" => Ok(der_sequence(
            &[
                // version INTEGER 0
                &[0x02, 0x01, 0x00][..],
                RSA_ENCRYPTION_ALGORITHM_IDENTIFIER,
                &der_octet_string(&der),
            ]
            .concat(),
        )),
        other => Err(GithubAppError(format!(
            "unsupported PEM key type {other:?} — expected \"RSA PRIVATE KEY\" (PKCS#1) or \"PRIVATE KEY\" (PKCS#8)"
        ))),
    }
}

/// Strips a PEM `-----BEGIN <label>----- ... -----END <label>-----` block
/// down to its label and base64-decoded DER bytes.
fn decode_pem(pem: &str) -> Result<(&str, Vec<u8>), GithubAppError> {
    let pem = pem.trim();
    let begin = pem
        .strip_prefix("-----BEGIN ")
        .ok_or_else(|| GithubAppError("private key is not PEM (missing BEGIN line)".to_string()))?;
    let (label, rest) = begin
        .split_once("-----")
        .ok_or_else(|| GithubAppError("malformed PEM BEGIN line".to_string()))?;
    let end_marker = format!("-----END {label}-----");
    let body = rest
        .trim_start()
        .strip_suffix(&end_marker)
        .map(str::trim)
        .ok_or_else(|| {
            GithubAppError(format!("malformed PEM — missing matching \"{end_marker}\""))
        })?;
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();

    use base64::Engine as _;
    let der = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|e| GithubAppError(format!("cannot base64-decode PEM body: {e}")))?;
    Ok((label, der))
}

fn der_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else {
        let bytes = len.to_be_bytes();
        let significant: Vec<u8> = bytes.iter().copied().skip_while(|&b| b == 0).collect();
        let mut out = vec![0x80 | significant.len() as u8];
        out.extend(significant);
        out
    }
}

fn der_sequence(contents: &[u8]) -> Vec<u8> {
    let mut out = vec![0x30];
    out.extend(der_length(contents.len()));
    out.extend_from_slice(contents);
    out
}

fn der_octet_string(contents: &[u8]) -> Vec<u8> {
    let mut out = vec![0x04];
    out.extend(der_length(contents.len()));
    out.extend_from_slice(contents);
    out
}

fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `X-GitHub-Api-Version` for the installation access token endpoint
/// (docs.github.com/en/rest/apps/apps#create-an-installation-access-token-for-an-app,
/// accessed 2026-10-01).
const GITHUB_API_VERSION: &str = "2022-11-28";

/// An installation access token — docs.github.com/en/rest/apps/apps#create-an-installation-access-token-for-an-app
/// (accessed 2026-10-01) returns more fields (e.g. `permissions`), but
/// `token` and `expires_at` are the only two callers in this codebase
/// need so far.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: String,
}

/// The installation access token endpoint's URL, with `installation_id`
/// interpolated — pure string construction, unit-testable without the
/// Workers runtime.
fn installation_access_token_url(installation_id: u64) -> String {
    format!("https://api.github.com/app/installations/{installation_id}/access_tokens")
}

/// The three headers GitHub's docs specify for this endpoint
/// (`Authorization: Bearer <app jwt>`, `Accept`, `X-GitHub-Api-Version`) —
/// pure construction, unit-testable without the Workers runtime.
fn installation_access_token_headers(app_jwt: &str) -> [(&'static str, String); 3] {
    [
        ("authorization", format!("Bearer {app_jwt}")),
        ("accept", "application/vnd.github+json".to_string()),
        ("x-github-api-version", GITHUB_API_VERSION.to_string()),
    ]
}

/// Exchanges a signed App JWT ([`sign_rs256`]'s output) for an
/// installation access token:
/// `POST /app/installations/{installation_id}/access_tokens`
/// (docs.github.com/en/rest/apps/apps#create-an-installation-access-token-for-an-app,
/// accessed 2026-10-01). Installation tokens expire one hour after
/// creation (same doc).
///
/// Nothing in `lib.rs` calls this yet — see module docs. It is exercised
/// by this module's unit tests (URL/header construction,
/// [`InstallationToken`] response parsing) but **not** live-verified
/// against GitHub: no real GitHub App installation exists in this
/// environment to call it against.
pub async fn fetch_installation_token(
    app_jwt: &str,
    installation_id: u64,
) -> Result<InstallationToken, GithubAppError> {
    let url = installation_access_token_url(installation_id);
    let headers = worker::Headers::new();
    for (name, value) in installation_access_token_headers(app_jwt) {
        headers
            .set(name, &value)
            .map_err(|e| GithubAppError(format!("cannot set {name} header: {e}")))?;
    }

    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Post);
    init.with_headers(headers);

    let request = worker::Request::new_with_init(&url, &init)
        .map_err(|e| GithubAppError(format!("cannot build installation token request: {e}")))?;

    let mut response = worker::Fetch::Request(request)
        .send()
        .await
        .map_err(|e| GithubAppError(format!("installation token request failed: {e}")))?;

    if response.status_code() != 201 {
        let body = response.text().await.unwrap_or_default();
        return Err(GithubAppError(format!(
            "installation token exchange failed: {} {body}",
            response.status_code()
        )));
    }

    response
        .json::<InstallationToken>()
        .await
        .map_err(|e| GithubAppError(format!("cannot decode installation token response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a PEM `BEGIN <label> ... END <label>` block from parts,
    /// same shape [`decode_pem`] parses — kept out of a single string
    /// literal so this file never contains an unbroken PEM marker for a
    /// secret scanner to flag; these are synthetic, non-key test
    /// fixtures, not real material.
    fn pem_block(label: &str, base64_body: &str) -> String {
        let dashes = "-".repeat(5);
        format!("{dashes}BEGIN {label}{dashes}\n{base64_body}\n{dashes}END {label}{dashes}\n")
    }

    #[test]
    fn claims_have_60s_clock_drift_tolerance_and_10min_ttl() {
        let claims = build_claims(123456, 1_000_000);
        assert_eq!(claims.iat, 1_000_000 - 60);
        assert_eq!(claims.exp, 1_000_000 + 600);
        assert_eq!(claims.iss, 123456);
    }

    #[test]
    fn signing_input_is_two_dot_separated_base64url_parts() -> Result<(), GithubAppError> {
        let claims = build_claims(1, 1_000);
        let input = signing_input(&claims)?;
        let parts: Vec<&str> = input.split('.').collect();
        assert_eq!(parts.len(), 2);
        assert!(!parts[0].is_empty());
        assert!(!parts[1].is_empty());
        Ok(())
    }

    #[test]
    fn signing_input_header_decodes_to_rs256_jwt() -> Result<(), Box<dyn std::error::Error>> {
        let claims = build_claims(1, 1_000);
        let input = signing_input(&claims)?;
        let header_b64 = input.split('.').next().ok_or("missing header segment")?;
        use base64::Engine as _;
        let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(header_b64)?;
        let header: serde_json::Value = serde_json::from_slice(&header_bytes)?;
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
        Ok(())
    }

    #[test]
    fn signing_input_payload_decodes_to_expected_claims() -> Result<(), Box<dyn std::error::Error>>
    {
        let claims = build_claims(42, 5_000);
        let input = signing_input(&claims)?;
        let payload_b64 = input.split('.').nth(1).ok_or("missing payload segment")?;
        use base64::Engine as _;
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64)?;
        let decoded: AppClaims = serde_json::from_slice(&payload_bytes)?;
        assert_eq!(decoded, claims);
        Ok(())
    }

    #[test]
    fn same_inputs_produce_identical_signing_input() -> Result<(), GithubAppError> {
        let a = signing_input(&build_claims(7, 1_000))?;
        let b = signing_input(&build_claims(7, 1_000))?;
        assert_eq!(a, b);
        Ok(())
    }

    #[test]
    fn pkcs1_pem_is_wrapped_into_a_pkcs8_private_key_info() -> Result<(), GithubAppError> {
        // A minimal (not cryptographically valid) PKCS#1 DER body is fine
        // here — this test only checks the DER envelope this module
        // builds around it, not key validity (that is exercised by the
        // `wrangler dev` WebCrypto smoke test, which needs a real RSA
        // key).
        let fake_pkcs1_der = b"not-a-real-rsa-key-but-fine-for-this-test";
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(fake_pkcs1_der);
        let pem = pem_block("RSA PRIVATE KEY", &encoded);

        let pkcs8 = pkcs8_der_from_pem(&pem)?;

        // PrivateKeyInfo ::= SEQUENCE { version, algorithm, OCTET STRING privateKey }
        assert_eq!(pkcs8[0], 0x30, "outer tag must be SEQUENCE");
        assert!(
            pkcs8
                .windows(RSA_ENCRYPTION_ALGORITHM_IDENTIFIER.len())
                .any(|w| w == RSA_ENCRYPTION_ALGORITHM_IDENTIFIER),
            "must contain the rsaEncryption AlgorithmIdentifier"
        );
        assert!(
            pkcs8
                .windows(fake_pkcs1_der.len())
                .any(|w| w == fake_pkcs1_der),
            "original PKCS#1 bytes must be preserved verbatim inside the OCTET STRING"
        );
        Ok(())
    }

    #[test]
    fn pkcs8_pem_passes_through_unwrapped() -> Result<(), GithubAppError> {
        let der_bytes = b"already-pkcs8-der-bytes";
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(der_bytes);
        let pem = pem_block("PRIVATE KEY", &encoded);

        let der = pkcs8_der_from_pem(&pem)?;
        assert_eq!(der, der_bytes);
        Ok(())
    }

    #[test]
    fn unsupported_pem_label_is_rejected() {
        let pem = pem_block("EC PRIVATE KEY", "AA==");
        assert!(pkcs8_der_from_pem(&pem).is_err());
    }

    #[test]
    fn installation_token_url_interpolates_installation_id() {
        assert_eq!(
            installation_access_token_url(42),
            "https://api.github.com/app/installations/42/access_tokens"
        );
    }

    #[test]
    fn installation_token_headers_carry_bearer_jwt_and_api_version() {
        let headers = installation_access_token_headers("my.jwt.value");
        assert_eq!(
            headers[0],
            ("authorization", "Bearer my.jwt.value".to_string())
        );
        assert_eq!(
            headers[1],
            ("accept", "application/vnd.github+json".to_string())
        );
        assert_eq!(
            headers[2],
            ("x-github-api-version", GITHUB_API_VERSION.to_string())
        );
    }

    #[test]
    fn installation_token_response_parses_documented_shape() -> Result<(), serde_json::Error> {
        // Realistic shape per docs.github.com/en/rest/apps/apps
        // #create-an-installation-access-token-for-an-app (extra fields
        // like `permissions`/`repository_selection` are present on the
        // real response and must be ignored, not rejected).
        let body = r#"{
            "token": "ghs_16C7e42F292c6912E7710c838347Ae178B4a",
            "expires_at": "2026-10-01T12:00:00Z",
            "permissions": { "issues": "write", "contents": "read" },
            "repository_selection": "all"
        }"#;
        let parsed: InstallationToken = serde_json::from_str(body)?;
        assert_eq!(parsed.token, "ghs_16C7e42F292c6912E7710c838347Ae178B4a");
        assert_eq!(parsed.expires_at, "2026-10-01T12:00:00Z");
        Ok(())
    }
}
