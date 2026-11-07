# 选择 relay 部署方式

[English](../comparison.md) | **简体中文**

这是决策指南，不是性能对比。说明哪类用户适合哪种部署模型，以及本项目 **不做什么**。

## 快速决策

| 你是... | 建议 |
|---|---|
| 一个人在自己的笔记本/NAS/VPS 上跑 Paseo，不想维护 Erlang | **本 relay**，单节点放在 Caddy/边缘 TLS 后 |
| 小团队，一个 daemon、几个 client | **本 relay**，单节点；监控容量 |
| 需要多 relay 节点且不想额外配置边缘 | **Elixir relay**（Syn ownership/409 reroute）或托管 relay |
| 完全不想自己运营公网 endpoint | **Paseo 托管 relay** |
| 安全敏感、希望审计 relay | **本 relay**（小型 Rust 代码、有界模型），但先读 [SECURITY.zh-CN.md](../../SECURITY.zh-CN.md) |
| 要面向公众运营 relay | **本 relay** 可用，但请读 [operator-policy.md](operator-policy.md)，提供边缘限流和隐私政策 |

## 对比维度

- **运行时/运维**：单个 Rust 静态二进制 vs BEAM release + Cowboy vs 托管。
- **集群模型**：边缘一致性哈希（本 relay）vs Syn ownership/reroute（Elixir）vs 托管。
- **失败模型**：节点 drain = 客户端重连；没有透明迁移。
- **内存/限制**：本 relay 显式配置上限、读 payload 前记账；Elixir 用 BEAM +
  capacity ledger。
- **TLS**：本 relay 可原生 rustls 或边缘终止；Elixir 通常在边缘。
- **认证**：两者按设计都没有 relay 认证；Paseo E2EE 保护内容，`serverId` 是 bearer
  secret。
- **生态**：Elixir relay 有内置集群行为；本项目面向更小、可审计的单节点部署和清晰
  的 wire contract。

## 本项目不做什么

- 没有内置集群、ownership handoff、reroute 或 split-brain 处理；
- 没有 relay-level 认证、per-user 配额或 token 撤销；
- 不做 payload 检查/解密/存储/重放保护；
- 没有透明 session 迁移或 HA 持久化；
- 除 health/ready/metrics/version 外没有管理 HTTP API。

如果这些是硬需求，不要部署本 relay；请用 Elixir relay 或托管 relay。这是
[完整中文设计](../../DESIGN.zh-CN.md)（英文规范 [DESIGN.md（英文）](../../DESIGN.md)）中明确的范围取舍。

## 迁移

从 Elixir relay 迁移见 [migration.md](migration.md) 或
[migration.md](migration.md)。预期 Paseo 客户端会重连一次。
