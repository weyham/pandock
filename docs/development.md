# 开发与测试

## 环境

- Rust stable
- Node.js 20+
- Windows WebView2 或 macOS WebKit 系统组件
- Windows 代码签名和 macOS 签名、公证材料仅用于正式发布，不进入仓库

## 首次准备

```powershell
npm --prefix ui ci
cargo fetch
```

`Cargo.lock` 和 `ui/package-lock.json` 必须提交，以保证可复现构建。

## 快速检查

```powershell
cargo fmt --all -- --check
cargo clippy -p pandock-core --all-targets -- -D warnings
cargo test -p pandock-core
cargo check -p pandock-app
npm --prefix ui run build
```

完整本地门禁：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\check.ps1
```

如依赖已经安装且不需要重新执行 `npm ci`：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\check.ps1 -SkipUiInstall
```

## 开发运行

```powershell
cargo run -p pandock-app
```

默认不自动显示设置窗口。可从托盘菜单打开，或通过开发环境变量控制启动行为，具体值不得写入仓库。

## 开发部署门禁（`app\` 运行目录）

`app\` 实例的更新只允许走 `scripts/deploy-dev.ps1`（原子、白名单、备份、精确路径停进程）。
在此基础上，每次部署必须**同时**满足：

1. **`SourceDir` 用绝对路径**，并记录源的「身份」：绝对路径 + 大小/mtime + `sha256`；
2. 部署后校验 `app\<exe>` 的 `sha256` **等于被审产物的 sha256**（不是"源=目标"式的自校验）；
3. 校验**只有新构建才满足的特征**，至少覆盖：
   - 运行日志出现新版本才有的日志行（例如 `native-sync notifications effective=`）；
   - `data/native-sync/sync.db` 的 `schema_migrations` 含本次迁移版本号、新表/新列存在；
   - 需要时在二进制里查特征串。

任一不满足即视为**部署失败**：停止、按回滚方案恢复、不要重试。
`data\` 永不被部署触碰；非白名单文件（`pandock-shell.dll` 等）不得在验收窗口内改动。
详见 `docs/问题与解决.md` 的「开发部署用相对路径命中陈旧构建」条目。

## 测试层次

1. Rust 单元测试：路径规范、错误分类、配置校验、状态映射和修订标识。
2. HTTP/API 契约测试：使用本地模拟服务和 `wiremock` 验证百度 API 交互。
3. WebDAV 语义测试：通过真实 HTTP 请求验证状态码、Range、ETag、条件请求和认证。
4. UI 构建测试：TypeScript 类型检查和 Vite 生产构建。
5. 人工端到端测试：真实百度账号、WebDAV 客户端和文件管理器。

## 覆盖率门禁

`1.0.0` 正式发布前必须安装并使用 `cargo-llvm-cov`，核心逻辑和关键契约路径覆盖率不得低于 80%。当前本地门禁尚未强制覆盖率，因为构建机未安装该工具；这是正式版前的明确缺口。

## 分支和合并

- 每个功能使用独立 worktree 和特性分支。
- 合并前 rebase 最新 `main`，重新执行完整门禁。
- PR 必须给出测试结果、覆盖率、风险和回滚方案。
- `1.0.0` 前由维护者本地完成检查和发布，不启用工作流。

## 版本管理

版本必须同时出现在：

- 根 `Cargo.toml` 的 workspace 版本；
- `src-tauri/tauri.conf.json`；
- 根 `package.json`；
- `ui/package.json` 与 `ui/package-lock.json`。

使用脚本统一更新：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\set-version.ps1 -Version 0.1.0
```
