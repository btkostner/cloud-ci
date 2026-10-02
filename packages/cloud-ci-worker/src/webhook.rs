//! Verifies GitHub webhook delivery signatures (docs/design/auth.md §
//! "Webhook signature verification"): every delivery carries
//! `X-Hub-Signature-256: sha256=<hex hmac>`, computed by GitHub as
//! `HMAC-SHA256(webhook_secret, raw_request_body)`. [`verify_signature`]
//! re-derives that HMAC over the raw body and compares it against the
//! header's hex-decoded signature via [`Mac::verify_slice`] — the same
//! constant-time pattern `ingest_token.rs`'s `verify()` uses (backed by
//! `subtle::ConstantTimeEq` inside the `hmac`/`digest` crates), not a
//! hand-rolled `==`/`!=` on hex strings. That manual-comparison mistake is
//! exactly what was found and fixed in `ingest_token.rs`'s HMAC check
//! before this module was written; it is not repeated here.
//!
//! The design doc sketches this with `crypto.subtle.verify`/WebCrypto, but
//! plain HMAC-SHA256 (unlike `github_app.rs`'s RS256 JWT signing) has no
//! platform-crypto dependency: the pure-Rust `hmac`/`sha2` crates already
//! used by `ingest_token.rs` compute and compare it correctly and
//! constant-time under both `cargo test` and the real `wasm32` Workers
//! runtime, so this module reuses that approach instead.
//!
//! `POST /webhooks/github` in `lib.rs` is the real route: it reads the raw
//! request body first, calls [`verify_signature`] with the
//! `GITHUB_WEBHOOK_SECRET` secret, and rejects with 401 before parsing
//! anything or touching D1 on any `Err` — see `lib.rs`'s
//! `handle_github_webhook` and `src/installations.rs` for the
//! `installation`/`installation_repositories` event handling this
//! verification gates.

use hmac::{Hmac, Mac};
use sha2::Sha256;

const SHA256_PREFIX: &str = "sha256=";

/// Why a `GITHUB_WEBHOOK_SECRET`-signed delivery failed verification.
/// Never panics; every failure mode (missing prefix, invalid hex, wrong
/// length, HMAC mismatch) is a typed variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookError {
    /// The `X-Hub-Signature-256` header value did not start with
    /// `sha256=`.
    MissingPrefix,
    /// The part after `sha256=` was not valid hex.
    InvalidHex,
    /// `secret` was rejected by the HMAC implementation (e.g. a key length
    /// the underlying `Hmac<Sha256>` cannot accept).
    InvalidKey,
    /// The recomputed HMAC did not match the header's signature.
    SignatureMismatch,
}

impl std::fmt::Display for WebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebhookError::MissingPrefix => {
                write!(f, "X-Hub-Signature-256 is missing the 'sha256=' prefix")
            }
            WebhookError::InvalidHex => {
                write!(f, "X-Hub-Signature-256 value is not valid hex")
            }
            WebhookError::InvalidKey => write!(f, "invalid HMAC key"),
            WebhookError::SignatureMismatch => {
                write!(f, "X-Hub-Signature-256 does not match the request body")
            }
        }
    }
}

impl std::error::Error for WebhookError {}

/// Verifies a GitHub webhook delivery: `signature_header` is the raw
/// `X-Hub-Signature-256` header value (`"sha256=<hex>"`), `raw_body` is
/// the exact, unparsed request body bytes GitHub signed, and `secret` is
/// the configured `GITHUB_WEBHOOK_SECRET`. Returns `Ok(())` only if the
/// header is well-formed *and* the signature matches; callers (the future
/// webhook route) must reject with 401 before parsing `raw_body` or
/// enqueueing anything on any `Err`.
pub fn verify_signature(
    secret: &[u8],
    signature_header: &str,
    raw_body: &[u8],
) -> Result<(), WebhookError> {
    let hex_sig = signature_header
        .strip_prefix(SHA256_PREFIX)
        .ok_or(WebhookError::MissingPrefix)?;
    let signature = decode_hex(hex_sig)?;

    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret).map_err(|_| WebhookError::InvalidKey)?;
    mac.update(raw_body);
    mac.verify_slice(&signature)
        .map_err(|_| WebhookError::SignatureMismatch)
}

