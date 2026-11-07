# Paseo Relay — 设计

[English](DESIGN.md) | **简体中文**

本文是英文 [DESIGN.md（英文）](DESIGN.md) 的完整中文翻译，描述单节点 Rust relay 的架构
和它维护的不变量。面向运维的默认值和操作流程见 [README.zh-CN.md](README.zh-CN.md)；
本文解释系统为什么设计成这样。

> 英文 `DESIGN.md` 是规范来源；如果翻译与英文不一致，以英文为准。

## 1. 目标

- **面向 Paseo 的协议兼容边缘 relay。** 在 `/ws` 上提供 Paseo v1 和 v2 WebSocket
  协议，路由语义和 close code 与 Elixir relay 相同，Paseo client 和 daemon 无需修改。
- **payload 不透明。** relay 从不解密、存储或记录 Paseo payload；唯一检查是有界的
  handshake 卫生检查。E2EE 存在于 Paseo client 与 daemon 之间，端到端。
- **滥用下有界。** 每个队列、缓冲和握手阶段都有配置上限；单个未认证 peer 不能放大成
  节点级内存耗尽。
- **单个静态二进制。** 无 Erlang/OTP、无 EPMD、无分布式 Erlang、无 cookie、无运行时
  解释器。一个 musl 二进制以非 root 用户运行在 scratch 容器中。
- **失败即关闭，绝不半开。** 协议、资源和生命周期失败都产生明确的 HTTP 状态码或
  WebSocket close code。

扩容被刻意 **交给边缘**：负载均衡器消费 `serverId` query 参数并做一致性哈希，把同一
`serverId` 的所有 socket 放在同一个 relay 节点上。relay 只存节点本地 session，没有
集群、所有权注册表、重路由协议或跨节点帧路径。

## 2. 架构

```
                        边缘负载均衡器（TLS、per-IP 限制、按 serverId 哈希）
                                             │
                        ┌────────────────────▼────────────────────┐
                        │              relay 进程                 │
                        │  hyper HTTP/1.1 server（accept loop）    │
                        │   ├─ GET /health /ready /metrics         │
                        │   └─ GET /ws ── upgrade 前校验           │
                        │              └─ hyper upgrade ──┐        │
                        │  per-socket reader task          │        │
                        │   ├─ RFC 6455 frame decode       │        │
                        │   ├─ ingress budget reservation  │        │
                        │   └─ route via session registry  │        │
                        │  per-socket writer task          │        │
                        │   ├─ bounded mpsc + byte permits │        │
                        │   └─ RFC 6455 frame encode/flush │        │
                        │  SessionRegistry: DashMap<serverId, Session>│
                        │   └─ per-session Mutex<仅拓扑>             │
                        │  Capacity: WS/TCP/per-IP 信号量 + budget    │
                        └──────────────────────────────────────────┘
```

### 2.1 运行时与 HTTP 层

- **Tokio 多线程运行时**；除 Tokio 外没有其他 async runtime。
- **hyper 1** 在 listener 上提供 HTTP/1.1，包括 WebSocket upgrade 握手和运维端点。
  不要求反向代理或 HTTP/2；按定义 WebSocket upgrade 是 HTTP/1.1。
- **自研 RFC 6455 codec**（`src/ws.rs`）。握手校验、帧解析、masking 规则、分片、
  control frame 规则、text payload 的 UTF-8 校验和 close payload 都直接实现。
  生产环境不使用第三方 WebSocket 库；`tokio-tungstenite` 只是集成测试用的
  dev-dependency。
- **TLS** 由 `tokio-rustls`/`rustls`（ring provider）提供，前提是设置了
  `PASEO_RELAY_TLS_CERT` 和 `PASEO_RELAY_TLS_KEY`（必须同时设置或同时不设置；只配
  一半会在启动时拒绝）。ALPN 限制为 `http/1.1`。否则 listener 是明文，TLS 必须在
  边缘终止。
