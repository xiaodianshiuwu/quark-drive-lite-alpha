# Tauri migration architecture

- `src/renderer`: bundled local interface, virtualized rows, `window.qk` adapter. No API credentials or networking in the renderer.
- `src-tauri`: native windows, restricted IPC, login Cookie collection, native dialogs and graceful exit. Only the bundled main window can call `qk_call`; remote login content receives no capabilities.
- `crates/quark-core`: shell-independent HTTP/API, Cookie rotation, QCK2 encrypted session and settings.
- `crates/quark-transfer`: shell-independent scheduler, durable queue and upload/download checkpoints; bounded streaming and transfer verification.

The original Electron/Node source is reference material during migration. Neither Electron nor Node is shipped with the Tauri application. There is no Electron sidecar. Existing third-party reference code is not newly copied into Rust; Rust request shapes follow documented protocol evidence.

The initial upload path keeps numbered OSS PUT requests sequential; prefetch settings must not be described as true concurrent server uploads. Unverified concurrent PUT must not be enabled. Restarted tasks pause instead of automatically modifying cloud data.

A profile lock prevents two desktop processes from writing the same session/task store. Graceful exit cancels active executors and persists paused tasks. Forced termination recovers the last durable checkpoints. Only fully validated bytes are published, with no overwrite of an existing destination.
