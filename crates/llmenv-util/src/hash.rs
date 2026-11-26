//! Short stable keys, for file names and for sent-record ledgers.

use sha2::{Digest, Sha256};

/// Hex characters kept from the SHA-256 digest; 64 bits is enough to key one session.
pub const HASH_HEX_CHARS: usize = 16;

/// The first [`HASH_HEX_CHARS`] lowercase hex characters of the SHA-256 of `bytes`.
#[must_use]
pub fn hash_prefix(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(HASH_HEX_CHARS)
        .collect()
}
