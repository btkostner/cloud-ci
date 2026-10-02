//! GitHub App JWT minting (docs/roadmap.md Phase 0 "GitHub App JWT" spike;
//! docs/design/auth.md § "GitHub App setup" "App-level authentication"
//! paragraph).
//!
//! This is the first of three layers this module will grow, in
//! increasing order of "needs the Workers runtime to actually run":
//!
//! 1. [`build_claims`] / [`signing_input`] (this commit) — pure claim
//!    construction and the unsigned `header.payload` string. No
//!    `worker`/WebCrypto dependency; unit-testable with plain
//!    `cargo test`, same as `ingest_token.rs`.
//! 2. The actual RS256 signature, via the Workers runtime's WebCrypto
//!    `SubtleCrypto.sign()` (next commit).
//! 3. The installation access token exchange that consumes the signed
//!    JWT (final commit).
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

fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
