//! Minimal ULID (Universally Unique Lexicographically Sortable Identifier)
//! generation: a 48-bit millisecond timestamp and 80 bits of randomness,
//! Crockford base32 encoded into a 26-character string.
//!
//! There is no `ulid` crate in this workspace's dependency tree yet, and the
//! one on crates.io pulls in `rand`, which needs its own `wasm32` random
//! source story on top of the one we already need. `RunCoordinator` already
//! requires a CSPRNG for ULIDs' random component, so `getrandom` with the
//! `js` feature (which calls the Workers runtime's `crypto.getRandomValues`,
//! the same path browsers use) is the minimal dependency; the ~30 lines of
//! Crockford base32 encoding on top are simple enough to own directly rather
//! than add a second crate for them.

const ENCODING: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Encodes a 48-bit millisecond timestamp and 80 bits of randomness as a
/// 26-character Crockford base32 ULID string. Pure function, independent of
/// any randomness or time source, so it is unit-testable natively.
pub fn encode(timestamp_ms: u64, random: [u8; 10]) -> String {
    let mut out = String::with_capacity(26);
    // 48-bit timestamp -> 10 base32 characters (50 bits of capacity; the top
    // 2 bits are always zero since a millisecond timestamp never exceeds
    // 2^48).
    for i in (0..10).rev() {
        let shift = i * 5;
        let idx = ((timestamp_ms >> shift) & 0x1f) as usize;
        out.push(ENCODING[idx] as char);
    }
    // 80-bit randomness -> 16 base32 characters, exact fit.
    let mut bits: u128 = 0;
    for b in random {
        bits = (bits << 8) | u128::from(b);
    }
    for i in (0..16).rev() {
        let shift = i * 5;
        let idx = ((bits >> shift) & 0x1f) as usize;
        out.push(ENCODING[idx] as char);
    }
    out
}

/// Generates a fresh ULID from the given millisecond timestamp and the
/// platform's secure RNG. Fails only if the RNG is unavailable; the error is
/// propagated rather than unwrapped, per this package's no-`unwrap`/`expect`/
/// `panic` rule.
pub fn generate(timestamp_ms: u64) -> Result<String, getrandom::Error> {
    let mut random = [0u8; 10];
    getrandom::getrandom(&mut random)?;
    Ok(encode(timestamp_ms, random))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_to_26_crockford_characters() {
        let id = encode(0, [0; 10]);
        assert_eq!(id.len(), 26);
        assert_eq!(id, "00000000000000000000000000"[..26]);
        assert!(id.bytes().all(|b| ENCODING.contains(&b)));
    }

    #[test]
    fn timestamp_component_is_lexicographically_sortable() {
        let earlier = encode(1_000, [0xff; 10]);
        let later = encode(2_000, [0; 10]);
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn distinct_randomness_gives_distinct_ids_for_same_timestamp() {
        let a = encode(42, [1; 10]);
        let b = encode(42, [2; 10]);
        assert_ne!(a, b);
        assert_eq!(&a[..10], &b[..10], "timestamp prefix must match");
    }

    #[test]
    fn generate_produces_distinct_valid_ulids() -> Result<(), getrandom::Error> {
        let a = generate(123)?;
        let b = generate(123)?;
        assert_eq!(a.len(), 26);
        assert_ne!(a, b);
        Ok(())
    }
}
