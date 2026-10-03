//! Pure redaction of secret-shaped content, applied to every failing-test
//! string (`message`, `stack_trace` lines, `system_out`, `system_err`)
//! **before** it is allowed into a context payload or fingerprinted —
//! `docs/design/ai.md`'s pipeline diagram's
//! `RED[Redact + exclude_paths + fingerprint]` step, and its "Goals"
//! section: "Treat every byte of repo, log, and PR content as untrusted
//! input to the model."
//!
//! # Relationship to `ai.exclude_paths`
//!
//! `ai.exclude_paths` (`cloud-ci-core::settings`'s `validate_ai`,
//! `docs/design/settings.md`'s field reference) is a *path-based*
//! exclusion: whole files/diff hunks under a glob are never read into the
//! context at all. This module is a different, broader mechanism:
//! *content-based* masking of secret-shaped substrings inside text that
//! **is** included (an assertion message, a stack trace line, captured
//! stdout/stderr) — a repo can have no `exclude_paths` configured at all
//! and still leak a token value printed by a failing test. The two
//! mechanisms are complementary, not redundant; this module does not
//! implement `exclude_paths` (that stays the documented, separate,
//! not-yet-wired "Settings integration" gap `ai_queue.rs`'s module docs
//! already name — this module has nothing to do with *which files* are
//! read, only with *what the text of an already-selected input contains*).
//!
//! # Where this runs (confirmed before fingerprinting)
//!
//! [`crate::ai_queue::assemble_failure_context`] redacts every
//! [`crate::ai_queue::FailingTestInput`] field through [`redact`] as its
//! very first step, before computing
//! [`crate::ai_insight::failure_fingerprint`] or anything else — so the
//! fingerprint itself is computed over already-redacted text. This is
//! deliberate and is the *correct* dedup behavior per ai.md's own
//! fingerprinting intent: two reports whose only difference is an
//! embedded secret's value (e.g. a leaked token that rotates between
//! runs) still fingerprint identically once redacted, and are
//! deduplicated as the same failure rather than treated as distinct ones.
//! [`crate::ai_insight::failure_fingerprint`]/`normalize` themselves still
//! take plain strings and do not redact — redaction is the caller's job,
//! upstream of fingerprinting, exactly as the pipeline diagram orders it.
//!
//! # Patterns covered, and why
//!
//! Hand-rolled character scanning, no `regex` dependency — same
//! discipline `ai_insight.rs`'s `normalize()` already established for
//! this crate (see that module's doc comment), for the same reasons: no
//! regex crate in this crate's dependency graph today, the patterns below
//! are all simple enough to scan directly, and a hand-rolled scanner
//! avoids a new dependency plus a regex engine's binary-size cost in a
//! wasm target.
//!
//! - **GitHub tokens** (`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_` followed by
//!   36+ alphanumeric characters) — this codebase's own domain: cloud-ci
//!   itself is a GitHub App, and a failing test's captured output
//!   plausibly includes a leaked installation/PAT-shaped token from a
//!   misconfigured fixture or a credential accidentally printed by the
//!   code under test.
//! - **AWS-style access key ids** (`AKIA` followed by exactly 16
//!   uppercase-letter-or-digit characters) — a common credential shape
//!   that shows up in CI logs for any repo that touches AWS.
//! - **`Authorization: Bearer <token>` / `Authorization: Basic <...>`
//!   header-shaped lines** — HTTP client/server logs and captured
//!   stdout/stderr commonly print the exact request/response header line
//!   verbatim; the value after the scheme is masked, the scheme itself is
//!   kept (useful signal, never secret-shaped on its own).
//! - **`.env`-style `KEY=value` lines** where `KEY` (case-insensitively)
//!   ends in `_TOKEN`, `_SECRET`, `_KEY`, or `_PASSWORD` — the common
//!   convention this codebase and most CI systems already use for secret
//!   environment variable names; a test runner's captured output commonly
//!   includes a dumped environment or a `.env` file's contents verbatim
//!   on failure.
//! - **Generic high-entropy base64-shaped blobs**, length 32+, confined
//!   to the base64 character set (`A-Za-z0-9+/`; trailing `=` padding is
//!   not part of the scanned token — see [`is_token_char`]'s doc
//!   comment), and containing at
//!   least one uppercase letter, one lowercase letter, and one digit —
//!   catches credential-shaped strings that don't match any of the named
//!   formats above (API keys, signing secrets, session tokens). The
//!   mixed-case-and-digit requirement is deliberate: it excludes a plain
//!   lowercase-hex git commit sha or content hash (common, legitimate,
//!   non-secret content in a stack trace or diff context) from being
//!   masked, while still catching the mixed-case base64 shape most
//!   generated secrets actually have.
//!
//! # Residual, inherent limitation
//!
//! Pattern-based redaction only catches secrets that match one of the
//! shapes above. A secret with no structural signature at all (an
//! ordinary English word used as a password, a short numeric PIN, a
//! proprietary internal format this module has never seen) is not and
//! cannot be caught by this or any purely structural scanner — this is an
//! honest, inherent limitation of pattern-based redaction, not a solved
//! problem. `exclude_paths` (see above) remains the mechanism for a
//! deployer who knows exactly which files/paths carry such secrets to
//! keep them out of the context entirely, upstream of this module.

