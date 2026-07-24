//! Policy body authentication: legacy domain-separated MAC and ed25519.
//!
//! Signature wire formats:
//! - MAC: raw 64-char hex (legacy)
//! - Public key: `ed25519:` + 128-char hex signature of the domain-separated digest

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

const ED25519_PREFIX: &str = "ed25519:";

pub fn mac_hex(key: &[u8], body_digest: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"summoner.trusted-policy.mac.v1\0");
    let key_len = (key.len() as u64).to_le_bytes();
    hash.update(key_len);
    hash.update(key);
    hash.update([0]);
    hash.update(body_digest.as_bytes());
    hex(hash.finalize())
}

/// Sign a policy body digest with a 32-byte ed25519 seed (hex or raw 32 bytes).
pub fn sign_ed25519(seed: &[u8], body_digest: &str) -> Result<String> {
    let seed = decode_seed(seed)?;
    let signing = SigningKey::from_bytes(&seed);
    let msg = sign_message(body_digest);
    let sig = signing.sign(&msg);
    Ok(format!("{ED25519_PREFIX}{}", hex(sig.to_bytes())))
}

/// Verify an ed25519 policy signature against a 32-byte public key.
pub fn verify_ed25519(pubkey: &[u8], body_digest: &str, signature: &str) -> Result<bool> {
    let Some(hex_sig) = signature.strip_prefix(ED25519_PREFIX) else {
        bail!("not an ed25519 policy signature");
    };
    let sig_bytes = decode_hex(hex_sig).context("decoding ed25519 signature")?;
    if sig_bytes.len() != 64 {
        bail!("ed25519 signature must be 64 bytes");
    }
    let pk_bytes = decode_pubkey(pubkey)?;
    let verifying = VerifyingKey::from_bytes(&pk_bytes).context("invalid ed25519 public key")?;
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sig_arr);
    let msg = sign_message(body_digest);
    Ok(verifying.verify(&msg, &signature).is_ok())
}

pub fn is_ed25519_signature(signature: &str) -> bool {
    signature.starts_with(ED25519_PREFIX)
}

pub fn generate_keypair() -> Result<(String, String)> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).context("generating ed25519 seed")?;
    let signing = SigningKey::from_bytes(&seed);
    let verifying = signing.verifying_key();
    Ok((hex(seed), hex(verifying.to_bytes())))
}

fn sign_message(body_digest: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(40 + body_digest.len());
    msg.extend_from_slice(b"summoner.trusted-policy.ed25519.v1\0");
    msg.extend_from_slice(body_digest.as_bytes());
    msg
}

fn decode_seed(seed: &[u8]) -> Result<[u8; 32]> {
    if seed.len() == 32 {
        let mut out = [0u8; 32];
        out.copy_from_slice(seed);
        return Ok(out);
    }
    let text = std::str::from_utf8(seed).context("signing key is not utf-8 or 32 raw bytes")?;
    let bytes = decode_hex(text.trim()).context("decoding ed25519 signing seed")?;
    if bytes.len() != 32 {
        bail!("ed25519 signing seed must be 32 bytes (64 hex chars)");
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn decode_pubkey(pubkey: &[u8]) -> Result<[u8; 32]> {
    if pubkey.len() == 32 {
        let mut out = [0u8; 32];
        out.copy_from_slice(pubkey);
        return Ok(out);
    }
    let text = std::str::from_utf8(pubkey).context("public key is not utf-8 or 32 raw bytes")?;
    let bytes = decode_hex(text.trim()).context("decoding ed25519 public key")?;
    if bytes.len() != 32 {
        bail!("ed25519 public key must be 32 bytes (64 hex chars)");
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        bail!("hex length must be even");
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).context("invalid hex"))
        .collect()
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed25519_roundtrip_sign_verify() {
        let (seed, pubkey) = generate_keypair().unwrap();
        let digest = "a".repeat(64);
        let sig = sign_ed25519(seed.as_bytes(), &digest).unwrap();
        assert!(is_ed25519_signature(&sig));
        assert!(verify_ed25519(pubkey.as_bytes(), &digest, &sig).unwrap());
        assert!(!verify_ed25519(pubkey.as_bytes(), &"b".repeat(64), &sig).unwrap());
    }

    #[test]
    fn mac_is_stable() {
        assert_eq!(mac_hex(b"key", "digest"), mac_hex(b"key", "digest"));
        assert_ne!(mac_hex(b"key", "digest"), mac_hex(b"other", "digest"));
    }
}
