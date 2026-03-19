pub mod config_storage;
pub mod identity;
pub mod sleep;
pub mod storage;

pub use config_storage::ConfigStorage;
pub use identity::Identity;
pub use sleep::Sleep;
pub use storage::{Storage, StorageError};

pub trait MeshStorage: ConfigStorage + storage::Storage {}
impl<T: ConfigStorage + storage::Storage> MeshStorage for T {}
