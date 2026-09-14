# TODO — Phase 2 hardening

Phase 2 of the hardening plan: items that degrade a must-work flow without
breaking it outright. Phase 1 (silent interop defects) is done — see PRs #5,
#6, #7, #8. This phase is not yet started.

Ordering follows the original plan's rationale, not strict priority — 2.1 is
the most involved and interacts directly with 1.1's auth-gate restructure.

---

## 2.1 Intermediate relay retransmission

`NUM_INTERMEDIATE_RETX` is already defined in `constants.rs` (currently `2`)
but never read anywhere. **User decision: implement fully to match upstream,
not skip for power** — a half-done retransmit path is worse than none.

Upstream: `NextHopRouter::sendWithNextHop` (`NextHopRouter.cpp:101-119`)
starts a retransmission for a packet **we are relaying** (not our own
`want_ack` — that's `ReliableRouter`'s job already) whenever a next hop was
actually assigned and `(hop_limit > 0 || want_ack)`:

```
if (!isFromUs(p) || !p->want_ack) && next_hop != NO_NEXT_HOP && (hop_limit > 0 || want_ack):
    startRetransmission(copy)   // PendingPacket(numReTx=NUM_INTERMEDIATE_RETX=3, "including the initial send")
```

On timeout, `doRetransmissions` resends via the *same* next hop until
exhausted, then the final retry clears `next_hop` (ours and NodeDB's) and
falls back to flooding — this is exactly what our `tick_retransmissions`
already does generically. `stopRetransmission` only cancels a queued TX if a
retry actually fired (`numRetransmissions < initialNumRetransmissions`) and
only when `isFromUs(p) || roleAllowsCancelingFromTxQueue(p)` — guards
against a fast MQTT/opaque ACK killing a packet that never went out on LoRa.

**Good news: no new data structure.** `PendingPacket` already has
`is_our_packet: bool` (false = "we're relaying for someone else") and
`tick_retransmissions` is already ownership-agnostic. The actual gap is
entirely on the *send* side: nothing today calls the retransmit-tracking
path for a packet we relay — only `from_app::transmit_from_ble_packet`
registers a `PendingPacket`, for our own originated `want_ack` sends. Add
tracking at the rebroadcast site in `from_radio/mod.rs` (Layer 3, where
`PendingRebroadcast` fires): when the packet we're about to relay has a
resolved `next_hop` (i.e. directed, not a flood) and
`hop_limit > 0 || want_ack`, register a `PendingPacket` with
`is_our_packet: false`, `retries_left: NUM_INTERMEDIATE_RETX`,
`ble_notify: None` (no local BLE client is waiting on someone else's
packet).

Note `constants.rs` currently has `2`, but upstream's comment says "Total
attempts... including the initial send" = 3 — align the constant's
*meaning* with `PendingPacket::new`'s existing `numRetransmissions - 1`
convention (see `router.rs`'s own doc) before trusting the literal `2`.

**Interacts directly with 1.1**: this is retransmit state, exactly what the
auth gate is meant to protect. An unauthenticated forged packet must never
reach this new registration path, since that would let a third party make
us hold and repeatedly resend garbage. (1.1 has already landed in `main`,
so this constraint is satisfied — just don't regress it.)

**Tests:**
- a directed packet with a resolved next hop registers a relay-owned `PendingPacket`
- it retransmits via the same next hop on timeout
- the final retry clears `next_hop` and floods
- a fast ACK arriving before any retry cancels tracking without touching the TX queue (mirrors `stopRetransmission`'s `numRetransmissions < initialNumRetransmissions` guard)
- a flooded (no next-hop) packet never registers relay tracking
- an *unauthenticated* forged directed packet is rejected before reaching this registration path at all

---

## 2.2 PSK nonce offset

Upstream `initNonce` writes `extra_nonce` at **offset 4**, overlapping the
packet-id u64 (`CryptoEngine.cpp:434-443`); we reserve bytes 12..16.
Unreachable today (the PSK path always passes 0) but must be corrected
before any future use.

The fix is *not* simply "move to 4..8" — upstream guards with
`if (extraNonce)`, so overwrite only when non-zero, or every packet we
currently send regresses.

**Tests:**
- zero-extra-nonce case is byte-identical to today's output
- non-zero case matches the overlapping layout

---

## 2.3 `getHopLimitForResponse`

(`RoutingModule.cpp:64-81`.) ACKs currently reuse the default hop limit, so
a 1-hop DM's ACK floods 3 hops — wasted airtime and battery. Includes the
0-hop case (`hop_start == 0` → respond with 0 hops).

Pure function of `(hops_away, configured_limit)`; touches `handlers/util.rs`
and `tx.rs`.

This is a power win, not just a correctness fix — worth calling out since it
partially offsets 2.1's cost.

---

## 2.4 NodeDB eviction

When the DB is full and nothing is stale, the **new node is currently
dropped** (`node_db.rs`), so a node rebooting into a full DB can never learn
its neighbours. Add favourite-aware LRU eviction.

**Tests:**
- the stalest entry is evicted to make room
- favourites and our own entry survive eviction
- an all-favourites DB rejects the new node rather than evicting one

---

## 2.5 Role table (`CLIENT_BASE`)

`CLIENT_BASE` currently behaves as plain `Client` (README already documents
this as a known limitation). Verify Repeater *should* keep relaying (it
should, per upstream) before changing anything — then either implement the
`CLIENT_BASE` distinction with per-role tests, or leave it and keep the
README's existing caveat accurate.

---

## Notes carried over from the plan

- **Power stance:** no new power work beyond what falls out of 2.1–2.3. The
  ~16% LongFast RX duty cycle remains the main win; guard it with a test
  asserting `rx_duty_cycle_params()` returns `Some` with a positive sleep
  window at every preset and `None` at preamble 16, so nobody "fixes" the
  64-symbol preamble constant toward upstream's degenerate 16.
- **Verification per item:** `cargo test --lib` (both with and without the
  `test-harness` feature), `cargo clippy --all-features -- -D warnings` and
  `cargo fmt` in all three crates, a real `cargo build --release` in
  `boards/nrf52` (not just `check`), a `CHANGELOG.md` entry under today's
  date (`Fixed`/`Changed`, behaviour-and-why, no filenames/symbols), and
  README updates (Known Limitations, feature tables) as items close.
- **On-hardware checks (ESP32 + Android app)**, once 2.1 lands: send a
  directed (non-broadcast) message through an intermediate relay and
  confirm it retransmits/falls back to flood correctly under a dropped ACK;
  otherwise the existing Phase 1 hardware checklist (DM to unknown-key vs.
  known-key node, node exchange with hops, region/preset reboot+reconnect)
  still applies as a regression check.
