//! Port (interface) for persistent storage.

use crate::domain::packet::RadioFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    Full,
    Empty,
    SerializationError,
    StorageError,
}

/// Port trait for persistent message storage (survives deep sleep)
///
/// `add`/`peek`/`pop`/`clear` are `async` — see `ConfigStorage`'s doc comment
/// for why (the nRF52 board's flash driver requires it for writes/erases).
/// `is_empty`/`is_full`/`count` stay sync: they only ever read in-RAM state,
/// never flash, on every adapter.
pub trait Storage {
    /// Add a radio frame to storage.
    async fn add(&mut self, frame: &RadioFrame) -> Result<(), StorageError>;

    /// Peek at the oldest frame without removing it.
    async fn peek(&mut self) -> Result<Option<RadioFrame>, StorageError>;

    /// Remove the oldest frame.
    async fn pop(&mut self) -> Result<(), StorageError>;

    /// Check if storage is empty.
    fn is_empty(&self) -> bool;

    /// Check if storage is full.
    fn is_full(&self) -> bool;

    /// Get current number of frames.
    fn count(&self) -> usize;

    /// Clear all frames.
    async fn clear(&mut self);
}
