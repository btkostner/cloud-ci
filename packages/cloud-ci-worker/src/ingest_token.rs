//! Mints the opaque, HMAC-signed "ingest token" `BeginRun` returns
//! (docs/design/byo-ci.md § Auth): `typ: "ingest"`, `scope: ["ingest:write"]`,
//! `repo_id`/`run_id` claims, 1-hour TTL. Format is `<base64url(claims
//! json)>.<base64url(hmac-sha256 signature)>` — not a JWT (no header, no
//! algorithm negotiation to parse), since the only consumer is this same
//! deployment verifying its own tokens.
//!
//! Pure string/crypto logic with no `worker` dependency, so it is
//! unit-testable natively.
//!
//! # Signing key
//!
//! There is no hardcoded or default signing key anywhere in this module —
//! [`mint`] takes the key as a parameter and the caller (`lib.rs`) is
//! required to resolve it from the Worker's `INGEST_TOKEN_SECRET` secret
//! binding (`env.secret("INGEST_TOKEN_SECRET")`), failing the request if
//! it isn't configured. A hardcoded fallback here would mean anyone who
//! reads this source (this repository is public) could forge a valid
//! ingest token for any `repo_id`/`run_id` against a real deployment. Local
//! dev sets this via `.dev.vars` (see `.dev.vars.example`); production sets
//! it via `wrangler secret put INGEST_TOKEN_SECRET`, pending ADR 0004's
//! eventual Secrets Store wiring.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const INGEST_TOKEN_TTL_SECONDS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IngestClaims {
    typ: String,
    scope: Vec<String>,
    repo_id: u64,
    run_id: String,
    /// Unix seconds.
    exp: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TokenError(String);

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TokenError {}

/// Mints an ingest token for `repo_id`/`run_id`, expiring `INGEST_TOKEN_TTL_SECONDS`
/// after `now_unix_s`, signed with `secret`. `now_unix_s` is a parameter
/// (not read from a clock internally) so minting is deterministic and
/// unit-testable. `secret` is a parameter (never a constant in this module)
/// so there is no key anywhere in source control that could sign a real
/// token — see module docs.
pub fn mint(
    secret: &[u8],
    repo_id: u64,
    run_id: &str,
    now_unix_s: u64,
) -> Result<String, TokenError> {
    let claims = IngestClaims {
        typ: "ingest".to_string(),
        scope: vec!["ingest:write".to_string()],
        repo_id,
        run_id: run_id.to_string(),
        exp: now_unix_s + INGEST_TOKEN_TTL_SECONDS,
    };
    let payload = serde_json::to_vec(&claims)
        .map_err(|e| TokenError(format!("cannot encode ingest token claims: {e}")))?;
    let payload_b64 = base64_url_encode(&payload);

    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .map_err(|e| TokenError(format!("invalid HMAC key: {e}")))?;
    mac.update(payload_b64.as_bytes());
    let signature_b64 = base64_url_encode(&mac.finalize().into_bytes());

    Ok(format!("{payload_b64}.{signature_b64}"))
}

/// Claims extracted from a successfully verified ingest token — only what
/// callers outside this module need (docs/design/byo-ci.md "Security
/// considerations": "a part PUT without a valid token for the owning run's
/// `repo_id` is rejected before touching R2").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedClaims {
    pub repo_id: u64,
    pub run_id: String,
}

/// Verifies an ingest token minted by [`mint`]: re-derives the HMAC over the
/// payload segment and checks it against the token's signature via
/// [`Mac::verify_slice`] (constant-time comparison, backed by
/// `subtle::ConstantTimeEq` inside the `hmac`/`digest` crates — never a
/// hand-rolled `==` on the encoded signature, which would leak timing
/// information byte-by-byte), checks `typ`/`scope`, and rejects an expired
/// token. `now_unix_s` is a parameter for the same determinism/testability
/// reason as [`mint`]'s.
pub fn verify(secret: &[u8], token: &str, now_unix_s: u64) -> Result<VerifiedClaims, TokenError> {
    let (payload_b64, signature_b64) = token
        .split_once('.')
        .ok_or_else(|| TokenError("malformed ingest token".to_string()))?;

    use base64::Engine as _;
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature_b64)
        .map_err(|e| TokenError(format!("cannot decode ingest token signature: {e}")))?;

    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .map_err(|e| TokenError(format!("invalid HMAC key: {e}")))?;
    mac.update(payload_b64.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| TokenError("ingest token signature mismatch".to_string()))?;

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| TokenError(format!("cannot decode ingest token payload: {e}")))?;
    let claims: IngestClaims = serde_json::from_slice(&payload)
        .map_err(|e| TokenError(format!("cannot decode ingest token claims: {e}")))?;

    if claims.typ != "ingest" || !claims.scope.iter().any(|s| s == "ingest:write") {
        return Err(TokenError(
            "ingest token missing ingest:write scope".to_string(),
        ));
    }
    if claims.exp <= now_unix_s {
        return Err(TokenError("ingest token expired".to_string()));
    }

    Ok(VerifiedClaims {
        repo_id: claims.repo_id,
        run_id: claims.run_id,
    })
}

fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_two_dot_separated_base64url_parts() -> Result<(), TokenError> {
        let token = mint(
            b"test-secret",
            1_296_269,
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            1_000,
        )?;
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 2);
        assert!(!parts[0].is_empty());
        assert!(!parts[1].is_empty());
        Ok(())
    }

    #[test]
    fn same_inputs_mint_identical_tokens() -> Result<(), TokenError> {
        let a = mint(b"test-secret", 1, "run-a", 500)?;
        let b = mint(b"test-secret", 1, "run-a", 500)?;
        assert_eq!(a, b);
        Ok(())
    }

    #[test]
    fn different_secrets_mint_different_signatures() -> Result<(), TokenError> {
        let a = mint(b"secret-a", 1, "run-a", 500)?;
        let b = mint(b"secret-b", 1, "run-a", 500)?;
        assert_ne!(a, b);
        Ok(())
    }

    #[test]
    fn payload_decodes_to_expected_claims() -> Result<(), Box<dyn std::error::Error>> {
        let token = mint(b"test-secret", 42, "run-x", 1_000)?;
        let payload_b64 = token.split('.').next().ok_or("missing payload segment")?;
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64)?;
        let claims: IngestClaims = serde_json::from_slice(&payload)?;
        assert_eq!(claims.typ, "ingest");
        assert_eq!(claims.scope, vec!["ingest:write".to_string()]);
        assert_eq!(claims.repo_id, 42);
        assert_eq!(claims.run_id, "run-x");
        assert_eq!(claims.exp, 1_000 + INGEST_TOKEN_TTL_SECONDS);
        Ok(())
    }

    #[test]
    fn verify_accepts_a_freshly_minted_token() -> Result<(), TokenError> {
        let token = mint(b"test-secret", 42, "run-x", 1_000)?;
        let claims = verify(b"test-secret", &token, 1_000)?;
        assert_eq!(
            claims,
            VerifiedClaims {
                repo_id: 42,
                run_id: "run-x".to_string(),
            }
        );
        Ok(())
    }

    #[test]
    fn verify_rejects_wrong_secret() -> Result<(), TokenError> {
        let token = mint(b"test-secret", 42, "run-x", 1_000)?;
        assert!(verify(b"wrong-secret", &token, 1_000).is_err());
        Ok(())
    }

    #[test]
    fn verify_rejects_expired_token() -> Result<(), TokenError> {
        let token = mint(b"test-secret", 42, "run-x", 1_000)?;
        let after_expiry = 1_000 + INGEST_TOKEN_TTL_SECONDS + 1;
        assert!(verify(b"test-secret", &token, after_expiry).is_err());
        Ok(())
    }

    #[test]
    fn verify_rejects_tampered_payload() -> Result<(), TokenError> {
        let token = mint(b"test-secret", 42, "run-x", 1_000)?;
        let (_, sig) = token
            .split_once('.')
            .ok_or(TokenError("bad token".to_string()))?;
        let tampered = format!("{}.{sig}", base64_url_encode(b"{\"typ\":\"ingest\"}"));
        assert!(verify(b"test-secret", &tampered, 1_000).is_err());
        Ok(())
    }

    #[test]
    fn verify_rejects_a_tampered_signature() -> Result<(), TokenError> {
        let token = mint(b"test-secret", 42, "run-x", 1_000)?;
        let (payload, sig) = token
            .split_once('.')
            .ok_or(TokenError("bad token".to_string()))?;
        // Flip the signature to one computed over different payload bytes
        // (not a manual byte mutation, which could coincidentally decode to
        // invalid base64url) so `verify_slice` must reject it on content,
        // not on a decode failure.
        let other = mint(b"test-secret", 42, "run-y", 1_000)?;
        let (_, other_sig) = other
            .split_once('.')
            .ok_or(TokenError("bad token".to_string()))?;
        assert_ne!(sig, other_sig);
        let forged = format!("{payload}.{other_sig}");
        assert!(verify(b"test-secret", &forged, 1_000).is_err());
        Ok(())
    }

    #[test]
    fn verify_rejects_malformed_token_with_no_dot() {
        assert!(verify(b"test-secret", "not-a-token", 1_000).is_err());
    }
}
