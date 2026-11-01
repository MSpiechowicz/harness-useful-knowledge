//! SHA-256 as lowercase hex, for the hashes a document and its chunks are known by.

use std::fmt::Write;

use sha2::{Digest, Sha256};

/// The SHA-256 of `bytes`, 64 lowercase hex characters.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);

    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }

    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
