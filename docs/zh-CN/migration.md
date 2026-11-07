# 从 Elixir relay 迁移

[English](../migration.md) | **简体中文**

本 Rust relay 在 **单节点** 上 wire-compatible，但不兼容 Elixir relay 的集群/
ownership 部署方式。

## 行为变化

- 没有 Syn、ownership registry、409 `x-reroute-target` reroute 或跨节点迁移。
  多节点请使用边缘 LB 对 `serverId` 做一致性哈希。
- 没有 BEAM release、EPMD、distribution、`RELEASE_*` 变量或 cookie。
- `serverId` / `connectionId` 校验为可打印 UTF-8；非法值返回 HTTP 400，而不是被路由。
- 新增进程内限制：per-IP WebSocket 上限、session 上限、每 connection client 上限、
  帧/消息读取 deadline。
- 可用 rustls 原生 TLS；否则在边缘终止 TLS。
- SIGTERM/SIGINT 执行有界 drain；`PASEO_RELAY_DRAIN` 启动即 unready。

## 环境变量映射

| Elixir | Rust |
|---|---|
| `PASEO_RELAY_HOST` / `PORT` | 相同 |
| `PASEO_RELAY_ACCEPTORS`, `CONNECTIONS_PER_ACCEPTOR` | `PASEO_RELAY_MAX_CONNECTIONS`, `PASEO_RELAY_MAX_TCP_CONNECTIONS` |
| `PASEO_RELAY_INGRESS_BUDGET_BYTES`, `INGRESS_WEIGHT` | 相同 |
| `PASEO_RELAY_DELIVERY_TIMEOUT_MS`, `DATA_ATTACH_TIMEOUT_MS`, `CONTROL_QUEUE_BYTES` | 相同 |
| `PASEO_RELAY_WEBSOCKET_MAX_HEAP_WORDS`, `MEMORY_WATERMARK_BYTES`, `TCP_RECEIVE_BUFFER_BYTES` | 移除（Rust 内存模型；改用显式上限） |
| `PASEO_RELAY_CLUSTER_*`, `OWNERSHIP_TARGET`, `REROUTE_HEADER`, `MIN_CLUSTER_SIZE` | 移除（单节点；边缘哈希） |
| `RELEASE_NODE`, `RELEASE_COOKIE`, `RELEASE_DISTRIBUTION` | 移除（无 Erlang） |
| `PASEO_RELAY_DRAIN` | 语义相同（启动期） |
| `PASEO_RELAY_LOG` | 相同（`tracing` EnvFilter），另有 `PASEO_RELAY_LOG_FORMAT=text/json` |

## 迁移步骤

1. 在私网部署一个 Rust relay 节点；验证 `/health`、`/ready` 和一次真实配对。
2. 增加带 TLS 和 `serverId` 一致性哈希的边缘 LB；用真实 Paseo client/daemon 验证。
3. 逐步切流。旧 relay 上的 session 会 drain；客户端重连，新 relay 创建新 session
   （E2EE 配对仍然有效，因为它是端到端，不是 relay 状态）。
4. 更新 dashboard/告警：指标族和名称不同；见
   [operations.md](operations.md)。
5. 移除旧 relay、集群配置和 Erlang distribution 暴露。

迁移对客户端来说是一次重连，不是透明 handoff。
