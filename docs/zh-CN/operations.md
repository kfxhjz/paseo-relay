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

默认情况下所有端点共享同一 listener。运维端点没有认证；不要把 `/metrics` 或 `/ready`
暴露到公网。

### 运维监听

设置 `PASEO_RELAY_OPS_PORT`（以及 `PASEO_RELAY_OPS_HOST`，默认 `127.0.0.1`）可以把
`/ready`、`/metrics`、`/version` 移到第二个明文监听上。此后公网监听只回答 `/ws` 和
`/health`，即使边缘少配了一条规则，也无法通过它访问运维端点。relay 直接暴露（原生
TLS）时推荐这种布局，它还有两个效果：

- 运维监听有自己独立的 64 个连接额度，所以公网监听打满 TCP 上限时，探针、容器
  healthcheck（`paseo-relay healthcheck` 会跟随该设置）和抓取仍然可用，编排器不会把
  一个只是"满了"的 relay 重启掉；
- 它在 drain 的 grace window 内继续回答，所以公网监听关闭之后仍能观察到 `/ready`
  （503）和 `/health`（200）。

启用后请把 readiness/liveness 探针、负载均衡健康检查和 Prometheus 都指向运维端口：
`/ready` 和 `/metrics` 在公网端口上返回 404。

## 日志

relay 按设计日志很少，并且永远不记录 `serverId`、`connectionId` 或 payload。当它开始
丢弃工作时会以 `warn` 级别说明，每种原因每 10 秒最多一行，并带上自上一行以来的事件数：

| `event` | 含义 |
|---|---|
| `ingress_exhausted` | 有 source 因为无法及时预留 ingress 预算而被 `1013` 关闭 |
| `ingress_reclaimed` | 为了给等待中的 source 腾出 ingress 预算，关闭了未完成的消息 |
| `read_timeout` | 帧或消息停滞，或超过了读取 deadline |
| `idle_timeout` | 有 socket 被 `PASEO_RELAY_WS_IDLE_TIMEOUT_MS` 关闭 |
| `slow_consumer` | 有 destination 在 delivery deadline 被 shed |
| `websocket_capacity` / `session_capacity` / `tcp_capacity` | upgrade 或连接因达到配置上限被拒绝 |
| `per_ip_limit` | 某个客户端达到了 `PASEO_RELAY_PER_IP_MAX_CONNECTIONS` |

每一种都有对应的 counter（见下）；日志行的意义在于，没有接指标系统的节点也能看出自己
正处于压力之下。

## 轮换 TLS 证书

使用原生 TLS 时，替换 `PASEO_RELAY_TLS_CERT` / `PASEO_RELAY_TLS_KEY` 指向的文件，然后
发送 `SIGHUP`（使用示例 unit 时即 `systemctl reload paseo-relay`），或者什么都不做：
relay 每 30 秒检查一次这两个文件，发现变化就重新加载。新连接拿到新证书，已建立的会话
不受影响。无法加载的证书/私钥（不可读、格式错误、证书与私钥不匹配）会被拒绝并记录一行
`warn`，继续使用旧证书。`v0.1.2` 及更早版本需要重启，并且收到 `SIGHUP` 会直接退出。

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
`paseo_relay_per_ip_rejections_total`、`paseo_relay_tcp_rejections_total`、
`paseo_relay_ingress_rejections_total`、`paseo_relay_ingress_reclaims_total`、
`paseo_relay_read_timeouts_total`、`paseo_relay_idle_timeouts_total`、
`paseo_relay_handshake_accepted_total`、`paseo_relay_handshake_rejected_total`。

拒绝类 counter 按原因拆分：

- `paseo_relay_connection_rejections_total` 统计因 WebSocket 容量、draining、非法
  upgrade/handshake、Origin 不允许/无法解析，或 upgrade 失败/超时而拒绝的请求；
- `paseo_relay_per_ip_rejections_total` 统计 per-IP WebSocket 上限（**不**计入
  `connection_rejections_total`）；
- `paseo_relay_session_rejections_total` 统计 `MAX_SESSIONS` 上限；
- `paseo_relay_tcp_rejections_total` 统计在 accept 时就被拒绝的 socket：达到
  `MAX_TCP_CONNECTIONS`，或命中 per-IP TCP 上限。

ingress 相关的 counter 用来区分"有压力"和"有损失"：

- `paseo_relay_ingress_reclaims_total` 统计为了给等待中的 source 腾预算而关闭的未完成
  消息。负载下出现一些是正常的；
- `paseo_relay_ingress_rejections_total` 统计因回收没能及时腾出足够预算而被关闭的
  source。这代表有用户被丢弃；