- **运维端点**与主 listener 共用：`GET`/`HEAD` `/health`、`/ready`、`/metrics`。
  它们按设计没有认证，因此部署必须把它们放在内网或在边缘过滤。

### 2.2 连接生命周期

1. **TCP accept。** listener 在读取请求之前先获取 TCP slot
   （`MAX_TCP_CONNECTIONS`）。HTTP header 阶段有 header timeout，慢 client 不能无限
   占用 slot。per-IP WebSocket slot（`PER_IP_MAX_CONNECTIONS`，0 = 不限制）在
   admission 阶段、upgrade 完成之前获取。
2. **请求分类。** `GET /ws` 是唯一可 upgrade 的路由；其他路径返回 `404`，运维端点
   只读。`/health` 与 readiness 独立，这样 liveness 探针不会在 drain 期间触发重启。
3. **upgrade 前校验。** 在分配任何 relay 状态之前，请求必须通过：
   - method/path 和 WebSocket upgrade header（`Connection: upgrade`、
     `Upgrade: websocket`、`Sec-WebSocket-Version: 13`）；
   - `Sec-WebSocket-Key` 必须是恰好 16 字节的规范 padded Base64；
   - query 校验：`serverId`（1..=256 字节、合法 UTF-8、无控制字符或空白）、
     `role=server|client`、`v=1|2`；
   - 可选的 Origin allowlist。
   失败时返回 `426`（不是 upgrade）、`400`（upgrade/query 非法）、`403`
   （Origin 不允许）或 `404`（未知路径）——**不创建 session、writer 或容量预留**。
4. **Admission。** 只有校验通过后，relay 才获取 WebSocket slot
   （`MAX_CONNECTIONS`）、预留 session slot（`MAX_SESSIONS`）、获取 per-IP slot、
   把 socket attach 到 session registry，并 spawn writer task。
5. **读写分离。** upgrade 后的 IO 被拆分：reader task 负责帧解码和路由；writer task
   负责该 socket 的编码和 flush。两者只通过有界 channel 通信，所以慢 peer 产生的是
   背压，而不是无界缓冲。
6. **detach 与清理。** 关闭时，connection runner 只从 session 拓扑中 detach 该 socket
   一次（socket ID 让过期或重复 detach 无害），归还容量 token，并让 idle sweep 在
   grace period 后移除空 session。

### 2.3 Session registry

`SessionRegistry` 是 `DashMap<String, Arc<Session>>`，以 `serverId` 为 key。每个
`Session` 是一个小的 `Mutex<SessionState>`，只保存 **拓扑**：socket handle（writer
队列 handle 加路由元数据），绝不保存 payload 字节。没有 per-session 进程，也没有
调用方可以杀掉的共享 “owner”；这是对共享 owner 类失败的结构性修复。

- **v1** 保留一个 server slot 和一个 client slot。给某个 role attach 新 socket 会替换
  旧 socket；被替换的 socket 在释放 registry lock 之后以 `1008`
  （"Replaced by new connection"）关闭。帧转发给对端 role。
- **v2 daemon** socket 包括 control socket（`role=server`，空 `connectionId`）和每个
  connection 的 data socket。attach control socket 时会发送 `sync` 消息，内含当前
  已 attach 的 `connectionId` 排序列表，并替换之前的 control socket（同样 `1008`）。
  attach data socket 会替换同一 `connectionId` 的已有 socket。
- **v2 client** attach 到 `connectionId`。client 通常省略它，此时 relay 生成
  `conn_<16 hex>`；如果提供，则校验后原样使用，以兼容 Elixir。client 会以
  `connected` 通知 control socket；最后一个 client detach 会移除 data socket
  （close `1001`）并以 `disconnected` 通知 control socket。data socket 死亡时，它的
  所有 client 以 `1012`（"Server disconnected"）关闭。