/// Minimum length, in bytes, for a base64-charset run to be considered a
/// high-entropy credential-shaped blob (see module docs). Chosen well
/// above typical English words or short identifiers while comfortably
/// below typical generated secret lengths (session tokens, signing keys,
/// API keys are almost universally 32+ characters).
const HIGH_ENTROPY_MIN_LEN: usize = 32;

/// `KEY` suffixes (case-insensitive) that mark a `.env`-style `KEY=value`
/// line's value as secret-shaped (module docs).
const SECRET_KEY_SUFFIXES: &[&str] = &["_TOKEN", "_SECRET", "_KEY", "_PASSWORD"];

/// GitHub token prefixes this module recognizes (module docs): personal
/// access token, OAuth, user-to-server, server-to-server/installation,
/// and refresh token, per GitHub's own documented prefix scheme.
const GITHUB_TOKEN_PREFIXES: &[&str] = &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"];

/// Redacts every secret-shaped substring in `input` (module docs' pattern
/// list), line by line, preserving everything else — including original
/// whitespace/punctuation around a redacted token — exactly as given.
/// Line splitting/rejoining collapses more than one trailing newline to
/// exactly one; every other byte of non-secret-shaped input is returned
/// unmodified.
pub fn redact(input: &str) -> String {
    let had_trailing_newline = input.ends_with('\n');
    let mut out = input
        .lines()
        .map(redact_line)
        .collect::<Vec<_>>()
        .join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    out
}

fn redact_line(line: &str) -> String {
    if let Some(masked) = redact_env_style_line(line) {
        return masked;
    }
    if let Some(masked) = redact_auth_header_line(line) {
        return masked;
    }
    redact_tokens_in_line(line)
}

/// A `.env`-style `[export ]KEY=value` line whose `KEY` ends (case
/// -insensitively) in one of [`SECRET_KEY_SUFFIXES`] — the whole value is
/// replaced with `<redacted>`, `KEY=` (and any leading whitespace/
/// `export ` prefix) kept verbatim. `None` if `line` is not of this
/// shape, or `KEY` doesn't match a secret-sounding suffix.
fn redact_env_style_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let leading_ws = &line[..line.len() - trimmed.len()];
    let (prefix, rest) = match trimmed.strip_prefix("export ") {
        Some(r) => ("export ", r),
        None => ("", trimmed),
    };
    let eq_pos = rest.find('=')?;
    let key = &rest[..eq_pos];
    let mut key_chars = key.chars();
    let first = key_chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let key_upper = key.to_ascii_uppercase();
    if !SECRET_KEY_SUFFIXES.iter().any(|s| key_upper.ends_with(s)) {
        return None;
    }
    Some(format!("{leading_ws}{prefix}{key}=<redacted>"))
}

/// An `Authorization: Bearer <token>` / `Authorization: Basic <...>`
/// header-shaped line — the scheme is kept, the credential value after it
/// is replaced with `<redacted>`. `None` if `line` is not of this shape.
fn redact_auth_header_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let leading_ws = &line[..line.len() - trimmed.len()];
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("authorization:") {
        return None;
    }
    let after_colon = &trimmed["authorization:".len()..];
    let rest_trimmed = after_colon.trim_start();
    let gap = &after_colon[..after_colon.len() - rest_trimmed.len()];
    let lower_rest = rest_trimmed.to_ascii_lowercase();
    let scheme = if lower_rest.starts_with("bearer ") {
        "Bearer"
    } else if lower_rest.starts_with("basic ") {
        "Basic"
    } else {
        return None;
    };
    Some(format!(
        "{leading_ws}Authorization:{gap}{scheme} <redacted>"
    ))
}

/// A token character for [`redact_tokens_in_line`]'s scanner: covers
/// every character that can appear inside a GitHub token, an AWS key, or
/// a base64-charset blob, so a single maximal run captures the whole
/// candidate token regardless of which pattern it ends up matching.
/// Deliberately excludes `=`: unlike `+`/`/`, `=` also commonly appears
/// as a `KEY=value` separator in arbitrary (non-secret-suffixed) text,
/// and including it would merge an unrelated `KEY` and `value` either
/// side of it into one run, hiding an `AKIA...`/`ghp_...`-shaped value
/// from its own prefix check. A high-entropy blob's own trailing `=`
/// padding is simply left outside the matched run and passed through
/// unredacted — an acceptable, documented trade-off since the padding
/// itself carries no information.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/')
}

