# 开发说明：Tauri 迁移分支

当前基线、验证范围和未完成项以根目录 MIGRATION_STATUS.md 为准。历史 Electron 开发说明可从基线提交读取；src/main 仅用于协议对照。

## 命令

```sh
npm ci
npm start
npm run check
npm run test:rust
npm test
npm run verify:native
npm run build
```

需要 Node.js 24、Rust stable 和平台原生 Tauri 构建依赖。package-lock.json 与 Cargo.lock 均需保留。npm 脚本按副本路径隔离构建缓存，CARGO_TARGET_DIR 可覆盖。

macOS 默认构建 .app；其他平台按 Tauri 的平台打包目标构建，尚未在本轮实机验证。源码运行与打包产物都使用系统 WebView，不包含 Node.js/Electron。

## 模块

- crates/quark-core：独立账号会话、API、文件操作、设置与加密存储。
- crates/quark-transfer：独立持久化任务、上传下载及协议校验。
- src-tauri：原生窗口、登录、系统对话框、受限命令与生命周期。
- src/renderer：原有界面、Tauri bridge、虚拟列表。

core/transfer 不依赖 Tauri。登录窗口的远端页面没有本地系统命令权限；仅打包主界面允许调用桥接。Cookie 只保存在本机加密资料目录，禁止提交资料目录、签名链接或凭据。任务检查点也属于本机私密运行资料。

登录绑定在换号或重新扫码时改变、普通Cookie轮换时保留；中断任务重启后为暂停，不自动访问云端。退出先保存暂停状态，最长等待8秒；提交中断的不确定阶段保留待核对记录，不自动重复创建上传。

## 验证边界

npm test 使用受控 Chrome fixture；verify:native 实际启动 WKWebView 并走 Rust IPC。二者均不代表真实云端传输已通过。真实账号驱动 cloud_acceptance 只操作本轮新建UUID目录，未重新获用户指令不得追加传输或性能测试。

用户资料采用本机随机密钥加密，不调用系统钥匙串或要求系统密码；同目录密钥不能防御拥有相同文件权限的进程。备份需同时保管密文与密钥。
