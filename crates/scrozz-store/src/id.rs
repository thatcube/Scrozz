//! Capture identifiers.
//!
//! IDs are **lexicographically ordered by creation time**. That single property
//! does a lot of work downstream: history pages with a plain `ORDER BY id`,
//! retention's "oldest first" needs no secondary sort to break ties inside a
//! millisecond, and two processes inserting concurrently (decision D11 has the
//! GUI and the CLI running at once) never produce interleaved-looking history.

use std::{
    hash::{BuildHasher as _, Hasher as _, RandomState},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::CaptureId;

/// Crockford base32 — no `I`, `L`, `O` or `U`, so an ID read aloud or copied out
/// of a log cannot be transcribed into a different one.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Number of characters in a generated identifier.
pub const ID_LEN: usize = 26;

/// Distinguishes IDs minted in the same millisecond by the same process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mints a fresh, time-sortable capture identifier.
#[must_use]
pub fn new_capture_id() -> CaptureId {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
        & 0xffff_ffff_ffff;
    CaptureId(encode(millis, entropy()))
}

/// Mints an identifier for a specific instant, used by tests and by import.
#[must_use]
pub fn capture_id_at(unix_millis: i64) -> CaptureId {
    let millis = u64::try_from(unix_millis).unwrap_or(0) & 0xffff_ffff_ffff;
    CaptureId(encode(millis, entropy()))
}

/// Whether `candidate` looks like an identifier this crate minted.
///
/// Identifiers are used to build sidecar filenames, so anything that could walk
/// out of the documents directory has to be refused before it reaches a path.
#[must_use]
pub fn is_valid_id(candidate: &str) -> bool {
    candidate.len() == ID_LEN
        && candidate
            .bytes()
            .all(|b| ALPHABET.contains(&b.to_ascii_uppercase()))
}

/// 80 bits of per-ID randomness.
///
/// `RandomState` is seeded from the OS at first use, so hashing a
/// monotonically-changing value through a fresh one yields bits an attacker
/// cannot predict and, far more importantly here, that two processes starting
/// simultaneously do not share.
fn entropy() -> u128 {
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());

    let mut low = RandomState::new().build_hasher();
    low.write_u64(counter);
    low.write_u32(nanos);
    let mut high = RandomState::new().build_hasher();
    high.write_u64(low.finish());

    (u128::from(high.finish()) << 64 | u128::from(low.finish())) & ((1 << 80) - 1)
}

/// 48 bits of timestamp then 80 bits of randomness, base32 in that order, so
/// byte order and time order agree.
fn encode(millis: u64, random: u128) -> String {
    let value = (u128::from(millis) << 80) | random;
    let mut out = [0u8; ID_LEN];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = (ID_LEN - 1 - i) * 5;
        *slot = ALPHABET[((value >> shift) & 0x1f) as usize];
    }
    String::from_utf8(out.to_vec()).expect("base32 alphabet is ASCII")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn ids_have_the_documented_shape() {
        let id = new_capture_id();
        assert_eq!(id.0.len(), ID_LEN);
        assert!(is_valid_id(&id.0), "{} should validate", id.0);
    }

    #[test]
    fn ids_are_unique_under_a_tight_loop() {
        // The interesting case: minted faster than the clock ticks, so only the
        // counter and the entropy separate them.
        let ids: HashSet<String> = (0..10_000).map(|_| new_capture_id().0).collect();
        assert_eq!(ids.len(), 10_000, "identifiers collided");
    }

    #[test]
    fn later_ids_sort_after_earlier_ones() {
        let early = capture_id_at(1_000_000_000_000);
        let late = capture_id_at(1_700_000_000_000);
        assert!(early.0 < late.0, "{} should sort before {}", early.0, late.0);
    }

    #[test]
    fn sorting_ids_recovers_chronological_order() {
        let mut ids: Vec<String> = [5i64, 1, 4, 2, 3]
            .into_iter()
            .map(|n| capture_id_at(1_700_000_000_000 + n * 1000).0)
            .collect();
        let expected: Vec<String> = {
            let mut sorted = ids.clone();
            sorted.sort();
            sorted
        };
        ids.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn rejects_identifiers_that_could_escape_the_documents_directory() {
        assert!(!is_valid_id("../../../etc/passwd"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("SHORT"));
        // Crockford excludes these four to keep transcription unambiguous.
        assert!(!is_valid_id(&"I".repeat(ID_LEN)));
        assert!(!is_valid_id(&"U".repeat(ID_LEN)));
    }
}
