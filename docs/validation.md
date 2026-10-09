# 验证矩阵

## 自动化基线

| 类别 | 覆盖内容 | 本地门禁 |
| --- | --- | --- |
| Rust 格式 | workspace 全量格式 | `cargo fmt --all -- --check` |
| Rust 静态检查 | 核心库全部 target | `cargo clippy -p pandock-core --all-targets -- -D warnings` |
| 核心单元测试 | 配置、路径、错误、资源版本和状态 | `cargo test -p pandock-core` |
| 百度 API 契约 | 授权、刷新、列表、元数据、上传、下载、移动、复制、删除 | 模拟 HTTP 服务 |
| WebDAV 契约 | 认证、PROPFIND、GET、HEAD、PUT、MKCOL、Range、条件请求和关闭 | 本地真实 HTTP 服务 |
| 桌面层 | Tauri 应用编译和配置一致性 | `cargo check -p pandock-app` |
| UI | TypeScript 与 Vite 生产构建 | `npm --prefix ui run build` |
| 保密 | 常见凭据模式与禁用文件 | `scripts/check-secrets.ps1` |

## 覆盖率门禁

`1.0.0` 前使用 `cargo-llvm-cov` 统计核心逻辑和关键契约路径，门禁阈值为 80%。当前基线未安装覆盖率工具，因此该项仍为未完成状态。

## 人工发布验证

正式发布前至少完成：

1. Windows portable ZIP 在干净目录解压并启动。
2. 首次配置、设备码授权、重启恢复和重新授权。
3. Windows 资源管理器挂载、浏览、上传、覆盖、下载、移动和删除。
4. 大文件分片上传和 Range 下载。
5. 中文、空格和长路径文件名。
6. 网络中断、端口冲突和 Token 刷新失败路径。
7. macOS 对应平台构建和 Finder 基本挂载（如发布 macOS）。
8. 验证结束后清理所有测试云端文件。

## 客户端矩阵

发布前按变更风险选择验证：

- Windows 资源管理器；
- macOS Finder；
- rclone；
- Cyberduck；
- 其他支持 WebDAV 的客户端。

至少记录客户端版本、操作、预期、实际结果、清理结果和脱敏日志。真实账号测试不得操作用户正式同步目录。

## 尚未自动化

以下项目当前不是持续自动化门禁：

- 真实百度账号端到端测试；
- 多客户端并发冲突；
- 长时间运行和内存稳定性；
- 100 MiB 以上文件；
- 百度上游故障注入；
- macOS 签名和公证。

这些项目必须在 `1.0.0` 前补充到本地发布清单或后续 CI。
