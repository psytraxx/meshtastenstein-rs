//! Meshtastic AES-CTR encryption/decryption
//!
//! Nonce format (16 bytes for AES-CTR):
//! ```text
//! [packet_id as u64 LE (8 bytes)][sender u32 LE (4 bytes)][extra_nonce u32 LE = 0 (4 bytes)]
//! ```

use aes::{
    Aes128, Aes256,
    cipher::{KeyIvInit, StreamCipher},
};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;
type Aes256Ctr = ctr::Ctr128BE<Aes256>;

/// Build the 16-byte CTR nonce from packet fields.
///
/// Matches the upstream Meshtastic C++ firmware (`CryptoEngine::encryptPacket`):
/// the 32-bit wire `packet_id` is widened to `u64` and written as the first
/// 8 little-endian bytes of the nonce. Writing only 4 bytes previously left
/// bytes 4..8 as zero — compatible by accident with `packet_id < 2^32` but
/// not with the documented format.
pub fn build_nonce(packet_id: u32, sender: u32) -> [u8; 16] {
    let mut nonce = [0u8; 16];
    // packet_id widened to u64 LE in bytes 0..8 (upstream: `*(uint64_t*)nonce = packetId`)
    nonce[0..8].copy_from_slice(&(packet_id as u64).to_le_bytes());
    // sender as u32 LE in bytes 8..12
    nonce[8..12].copy_from_slice(&sender.to_le_bytes());
    // bytes 12..16 remain zero (extra_nonce)
    nonce
}

/// Error type for crypto operations
#[derive(Debug)]
pub struct CryptoError;

/// Copy a PSK slice (up to 32 bytes) into a fixed-size buffer suitable for `crypt_packet`.
///
/// Returns `(buf, len)` where `buf[..len]` contains the key.
/// Callers pass `&buf[..len]` to `crypt_packet`.
pub fn copy_psk(psk: &[u8]) -> ([u8; 32], usize) {
    let mut buf = [0u8; 32];
    let len = psk.len().min(32);
    buf[..len].copy_from_slice(&psk[..len]);
    (buf, len)
}

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
            let key: &[u8; 16] = key.try_into().map_err(|_| CryptoError)?;
            let mut cipher = Aes128Ctr::new(key.into(), (&nonce).into());
            cipher.apply_keystream(data);
            Ok(())
        }
        32 => {
            let key: &[u8; 32] = key.try_into().map_err(|_| CryptoError)?;
            let mut cipher = Aes256Ctr::new(key.into(), (&nonce).into());
            cipher.apply_keystream(data);
            Ok(())
        }
        _ => Err(CryptoError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::DEFAULT_PSK;

    /// `build_nonce`'s exact byte layout, verified field-by-field rather than
    /// only through a round trip — a round trip alone can't catch e.g. the
    /// packet_id and sender swapping places, since encrypt/decrypt would
    /// still agree with each other while disagreeing with every other node
    /// on the mesh.
    #[test]
    fn build_nonce_matches_the_documented_byte_layout() {
        let nonce = build_nonce(0x1234_5678, 0xAABB_CCDD);
        // bytes 0..8: packet_id widened to u64 LE
        assert_eq!(&nonce[0..8], &0x1234_5678u64.to_le_bytes());
        // bytes 8..12: sender u32 LE
        assert_eq!(&nonce[8..12], &0xAABB_CCDDu32.to_le_bytes());
        // bytes 12..16: extra_nonce, always zero on the PSK path today
        assert_eq!(&nonce[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn build_nonce_widens_packet_id_rather_than_truncating() {
        // A packet_id with its high bit set must not bleed into the sender
        // field — this is exactly the bug the doc comment above `build_nonce`
        // describes as previously fixed (writing only 4 bytes of the u64).
        let nonce = build_nonce(0xFFFF_FFFF, 0);
        assert_eq!(&nonce[0..8], &[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);
        assert_eq!(&nonce[8..12], &[0, 0, 0, 0]);
    }

    /// AES-128-CTR against an independently computed reference vector (two
    /// separate implementations — Python's `cryptography` library and
    /// `openssl enc`, cross-checked against each other, plus this crate's own
    /// `ccm`/`aes`/`ctr` stack directly during development), using the real
    /// Meshtastic `DEFAULT_PSK`. A round-trip-only test proves self-
    /// consistency; this proves interop with what a real node would produce
    /// for the same inputs.
    #[test]
    fn aes128_ctr_matches_an_independently_computed_reference_vector() {
        let key = DEFAULT_PSK;
        let packet_id = 42u32;
        let sender = 0x1234_5678u32;
        let mut data = *b"Hello mesh!";

        crypt_packet(&key, packet_id, sender, &mut data).expect("16-byte key must be accepted");

        assert_eq!(
            data,
            [
                0xdd, 0xd0, 0x8e, 0x9a, 0x0b, 0x64, 0x8b, 0xef, 0xd2, 0x63, 0xcf
            ]
        );
    }

    /// Same reference-vector approach as above, for the AES-256 branch —
    /// upstream picks the key size purely from PSK length, and that branch
    /// had no coverage at all before this.
    #[test]
    fn aes256_ctr_matches_an_independently_computed_reference_vector() {
        let key: [u8; 32] = {
            let mut k = [0u8; 32];
            for (i, b) in k.iter_mut().enumerate() {
                *b = i as u8;
            }
            k
        };
        let packet_id = 7u32;
        let sender = 0xCAFE_BABEu32;
        let mut data = *b"AES256 test msg!";

        crypt_packet(&key, packet_id, sender, &mut data).expect("32-byte key must be accepted");

        assert_eq!(
            data,
            [
                0x3d, 0xb7, 0x08, 0xb5, 0xb0, 0x78, 0x5a, 0xee, 0xaa, 0x27, 0x7f, 0x8b, 0x1a, 0xf7,
                0x10, 0x3f
            ]
        );
    }

    #[test]
    fn ctr_mode_is_symmetric_encrypt_equals_decrypt() {
        let key = DEFAULT_PSK;
        let packet_id = 99u32;
        let sender = 0x1111_2222u32;
        let original = *b"round trip me";
        let mut data = original;

        crypt_packet(&key, packet_id, sender, &mut data).unwrap();
        assert_ne!(
            data, original,
            "encryption should actually change the bytes"
        );
        crypt_packet(&key, packet_id, sender, &mut data).unwrap();
        assert_eq!(
            data, original,
            "decrypting with the same key/nonce must recover the plaintext"
        );
    }

    #[test]
    fn an_unsupported_key_length_is_rejected() {
        let mut data = *b"data";
        for bad_len in [0, 1, 15, 17, 24, 31, 33] {
            let key = alloc::vec![0u8; bad_len];
            assert!(
                crypt_packet(&key, 1, 1, &mut data).is_err(),
                "key length {bad_len} must be rejected, not silently accepted"
            );
        }
    }

    #[test]
    fn copy_psk_truncates_to_32_bytes_and_reports_the_real_length() {
        let (buf, len) = copy_psk(&[0xAA; 40]);
        assert_eq!(len, 32);
        assert!(buf[..32].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn empty_data_is_a_no_op_rather_than_an_error() {
        let mut data: [u8; 0] = [];
        assert!(crypt_packet(&DEFAULT_PSK, 1, 1, &mut data).is_ok());
    }
}
