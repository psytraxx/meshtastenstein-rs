//! Host-side test harness for `MeshCtx`-driven handler tests.
//!
//! Handlers take `&mut MeshCtx<'_, S>`, and `MeshCtx` is a projection of
//! 25 fields — three of them `Sender<'static, ...>` handles into Embassy
//! `Channel`s. No unit test can drive that signature directly, which is why
//! the pre-existing 82 tests are all pure-function tests on things like
//! `TxBuilder::build` and `MeshRouter::should_filter_received` that don't
//! need a `MeshCtx` at all. Assertions like "an unauthenticated packet
//! queues nothing to BLE or LoRa" need to observe what a full handler
//! dispatch actually sent — that's what `TestBed` is for.
//!
//! Entirely `#[cfg(test)]`-gated behind the `test-harness` feature (never
//! enabled by a board build — no chip dependency is added to core). Needs
//! `std` for two reasons, both worth calling out since this crate is
//! otherwise `#![no_std]`:
//!
//! 1. `embassy_time::Instant::now()` requires exactly one crate in the final
//!    binary's dependency graph to implement `_embassy_time_now`
//!    (`embassy_time_driver::time_driver_impl!`). Board builds get this from
//!    their own board-specific driver; under `cargo test --lib`, no board is
//!    in the graph, so linking fails with `undefined symbol: _embassy_time_now`
//!    (verified empirically — this is not a hypothetical). `embassy-time`'s
//!    own `std` feature registers a `std::time::Instant`-backed driver, so
//!    enabling it here is sufficient; no custom driver code is needed.
//! 2. `std::vec::Vec` (via `alloc`) already works fine under `no_std` +
//!    `extern crate alloc`, but the harness fixtures below use plain
//!    `std::` conveniences (e.g. for the fake storage backing store) for
//!    brevity, so this module declares its own `extern crate std;`.

extern crate std;

use crate::{
    domain::{
        context::{ChannelMetrics, MeshCtx, SessionPasskey},
        device::DeviceState,
        node_db::NodeDB,
        packet::RadioFrame,
        router::{MeshRouter, PendingPacket, PendingRebroadcast},
    },
    inter_task::channels::{Channels, FromRadioMessage, LedCommand},
    ports::{ConfigStorage, EntropySource, Storage, StorageError},
};
use core::sync::atomic::AtomicBool;
use embassy_time::Instant;
use std::vec::Vec;

/// Deterministic `EntropySource` for tests: cycles through a fixed sequence
/// (defaulting to all-zero) so rebroadcast-jitter and nonce assertions never
/// flake. Construct with `FakeEntropy::sequence(&[...])` to control specific
/// draws (e.g. to pin a PKC `extra_nonce` in a test).
pub struct FakeEntropy {
    values: Vec<u32>,
    next: core::cell::Cell<usize>,
}

impl FakeEntropy {
    pub fn zero() -> Self {
        Self::sequence(&[0])
    }

    pub fn sequence(values: &[u32]) -> Self {
        Self {
            values: values.to_vec(),
            next: core::cell::Cell::new(0),
        }
    }
}

impl EntropySource for FakeEntropy {
    fn random_u32(&self) -> u32 {
        let i = self.next.get();
        let v = self.values[i % self.values.len()];
        self.next.set(i + 1);
        v
    }
}

/// In-memory `MeshStorage` (`ConfigStorage` + `Storage`) backing store, for
/// tests that need config/bond/NodeDB/keypair persistence or the message
/// ring without touching real flash. All operations are infallible against
/// this backing store; `Result`-returning methods still return `Ok` so
/// call sites exercise their real success path.
#[derive(Default)]
pub struct FakeStorage {
    pub saved_state: Option<DeviceState>,
    pub bond: Option<[u8; 48]>,
    pub node_db_snapshot: Option<Vec<u8>>,
    pub pkc_keypair: Option<([u8; 32], [u8; 32])>,
    pub ring: Vec<RadioFrame>,
}

impl ConfigStorage for FakeStorage {
    async fn save_state(&mut self, device: &DeviceState) -> Result<(), StorageError> {
        self.saved_state = Some(device.clone());
        Ok(())
    }

    async fn load_state(&mut self, device: &mut DeviceState) {
        if let Some(saved) = &self.saved_state {
            *device = saved.clone();
        }
    }

