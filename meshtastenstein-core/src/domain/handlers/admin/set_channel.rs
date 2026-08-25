use crate::{
    domain::{
        channels::{ChannelConfig, ChannelRole},
        context::MeshCtx,
    },
    ports::MeshStorage,
    proto::Channel,
};
use log::{info, warn};

/// Meshtastic PSKs are 0 bytes (unencrypted), 1 byte (expands to the default
/// key), or a real AES-128/256 key (16 or 32 bytes) — anything else fails
/// `crypt_packet` at encrypt/decrypt time. Reject it here instead of storing
/// a valid-length-but-wrong key that silently breaks the channel later.
fn is_valid_psk_len(len: usize) -> bool {
    matches!(len, 0 | 1 | 16 | 32)
}

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, ch: Channel) {
    info!("[Admin] Setting channel {}", ch.index);

    if let Some(settings) = ch.settings {
        if !is_valid_psk_len(settings.psk.len()) {
            warn!(
                "[Admin] Rejecting SetChannel {}: invalid PSK length {} (must be 0, 1, 16, or 32 bytes)",
                ch.index,
                settings.psk.len()
            );
            return;
        }

        let mut new_ch = ChannelConfig {
            index: ch.index as u8,
            name: heapless::String::new(),
            psk: heapless::Vec::new(),
            role: ChannelRole::try_from(ch.role).unwrap_or(ChannelRole::Secondary),
        };
        new_ch.name.push_str(&settings.name).ok();
        new_ch
            .psk
            .extend_from_slice(&settings.psk)
            .expect("length already validated against the 32-byte heapless::Vec capacity");

        ctx.device.channels.set(ch.index as u8, new_ch);
        if let Err(e) = ctx.storage.save_state(ctx.device).await {
            warn!("[Admin] Failed to persist SetChannel {}: {:?}", ch.index, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_four_meshtastic_psk_lengths() {
        assert!(is_valid_psk_len(0)); // unencrypted
        assert!(is_valid_psk_len(1)); // expands to the default PSK
        assert!(is_valid_psk_len(16)); // AES-128
        assert!(is_valid_psk_len(32)); // AES-256
    }

    #[test]
    fn rejects_lengths_that_would_only_fail_later_in_crypt_packet() {
        for len in [2, 8, 15, 17, 24, 31, 33, 64] {
            assert!(!is_valid_psk_len(len), "length {len} should be rejected");
        }
    }
}
