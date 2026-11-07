# 设计摘要

[English DESIGN.md](../../DESIGN.md) | **简体中文**

本文是完整设计的中文摘要；完整中文版见 [DESIGN.zh-CN.md](../../DESIGN.zh-CN.md)，英文规范版见
[DESIGN.md（英文）](../../DESIGN.md)。

## 目标

- **Paseo 线协议兼容**：`/ws` 上的 v1/v2 语义与 close code 与 Elixir relay 一致，
  Paseo client/daemon 无需改动。
- **payload 不透明**：不解密、不存储、不记录；唯一检查是有界的
  `hello`/`e2ee_hello` key 卫生检查。
- **默认有界**：每个队列/缓冲/handshake 阶段都有上限；单个未认证 peer 不能放大成
  节点级内存耗尽。
- **单静态二进制**：无 Erlang/OTP、EPMD、distribution、cookie；scratch 非 root。

扩容完全交给边缘：LB 对 `serverId` query 做一致性哈希，把同一 session 的所有 socket
送到同一 relay 进程。relay 内没有 ownership registry、reroute 或跨节点帧路径。

## 架构

```
client ──wss──┐
              ├── edge (TLS, per-IP limits, hash on serverId) ──► relay
daemon ───────┘                                                   │
                     hyper HTTP/1.1 + 自研 RFC 6455 codec          │
                     per-socket reader ──► SessionRegistry         │
                     per-socket writer ◄── bounded queues          │
                     ingress budget / caps / metrics               │
```

- **hyper** 只提供 HTTP/1.1 与 `/ws` upgrade；WebSocket 帧由 `src/ws.rs` 自己解析，
  这样可以在读取 payload 之前预扣 ingress 预算。
- **SessionRegistry** 是 `DashMap<serverId, Arc<Session>>`；Session 只保存拓扑（socket
  handle、connectionId、v1/v2 槽位），不保存 payload。
- **每个 socket 一个 reader task + 一个 writer task**；writer 队列同时受 mpsc 条数和
  字节信号量限制。
- **Capacity** 管 WebSocket/TCP/session/per-IP 上限和节点级加权 ingress 预算。
- **SIGTERM/SIGINT** 触发有界 drain，ready 变 503，宽限期后以 `1012` 关闭。

## 会话语义

- v1：每 `serverId` 一个 server/一个 client；新 socket 替换旧 socket（`1008`），帧转发
  给对端 role。
- v2：control socket（server + 空 connectionId）+ 每 client connection 一个 data socket；
  支持 `sync`/`connected`/`disconnected`、替换、data 断开 `1012`、最后 client 断开
  `1001`、control ping/pong 和 10 s nudge/15 s `1011` watchdog。
- 空 session 在 `SESSION_IDLE_MS` 后被清理；`MAX_SESSIONS` 限制总数。

## 关键不变量

| # | 不变量 |
|---|---|
| I1 | payload 不透明，不写日志/磁盘；指标只计数。 |
| I2 | 内存由配置上限约束，而不是 peer 行为；frame/message/control 上限、writer 字节上限、mpsc 上限、ingress 预算、连接/session/per-IP 上限、HTTP header timeout。 |
| I3 | 慢 destination 对 source 施加背压，不会无限增长队列；deadline 到后 shed。 |
| I4 | 连接失败只影响自身及协议规定的对端；source 断开不会关闭 destination。 |
| I5 | fail closed：显式 HTTP 状态或 WebSocket close code，无半开状态。 |
| I6 | 单节点是 session 的唯一权威；没有 split-brain。 |
| I7 | drain 可观测且有界。 |
| I8 | 拓扑竞态不能删除更新的 socket；socket id 校验。 |
| I9 | 在创建昂贵状态之前完成 admission。 |
| I10 | 可观测性不泄漏路由秘密；日志/metrics 无 ID/payload。 |

## 失败语义

| 条件 | 结果 |
|---|---|
| 未知路径/不支持 method | 404 |
| `/ws` 非 upgrade | 426 |
| 非法 serverId/role/v/connectionId/upgrade header | 400，不创建状态 |
| Origin 不在 allowlist | 403，不创建状态 |
| WS 容量满、session 满、draining | 503（不 upgrade） |
| per-IP 超限 | 429（不 upgrade） |
| native TLS 握手失败 | TCP 直接断开 |
| 帧/消息超限 | `1009` |
| 坏帧/masking/close payload | `1002` |
| text 非法 UTF-8 | `1007` |
| 被新 socket 替换 | `1008` |
| handshake key 非法 | `1008` |
| ingress 预算等待超时 | `1013` |
| delivery deadline/慢 destination | `1013` |
| v2 client 等不到 data socket | `1013` |
| 优雅 drain/重启 | `1012` |
| v2 server data socket 断开 | clients `1012` |
| 最后一个 v2 client detach | data socket `1001` |
| 正常关闭 | `1000`（合法收到 code 回显） |

## 安全设计

- relay 被 Paseo 视为不可信：E2EE 保护内容；relay 能看到 IP、时序、帧大小、
  `serverId`/`connectionId` 和握手公钥。
- `serverId` 是 bearer routing secret，不是认证；没有 relay-level auth。
- 无 Erlang/EPMD/cookie；没有 4369/distribution 暴露。
- TLS 可在边缘终止或用 rustls 原生提供；公网必须 `wss`。
- per-IP/全局/session/client 上限 + ingress 预算 + 有界 handshake 阶段；边缘限流仍是
  必要控制。
- handshake key 检查是协议卫生，不是安全边界；它不认证 peer。

## 加固决策摘要

- upgrade/query 全校验后才创建状态；失败不占容量。
- 每条消息固定开销 + 读取前计入 ingress 预算。
- source/destination 隔离；慢 consumer 在自己的 deadline 淘汰。
- 单节点 + 边缘 `serverId` 哈希；无 ownership/reroute。
- 无 Erlang distribution；可选原生 TLS；结构化日志不含 ID/payload。
- SIGTERM/SIGINT 有界 drain；readiness 翻转。
