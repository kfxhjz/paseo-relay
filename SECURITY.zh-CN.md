# 安全策略

[English](SECURITY.md) | **简体中文**

英文 `SECURITY.md` 是规范版本；本文是中文说明。若两者冲突，以英文为准。

## 支持版本

安全修复会提供给最新发布的 minor 版本（当前 `0.1.x`）。开发在 `main`；如果漏洞
尚未发布修复，请也测试 `main`。

## 报告漏洞

**不要开公开 issue**。请使用本仓库 GitHub 的私密漏洞报告（Security → Report a
vulnerability），或通过仓库配置的安全联系方式联系维护者。

报告请包含：

- 受影响版本或 commit；
- 最小复现或协议序列；
- 你认为的影响（机密性、完整性、可用性或元数据泄漏）；
- 部署方式是直连还是经过边缘代理/负载均衡。

不要包含真实的 `serverId`、配对链接、凭证或客户 payload。需要展示协议字节时，
请使用隔离的测试 session。

我们会在 3 个工作日内确认，并与你协调披露时间线；请在修复发布前不要公开披露。

## 审计状态

本 relay **没有经过独立第三方安全审计**。仓库中有代码评审记录、真实 socket 测试、
fuzz targets 和 Paseo 兼容性证据；这些不能替代审计。v0.1.1 是早期软件，请根据自己的
部署阅读威胁模型。

## 威胁模型

Paseo 把 relay 当作 **不可信** 组件。消息内容由 Paseo client 与 daemon 之间的
端到端加密保护（Noise NK 或 legacy box）；relay 转发不透明帧，不需要也不能解密。
relay 能看到 IP、时序、帧大小、`serverId`/`connectionId` 以及明文握手中的公钥。

`serverId` 是 **bearer 路由秘密**，不是认证 token。知道它的人可以 attach 到该
session 并破坏可用性。它出现在 WebSocket URL query 中；请在边缘/代理日志中脱敏
query string，并把配对链接和二维码当作秘密。relay 按设计没有 relay-level 认证；
需要更强隔离的部署应放在 VPN/mTLS 或带认证的边缘之后。

范围内：

- 网络输入可触发的内存安全/panic；
- 绕过文档化上限或 ingress budget 的资源耗尽；
- 协议混淆、跨 session 投递或会破坏 Paseo session 的 close code 变化；
- 静默禁用文档化保护措施的 TLS/配置 bug；
- 意外记录 ID、payload 或秘密。

范围外（按设计，文档已说明）：

- relay 能观察元数据，或丢弃/延迟/重排帧；
- 依赖消耗已配置、已文档化容量的可用性攻击（请用边缘限流和监控）；
- 没有 relay 认证；
- Paseo client/daemon 的 E2EE 实现安全；
- 由你自己的部署日志导致的 `serverId` 泄漏。

## 加固基线

见 README 的 hardening checklist 和 [docs/zh-CN/deployment.md](docs/zh-CN/deployment.md)。
至少：终止 TLS、不要公开 `/metrics` 和 `/ready`、在边缘做 per-IP 限制、用提供的
非 root 用户以 read-only 运行容器，并保持 relay 和 Rust 工具链更新。连接上限要按
主机内存设置：一个 idle 已接受 socket 约占 23 KiB，按
`PASEO_RELAY_MAX_TCP_CONNECTIONS × ~30 KiB` 规划（内置 25,000 socket 默认值约需
700–800 MiB），小节点必须调低上限。
