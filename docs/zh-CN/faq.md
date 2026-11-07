# 常见问题

[English](../faq.md) | **简体中文**

## 这是 `relay.paseo.sh` 背后的 relay 吗？

不是。这是一个社区维护的单节点实现。Paseo 使用的托管 relay 是另一套部署（目前是
Elixir 服务）。本项目在单 relay endpoint 上 wire-compatible，面向自托管。

## 还需要同时运行 Elixir relay 吗？

不需要。这是独立 relay。Paseo daemon/client 通过正常的 relay 配置
（`PASEO_RELAY_ENDPOINT`、TLS、配对）连接它，不需要 Erlang/OTP。

## 可以运行多个节点吗？

可以，但必须在边缘 LB 上按 `serverId` query 参数做一致性哈希。relay 没有内置集群
或透明 failover；节点 drain 时客户端重连。见
[deployment.md](deployment.md)。

## Paseo 已经 E2EE，为什么还要 TLS？

公网必须 TLS。E2EE 保护 payload 内容，但不保护 `serverId`、元数据或可用性。
请在边缘终止 TLS，或使用原生 rustls。

## 会解密或记录消息吗？

不会。payload 原样转发；唯一的内容检查是有界的 `hello`/`e2ee_hello` key 卫生检查。
`serverId`、`connectionId` 和 payload 不会写日志。

## 为什么 relay 不做认证？

Paseo 的威胁模型把 relay 当作不可信；内容安全由 E2EE 负责。`serverId` 是 bearer
secret。见 ADR 0003。

## 如何让 Paseo 连到这个 relay？

在 daemon 上设置 relay endpoint/TLS 环境（或 `config.json`），然后
`paseo daemon pair --relay` 配对。见中文 README 的“与 Paseo 配合使用”。

## 怎么知道它兼容？

Paseo 自带 relay 传输 E2E
（`relay-transport.e2e.test.ts`），可以通过 `PASEO_RELAY_E2E_PORT` 指向外部 relay。
兼容性工作流固定 Paseo commit 并运行它；见
[compatibility.md](compatibility.md)。

## 为什么用 Rust 而不是 Elixir？

单静态二进制、无 BEAM/EPMD/cookie/distribution、原生 TLS、显式有界内存记账、小容器。
代价是没有内置集群 ownership；亲和性交给边缘。见 ADR 0001。

## 默认限制是多少？

默认 20,000 WebSocket、20,000 session、512 MiB 加权 ingress、单消息
32 MiB−14、v2 control 帧 64 KiB、delivery timeout 30 s。都可配置；见
[configuration.md](configuration.md)。
