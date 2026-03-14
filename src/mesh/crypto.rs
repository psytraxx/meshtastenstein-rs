//! Meshtastic AES-CTR encryption/decryption
//!
//! Nonce format (16 bytes for AES-CTR):
//! ```text
//! [packet_id as u64 LE (8 bytes)][sender u32 LE (4 bytes)][extra_nonce u32 LE = 0 (4 bytes)]
//! ```

use aes::Aes128;
use aes::Aes256;
use aes::cipher::{KeyIvInit, StreamCipher};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;
type Aes256Ctr = ctr::Ctr128BE<Aes256>;

/// Build the 16-byte CTR nonce from packet fields
pub fn build_nonce(packet_id: u32, sender: u32) -> [u8; 16] {
    let mut nonce = [0u8; 16];
    // packet_id as u64 LE in first 8 bytes
    nonce[0..4].copy_from_slice(&packet_id.to_le_bytes());
    // bytes 4..8 are zero (upper 32 bits of u64)
    // sender as u32 LE in bytes 8..12
    nonce[8..12].copy_from_slice(&sender.to_le_bytes());
    // bytes 12..16 are zero (extra_nonce)
    nonce
}

/// Error type for crypto operations
#[derive(Debug)]
pub struct CryptoError;

/// Encrypt or decrypt data in-place using AES-128-CTR or AES-256-CTR.
/// CTR mode is symmetric, so encrypt == decrypt.
pub fn crypt_packet(
    key: &[u8],
    packet_id: u32,
    sender: u32,
    data: &mut [u8],
) -> Result<(), CryptoError> {
    if data.is_empty() {
        return Ok(());
    }

    let nonce = build_nonce(packet_id, sender);

    match key.len() {
        16 => {
            let mut cipher = Aes128Ctr::new(key.into(), &nonce.into());
            cipher.apply_keystream(data);
            Ok(())
        }
        32 => {
            let mut cipher = Aes256Ctr::new(key.into(), &nonce.into());
            cipher.apply_keystream(data);
            Ok(())
        }
        _ => Err(CryptoError),
    }
}