    async fn save_bond(&mut self, bytes: &[u8; 48]) -> Result<(), StorageError> {
        self.bond = Some(*bytes);
        Ok(())
    }

    async fn load_bond(&mut self) -> Option<[u8; 48]> {
        self.bond
    }

    async fn clear_bond(&mut self) {
        self.bond = None;
    }

    async fn erase_config(&mut self) {
        self.saved_state = None;
    }

    async fn save_node_db(&mut self, _db: &NodeDB) -> Result<(), StorageError> {
        // Tests that need snapshot round-trip fidelity exercise
        // `domain::persistence`'s encode/decode functions directly rather
        // than through this fake — see `node_db.rs`'s own test module.
        self.node_db_snapshot = Some(Vec::new());
        Ok(())
    }

    async fn load_node_db(&mut self, _db: &mut NodeDB) {}

    async fn load_pkc_keypair(&mut self) -> Option<([u8; 32], [u8; 32])> {
        self.pkc_keypair
    }

    async fn save_pkc_keypair(
        &mut self,
        priv_key: &[u8; 32],
        pub_key: &[u8; 32],
    ) -> Result<(), StorageError> {
        self.pkc_keypair = Some((*priv_key, *pub_key));
        Ok(())
    }
}

impl Storage for FakeStorage {
    async fn add(&mut self, frame: &RadioFrame) -> Result<(), StorageError> {
        self.ring.push(frame.clone());
        Ok(())
    }

    async fn peek(&mut self) -> Result<Option<RadioFrame>, StorageError> {
        Ok(self.ring.first().cloned())
    }

    async fn pop(&mut self) -> Result<(), StorageError> {
        if self.ring.is_empty() {
            Ok(())
        } else {
            self.ring.remove(0);
            Ok(())
        }
    }

    fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    fn is_full(&self) -> bool {
        false
    }

    fn count(&self) -> usize {
        self.ring.len()
    }

    async fn clear(&mut self) {
        self.ring.clear();
    }
}

/// Owns every field `MeshCtx` borrows from, plus a `'static` `Channels` so
/// `ctx()` can hand out real `Sender` handles. One `TestBed` per test —
/// leaking a fresh `Channels` per instance is fine in a test process.
pub struct TestBed {
    pub device: DeviceState,
    pub node_db: NodeDB,
    pub storage: FakeStorage,
    pub router: MeshRouter,
    pub pending_packets: heapless::Vec<PendingPacket, 8>,
    pub pending_rebroadcast: heapless::Vec<PendingRebroadcast, 8>,
    pub my_position_bytes: heapless::Vec<u8, 64>,
    pub session_passkey: Option<SessionPasskey>,
    pub from_radio_id: u32,
    pub ble_connected: bool,
    pub last_nodeinfo_tx: Option<Instant>,
    pub last_position_tx: Instant,
    pub last_lora_telemetry: Option<Instant>,
    pub last_neighborinfo_tx: Option<Instant>,
    pub channel_metrics: ChannelMetrics,
    pub reboot_after_secs: Option<u32>,
    pub shutdown_after_secs: Option<u32>,
    pub node_id_str: alloc::string::String,
    pub boot_time: Instant,
    pub pkc_pub_bytes: [u8; 32],
    pub pkc_priv_bytes: [u8; 32],
    pub entropy: FakeEntropy,
    pub tx_enabled: AtomicBool,
    channels: &'static Channels,
}

impl TestBed {
    /// Build a bed for a node with the given MAC and no PSK/PKC keys set up
    /// beyond the default primary channel (`DeviceState::new`'s default).
    /// Individual tests mutate `device`, `node_db`, etc. directly before
    /// calling `ctx()`.
    pub fn new(mac: &[u8; 6]) -> Self {
        let device = DeviceState::new(mac);
        let node_num = device.my_node_num;
        // `Box::leak` rather than a real `StaticCell`: this is test-only
        // code running in a std test binary, so the per-test leak is fine
        // and avoids pulling `static_cell` in as a real dependency.
        let channels: &'static Channels =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(Channels::new()));
        Self {
            node_id_str: crate::domain::handlers::util::build_node_id_string(node_num),
            router: MeshRouter::new(node_num),
            device,
            node_db: NodeDB::new(node_num),
            storage: FakeStorage::default(),
            pending_packets: heapless::Vec::new(),
            pending_rebroadcast: heapless::Vec::new(),
            my_position_bytes: heapless::Vec::new(),
            session_passkey: None,
            from_radio_id: 1,
            ble_connected: false,
            last_nodeinfo_tx: None,
            last_position_tx: Instant::now(),
            last_lora_telemetry: None,
            last_neighborinfo_tx: None,
            channel_metrics: ChannelMetrics::default(),
            reboot_after_secs: None,
            shutdown_after_secs: None,
            boot_time: Instant::now(),
            pkc_pub_bytes: [0u8; 32],
            pkc_priv_bytes: [0u8; 32],
            entropy: FakeEntropy::zero(),
            tx_enabled: AtomicBool::new(true),
            channels,
        }
    }