- **Idle sweep。** 空 session 在配置的 idle grace period 后被移除。`MAX_SESSIONS`
  限制 `serverId` 条目数；达到 per-`connectionId` client 上限的 session 拒绝后续
  attach。
- **无跨 session 影响。** session lock 下的操作短且同步（持锁时没有 `.await`）；
  影响其他 socket 的协议动作（替换、data 死亡、最后 client）在锁内计算，锁释放后
  执行。

### 2.4 投递路径与背压

relay 的核心可靠性规则是 **源端背压，而不是缓冲**：

1. reader 一次解析一帧。在读取 payload 之前，它从节点级预算中预留加权 ingress
   字节。
2. 它从 registry 解析 destination，并把帧 enqueue 到 destination 的 writer 队列，
   等待有界 byte permit。
3. writer 写入并 flush 帧，然后 ack；路由 task 等待该 ack（受 delivery deadline
   约束），两消息预读的 hand-off 在等待时阻塞 reader。于是内核 TCP 发送压力以 TCP
   接收窗口压力的形式传回生产者，而不是被 relay 内存吸收。
4. 如果在 ingress wait timeout 内无法预留预算，或者 writer 队列/写入在 delivery
   deadline 内没有进展，source（或慢 destination）以可重试的 `1013` 关闭。

每个 destination 的 writer 队列有两重限制：1024 条消息的 mpsc 上限，加上一个字节
信号量，其大小为 `max(control_queue_bytes, max_message_bytes + 64 KiB)`。单条最大
消息总能放下，所以淘汰由 deadline 触发，而不是由无法预留的 slot 触发。Ping/pong
条目不占字节信号量，但受 mpsc 条数和 RFC 125 字节 control frame 上限约束。
per-connection reader 最多把两条已完成消息交给 routing task，因此预读也有界。
control socket 有自己的 64 KiB 帧上限。

### 2.5 Ingress 预算

节点级信号量限制所有连接中 **in-flight** 的 payload 字节。每条消息的计费是
`(payload_len + 256) * ingress_weight`：固定的 256 字节开销让空帧和小帧不是免费的
；weight 建模了 buffered payload 可能被保留多次的事实（read buffer、writer
queue、kernel）。预留在 RAII guard 中，所以取消、panic 或 socket 死亡总会归还字节。
预算在 **读取 payload 之前** 预留，因此超大或预算耗尽的消息在被缓冲之前就被
拒绝。

### 2.6 生命周期与优雅关闭

- `SIGTERM`/`SIGINT` 启动 drain：listener 停止 accept，`/ready` 返回 `503`，新
  upgrade 以 `503` 拒绝；现有 session 在配置的 grace window 内继续工作，然后剩余
  socket 以 `1012`（"Relay shutting down"）关闭，进程在短暂等待 close frame flush
  后退出。`SIGKILL` 永远不需要作为第一信号。
- `PASEO_RELAY_DRAIN=true` 让进程启动即 draining（适合在停止节点前先把它移出轮换）。
- Session 是节点本地且临时的；重启总是安全的，因为 Paseo client 和 daemon 会重连。
  按设计不持久化任何东西。

## 3. 协议模型

### 3.1 通用

- `/ws` query 参数：`serverId`（必填，1..=256 字节）、`role`（`server` 或
  `client`）、`v`（默认 `1`，或 `2`）、`connectionId`（仅 v2）。
- ID 按 UTF-8 校验；包含控制字符或空白会被拒绝。超大 ID 在创建状态之前返回 `400`
。
- 帧转发时保留 opcode。Binary payload 完全透明；text payload 在 framing 层做 UTF-8
  校验（协议要求），但不为路由解析内容。
- 透明性有一个狭窄例外：client 发来的、最多 1 MiB 且首个非空白字节为 `{` 的帧会被
  预筛为可能的 Paseo `hello`/`e2ee_hello` 握手；只有这时才解析，其 `key` 必须是
  规范 32 字节 X25519 公钥的规范 padded Base64。这是协议卫生，不是安全边界；失败会
  以 `1008` 关闭 client，且绝不转发。

