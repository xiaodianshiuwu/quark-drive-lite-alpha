//! Shell independent, bounded and restartable Quark transfer engine.
mod download;
mod manager;
mod protocol;
mod sha1_state;
mod upload;
pub use manager::TransferManager;
