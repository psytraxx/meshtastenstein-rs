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
//!     hop_limit: DEFAULT_HOP_LIMIT,
//! }
//! .build(device, router, node_db, packet_id, None)?;
//! ```

extern crate alloc;

use crate::{
    constants::{DEFAULT_HOP_LIMIT, NO_NEXT_HOP},
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
    /// Hop limit for the OTA header.
    pub hop_limit: u8,
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
            hop_limit: DEFAULT_HOP_LIMIT,
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

        let header = PacketHeader {
            destination: self.dest,
            sender: device.my_node_num,
            packet_id,
            flags: PacketHeader::make_flags(
                self.want_ack && !is_broadcast,
                false,
                self.hop_limit,
                self.hop_limit,
            ),
            channel_index: channel_hash,
            next_hop,
            relay_node,
        };

        RadioFrame::from_parts(&header, &enc_buf)
    }
}
