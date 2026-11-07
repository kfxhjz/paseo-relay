# 运维

[English](../operations.md) | **简体中文**

## 运行时端点

| 路径 | 用途 | 暴露范围 |
|---|---|---|
| `/health` | liveness；进程存活期间 200（draining 也是） | 编排/LB |
| `/ready` | 仅在愿意接受新工作时 200；draining、shutting down 或 WebSocket/session 容量满时 503 | LB readiness、监控 |
| `/metrics` | Prometheus 文本（只有聚合值，无 ID） | 仅内网监控 |
| `/version` | build version + git revision | 内网/可选 |
| `/ws` | Paseo WebSocket 协议 | 公网（TLS/LB 后） |

所有端点共享同一 listener。运维端点没有认证；不要把 `/metrics` 或 `/ready` 暴露到
公网。

## 节点 drain

1. 从负载均衡摘除节点（或先让它 unready）。
2. 等 LB 不再发送新连接（观察 readiness）。
3. 发送 `SIGTERM`。relay 停止 accept，`/ready` 变 503，现有 session 保留
   `PASEO_RELAY_SHUTDOWN_GRACE_MS`，然后以 `1012` 关闭并退出。
4. 确保编排器 grace 大于 relay grace
   （`terminationGracePeriodSeconds`/`TimeoutStopSec` > `PASEO_RELAY_SHUTDOWN_GRACE_MS`）。
5. 只在最后手段使用 `SIGKILL`；它会跳过 close frame，客户端只能立刻重连。

`PASEO_RELAY_DRAIN=true` 启动即 unready。它适合节点只接收旧流量、或运维准备轮换时
使用；这是启动期 flag，进程没有运行期“取消 drain”的控制面。

## 指标参考

Counter：`paseo_relay_frames_forwarded_total`、`paseo_relay_bytes_forwarded_total`、
`paseo_relay_slow_consumer_disconnects_total`、`paseo_relay_delivery_timeouts_total`、
`paseo_relay_connection_rejections_total`、`paseo_relay_session_rejections_total`、
`paseo_relay_per_ip_rejections_total`、`paseo_relay_handshake_accepted_total`、
`paseo_relay_handshake_rejected_total`。

Gauge：`paseo_relay_ready`、`paseo_relay_draining`、
`paseo_relay_active_websockets`、`paseo_relay_active_sessions`、
`paseo_relay_ingress_reserved_bytes`、`paseo_relay_max_frame_bytes`、
`paseo_relay_uptime_seconds`、`paseo_relay_build_info{version,revision}`。

建议告警：节点 unready 超过 2 分钟、connection rejection 上升、slow-consumer 断开、
ingress 预留超过配置预算约 80%、实例从 scrape targets 消失。见
[examples/alerts.yml](../../examples/alerts.yml)。

## 容量与排障

- `paseo_relay_ingress_reserved_bytes` 接近
  `PASEO_RELAY_INGRESS_BUDGET_BYTES` 说明有大消息 in-flight 或慢 destination；
  查看 slow-consumer/delivery 指标。
- `active_sessions` 包含空 session，直到 idle sweep 清理；重连风暴后这是正常的。
- `connection_rejections_total` 包含非法 upgrade、draining、per-IP 限制和容量；
  需结合边缘日志归因。
- relay 不记录 `serverId`、`connectionId` 或 payload。需要按用户归因时使用边缘日志
  （尽量脱敏 query string）。
- `/version` 和 `build_info` 标识实际部署的 commit。如果 checkout 没有 `.git`，
  构建时设置 `PASEO_RELAY_BUILD_REVISION`。

## 排障

| 现象 | 可能原因 |
|---|---|
| 所有节点 `/ready` 503 | draining、容量满、session 满或节点级 capacity 故障；查看 `/metrics` 和日志 |
| session 被分到不同节点 | 边缘没有按 `serverId` 哈希；修正 LB hash policy |
| 节点重启后某用户连不上 | 客户端会重连；确认边缘哈希环没有把 `serverId` 映射到别处 |
| 持续 1012/1013 关闭 | 边缘/LB idle 或 write timeout 对长连接太短；检查 `PASEO_RELAY_DELIVERY_TIMEOUT_MS` 和 proxy tunnel timeout |
| native TLS 握手失败 | 证书/密钥不匹配，或 UID 65532 没有读权限 |
