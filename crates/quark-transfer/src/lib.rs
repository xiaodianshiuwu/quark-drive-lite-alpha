//! Shell independent, bounded and restartable Quark transfer engine.
mod download;
mod manager;
mod protocol;
mod upload;
pub use manager::TransferManager;
