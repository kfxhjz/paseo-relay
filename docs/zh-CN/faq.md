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
32 MiB−14、v2 control 帧 64 KiB、delivery timeout 30 s，以及每条消息最多 1024 个
分片。payload 字节按 8 KiB chunk 在到达时按 `chunk_bytes × weight + 192` 加权单位
计费（192 为每 chunk 记账）；每消息固定 256 字节开销只计一次；单个连接最多持有
`min(budget, 4 × 一条最大消息的计费)` 的 ingress 预算。

部分策略固定、不可配置：每条消息 1024 分片上限、v2 control 帧 64 KiB 上限、
handshake 卫生检查 64 KiB 上限、每连接 4× ingress 上限，以及 upgrade 前 HTTP 上限
（16 KiB 读缓冲、16 KiB header、64 个 header 字段）。其余都可配置；见
[configuration.md](configuration.md) 的 runtime contract。

## 需要多少内存？

idle 已接受 socket 是主要开销：relay 在边缘后为明文时（hyper 读写缓冲加 task）每个
约 23 KiB，原生 TLS（rustls 缓冲）时约 33 KiB，因此按
`PASEO_RELAY_MAX_TCP_CONNECTIONS × ~30 KiB`（明文）或 `× ~35 KiB`（原生 TLS）加
in-flight ingress 预算规划。内置 25,000 TCP / 20,000 WebSocket 默认值在无 payload
流量前就需要约 700–800 MiB（明文）或约 850–900 MiB（原生 TLS）；512 MB 主机必须
调低上限（Compose 示例使用 5,000 / 4,000 / 4,000）。upgrade 前 HTTP 路径还受
hyper 限制：16 KiB 缓冲/header、64 个 header。
