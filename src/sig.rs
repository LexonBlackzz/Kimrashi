//! The release signature, shared by the updater (verifies) and `tools/sign.rs` (signs).
//!
//! An ed25519 signature over a message naming the version and the exe's SHA-256, so a
//! valid old release cannot be presented as a newer one. The private key stays on the
//! release machine; only `PUBLIC_KEY` is in the source.

use ed25519_dalek::{Signature, VerifyingKey};

/// Made by `kimrashi-sign keygen`; the matching private key is not in the repository.
pub const PUBLIC_KEY: [u8; 32] = [240, 206, 9, 148, 225, 158, 57, 74, 50, 161, 168, 138, 25, 46, 211, 157, 124, 236, 145, 158, 201, 180, 36, 241, 180, 20, 22, 102, 157, 156, 56, 138];

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.trim();
    if text.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn message(version: [u64; 3], sha256: &[u8; 32]) -> Vec<u8> {
    format!("kimrashi {}.{}.{}\n{}", version[0], version[1], version[2], hex(sha256)).into_bytes()
}

/// `signature` is the 128 hex characters `kimrashi-sign sign` writes.
pub fn verify(key: &[u8; 32], version: [u64; 3], sha256: &[u8; 32], signature: &str) -> Result<(), String> {
    let signature = Signature::from_bytes(&unhex::<64>(signature).ok_or("the signature file is malformed")?);
    VerifyingKey::from_bytes(key)
        .map_err(|_| "the program has no valid release key built in".to_string())?
        .verify_strict(&message(version, sha256), &signature)
        .map_err(|_| "the signature does not match: this was not signed with the Kimrashi release key".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key() -> SigningKey { SigningKey::from_bytes(&[7u8; 32]) }

    #[test]
    fn a_signature_covers_version_and_hash() {
        let (public, hash) = (key().verifying_key().to_bytes(), [9u8; 32]);
        let signature = hex(&key().sign(&message([0, 4, 1], &hash)).to_bytes());
        assert_eq!(signature.len(), 128);
        assert!(verify(&public, [0, 4, 1], &hash, &signature).is_ok());
        assert!(verify(&public, [0, 4, 1], &hash, &format!("{signature}\n")).is_ok());
        assert!(verify(&public, [0, 4, 2], &hash, &signature).is_err(), "another version");
        assert!(verify(&public, [0, 4, 1], &[8u8; 32], &signature).is_err(), "another file");
        let other = SigningKey::from_bytes(&[8u8; 32]).verifying_key().to_bytes();
        assert!(verify(&other, [0, 4, 1], &hash, &signature).is_err(), "another key");
        assert!(verify(&public, [0, 4, 1], &hash, "zz").is_err(), "malformed");
    }
}
