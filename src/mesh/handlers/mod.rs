//! Packet handlers — one module per data-flow direction.
//!
//! - [`from_radio`] — packets arriving from the LoRa radio
//! - [`from_app`]   — packets arriving from the BLE app (phone → device)
//! - [`admin`]      — `AdminMessage` dispatch, shared by both directions

pub mod admin;
pub mod from_app;
pub mod from_radio;

// Re-export the types mesh_task needs directly
pub use from_app::{AppAction, AppContext};
pub use from_radio::{RadioContext, RadioResult};
