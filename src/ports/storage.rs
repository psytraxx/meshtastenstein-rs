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
pub trait Storage {
    /// Add a radio frame to storage.
    fn add(&mut self, frame: &RadioFrame) -> Result<(), StorageError>;

    /// Peek at the oldest frame without removing it.
    fn peek(&mut self) -> Result<Option<RadioFrame>, StorageError>;

    /// Remove the oldest frame.
    fn pop(&mut self) -> Result<(), StorageError>;

    /// Check if storage is empty.
    fn is_empty(&self) -> bool;

    /// Check if storage is full.
    fn is_full(&self) -> bool;

    /// Get current number of frames.
    fn count(&self) -> usize;

    /// Clear all frames.
    fn clear(&mut self);
}
