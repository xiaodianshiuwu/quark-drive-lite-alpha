# 夸克网盘极简版 · Tauri 迁移开发版

原有界面迁移到 Tauri 2，系统 WebView 承载本地界面，Rust 核心负责网络、会话、文件操作和传输。应用运行不需要 Node.js 或 Electron；Node.js 仅用于开发工具。

本分支尚未合并、发布。当前进度、验证边界和恢复入口见 [MIGRATION_STATUS.md](MIGRATION_STATUS.md)。原版 Electron 实现保留在 `src/main` 供协议对照，不参与 Tauri 构建；历史使用说明见基线提交。

## 开发

需要 Rust stable、Node.js 24 和对应平台的 [Tauri 2 原生构建依赖](https://v2.tauri.app/start/prerequisites/)。

```sh
npm ci
npm start
```

```sh
npm run test:rust
npm test
npm run verify:native
npm run build
```

`npm start` / `npm run build` 自动把 Rust 产物放到当前用户的构建缓存中，避免 macOS 云端文档目录引入签名扩展属性；也可用 `CARGO_TARGET_DIR` 指定目录。纯 Rust 核心可单独 `cargo test -p quark-core -p quark-transfer --locked`。

## 数据与登录

Tauri 使用独立应用资料目录 `cn.quarkdrive.lite.tauri`，不会自动复制原客户端登录资料。点击右上角登录，在独立原生登录窗口扫码；Cookie 仅由 Rust 读取及发送。远端登录网页没有本地文件或传输权限。

会话采用 QCK2 AES-256-GCM 加密，本地随机密钥与密文均保存在用户资料目录，日常启动不调用系统钥匙串或要求系统密码。同目录密钥不能防止拥有相同文件权限的其他进程；备份时需同时保管密文和密钥。

任务与分片进度保存在应用资料目录，重启后中断任务显示为暂停，由用户继续。上传支持 SHA-1 前缀状态握手的并行 PUT（默认 3），并保留顺序兼容设置及协议拒绝时的降级。下载使用 1 MiB 分片、受控并发、强 ETag 绑定、Range/长度及回读校验，安全发布时不覆盖已有文件。重新扫码会隔离旧登录的任务；同一登录态重启可继续。真实传输验证的剩余限制见迁移状态。

## 安全测试边界

真实账号测试仅能操作本轮新建的唯一测试目录及其子项。不得清理用户原有文件；不得提交 Cookie、签名直链或用户资料目录。Windows/Linux 原生构建与运行验证以迁移状态文档的实际记录为准。

## 协议与许可

接口证据见 [docs/API_NOTES.md](docs/API_NOTES.md)、桥接契约见 [docs/IPC_CONTRACT.md](docs/IPC_CONTRACT.md)。原型和第三方协议参考保留各自许可；本应用源码采用 MIT，详见 [LICENSE](LICENSE)。
