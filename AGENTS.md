# AGENTS.md

本文件为 AI 编码助手（Codex 等）在本仓库工作时的约定。

## 项目

Pandock：百度网盘开放平台应用目录的本地桥接工具（WebDAV + Windows 占位符同步）。
技术栈：Tauri 2 + Rust（core / nativesync / src-tauri / updater 四个 crate）+ React/TypeScript 设置窗口。

## 硬性规则

1. **绝不把密钥写入仓库**：token、私钥、密码不得出现在代码、文档、日志或提交信息中；
2. 所有 release 构建必须走 `scripts/build-velopack.ps1`，或显式携带 `--features custom-protocol`——裸 `cargo build --release` 的产物设置页会白屏（Tauri devUrl 判定）；
3. 提交前必须通过 `scripts/check.ps1`（fmt、clippy -D warnings、测试、覆盖率、UI 构建、密钥扫描）；
4. 契约（WebDAV 行为、更新 feed、配置结构）变更必须同步修改测试与相关文档；
5. 文档为追加式维护：并集合并，不整段覆盖；
6. 面向用户的可见名称一律为 **Pandock**。；
7. **运行目录 `app\` 的实例更新只允许走 `scripts/deploy-dev.ps1`**（原子、白名单、备份、
   精确路径停进程）；禁止手工 kill+copy；`app\data\` 永不被部署触碰。

## 目录

- `core/` 无 UI 依赖的核心逻辑（OAuth、百度 API、WebDAV 文件系统、更新校验）
- `nativesync/` Windows Cloud Files 同步引擎（Windows-only，macOS 构建须跳过）
- `src-tauri/` 桌面外壳与系统集成
- `updater/` 便携版更新 helper
- `ui/` 设置窗口前端
- `docs/adr/` 架构决策记录（追加式，不删除、不篡改历史条目）
