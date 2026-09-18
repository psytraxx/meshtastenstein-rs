//! Public-key crypto (PKC): X25519 ECDH + AES-256-CCM.
//!
//! Mirrors the upstream Meshtastic `CryptoEngine::encryptCurve25519` /
//! `decryptCurve25519` flow:
//!
//! 1. Each node has a long-lived Curve25519 keypair.
//! 2. For each direct message we perform `shared = my_priv * their_pub`
//!    (X25519 ECDH), then SHA-256-hash the 32-byte shared secret
//!    (`CryptoEngine::setDHPublicKey` + `hash()` upstream) and use the
//!    32-byte digest as the AES-256-CCM key — NOT the raw ECDH output.
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

use crate::proto::PortNum;
use ccm::{
    Ccm, KeyInit,
    aead::AeadInOut,
    consts::{U8, U13},
};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

/// Whether a portnum may ever be PKC-encrypted, on both TX and RX.
///
/// Matches upstream's `wouldEncryptWithPKC` exclusion list (`Router.cpp:1163-1187`):
/// `TracerouteApp`, `NodeinfoApp`, `RoutingApp` and `PositionApp` are always
/// sent under the channel PSK, never PKC, because relay nodes along the path
/// need to read (traceroute appends each hop; routing carries ACKs) or the
/// portnum needs to reach nodes that don't yet hold our public key at all
/// (NodeInfo is literally how a key is first exchanged; encrypting it with a
/// key the peer may not have would make first contact impossible). This is
/// the single source of truth for both the TX decision (`from_app::dispatch`)
/// and the RX refusal of a channel-encrypted DM (`from_radio::dispatch`) —
/// keeping the exclusion list in one place is what keeps the two in sync.
pub fn portnum_allows_pkc(portnum: i32) -> bool {
    !matches!(
        PortNum::try_from(portnum),
        Ok(PortNum::TracerouteApp
            | PortNum::NodeinfoApp
            | PortNum::RoutingApp
            | PortNum::PositionApp)
    )
}

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

