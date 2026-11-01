//! The ids of proposals: random version 4 UUIDs in lowercase. The id is the file name, so only this
//! shape is ever used to build a path.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::hex;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A new id. The randomness is the operating system's, through the keys of the standard library's
/// hasher, mixed with the time, the process and a counter: an id is a name, never a secret.
pub fn random_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let count = COUNTER.fetch_add(1, Ordering::SeqCst);

    let mut seed = Sha256::new();

    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u64(count);
        hasher.write_u32(std::process::id());
        seed.update(hasher.finish().to_le_bytes());
    }

    seed.update(nanos.to_le_bytes());

    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&seed.finalize()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let digits = hex(&bytes);

    format!(
        "{}-{}-{}-{}-{}",
        &digits[..8],
        &digits[8..12],
        &digits[12..16],
        &digits[16..20],
        &digits[20..]
    )
}

/// True for a lowercase UUID: `8-4-4-4-12` lowercase hex digits.
pub fn is_held_id(raw: &str) -> bool {
    let groups: Vec<&str> = raw.split('-').collect();
    let lengths = [8, 4, 4, 4, 12];

    groups.len() == lengths.len()
        && groups.iter().zip(lengths).all(|(group, length)| {
            group.len() == length
                && group
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}
