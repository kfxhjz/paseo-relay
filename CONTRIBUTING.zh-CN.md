# 贡献指南

[English](CONTRIBUTING.md) | **简体中文**

感谢帮助改进 Paseo Relay。这是一个小型、安全敏感的数据面组件：正确性、内存有界和
清晰失败模式比功能数量更重要。

## 开发环境

- Rust `1.99.1`（`rust-toolchain.toml` 固定，rustup 会自动安装）
- cargo fmt、clippy 和测试工具链
- Docker 仅用于可选的容器检查

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
python3 scripts/check-docs.py   # 本地链接 + 中英文配对检查
scripts/ci.sh          # 与 CI 相同，包含容器 smoke test
```

测试使用真实 listener 和真实 WebSocket client（`tokio-tungstenite`），不是 mock。
新增协议行为请加集成测试到 `tests/relay_integration.rs`，纯逻辑加模块内单元测试。

## 变更规范

- **安全和行为变更先写失败测试**（red test），再修复，并在 PR 描述中记录证据。
- 不要在没有解释新边界的情况下移除 bound、timeout 或 cap。
- 不要在日志中记录 `serverId`、`connectionId`、payload 或 TLS key material。
- 保持 payload 路径不透明；唯一允许的协议检查是有界的 `hello`/`e2ee_hello` 卫生检查。
- 保持 Paseo wire 行为。如果刻意不同于 Elixir relay，请在
  `docs/compatibility.md` 和 README 中说明。
- 新依赖需要理由、与 Apache-2.0 兼容的许可证，并通过 `cargo audit`/review。

## 对 Paseo 做兼容性测试

最强的兼容性测试是 Paseo 自带的 relay E2E。它可以指向已运行的 relay：

```sh
# 在已安装依赖的 Paseo 浅 checkout 中：
PASEO_RELAY_E2E_PORT=4000 FORCE_RELAY_E2E=1 \
  npx vitest run packages/server/src/server/daemon-e2e/relay-transport.e2e.test.ts --maxWorkers=1
```

详见 [docs/zh-CN/compatibility.md](docs/zh-CN/compatibility.md)。如果改动 wire
行为，请在环境允许时运行它。

## 文档与翻译

见 [docs/zh-CN/translation.md](docs/zh-CN/translation.md) 和 [docs/zh-CN/translation.md](docs/zh-CN/translation.md)。

## Pull request

- 描述用户可见变化和它解决的风险；
- 附上运行过的命令和结果；
- 重构和行为变更分开；
- runtime contract、兼容性或安全模型变化时更新 README/DESIGN/docs；
- 提交即表示同意以仓库的 Apache-2.0 许可证贡献。

## 行为准则

本项目遵循 [CODE_OF_CONDUCT.zh-CN.md](CODE_OF_CONDUCT.zh-CN.md)。保持友善、善意假设，讨论
聚焦技术。
