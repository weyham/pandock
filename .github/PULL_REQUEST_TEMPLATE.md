## 目标

<!-- 说明本 PR 解决的问题。 -->

## 变更范围

<!-- 列出主要文件和契约变化。 -->

## 非目标

<!-- 说明本 PR 明确不处理的内容。 -->

## 验证

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy -p clouddock-core --all-targets -- -D warnings`
- [ ] `cargo test -p clouddock-core`
- [ ] `cargo check -p clouddock-app`
- [ ] UI 生产构建通过
- [ ] 敏感信息扫描通过

测试结果：

```text

```

## 风险与回滚

- 风险：
- 回滚：

## 安全与保密

- [ ] 未提交 secrets、用户数据、日志、数据库、备份或签名材料
- [ ] 错误与日志不包含凭据
- [ ] 新增依赖已检查来源和许可证
