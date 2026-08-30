//! Outgoing LoRa frame builder.
//!
//! `TxBuilder` is the single path for constructing a `RadioFrame` from
//! application-level parameters. It replaces the three independent
//! encode+encrypt+header sequences that previously existed in:
//!   - `handlers::util::lora_send`
//!   - `handlers::util::send_routing_ack`
//!   - `handlers::from_app::transmit_from_ble_packet`
//!
//! Usage:
//! ```ignore
//! let frame = TxBuilder {
//!     dest,
//!     portnum: PortNum::NodeinfoApp.into(),
//!     inner_payload: payload_bytes,
//!     channel_idx: None, // None = primary channel
//!     want_ack: false,
//!     want_response: false,
//!     request_id: 0,
//!     hop_limit: None, // use the configured device default
//! }
//! .build(device, router, node_db, packet_id, None)?;
//! ```

extern crate alloc;

use crate::{
    constants::{MAX_HOP_LIMIT, NO_NEXT_HOP},
    domain::{
        crypto_pkc::{PKC_OVERHEAD, derive_shared_key, encrypt_pkc, keypair_from_seed},
        crypto_psk,
        device::DeviceState,
        node_db::NodeDB,
        packet::{BROADCAST_ADDR, PacketHeader, RadioFrame},
        router::MeshRouter,
    },
    proto::{Data, PortNum},
};
use log::warn;
use prost::Message;

/// All parameters needed to build one outgoing LoRa frame.
pub struct TxBuilder {
    /// Destination node number (use `BROADCAST_ADDR` for floods).
    pub dest: u32,
    /// PortNum value for the `Data` wrapper.
    pub portnum: i32,
    /// Application-level payload bytes (go into `Data.payload`).
    pub inner_payload: alloc::vec::Vec<u8>,
    /// Channel slot to use. `None` → primary channel.
    pub channel_idx: Option<u8>,
    /// Set `want_ack` in the OTA header (triggers retransmission tracking).
    pub want_ack: bool,
    /// Set `want_response` in the `Data` wrapper.
    pub want_response: bool,
    /// Set `request_id` in the `Data` wrapper (for ACK packets).
    pub request_id: u32,
    /// Set `reply_id` in the `Data` wrapper (links this packet to a prior message,
    /// used for reactions and threaded replies).
    pub reply_id: u32,
    /// Set `emoji` in the `Data` wrapper (non-zero = treat payload as emoji reaction).
    pub emoji: u32,
    /// Hop limit for the OTA header. `None` uses `device.hop_limit` (the
    /// configured default); `Some(n)` overrides it — used only when
    /// forwarding a per-packet hop limit the phone explicitly set. Either
    /// way the result is clamped to `MAX_HOP_LIMIT` since the wire field is
    /// only 3 bits.
    pub hop_limit: Option<u8>,
}

impl Default for TxBuilder {
    fn default() -> Self {
        Self {
            dest: BROADCAST_ADDR,
            portnum: PortNum::UnknownApp.into(),
            inner_payload: alloc::vec![],
            channel_idx: None,
            want_ack: false,
            want_response: false,
            request_id: 0,
            reply_id: 0,
            emoji: 0,
            hop_limit: None,
        }
    }
}

