pub mod config_storage;
pub mod entropy;
pub mod identity;
pub mod reboot;
pub mod sleep;
pub mod storage;
pub mod watchdog;

pub use config_storage::ConfigStorage;
pub use entropy::EntropySource;
pub use identity::Identity;
pub use reboot::Reboot;
pub use sleep::Sleep;
pub use storage::{Storage, StorageError};
pub use watchdog::Watchdog;

pub trait MeshStorage: ConfigStorage + storage::Storage {}
impl<T: ConfigStorage + storage::Storage> MeshStorage for T {}
