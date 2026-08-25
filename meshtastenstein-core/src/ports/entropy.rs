//! Port (interface) for hardware random-number generation.

/// Port trait for a hardware TRNG source.
///
/// Implementations are expected to be cheap to construct/call repeatedly
/// (e.g. wrapping a peripheral handle that doesn't need exclusive ownership),
/// mirroring how `esp_hal::rng::Rng::new()` works today.
pub trait EntropySource {
    /// Return one random 32-bit value.
    fn random_u32(&self) -> u32;
}