impl TxBuilder {
    /// Build the `RadioFrame`, or return `None` if encryption or framing fails.
    ///
    /// `packet_id` must be pre-allocated by the caller (via `device.next_packet_id()`).
    ///
    /// `pkc_keys` is `Some((priv_bytes, extra_nonce))` when the caller wants PKC
    /// encryption. Pass `None` to use PSK (or no encryption for unencrypted channels).
    pub fn build(
        self,
        device: &DeviceState,
        router: &MeshRouter,
        node_db: &NodeDB,
        packet_id: u32,
        pkc_keys: Option<(&[u8; 32], u32)>,
    ) -> Option<RadioFrame> {
        // Encode Data wrapper
        let mut enc_buf = Data {
            portnum: self.portnum,
            payload: self.inner_payload,
            want_response: self.want_response,
            request_id: self.request_id,
            reply_id: self.reply_id,
            emoji: self.emoji,
            ..Default::default()
        }
        .encode_to_vec();

        let preset_name = device.modem_preset.display_name();
        let channel = self
            .channel_idx
            .and_then(|idx| device.channels.get(idx))
            .or_else(|| device.channels.primary());

        let channel_hash = if pkc_keys.is_some() {
            // PKC packets carry channel_hash = 0 on the wire (upstream convention)
            0u8
        } else {
            channel.map(|c| c.hash(preset_name)).unwrap_or(0)
        };

        // Encrypt
        if let Some((priv_bytes, extra_nonce)) = pkc_keys {
            // PKC: X25519 ECDH → shared secret → AES-256-CCM
            let dest_pub_key = node_db.get(self.dest).and_then(|e| e.pub_key)?;
            let (my_secret, _) = keypair_from_seed(*priv_bytes);
            let peer_pub = x25519_dalek::PublicKey::from(dest_pub_key);
            let shared_key = derive_shared_key(&my_secret, &peer_pub);

            let plaintext_len = enc_buf.len();
            let mut pkc_buf = alloc::vec![0u8; plaintext_len + PKC_OVERHEAD];
            match encrypt_pkc(
                &shared_key,
                packet_id,
                device.my_node_num,
                extra_nonce,
                &enc_buf,
                &mut pkc_buf,
            ) {
                Ok(written) => {
                    pkc_buf.truncate(written);
                    enc_buf = pkc_buf;
                }
                Err(_) => {
                    warn!("[TX] PKC encrypt failed for {:08x}", self.dest);
                    return None;
                }
            }
        } else if let Some(ch) = channel
            && ch.is_encrypted()
        {
            let (psk_copy, psk_len) = crypto_psk::copy_psk(ch.effective_psk());
            crypto_psk::crypt_packet(
                &psk_copy[..psk_len],
                packet_id,
                device.my_node_num,
                &mut enc_buf,
            )
            .ok()?;
        }

        // Header
        let is_broadcast = self.dest == BROADCAST_ADDR;
        let next_hop = if is_broadcast {
            NO_NEXT_HOP
        } else {
            router.get_next_hop(node_db, self.dest, 0)
        };
        let relay_node = (device.my_node_num & 0xFF) as u8;
        let hop_limit = self
            .hop_limit
            .unwrap_or(device.hop_limit)
            .min(MAX_HOP_LIMIT);

        let header = PacketHeader {
            destination: self.dest,
            sender: device.my_node_num,
            packet_id,
            flags: PacketHeader::make_flags(
                self.want_ack && !is_broadcast,
                false,
                hop_limit,
                hop_limit,
            ),
            channel_index: channel_hash,
            next_hop,
            relay_node,
        };

        RadioFrame::from_parts(&header, &enc_buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::node_db::NodeDB;

    const NODE_NUM: u32 = 0x1234_5678;

    fn built_hop_limit(builder_hop_limit: Option<u8>, device_hop_limit: u8) -> u8 {
        let mut device = DeviceState::new(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);
        device.my_node_num = NODE_NUM;
        device.hop_limit = device_hop_limit;
        let router = MeshRouter::new(NODE_NUM);
        let node_db = NodeDB::new(NODE_NUM);

        let frame = (TxBuilder {
            dest: 0xAABB_CCDD,
            hop_limit: builder_hop_limit,
            ..Default::default()
        })
        .build(&device, &router, &node_db, 1, None)
        .expect("build should succeed for a plain unencrypted unicast");

        frame.header().unwrap().hop_limit()
    }

    #[test]
    fn default_hop_limit_falls_back_to_the_configured_device_value() {
        assert_eq!(built_hop_limit(None, 5), 5);
    }

    #[test]
    fn an_explicit_hop_limit_overrides_the_device_default() {
        assert_eq!(built_hop_limit(Some(2), 5), 2);
    }

    #[test]
    fn hop_limit_is_clamped_to_max_hop_limit_regardless_of_source() {
        // Neither path should be able to encode more than the 3-bit wire
        // field can hold.
        assert_eq!(built_hop_limit(None, 250), MAX_HOP_LIMIT);
        assert_eq!(built_hop_limit(Some(250), 3), MAX_HOP_LIMIT);
    }
}
