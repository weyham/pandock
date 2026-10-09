# WebDAV 行为

## 路径映射

WebDAV 根目录映射到：

```text
/apps/<应用名称>/
```

`/dav/` 与实际应用目录之间不暴露绝对网盘路径。

## API 对应关系

| WebDAV 操作 | 百度开放平台操作 |
| --- | --- |
| `PROPFIND` | `list` / `filemetas` |
| `GET` | `filemetas` 获取 `dlink` 后下载 |
| `HEAD` | `filemetas` |
| `PUT` | `precreate` -> `locateupload` -> `superfile2` -> `create` |
| `MKCOL` | `create`，`isdir=1` |
| `DELETE` | `filemanager`，`opera=delete` |
| `MOVE` | `filemanager`，`opera=move` |
| `COPY` | `filemanager`，`opera=copy` |

## 语义保证

- 对已存在目录执行 `MKCOL` 返回冲突语义，不自动改名。
- 父目录不存在时返回 `404`，不把上游错误伪装成成功。
- `PUT` 只有在三阶段上传和元数据确认全部成功后才返回成功。
- `GET` 支持 Range；上游忽略 Range 时，只有从偏移 0 开始的请求可在严格长度校验后截取。
- `HEAD` 返回稳定的长度和可用的 ETag。
- 条件请求不满足时返回 `412`。
- 解压、删除和覆盖操作均采用真实 WebDAV 语义，不静默丢失冲突。
- 所有路径在进入云端前执行规范化，拒绝遍历、NUL 和越界路径。

## 上游错误映射

核心错误分类包括：

- 不存在；
- 已存在；
- 父目录缺失；
- 权限或认证失败；
- 配额不足；
- 限流；
- 暂时故障；
- 不支持的操作；
- 无效请求；
- 未知错误。

WebDAV 层将稳定错误类别映射为协议状态码；上游错误文本不能包含 Token 或 Secret。

## 已知边界

- 百度开放平台的文件管理操作可能是异步完成。实现会在返回成功前执行有限确认，避免假成功。
- 百度 `dlink` 的 Range 行为不完全一致，客户端必须能够处理严格校验后的失败。
- 并发写冲突、目录级 MOVE/COPY 和长时间锁语义仍需按验证矩阵持续覆盖。
- 真实账号测试会消耗配额并产生云端写入，必须在隔离目录中执行并清理。