### 3.2 v1

每个 `serverId` 一个 server 和一个 client。某个 role 的新 socket 会替换旧 socket
（`1008`，"Replaced by new connection"）。帧转发给对端 role 的 socket。重连之间没有
缓冲；如果 peer 不存在，帧无法投递，会在该 socket 丢弃（session 拓扑不受影响）。

### 3.3 v2

- daemon 打开一个 **control socket**（`role=server`，空 `connectionId`）。relay 回复
  `{"type":"sync","connectionIds":[...]}`。新 control socket 会替换旧 control socket。
- 对每个 client connection，daemon 用对应 `connectionId` 打开一个 **data socket**。
  data socket 按需 attach；client 帧只等待 data socket `DATA_ATTACH_TIMEOUT_MS`。
- v2 **client** 省略 `connectionId`；relay 分配 `conn_<16 hex>`，并通过 control
  socket 以 `{"type":"connected","connectionId":...}` 报告给 daemon。最后一个 client
  detach 时发出 `{"type":"disconnected",...}` 并以 `1001` 关闭 data socket。
- 如果 data socket 断开，该 `connectionId` 下所有 client 以 `1012` 关闭。
- control 输入除 text `{"type":"ping"}` 外忽略；ping 回复
  `{"type":"pong","ts":<unix ms>}`；control 帧上限 64 KiB。

## 4. 不变量

| # | 不变量 | 执行方式 |
|---|---|---|
| I1 | payload 不透明，绝不记录或持久化。 | 除有界 handshake 卫生检查外不解析 payload；指标只计字节，不记内容；`serverId` 和 payload 字节绝不记录。 |
| I2 | 内存由配置约束，而不是由 peer 行为约束。 | 帧/消息上限、control 帧上限、data frame 的 writer 字节信号量、有界 mpsc、全局 ingress 预算、connection/session/per-IP 上限、HTTP header timeout；ping/pong 帧还受 mpsc 条数和 RFC 125 字节限制约束。 |
| I3 | 慢 destination 对 source 施加背压；队列绝不无界增长。 | source 等待 ingress 预留和 writer ack；delivery deadline 触发淘汰而不是缓冲。 |
| I4 | 连接失败只影响该连接及其协议定义的对端。 | detach 只移除自己的 handle；writer ack 独立于 source 存活；唯一的跨 socket 关闭是替换（`1008`）、v2 data 死亡（`1012`）和最后 client detach（`1001`）。 |
| I5 | 失败即关闭：没有半开或静默丢弃的状态转换。 | 握手/query 错误返回 HTTP 错误且不创建状态；framing 错误用明确 close code；shutdown 用 `1012`。 |
| I6 | 一个 relay 节点是其 session 的唯一权威；没有脑裂。 | 没有集群成员、共享所有权或跨节点 session 状态；`serverId` 亲和性由边缘负责。 |
| I7 | Drain 可观测且有界。 | drain 一开始 readiness 就变 `503`，upgrade 被拒绝，grace window 有界，剩余 socket 以 `1012` 关闭。 |
| I8 | 拓扑竞态不能移除更新的 socket。 | per-session mutex，持锁无 await；detach 时校验 socket ID；替换关闭在锁外执行。 |
| I9 | admission 在分配之前发生。 | 容量和 per-IP 信号量、upgrade 校验、session 上限都在创建昂贵状态之前检查。 |
| I10 | 可观测性绝不泄漏路由秘密。 | `/metrics` 只有聚合值（没有 `serverId` label）；日志绝不包含 `serverId`、`connectionId` 或 payload。 |

## 5. 失败语义