/// Decodes a hex string into bytes. Rejects odd-length input or any
/// non-hex-digit character; never panics.
fn decode_hex(s: &str) -> Result<Vec<u8>, WebhookError> {
    if !s.len().is_multiple_of(2) {
        return Err(WebhookError::InvalidHex);
    }
    let bytes = s.as_bytes();
    let (pairs, _) = bytes.as_chunks::<2>();
    let mut out = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let hi = hex_digit(pair[0]).ok_or(WebhookError::InvalidHex)?;
        let lo = hex_digit(pair[1]).ok_or(WebhookError::InvalidHex)?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"a-realistic-webhook-secret-value";
    const OTHER_SECRET: &[u8] = b"a-different-webhook-secret-value";

    /// A realistic GitHub `push`-shaped payload, not a toy one-liner.
    fn realistic_body() -> Vec<u8> {
        br#"{"ref":"refs/heads/main","before":"0000000000000000000000000000000000000000","after":"6dcb09b5b57875f334f61aebed695e2e4193db5","repository":{"id":123456,"full_name":"acme/widgets","private":false,"owner":{"login":"acme","id":9876}},"pusher":{"name":"octocat","email":"octocat@example.com"},"commits":[{"id":"6dcb09b5b57875f334f61aebed695e2e4193db5","message":"Fix off-by-one in the ingest queue","timestamp":"2026-09-30T12:00:00Z","author":{"name":"Octo Cat","email":"octocat@example.com"}}]}"#
            .to_vec()
    }

    fn sign(secret: &[u8], body: &[u8]) -> Result<String, WebhookError> {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(secret).map_err(|_| WebhookError::InvalidKey)?;
        mac.update(body);
        let tag = mac.finalize().into_bytes();
        let hex: String = tag.iter().map(|b| format!("{b:02x}")).collect();
        Ok(format!("sha256={hex}"))
    }

    #[test]
    fn accepts_a_valid_signature_over_a_realistic_body() -> Result<(), WebhookError> {
        let body = realistic_body();
        let header = sign(SECRET, &body)?;
        assert_eq!(verify_signature(SECRET, &header, &body), Ok(()));
        Ok(())
    }

    #[test]
    fn rejects_a_tampered_body_with_the_original_signature()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = realistic_body();
        let header = sign(SECRET, &body)?;

        let mut tampered = body.clone();
        // Flip one byte deep in the commit message, not at the edges.
        let idx = tampered
            .iter()
            .position(|&b| b == b'o')
            .ok_or("body contains the letter o")?;
        tampered[idx] = b'0';

        assert_eq!(
            verify_signature(SECRET, &header, &tampered),
            Err(WebhookError::SignatureMismatch)
        );
        Ok(())
    }

    #[test]
    fn rejects_a_signature_computed_with_a_different_secret() -> Result<(), WebhookError> {
        let body = realistic_body();
        let header_for_other_secret = sign(OTHER_SECRET, &body)?;
        let header_for_secret = sign(SECRET, &body)?;

        // Prove the two secrets actually produce different signatures for
        // the same body, not just "any non-matching string is rejected".
        assert_ne!(header_for_other_secret, header_for_secret);

        assert_eq!(
            verify_signature(SECRET, &header_for_other_secret, &body),
            Err(WebhookError::SignatureMismatch)
        );
        Ok(())
    }

    #[test]
    fn rejects_a_missing_sha256_prefix() -> Result<(), Box<dyn std::error::Error>> {
        let body = realistic_body();
        // Correct hex digest, but without the required "sha256=" prefix.
        let header = sign(SECRET, &body)?;
        let bare_hex = header
            .strip_prefix(SHA256_PREFIX)
            .ok_or("sign() adds the prefix")?;

        assert_eq!(
            verify_signature(SECRET, bare_hex, &body),
            Err(WebhookError::MissingPrefix)
        );
        Ok(())
    }

    #[test]
    fn rejects_a_non_hex_signature_value() {
        let body = realistic_body();
        assert_eq!(
            verify_signature(SECRET, "sha256=not-hex-at-all-zzz", &body),
            Err(WebhookError::InvalidHex)
        );
    }

    #[test]
    fn accepts_an_empty_body_with_its_correctly_computed_signature() -> Result<(), WebhookError> {
        let body: &[u8] = b"";
        let header = sign(SECRET, body)?;
        assert_eq!(verify_signature(SECRET, &header, body), Ok(()));
        Ok(())
    }
}
