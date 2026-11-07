# Paseo Relay 中文文档

[English README](../../README.md) | [English docs](../README.md) | **简体中文**

这里是 Paseo Relay 的中文文档索引。英文文档是规范来源；中文翻译会在发布前同步。
如果两者出现不一致，以英文文档为准。

## 入门

- [中文 README](../../README.zh-CN.md) — 项目介绍、快速开始、与 Paseo 配合使用
- [五分钟 VPS 快速部署](quickstart.md) — Caddy 自动 HTTPS + Compose
- [选择 relay 部署方式](comparison.md)
- [运营公共 relay](operator-policy.md)
- [协议与 close code](protocol.md) — `/ws` query、v1/v2 语义、帧限制、错误码
- [完整设计（中文）](../../DESIGN.zh-CN.md) — 架构、不变量、失败语义、安全设计
- [设计摘要](design-summary.md) — 快速摘要（完整设计见上）
- [运行时配置](configuration.md) — 全部环境变量、CLI、健康检查

## 部署与运维

- [部署指南](deployment.md) — TLS、Nginx/HAProxy/Kubernetes、drain、容量
- [运维、指标与告警](operations.md) — 指标含义、告警建议、排障
- [从 Elixir relay 迁移](migration.md) — 配置映射和行为变化

## 兼容性与性能

- [Paseo 兼容性](compatibility.md) — 特性矩阵、与 Elixir relay 的差异、E2E 证据
- [Benchmark 方法与数据](benchmarks.md) — 可复现的负载测试
- [常见问题](faq.md)
- [2026-11-07 Paseo E2E 证据](evidence/paseo-e2e-2026-11-07.md)

## 安全与贡献

- [安全策略](../../SECURITY.zh-CN.md)
- [支持渠道](../../SUPPORT.zh-CN.md)
- [贡献指南](../../CONTRIBUTING.zh-CN.md)
- [文档与翻译策略](translation.md)
- [架构决策记录（中文摘要）](adr-summary.md) / [英文原文](../adr/)

## 英文规范文档

- [DESIGN.md（English）](../../DESIGN.md)
- [protocol.md（English）](../protocol.md)
- [configuration.md（English）](../configuration.md)
- [deployment.md（English）](../deployment.md)
- [compatibility.md（English）](../compatibility.md)
- [operations.md（English）](../operations.md)
- [migration.md（English）](../migration.md)
- [benchmarks.md（English）](../benchmarks.md)
- [faq.md（English）](../faq.md)
- [quickstart.md（English）](../quickstart.md)
- [comparison.md（English）](../comparison.md)
- [operator-policy.md（English）](../operator-policy.md)
- [translation.md（English）](../translation.md)