    /// Build a `MeshCtx` borrowing every field above. Mirrors
    /// `MeshOrchestrator::make_ctx` field-for-field — keep the two in sync.
    pub fn ctx(&mut self) -> MeshCtx<'_, FakeStorage> {
        MeshCtx {
            device: &mut self.device,
            node_db: &mut self.node_db,
            storage: &mut self.storage,
            router: &mut self.router,
            pending_packets: &mut self.pending_packets,
            pending_rebroadcast: &mut self.pending_rebroadcast,
            my_position_bytes: &mut self.my_position_bytes,
            session_passkey: &mut self.session_passkey,
            from_radio_id: &mut self.from_radio_id,
            ble_connected: &mut self.ble_connected,
            last_nodeinfo_tx: &mut self.last_nodeinfo_tx,
            last_position_tx: &mut self.last_position_tx,
            last_lora_telemetry: &mut self.last_lora_telemetry,
            last_neighborinfo_tx: &mut self.last_neighborinfo_tx,
            channel_metrics: &mut self.channel_metrics,
            reboot_after_secs: &mut self.reboot_after_secs,
            shutdown_after_secs: &mut self.shutdown_after_secs,
            node_id_str: self.node_id_str.as_str(),
            boot_time: self.boot_time,
            pkc_pub_bytes: &self.pkc_pub_bytes,
            pkc_priv_bytes: &self.pkc_priv_bytes,
            tx_to_ble: self.channels.ble_tx.sender(),
            tx_to_lora: self.channels.lora_tx.sender(),
            led_commands: self.channels.led_cmd.sender(),
            disconn_cmd: self.channels.disconn_cmd.sender(),
            entropy: &self.entropy,
            tx_enabled: &self.channels.tx_enabled,
        }
    }

    /// Drain every `RadioFrame` currently queued for LoRa TX (non-blocking).
    pub fn sent_to_lora(&self) -> Vec<RadioFrame> {
        let mut out = Vec::new();
        while let Ok(frame) = self.channels.lora_tx.try_receive() {
            out.push(frame);
        }
        out
    }

    /// Drain every `FromRadioMessage` currently queued for BLE TX (non-blocking).
    pub fn sent_to_ble(&self) -> Vec<FromRadioMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = self.channels.ble_tx.try_receive() {
            out.push(msg);
        }
        out
    }

    /// Drain every queued LED command (non-blocking). Occasionally useful to
    /// confirm a handler did *not* run its normal blink-on-receive path
    /// (e.g. the point of 1.1's forged-packet test).
    pub fn led_commands(&self) -> Vec<LedCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.channels.led_cmd.try_receive() {
            out.push(cmd);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_test_bed_starts_with_empty_queues() {
        let bed = TestBed::new(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);
        assert!(bed.sent_to_lora().is_empty());
        assert!(bed.sent_to_ble().is_empty());
        assert!(bed.led_commands().is_empty());
    }

    #[test]
    fn ctx_reflects_the_bed_s_own_node_num() {
        let mut bed = TestBed::new(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);
        let expected = bed.device.my_node_num;
        let ctx = bed.ctx();
        assert_eq!(ctx.device.my_node_num, expected);
    }

    #[test]
    fn fake_entropy_cycles_through_its_sequence() {
        let e = FakeEntropy::sequence(&[1, 2, 3]);
        assert_eq!(e.random_u32(), 1);
        assert_eq!(e.random_u32(), 2);
        assert_eq!(e.random_u32(), 3);
        assert_eq!(e.random_u32(), 1);
    }
}
