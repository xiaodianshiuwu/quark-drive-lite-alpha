# Tauri 2 + Rust 迁移交接

更新：2026-10-15。按用户最新要求只做基本功能审查、测试和打包，不追加云端传输、性能或大文件测试。不使用子智能体继续工作。

## 基线与副本

- 远端：xiaodianshiuwu/quark-drive-lite，默认分支 main；本轮已 fetch 核对。
- 干净基线：../quark-drive-baseline-20261015
- 基线 SHA：06ce2cf5e92c1a22903da41ce66e19d2b184f1de
- 开发副本：../quark-drive-tauri-20261015-2051
- 迁移分支：codex/tauri2-rust-20261015-2051
- 基线与副本均有独立 .git，不共享工作目录。原版 ../qck、另一迁移副本的未提交修改均保留。
- 本副本重新 npm ci；未复制 node_modules、构建缓存或原应用凭据。后续扫码产生的登录态保存在独立 Tauri 用户资料目录。
- 不合并 main，不推送、不发布。

## 已实现

- Tauri 2 原生外壳加载现有本地界面；原生文件选择、拖放、定位、菜单及退出；不依赖 Electron 运行时。
- 独立 quark-core：账号会话、Cookie 域/path 隔离、API、分页/搜索/文件操作、设置和 QCK2 AES-GCM 本地持久化。日常启动不调用系统密码或钥匙串。
- 独立 quark-transfer：任务持久化、重启暂停、继续/重试/取消；登录绑定跨重启和 Cookie 轮换保留，重新登录会隔离旧任务，防止换号续传。
- 下载：1 MiB 共享工作队列，按大小选择连接数，自动/顺序/并行策略，单文件最多16、全局32连接；受控流式落盘。强 ETag 绑定、Range/Content-Range/长度校验、完成后 MD5/SHA-1/SHA-256 全文件回读。有服务端摘要时另行比较；无摘要时不声称与源文件已独立校验。
- 下载续传逐片 SHA-256 核对；损坏片重新下载；弱/缺失 ETag 回退完整响应；最终发布不覆盖已有用户文件。
- 上传：秒传、真实 SHA-1 前缀状态和 X-Oss-Hash-Ctx 并行 PUT，默认3；设置为1使用兼容顺序预登记/PUT。协议拒绝时在同一 upload_id 内顺序回退。保留源文件变化、逐片 MD5/ETag、提交歧义和云端结果确认检查；全局上传缓冲上限64 MiB。
- 文件列表虚拟化及账号/任务推送竞态修复。

## 复用与许可

已核查 ~/Desktop/deepseekharness/quark-eval 的评估报告、许可矩阵与自研脚本。复用该项目自研 SHA-1 状态导出及协议事实，未复制 AGPL/GPL 上传器。评估中的旧 IMPLEMENTATION_PLAN 仍有 Electron 建议，本分支遵循用户明确的 Tauri 迁移要求。原 Electron 源码留在 src/main 供协议对照，不参与打包。

## 验证记录

- npm ci：通过，依赖锁定。
- npm run test:rust -- --offline：23 项 core、17 项 transfer；全部通过，结果见 .work/rust-final.log。
- QCK_BROWSER_PATH 指定本机 Chrome 后 npm test：11/11、0跳过，含5100项实际DOM虚拟滚动、文件操作、任务交互和设置。为受控fixture，不代表真实云端。
- npm run verify:native：真实 WKWebView/本地界面/Rust IPC 12/12；独立临时资料目录，报告 .work/native-smoke.json。
- 用户已在新 Tauri 原生窗口扫码。真实判活、列表、创建本轮唯一测试目录及清理接口已调用。
- 曾开始32 MiB上传，分片阶段通过，完成请求返回HTTP400。发现完成XML必须保留服务端ETag大小写，已修并加本地HTTP回归。按用户最新要求不追加真实回传复测；32 MiB全量下载/哈希回读没有完成，不能宣称真实传输验收通过。
- 本轮云端目录仅为 .work/cloud-acceptance-20261015/ledger.json 记录的唯一UUID目录。已通过只读列表确认目录不存在；清理记录与确认结果保存在同一文件，不触碰用户原有数据。
- npm run build：macOS arm64 .app；构建成功，约8.83 MiB；结果见 .work/build-final.log。已修复本机打包资源签名封装并通过 codesign --verify --deep --strict；实际启动产物并确认进程正常运行。仅本机临时签名，未公证、未发布。

## 启动与恢复

在开发副本执行：

```sh
npm ci
npm start
```

基本检查：npm run test:rust、npm test、npm run verify:native；打包：npm run build。
构建缓存由 scripts/build-env.js 按副本路径选择，避免 Documents 扩展属性污染 macOS 签名；也可显式设置 CARGO_TARGET_DIR。

真实账号验收驱动：crates/quark-transfer/examples/cloud_acceptance.rs。它只修改本轮UUID目录，保留日志归属记录，支持 --cleanup 和 --check-cleanup。仅在用户重新要求时运行新的传输验收。

## 剩余限制

- 上传ETag修复后的真实完成、真实下载、云端暂停/取消/恢复未复测。
- 重新扫码后旧任务会被登录绑定隔离，当前需重新创建；服务器缺少稳定用户ID时不冒险自动迁移旧任务。
- 多GB、真实链路强退恢复、Windows/Linux/macOS Intel 原生运行与同功能性能对照均未运行。
- 不把早先评估目录的实验数字当作本产品验证结果。

下一条命令：npm start。继续开发前先看 git status，本轮没有要求追加传输压测。