/// Scans `line` for maximal runs of [`is_token_char`] characters and
/// replaces any run that [`is_secret_shaped`] with `<redacted>`,
/// preserving every other character (including whitespace and
/// punctuation between tokens) exactly as given — this is what keeps
/// ordinary non-secret text byte-identical through [`redact`].
fn redact_tokens_in_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if is_token_char(chars[i]) {
            let start = i;
            while i < chars.len() && is_token_char(chars[i]) {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            if is_secret_shaped(&token) {
                out.push_str("<redacted>");
            } else {
                out.push_str(&token);
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn is_secret_shaped(token: &str) -> bool {
    is_github_token(token) || is_aws_access_key(token) || is_high_entropy_blob(token)
}

fn is_github_token(token: &str) -> bool {
    for prefix in GITHUB_TOKEN_PREFIXES {
        if let Some(rest) = token.strip_prefix(prefix)
            && rest.len() >= 36
            && rest.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return true;
        }
    }
    false
}

fn is_aws_access_key(token: &str) -> bool {
    match token.strip_prefix("AKIA") {
        Some(rest) => {
            rest.len() == 16
                && rest
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        }
        None => false,
    }
}

fn is_high_entropy_blob(token: &str) -> bool {
    if token.len() < HIGH_ENTROPY_MIN_LEN {
        return false;
    }
    let base64_charset = token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'));
    if !base64_charset {
        return false;
    }
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    has_upper && has_lower && has_digit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_github_token() {
        let input = "fetch failed: token ghp_abcdEFGH0123456789abcdEFGH0123456789 was rejected";
        let out = redact(input);
        assert!(!out.contains("ghp_abcdEFGH0123456789abcdEFGH0123456789"));
        assert!(out.contains("<redacted>"));
        assert!(out.contains("fetch failed: token"));
        assert!(out.contains("was rejected"));
    }

    #[test]
    fn redacts_aws_access_key() {
        let input = "AWS_ACCESS_KEY_ID=AKIAABCDEFGHIJKLMNOP in env dump";
        let out = redact(input);
        assert!(!out.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(out.contains("<redacted>"));
    }

    #[test]
    fn redacts_bearer_authorization_header() {
        let input = "Authorization: Bearer sk_live_abcdefghijklmnopqrstuvwxyz0123456789";
        let out = redact(input);
        assert_eq!(out, "Authorization: Bearer <redacted>");
    }

    #[test]
    fn redacts_basic_authorization_header() {
        let input = "  Authorization: Basic dXNlcjpwYXNzd29yZA==";
        let out = redact(input);
        assert_eq!(out, "  Authorization: Basic <redacted>");
    }

    #[test]
    fn redacts_env_style_secret_line() {
        let input = "DATABASE_PASSWORD=hunter2correcthorsebatterystaple";
        let out = redact(input);
        assert_eq!(out, "DATABASE_PASSWORD=<redacted>");
    }

    #[test]
    fn redacts_env_style_secret_line_with_export_prefix() {
        let input = "export API_SECRET=abc123";
        let out = redact(input);
        assert_eq!(out, "export API_SECRET=<redacted>");
    }

    #[test]
    fn redacts_generic_high_entropy_blob() {
        let input = "signed with sig=aB3xQ9mK2pL7vN4tR8wZ1yU6cE0fH5jD2sA9bC3dE7fG==";
        let out = redact(input);
        assert!(!out.contains("aB3xQ9mK2pL7vN4tR8wZ1yU6cE0fH5jD2sA9bC3dE7fG"));
        assert!(out.contains("<redacted>"));
    }

    #[test]
    fn does_not_mangle_ordinary_assertion_message() {
        let input = "expected 42 but got 17, user.name was \"Alice\" not \"Bob\"";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn does_not_mangle_ordinary_stack_trace() {
        let input =
            "    at createUser (src/user.ts:44:12)\n    at async Test.run (test/user.test.ts:10:3)";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn does_not_redact_plain_git_sha() {
        // A lowercase-hex-only git commit sha must not be treated as a
        // high-entropy blob (module docs: excludes plain hex hashes).
        let input = "failed at commit 4b1a9c2d7e8f0123456789abcdef0123456789ab";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn does_not_redact_ordinary_env_line_without_secret_suffix() {
        let input = "NODE_ENV=production";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn redaction_runs_before_fingerprinting_enables_dedup() {
        // Same message shape, different embedded secret value — after
        // redaction both become byte-identical, which is the correct
        // dedup behavior per ai.md's fingerprinting intent.
        let a = "Authorization: Bearer aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let b = "Authorization: Bearer bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert_eq!(redact(a), redact(b));
    }
}