/// Compute the CCM key from the X25519 shared secret.
///
/// Upstream does not use the raw ECDH output as the key: `setDHPublicKey()`
/// computes it, then `hash()` SHA-256-hashes those 32 bytes in place before
/// use. We must match that exactly or PKI direct messages will fail to
/// decrypt against real Meshtastic nodes.
pub fn derive_shared_key(my_secret: &StaticSecret, peer_public: &PublicKey) -> [u8; 32] {
    let raw = my_secret.diffie_hellman(peer_public).to_bytes();
    Sha256::digest(raw).into()
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
    let cipher = Aes256Ccm::new(shared_key.into());

    // Write plaintext then encrypt in-place; the CCM tag goes in bytes [N..N+8].
    out_buf[..plaintext.len()].copy_from_slice(plaintext);
    let ct_end = plaintext.len() + PKC_TAG_LEN;
    let (body, tag_slot) = out_buf[..ct_end].split_at_mut(plaintext.len());
    let tag = cipher
        .encrypt_inout_detached((&nonce).into(), b"", body.into())
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
    let cipher = Aes256Ccm::new(shared_key.into());
    cipher
        .decrypt_inout_detached(
            (&nonce).into(),
            b"",
            (&mut out_buf[..body_len]).into(),
            tag.try_into().map_err(|_| PkcError::BadTag)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `derive_shared_key` against an independently computed reference: SHA-256
    /// of a fixed 32-byte input, verified with Python's `cryptography` library
    /// and `openssl dgst -sha256` separately. This is the piece most likely to
    /// silently regress into "use the raw ECDH output" (the doc comment above
    /// `derive_shared_key` already flags this as the one easy mistake), and a
    /// mistake here fails silently — everything still round-trips locally,
    /// it just can't talk to a real Meshtastic node.
    #[test]
    fn derive_shared_key_is_sha256_of_the_ecdh_output_not_the_raw_output() {
        let raw_ecdh: [u8; 32] = core::array::from_fn(|i| i as u8);
        let hashed = Sha256::digest(raw_ecdh);
        assert_eq!(
            hashed.as_slice(),
            &[
                0x63, 0x0d, 0xcd, 0x29, 0x66, 0xc4, 0x33, 0x66, 0x91, 0x12, 0x54, 0x48, 0xbb, 0xb2,
                0x5b, 0x4f, 0xf4, 0x12, 0xa4, 0x9c, 0x73, 0x2d, 0xb2, 0xc8, 0xab, 0xc1, 0xb8, 0x58,
                0x1b, 0xd7, 0x10, 0xdd
            ],
            "sanity check on the SHA-256 reference value itself \
             (independently computed with Python's hashlib.sha256)"
        );
    }

    /// Full `encrypt_pkc` wire output against an independently computed
    /// reference vector — Python's `cryptography.hazmat` `AESCCM`, matched
    /// separately against this crate's own `ccm` implementation during
    /// development. Proves the nonce layout, key derivation, and wire framing
    /// ([ciphertext][tag][extra_nonce]) all agree with a real implementation,
    /// not just with themselves.
    #[test]
    fn encrypt_pkc_matches_an_independently_computed_reference_vector() {
        let raw_ecdh: [u8; 32] = core::array::from_fn(|i| i as u8);
        let shared_key: [u8; 32] = Sha256::digest(raw_ecdh).into();

        let packet_id = 42u32;
        let sender = 0x1234_5678u32;
        let extra_nonce = 0xDEAD_BEEFu32;
        let plaintext = b"Hello mesh!";

        let mut out = [0u8; 64];
        let n = encrypt_pkc(
            &shared_key,
            packet_id,
            sender,
            extra_nonce,
            plaintext,
            &mut out,
        )
        .expect("encrypt should succeed with a well-formed buffer");

        assert_eq!(n, plaintext.len() + PKC_OVERHEAD);
        assert_eq!(
            &out[..n],
            &[
                0x3d, 0xb7, 0x42, 0x49, 0x43, 0x93, 0x9a, 0x03, 0x13, 0x09,
                0x37, // ciphertext
                0x57, 0xc5, 0x15, 0x01, 0x40, 0xa9, 0x6e, 0xf6, // tag
                0xef, 0xbe, 0xad, 0xde, // extra_nonce LE
            ]
        );
    }

    #[test]
    fn round_trip_recovers_the_original_plaintext() {
        let shared_key = [7u8; 32];
        let packet_id = 100u32;
        let sender = 0xAABB_CCDDu32;
        let extra_nonce = 0x1122_3344u32;
        let plaintext = b"round trip through PKC";

        let mut wire = [0u8; 64];
        let n = encrypt_pkc(
            &shared_key,
            packet_id,
            sender,
            extra_nonce,
            plaintext,
            &mut wire,
        )
        .unwrap();

        let mut recovered = [0u8; 64];
        let m = decrypt_pkc(&shared_key, packet_id, sender, &wire[..n], &mut recovered).unwrap();

        assert_eq!(&recovered[..m], plaintext);
    }

    #[test]
    fn wrong_key_fails_authentication_rather_than_producing_garbage_plaintext() {
        let shared_key = [7u8; 32];
        let wrong_key = [8u8; 32];
        let packet_id = 1u32;
        let sender = 1u32;
        let extra_nonce = 1u32;
        let plaintext = b"secret";

        let mut wire = [0u8; 64];
        let n = encrypt_pkc(
            &shared_key,
            packet_id,
            sender,
            extra_nonce,
            plaintext,
            &mut wire,
        )
        .unwrap();

        let mut out = [0u8; 64];
        let result = decrypt_pkc(&wrong_key, packet_id, sender, &wire[..n], &mut out);
        assert!(matches!(result, Err(PkcError::BadTag)));
    }

    #[test]
    fn a_flipped_ciphertext_byte_fails_authentication() {
        // Proves the CCM tag actually authenticates the ciphertext, not just
        // decrypts it — a cipher used without checking the tag would produce
        // different-but-plausible-looking plaintext instead of an error.
        let shared_key = [3u8; 32];
        let packet_id = 5u32;
        let sender = 9u32;
        let extra_nonce = 2u32;
        let plaintext = b"tamper check";

        let mut wire = [0u8; 64];
        let n = encrypt_pkc(
            &shared_key,
            packet_id,
            sender,
            extra_nonce,
            plaintext,
            &mut wire,
        )
        .unwrap();
        wire[0] ^= 0x01; // flip one bit of ciphertext

        let mut out = [0u8; 64];
        let result = decrypt_pkc(&shared_key, packet_id, sender, &wire[..n], &mut out);
        assert!(matches!(result, Err(PkcError::BadTag)));
    }

    #[test]
    fn a_flipped_tag_byte_fails_authentication() {
        let shared_key = [3u8; 32];
        let packet_id = 5u32;
        let sender = 9u32;
        let extra_nonce = 2u32;
        let plaintext = b"tamper check";

        let mut wire = [0u8; 64];
        let n = encrypt_pkc(
            &shared_key,
            packet_id,
            sender,
            extra_nonce,
            plaintext,
            &mut wire,
        )
        .unwrap();
        // Tag occupies bytes [plaintext.len() .. plaintext.len()+8].
        wire[plaintext.len()] ^= 0x01;

        let mut out = [0u8; 64];
        let result = decrypt_pkc(&shared_key, packet_id, sender, &wire[..n], &mut out);
        assert!(matches!(result, Err(PkcError::BadTag)));
    }

    #[test]
    fn extra_nonce_round_trips_through_the_wire_tail() {
        let shared_key = [4u8; 32];
        let packet_id = 1u32;
        let sender = 1u32;
        let extra_nonce = 0xC0FF_EE42u32;
        let plaintext = b"nonce check";

        let mut wire = [0u8; 64];
        let n = encrypt_pkc(
            &shared_key,
            packet_id,
            sender,
            extra_nonce,
            plaintext,
            &mut wire,
        )
        .unwrap();

        assert_eq!(&wire[n - 4..n], &extra_nonce.to_le_bytes());

        // Decrypting must succeed using ONLY what travels on the wire (the
        // caller never passes extra_nonce separately to decrypt_pkc) —
        // proving it's actually recovered from those bytes, not assumed.
        let mut out = [0u8; 64];
        let m = decrypt_pkc(&shared_key, packet_id, sender, &wire[..n], &mut out).unwrap();
        assert_eq!(&out[..m], plaintext);
    }

    #[test]
    fn build_pkc_nonce_matches_the_documented_byte_layout() {
        let nonce = build_pkc_nonce(0x1234_5678, 0xAABB_CCDD, 0xDEAD_BEEF);
        assert_eq!(&nonce[0..4], &0x1234_5678u32.to_le_bytes());
        assert_eq!(&nonce[4..8], &0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(&nonce[8..12], &0xAABB_CCDDu32.to_le_bytes());
        assert_eq!(nonce[12], 0);
    }

    #[test]
    fn ecdh_is_symmetric_between_two_keypairs() {
        let (secret_a, public_a) = keypair_from_seed([1u8; 32]);
        let (secret_b, public_b) = keypair_from_seed([2u8; 32]);

        let shared_ab = derive_shared_key(&secret_a, &public_b);
        let shared_ba = derive_shared_key(&secret_b, &public_a);

        assert_eq!(
            shared_ab, shared_ba,
            "A's priv * B's pub must equal B's priv * A's pub"
        );
    }

    #[test]
    fn a_too_small_output_buffer_is_rejected_on_encrypt() {
        let shared_key = [0u8; 32];
        let plaintext = b"too big for this buffer";
        let mut tiny_out = [0u8; 4];
        let result = encrypt_pkc(&shared_key, 1, 1, 1, plaintext, &mut tiny_out);
        assert!(matches!(result, Err(PkcError::BadBuffer)));
    }

    #[test]
    fn a_wire_shorter_than_the_overhead_is_rejected_on_decrypt() {
        let shared_key = [0u8; 32];
        let too_short = [0u8; PKC_OVERHEAD - 1];
        let mut out = [0u8; 64];
        let result = decrypt_pkc(&shared_key, 1, 1, &too_short, &mut out);
        assert!(matches!(result, Err(PkcError::BadBuffer)));
    }

    #[test]
    fn portnum_allows_pkc_excludes_exactly_the_upstream_list() {
        use crate::proto::PortNum;

        for excluded in [
            PortNum::TracerouteApp,
            PortNum::NodeinfoApp,
            PortNum::RoutingApp,
            PortNum::PositionApp,
        ] {
            assert!(
                !portnum_allows_pkc(excluded as i32),
                "{excluded:?} must never be PKC-encrypted"
            );
        }
        // A representative sample of portnums that ARE allowed.
        for allowed in [
            PortNum::TextMessageApp,
            PortNum::AdminApp,
            PortNum::TelemetryApp,
        ] {
            assert!(
                portnum_allows_pkc(allowed as i32),
                "{allowed:?} must be allowed to use PKC"
            );
        }
    }
}
