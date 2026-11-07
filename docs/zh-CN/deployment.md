# 部署指南

[English](../deployment.md) | **简体中文**

Paseo Relay 是 **单节点** 组件。水平扩展和高可用由边缘负载均衡完成：同一个 Paseo
`serverId` 的所有 socket 必须落在同一个 relay 进程上。relay 本身没有内置集群、
ownership registry 或 409 reroute；这是刻意取舍（见[完整中文设计](../../DESIGN.zh-CN.md)，英文规范 [DESIGN.md（英文）](../../DESIGN.md)）。

## 选择拓扑

| 拓扑 | 适用场景 | 亲和性来源 |
|---|---|---|
| 单个 relay 节点 | 个人/小团队自托管 | 无需（单进程） |
| N 个 relay + 边缘 LB | 公网或多租户 | 对 `serverId` query 参数做一致性哈希 |
| 多个独立 relay | 不同信任域/租户 | 不同 DNS/endpoint |

**不要** 用 round-robin DNS、普通 `least_conn` 或随机负载均衡。同一 `serverId` 的
Paseo daemon 和 client 必须到达同一个 relay 进程，否则无法配对。

## TLS

公网部署必须使用 `wss://`。可以：

- 在边缘终止 TLS（推荐），relay 只监听私网，并转发 WebSocket upgrade；或
- 挂载 PEM 证书链和私钥，设置 `PASEO_RELAY_TLS_CERT` / `PASEO_RELAY_TLS_KEY`，
  由 relay 用 rustls 直接提供 HTTPS/WSS。两者必须同时设置，否则启动失败。

Paseo payload 虽然 E2EE，但 `serverId`、连接元数据和可用性仍会暴露给明文网络上的
被动/主动攻击者，所以公网不能只跑 `ws://`。

## 边缘负载均衡（多节点必需）

LB 必须：

1. 终止 TLS，并按 client IP 做连接/请求速率限制；
2. 解析 WebSocket query string，按 **`serverId` 参数值** 做一致性哈希
   （不是整个 URL，也不是 cookie）；
3. 保持 WebSocket 长连接（长 read/tunnel timeout）；
4. 后端健康检查用 `/ready`（不要只用 `/health`），这样 draining 节点会被摘除；
5. 不把 `/metrics` 暴露到公网。

示例：

- Nginx：[examples/nginx.conf](../../examples/nginx.conf)
- HAProxy：[examples/haproxy.cfg](../../examples/haproxy.cfg)
- Kubernetes + ingress-nginx：Ingress annotation 设为
  `nginx.ingress.kubernetes.io/upstream-hash-by: "$arg_serverId"`，liveness 用
  `/health`，readiness 用 `/ready`。见 [examples/kubernetes.yaml](../../examples/kubernetes.yaml)。
- Envoy：使用 `RING_HASH` cluster，route hash policy 选
  `query_parameter: { name: serverId }`。

也可以自己写一个小 gateway 读 query 并做同样的哈希；relay 不依赖特定厂商。

### 代理后的源 IP

LB 终止连接后，relay 看到的每个连接都来自代理 IP。此时
`PASEO_RELAY_PER_IP_MAX_CONNECTIONS` 限制的是 **代理**，不是最终用户。请在代理后
保持默认 `0`，在边缘做 per-client 限制。如果是 relay 直接暴露、没有代理，再按用户
规模设置（例如 `256`）。

## 单节点容器

```sh
docker build -t paseo-relay .
docker run --rm --read-only --cap-drop ALL \
  --security-opt no-new-privileges \
  -p 127.0.0.1:4000:4000 paseo-relay
```

镜像是 `scratch` 中的静态 musl 二进制，以 UID/GID `65532` 运行，不写磁盘，并内置
`healthcheck` 子命令（镜像 `HEALTHCHECK` 调用 `/paseo-relay healthcheck`）。
不需要 shell 或 `curl`。

Compose 示例见 [examples/docker-compose.yml](../../examples/docker-compose.yml)。

## Kubernetes

以 [examples/kubernetes.yaml](../../examples/kubernetes.yaml) 为起点。关键配置：

- `runAsNonRoot: true`、`runAsUser: 65532`、`readOnlyRootFilesystem: true`、
  `allowPrivilegeEscalation: false`，drop `ALL` capabilities；
- readiness probe `GET /ready`，让 draining 节点从 Service endpoints 摘除；
- liveness/startup probe `GET /health`；
- `terminationGracePeriodSeconds` 大于 `PASEO_RELAY_SHUTDOWN_GRACE_MS`
  （默认 10 s），例如 30 s；
- ServiceAccount 不需要 API 权限；进程不需要写文件；
- NetworkPolicy：只允许 ingress controller/proxy 入站；`/metrics` 只给监控；
- 保持 replicas 稳定。扩容时增加 relay 节点并更新边缘一致性哈希环；不要用会让
  环频繁重排的 HPA。

## systemd

示例 unit 见 [examples/paseo-relay.service](../../examples/paseo-relay.service)。
用 `EnvironmentFile=` 提供 runtime contract，`Restart=always`，并给
`TimeoutStopSec` 留出大于 drain grace 的时间（例如 `TimeoutStopSec=30`）。

## 健康检查、readiness 与 drain

- `GET /health` — liveness；draining 期间仍 200。
- `GET /ready` — 仅在愿意接受新工作时 200；draining、shutting down、WebSocket/session
  满或 capacity 异常时 503。
- `SIGTERM`/`SIGINT` — 停止 accept，`/ready` 变 503；现有 session 继续运行
  `PASEO_RELAY_SHUTDOWN_GRACE_MS`，然后以 `1012` 关闭并退出。
- `PASEO_RELAY_DRAIN=true` — 启动即 unready；适合维护启动或先摘流再停止。

推荐滚动流程：先从 LB 摘除（readiness），等 LB 观察到 `/ready=503`，发 SIGTERM，
等待至少 grace window，再在必要时 kill。

## 指标与告警

从内网抓取 `/metrics`。指标只有聚合值，没有 `serverId` label。示例见
[examples/prometheus.yml](../../examples/prometheus.yml)、
[examples/alerts.yml](../../examples/alerts.yml)，指标说明见
[operations.md](operations.md)。

## 容量规划

容器内存要大于配置的 ingress budget 加每 socket 开销：

- `PASEO_RELAY_INGRESS_BUDGET_BYTES`（默认 512 MiB 加权）限制 in-flight payload 记账；
- `PASEO_RELAY_MAX_CONNECTIONS`（默认 20,000）限制 WebSocket；
- `PASEO_RELAY_MAX_TCP_CONNECTIONS`（默认 25,000）限制所有 TCP，包括 upgrade 前 HTTP；
- `PASEO_RELAY_MAX_SESSIONS`（默认 20,000）限制 idle `serverId` 条目；
- `PASEO_RELAY_CONTROL_QUEUE_BYTES` / writer 队列限制每 destination 缓冲。

relay 自身内存很小，与活跃 socket 和排队 payload 成正比；请用 `/metrics` 和容器
RSS 在真实负载下测量。
