//! Public-key crypto (PKC): X25519 ECDH + AES-256-CCM.
//!
//! Mirrors the upstream Meshtastic `CryptoEngine::encryptCurve25519` /
//! `decryptCurve25519` flow:
//!
//! 1. Each node has a long-lived Curve25519 keypair.
//! 2. For each direct message we perform `shared = my_priv * their_pub`
//!    (X25519 ECDH) and use the 32-byte shared secret as the AES-256-CCM key.
//! 3. The 13-byte CCM nonce packs `packet_id (4) || extra_nonce (4) ||
//!    sender (4) || 0x00` — the upstream layout, not the truncated AES-CTR
//!    layout used by PSK channels.
//! 4. Tag length is 8 bytes (CCM `M=8`), matching upstream.
//!
//! ## Wire format overhead
//!
//! Each PKC frame adds `PKC_OVERHEAD = 12` bytes beyond the plaintext:
//! `[ciphertext N] [CCM tag 8] [extra_nonce 4]`. This matches upstream's
//! `MESHTASTIC_PKC_OVERHEAD`. The receiver identifies PKC packets by channel
//! hash == 0 combined with a stored public key for the sender.

use ccm::{
    Ccm, KeyInit,
    aead::{AeadInPlace, generic_array::GenericArray},
    consts::{U8, U13},
};
use x25519_dalek::{PublicKey, StaticSecret};

/// AES-256-CCM with 13-byte nonce and 8-byte tag (matches upstream).
type Aes256Ccm = Ccm<aes::Aes256, U8, U13>;

/// Length of the CCM authentication tag, in bytes. Appended to ciphertext.
pub const PKC_TAG_LEN: usize = 8;

/// Length of the wire-format `extraNonce` field that travels with the
/// ciphertext so the receiver can reconstruct the same nonce.
pub const PKC_EXTRA_NONCE_LEN: usize = 4;

/// Total overhead per frame beyond the plaintext. Matches upstream
/// `MESHTASTIC_PKC_OVERHEAD`. Wire layout: `[ct] [tag 8B] [nonce 4B]`.
pub const PKC_OVERHEAD: usize = PKC_TAG_LEN + PKC_EXTRA_NONCE_LEN; // 12

#[derive(Debug)]
pub enum PkcError {
    /// Authentication tag mismatch — message tampered with or wrong key.
    BadTag,
    /// Buffer too small to hold ciphertext + tag, or too small for plaintext.
    BadBuffer,
}

/// Construct the 13-byte CCM nonce from packet metadata.
///
/// Layout matches upstream:
/// `packet_id LE (4) || extra_nonce LE (4) || sender LE (4) || 0x00`
pub fn build_pkc_nonce(packet_id: u32, sender: u32, extra_nonce: u32) -> [u8; 13] {
    let mut nonce = [0u8; 13];
    nonce[0..4].copy_from_slice(&packet_id.to_le_bytes());
    nonce[4..8].copy_from_slice(&extra_nonce.to_le_bytes());
    nonce[8..12].copy_from_slice(&sender.to_le_bytes());
    // byte 12 stays zero
    nonce
}

/// Compute the X25519 shared secret used as the CCM key.
pub fn derive_shared_key(my_secret: &StaticSecret, peer_public: &PublicKey) -> [u8; 32] {
    my_secret.diffie_hellman(peer_public).to_bytes()
}

/// Encrypt `plaintext` into `out_buf`, producing the upstream wire format:
/// `[ciphertext N] [CCM tag 8B] [extra_nonce 4B]` = `N + PKC_OVERHEAD` bytes.
///
/// `out_buf` must be at least `plaintext.len() + PKC_OVERHEAD` bytes long.
/// Returns the number of bytes written.
pub fn encrypt_pkc(
    shared_key: &[u8; 32],
    packet_id: u32,
    sender: u32,
    extra_nonce: u32,
    plaintext: &[u8],
    out_buf: &mut [u8],
) -> Result<usize, PkcError> {
    let needed = plaintext.len() + PKC_OVERHEAD;
    if out_buf.len() < needed {
        return Err(PkcError::BadBuffer);
    }

    let nonce = build_pkc_nonce(packet_id, sender, extra_nonce);
    let cipher = Aes256Ccm::new(GenericArray::from_slice(shared_key));

    // Write plaintext then encrypt in-place; the CCM tag goes in bytes [N..N+8].
    out_buf[..plaintext.len()].copy_from_slice(plaintext);
    let ct_end = plaintext.len() + PKC_TAG_LEN;
    let (body, tag_slot) = out_buf[..ct_end].split_at_mut(plaintext.len());
    let tag = cipher
        .encrypt_in_place_detached(GenericArray::from_slice(&nonce), b"", body)
        .map_err(|_| PkcError::BadBuffer)?;
    tag_slot.copy_from_slice(tag.as_slice());
    // Append extra_nonce in bytes [N+8..N+12] — upstream reads it from there.
    out_buf[ct_end..ct_end + PKC_EXTRA_NONCE_LEN].copy_from_slice(&extra_nonce.to_le_bytes());
    Ok(needed)
}

/// Decrypt a PKC frame in upstream wire format:
/// `[ciphertext N] [CCM tag 8B] [extra_nonce 4B]` = `N + PKC_OVERHEAD` bytes.
///
/// The extra_nonce is extracted from the last 4 bytes and used to reconstruct
/// the CCM nonce. On success returns the plaintext length written into
/// `out_buf`. On authentication failure `BadTag` is returned.
pub fn decrypt_pkc(
    shared_key: &[u8; 32],
    packet_id: u32,
    sender: u32,
    wire: &[u8], // ciphertext + tag + extra_nonce (total = plaintext_len + 12)
    out_buf: &mut [u8],
) -> Result<usize, PkcError> {
    if wire.len() < PKC_OVERHEAD {
        return Err(PkcError::BadBuffer);
    }
    let body_len = wire.len() - PKC_OVERHEAD;
    if out_buf.len() < body_len {
        return Err(PkcError::BadBuffer);
    }

    // Extract extra_nonce from the last 4 bytes.
    let extra_nonce = u32::from_le_bytes([
        wire[wire.len() - 4],
        wire[wire.len() - 3],
        wire[wire.len() - 2],
        wire[wire.len() - 1],
    ]);

    let tag_start = body_len;
    let (ct, tag) = wire[..tag_start + PKC_TAG_LEN].split_at(body_len);
    out_buf[..body_len].copy_from_slice(ct);

    let nonce = build_pkc_nonce(packet_id, sender, extra_nonce);
    let cipher = Aes256Ccm::new(GenericArray::from_slice(shared_key));
    cipher
        .decrypt_in_place_detached(
            GenericArray::from_slice(&nonce),
            b"",
            &mut out_buf[..body_len],
            GenericArray::from_slice(tag),
        )
        .map_err(|_| PkcError::BadTag)?;
    Ok(body_len)
}

/// Construct an X25519 keypair from a 32-byte seed.
///
/// First boot: feed 32 bytes from the hardware RNG, persist both halves to
/// NVS, and reuse them for the lifetime of the device. Subsequent boots:
/// reload from NVS and skip generation.
pub fn keypair_from_seed(seed: [u8; 32]) -> (StaticSecret, PublicKey) {
    let secret = StaticSecret::from(seed);
    let public = PublicKey::from(&secret);
    (secret, public)
}