- `paseo_relay_read_timeouts_total` 统计因帧/消息停滞或超过读取 deadline 而被关闭的
  source；
- `paseo_relay_idle_timeouts_total` 统计被可选的 idle 超时关闭的 socket。

在 admission 之前因 query 参数校验返回 `400` 的失败不计入
`connection_rejections_total`；upgrade/handshake 校验返回 `400`/`426` 的会计入。

Gauge：`paseo_relay_ready`、`paseo_relay_draining`、
`paseo_relay_active_websockets`、`paseo_relay_active_sessions`、
`paseo_relay_ingress_reserved_bytes`、`paseo_relay_ingress_budget_bytes`、
`paseo_relay_ingress_small_reserved_bytes`、`paseo_relay_ingress_small_budget_bytes`、
`paseo_relay_max_frame_bytes`、`paseo_relay_uptime_seconds`、
`paseo_relay_build_info{version,revision}`。两个 `ingress_small_*` gauge 描述小消息
预留池；`ingress_reserved_bytes` 和 `ingress_budget_bytes` 只描述主预算。

建议告警：节点 unready 超过 2 分钟、connection rejection 上升、slow-consumer 断开、
ingress 预留超过配置预算约 80%（用
`paseo_relay_ingress_reserved_bytes / paseo_relay_ingress_budget_bytes` 比率，对任意
配置预算都成立）、`paseo_relay_ingress_rejections_total` 出现任何增长、
`paseo_relay_ingress_reclaims_total` 持续增长、实例从 scrape targets 消失。见
[examples/alerts.yml](https://github.com/kfxhjz/paseo-relay/blob/main/examples/alerts.yml)。

## 容量与排障

- `paseo_relay_ingress_reserved_bytes / paseo_relay_ingress_budget_bytes` 接近
  `0.8` 说明有大消息 in-flight 或慢 destination；查看 slow-consumer/delivery
  指标。该比率对任意配置预算都适用，不像绝对字节阈值那样依赖默认值。
- idle 内存主要由已接受 socket 决定，而不是 payload：明文时一个 idle socket 约占
  23 KiB，原生 TLS 时约 33 KiB，因此 `PASEO_RELAY_MAX_TCP_CONNECTIONS × ~30 KiB`
  （明文）或 `× ~35 KiB`（原生 TLS）是下限。内置 25,000 socket 默认值在无流量时
  就需约 700–800 MiB（明文）或约 850–900 MiB（原生 TLS）；小节点请调低这些上限。
- 因为 TCP slot 在整个 socket 生命周期内持有，`PASEO_RELAY_MAX_TCP_CONNECTIONS`
  要比 `PASEO_RELAY_MAX_CONNECTIONS` 留出余量（至少 64–128 个 slot，探针突发时
  更多）。两者相等时，满 WebSocket 负载可能在 accept 阶段拒绝 `/health`、
  `/ready`、`/metrics` 和探针，即使进程本身健康。
- `active_sessions` 统计的是有人 attach 的 session：session 在最后一个 socket 关闭时
  立即移除。（`v0.1.2` 及更早版本会把空 session 保留 `PASEO_RELAY_SESSION_IDLE_MS`，
  所以重连风暴之后该 gauge 会居高不下。）
- 每个 socket 都是一个文件描述符。如果日志里出现带 "too many open files" 的
  `accept failed`，说明进程的 open-file 上限低于 `PASEO_RELAY_MAX_TCP_CONNECTIONS`；
  请调高它（`LimitNOFILE=`、`ulimits.nofile`）。在 Linux 上 relay 启动时会对此告警。
- `connection_rejections_total` 覆盖 WebSocket 容量、draining、非法 upgrade/Origin
  以及 upgrade 失败/超时；per-IP 拒绝由 `per_ip_rejections_total` 单独统计。需结合
  边缘日志归因。
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
| `/ready` 或 `/metrics` 返回 404 | 设置了 `PASEO_RELAY_OPS_PORT`：它们只在运维端口上提供 |
| 大文件上传被 `1013` "Relay ingress reclaimed" 关闭 | ingress 预算已满且有别的 source 在等待；该上传处于停滞、慢于 `PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES`，或传输时间超过 `PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS`。按主机内存调大预算，或针对慢速链路调整这两个设置 |
| source 在消息中途被 `1013` "Relay read timeout" 关闭 | 在 `PASEO_RELAY_READ_STALL_TIMEOUT_MS`（默认 30 秒）内没有任何 payload 进展，或超过了 frame/message deadline |
| 某个设置不生效 | 查看启动日志或 `--check-config` 是否有 "is not a relay setting" 警告：变量名拼错了 |
