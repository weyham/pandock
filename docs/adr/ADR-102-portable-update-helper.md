# ADR-102：portable app/ 使用独立 clouddock-updater.exe

- 状态：修订草案，待审核
- 日期：2026-09-24
- 关联：Stage B

## 背景

Windows 不允许覆盖正在运行的 exe。Tauri updater 主要面向安装器，而 CloudDock 1.0.0 是 app/ 绿色 portable 布局。需要保证云盘数据、配置和系统凭据在更新后不丢失，并在替换失败时回滚。

## 决策

采用独立 clouddock-updater.exe，而不是同一个 clouddock.exe 的 updater 模式。

理由：

- helper 职责单一，不加载 Tauri/WebView；
- 主程序被替换时 helper 可以继续运行；
- 更容易签名、审计、回滚和测试；
- 未来安装器模式可以复用更新规划逻辑。

## Helper 位置

必须从：

~~~text
app/updates/staging/<version>/clouddock-updater.exe
~~~

运行。

禁止：

- 从 app/clouddock.exe 自身运行；
- 从 app/ 根目录旧 helper 运行；
- 从非 staging 的未签名位置运行。

## 替换时序

1. 主程序下载、验签、SHA-256 校验和安全解压到 staging。
2. 停止健康检查和 WebDAV，释放 19090。
3. 写 journal = prepared。
4. 启动 staging 中的 clouddock-updater.exe。
5. 主程序退出。
6. helper 等待 parent PID 消失。
7. helper 获取 app/.update.lock。
8. helper 校验 journal、helper 协议、目标 app 和 staging。
9. 重命名旧程序文件到 rollback。
10. 移动新文件到 app/。
11. 启动新 clouddock.exe，传入 readiness-token。
12. 新版本写 readiness marker。
13. helper 写 completed，清理旧文件和 staging。
14. 失败时恢复旧文件并尝试启动旧版本。

## 替换白名单

允许：clouddock.exe、WebView2Loader.dll、VERSION.txt、LICENSE.txt 和 manifest allowlist 运行时文件。
禁止：app/data/、config.json、uploads/、backups/、keyring、用户文件。

## Journal 恢复

状态：

~~~text
prepared
parent_exited
lock_acquired
backup_complete
replace_started
replace_complete
launch_started
ready
rollback_started
rolled_back
completed
failed
~~~

恢复规则：

- prepared 前：清理 staging；
- backup_complete 前：旧文件仍在，可重试；
- backup_complete 到 replace_complete：使用 rollback 恢复；
- replace_complete 到 ready：启动失败则回滚；
- ready 后：成功，清理；
- 主程序启动发现 stale journal：进入修复提示，不自动删除 data/。

## 防降级

- 自动渠道禁止降级；
- 语义版本必须递增；
- 仅允许本地 journal 回滚到更新前版本；
- 强制/测试降级只允许编译期 dev-updater feature；
- 正式构建不得启用 dev-updater。

## 平台范围

- Windows portable：自动原位更新。
- macOS：只检查更新和手动下载，不自动替换。
- Linux：不在 1.0.0 范围。

## 后果

正面：

- 真正支持 portable 更新；
- data/ 和凭据不丢失；
- 支持 journal 和回滚；
- 不改变 portable 分发方式。

负面：

- 需要额外 helper 和签名；
- 需要 Windows e2e；
- 可能触发杀毒软件；
- 需要处理更新中断和恢复。