| 条件 | 结果 |
|---|---|
| 未知路径或不支持的方法 | HTTP `404` |
| 对 `/ws` 的非 upgrade 请求 | HTTP `426` |
| 非法 `serverId`、`role`、`v`、`connectionId` 或非法 upgrade header | HTTP `400`，不创建状态 |
| Origin 不在 `PASEO_RELAY_ALLOWED_ORIGINS` | HTTP `403`，不创建状态 |
| WS 容量满、session 上限达到或正在 draining | HTTP `503`（不 upgrade） |
| 超过 per-IP 连接上限 | HTTP `429`（不 upgrade） |
| native TLS 模式下 TLS 握手失败 | 在 HTTP 之前直接断开 TCP |
| client 帧/消息超过配置上限 | WebSocket close `1009` |
| 坏帧、错误 masking、坏 close payload | WebSocket close `1002` |
| text 帧非法 UTF-8 | WebSocket close `1007` |
| 被更新 socket 替换 | WebSocket close `1008` |
| 非法 Paseo handshake key | WebSocket close `1008` |
| ingress 预算超过等待超时 | WebSocket close `1013`，"Relay ingress capacity" |
| 超过 delivery deadline / 慢 destination | WebSocket close `1013`，"Slow consumer" / "Delivery unavailable" |
| v2 client 在 attach timeout 内没有 data socket | WebSocket close `1013`，"Data route unavailable" |
| 优雅 drain / server 重启 | WebSocket close `1012`，"Relay shutting down" |
| v2 server data socket 断开 | Client socket close `1012`，"Server disconnected" |
| 一个 connection 的最后 v2 client detach | Data socket close `1001`，"Client disconnected" |
| 正常关闭 | `1000`；收到允许范围内的 close code 会回显 |

（admission 拒绝的 HTTP 状态码是文档化 endpoint contract 的一部分；见 README。）

## 6. 安全设计

- **威胁模型。** Paseo 把 relay 当作不可信：relay 能看到 IP 地址、时序、帧大小、
  `serverId`/`connectionId` 以及明文握手中的公钥。`serverId` 是 bearer 路由秘密；
  知道它的人可以 attach 到该 session。relay 按设计没有认证（Paseo E2EE 提供内容的
  机密性和真实性），因此部署必须把 `serverId` 当作秘密，并把可用性攻击纳入考虑。
- **无分布式 Erlang。** 二进制不包含 Erlang VM、EPMD、node cookie 或 distribution
  listener。4369 或 distribution 端口上没有可暴露的东西。
- **TLS。** 在边缘终止，或用 rustls 加挂载的 PEM 文件原生提供。明文只允许在
  loopback/私网使用。
- **限流。** per-IP 连接限制、全局 WebSocket/TCP/session 上限、有界握手阶段和
  ingress 预算；额外的 per-IP 和请求速率限制属于边缘。
- **秘密卫生。** `serverId`、`connectionId` 和 payload 绝不记录。指标只带聚合计数器；
  Prometheus 绝不能暴露到公网。
- **握手校验是卫生检查。** 它廉价地拒绝畸形 X25519 编码（只处理小型 JSON 形状的
  client 帧），但不认证 peer，不能描述为安全控制。

## 7. 加固决策

以下是明确的设计要求，不是与任何其他实现的对比。

| 方面 | 决策 |
|---|---|
| Upgrade | 在创建任何 session、writer 或容量状态之前校验 method/path、upgrade header、key 编码和 query 参数；被拒绝的请求不分配任何状态。 |
| 消息记账 | 每条消息计固定开销加加权 payload；读取 payload 前预留；限制队列、读取 deadline 和每个 source 的 in-flight 字节。 |
| 隔离 | 没有共享 owner 进程；session 只持短拓扑锁；source detach 绝不关闭 destination；慢 consumer 在自己的 deadline 被淘汰。 |
| 失败 | 失败即关闭，返回明确的 HTTP 状态或 WebSocket close code；没有半开状态。 |
| 扩展 | 单节点；边缘 `serverId` 亲和性；没有 ownership 注册表、reroute 或脑裂仲裁。 |
| 运行时 | 无 Erlang/OTP、EPMD、cookie 或 distribution；静态 musl 二进制；可选原生 TLS。 |
| 可观测性 | 只有聚合指标；结构化日志不含 `serverId`、`connectionId` 或 payload。 |
| 生命周期 | 有界 SIGTERM/SIGINT drain；readiness 翻转；剩余 socket 以 `1012` 关闭。 |
| 供应链 | scratch 非 root 运行时；digest-pinned builder；SHA-pinned CI actions；provenance/SBOM。 |

