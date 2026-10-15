# Tauri 2 + Rust migration

Updated: 2026-10-15. Development only; no merge or release authorized.

## Recovery entry

- Baseline: `../qck-baseline-20261015-1322` (independent clean Git clone)
- Baseline main: `06ce2cf5e92c1a22903da41ce66e19d2b184f1de`
- Development: this independent clone, branch `codex/tauri2-rust-20261015-1322`
- Original `../qck` preserved; `../qck-public` fast-forwarded to current origin/main before cloning.
- Dependencies, build caches and real account credentials were not copied.

## In progress

Tauri native shell and password-free native-webview login; standalone Rust core for sessions/API/settings; standalone Rust transfer engine with persistent checkpoints; bridge retaining original UI and virtualized file list.

Next command: `CARGO_TARGET_DIR="$HOME/Library/Caches/quark-tauri-20261015-1322/target" cargo test --workspace --locked`, then native login and transfer acceptance.

## Verified milestones

- Workspace compile and first native app launch passed.
- Native WKWebView smoke: 11/11 passed (`.work/native-smoke.json`), real local interface and Rust IPC, settings persistence, auth/error bridge and view switching.
- Rust core: 11/11 passed; the first transfer implementation has no transfer tests yet and is under review.
- Renderer: 5/5 passed, including 5100-row virtualized DOM in an isolated headless Chrome fixture.
- No real cloud login or upload/download migration test has run yet. No packaged app has been validated yet.

## Validation boundary

Real account login will require a QR scan in the new native login window. Cloud writes must be limited to a unique test directory created for this migration run. No multi-GB cloud validation has been performed.