## 8. 非目标

- **内置集群/多节点所有权。** relay 按设计是单节点；亲和性和扩展由边缘负载均衡器
  提供。
- **认证或授权。** Paseo E2EE 是机密性和真实性层；relay 不增加 token、lease 或
  session 撤销。`serverId` 作为秘密很重要，必须按秘密处理。
- **payload 检查、解密、存储或重放保护。** 不读取、保留或记录消息内容；重放/新鲜度
  是 Paseo E2EE 的职责。
- **透明 failover/session 迁移。** socket 是临时的；client 通过正常路由重连。
- **HA 持久化、管理 API、per-tenant 指标或 per-`serverId` 配额。** 边缘策略是预期
  的控制点。

## 9. 取舍与开放问题

- **一致性哈希是运维依赖。** 如果边缘把一个 `serverId` 映射到两个节点（或在 rollout
  期间重映射），Paseo session 的两端可能落在不同 relay，session 就无法连接。部署
  应使用稳定的 hash ring、较小的 vnode churn，并在移除节点前 drain。
- **可用性是残余风险。** 没有认证时，任何知道 `serverId` 的人都能扰乱该 session。
  这是接受的取舍，通过保密 `serverId` 和边缘限流缓解。
- **per-IP 限制较粗。** NAT 用户共享 IP，因此限制必须按部署规模设置（或用 `0`
  禁用）。per-`serverId` 配额和 connect-rate 限制刻意不在进程内实现。
- **固定开销和 weight 是启发式。** 256 字节开销和 weight 4 是保守值，而不是针对每个
  allocator 测量出来的；它们是可配置、可评审的常量。
- **HTTP 状态映射** 的 admission 拒绝在 README 中记录；协议 close code 是兼容性
  contract 的一部分，未经 Paseo client/daemon 兼容性评审不应修改。

## 10. 测试

测试套件使用真实 listener 和真实 WebSocket client（`tokio-tungstenite`），不是 mock：

- 单元测试覆盖配置解析/校验（包括 TLS 配对和预算下限不变量）、query 解析和 ID
  校验、RFC 6455 upgrade 和 frame codec round-trip、handshake key 校验、容量/预算
  记账和 guard 释放、session 拓扑转换、writer 队列行为和指标渲染。
- 集成测试（`tests/relay_integration.rs`）覆盖 `/health`、`/ready`、`/metrics`；
  v1 双向转发和替换（`1008`）；v2 control/data/client 通知和 control ping/pong；
  failed upgrade 不创建 session；非法 handshake key（`1008`）；非法 UTF-8
  `connectionId`（`400`）；超大帧（`1009`）；WebSocket 容量（`503`）；per-IP 限制
  （`429`）；缺失 v2 data route（`1013`）；drain（`1012`）；以及 forwarded-frame
  指标。
- 较新的回归测试覆盖只有 ping 流量时的分片消息 deadline、control nudge/`1011`
  watchdog、per-connection client 上限、native 与 browser client 的 Origin
  allowlist、停滞的 frame payload、以及 session-capacity admission。
- CI 和 `scripts/ci.sh` 中的 container smoke test 构建静态镜像并从宿主机轮询
  `/health`，因为 scratch 镜像没有 shell。

用 `cargo test` 或 `scripts/ci.sh` 运行全部检查。
