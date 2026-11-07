# Paseo 兼容性

[English](../compatibility.md) | **简体中文**

本 relay 实现 Paseo 官方文档描述的 relay wire contract：
[relay endpoint security](https://github.com/getpaseo/paseo/blob/main/docs/relay-endpoint-security.md)、
[security](https://github.com/getpaseo/paseo/blob/main/SECURITY.md)。payload 不透明且
端到端加密；relay 从不解密。

## 定位

- **线协议兼容**：单 relay endpoint 下的 Paseo v1/v2 relay session；
- **社区实现**：不是 `relay.paseo.sh:443` 背后的托管 relay，也不内置集群/ownership
  failover；
- 多节点部署的亲和性由边缘完成（对 `serverId` 一致性哈希），不是 relay。

## 支持的协议面

| 特性 | 状态 |
|---|---|
| v1 `role=server` / `role=client` 配对 | 支持；新 socket 替换旧 socket（`1008`） |
| v2 daemon control socket（`role=server`，空 `connectionId`） | 支持：初始 `sync`、替换、control ping/pong |
| v2 daemon data socket（非空 `connectionId`） | 支持：替换、client fanout、断开传播 |
| v2 client 生成 `conn_<16 hex>` ID | 支持（client 省略时由 relay 分配） |
| `connected` / `disconnected` / `sync` | 支持 |
| 最后一个 client detach → data socket `1001` | 支持 |
| data socket 断开 → clients `1012` | 支持 |
| control nudge + `1011 Control unresponsive` | 支持 |
| Protected / legacy Paseo E2EE | 原样透传；relay 不协商/检查 E2EE 记录 |
| Legacy `hello` / `e2ee_hello` key 卫生 | 仅检查小型 JSON 形状 client 帧；非法 key `1008` |
| 最大 masked client 帧/消息 | `32 MiB - 14` 字节（超限 `1009`） |
| v2 control 帧上限 | 64 KiB |
| WebSocket ping/pong | 支持（relay 回 pong） |
| WebSocket 压缩 | 不协商（Paseo 不要求） |

## 与 Elixir relay 的已知差异

| 方面 | Elixir relay | 本 relay |
|---|---|---|
| 集群 | Syn ownership + 409 `x-reroute-target` reroute、分区处理 | 单节点；需要边缘一致性哈希 |
| 配置 | `PASEO_RELAY_CLUSTER_*`、`RELEASE_*`、acceptors、watermark、ownership target | 更小的环境变量契约；见 README |
| ID | `serverId`/`connectionId` 任意字节，最多 256 | 合法 UTF-8、可打印、无空白；非法 → HTTP 400 |
| per-IP 限制 | 进程内无 | 可选 `PER_IP_MAX_CONNECTIONS`，默认 0（推荐边缘） |
| 每个 connectionId 的 client 上限 | 无界 broadcast set | 可配置上限，默认 32 |
| 读停滞关闭 | 无 source frame read deadline | `FRAME_READ_TIMEOUT_MS` / `MESSAGE_READ_TIMEOUT_MS` → `1013` |
| TLS | 通常在边缘终止；listener 明文 | 边缘终止或原生 rustls |
| 运行时 | BEAM release + Cowboy/Ranch | 静态 Rust 二进制 + hyper/自定义 RFC 6455 codec |
| 指标 | 不同指标族/名称 | 见 README/OPERATIONS |

### 单节点语义

同一个 `serverId` 的所有 socket 必须路由到同一个 relay 节点。滚动发布时，节点在
drain window 内关闭 session，Paseo 客户端经新哈希环重连。如果边缘把同一个
`serverId` 分到两个节点，Paseo 会看到两个独立 session，无法配对。没有透明迁移。

## 证据：Paseo 官方 E2E

Paseo 自带端到端 relay 传输测试：
`packages/server/src/server/daemon-e2e/relay-transport.e2e.test.ts`。它支持通过
`PASEO_RELAY_E2E_PORT` 指向已运行的 relay，并用 `FORCE_RELAY_E2E=1` 强制运行。

兼容性工作流固定 Paseo commit 并对构建出的 relay 运行该文件；见
`.github/workflows/paseo-compat.yml` 和 `scripts/compat-paseo.sh`。

**2026-11-07 已在 Paseo v0.15.3
（`ea30d2616a6ea1ad51447cde6e2fc6da4904fe21`；也在
`f21699a37a3d49e8c61bd6d8b65f8a7070f0470c` 复现）验证：**

- 官方 external-relay 运行：**9 passed, 2 skipped, 0 failed**，两次结果一致
  （27.9 s / 31.1 s；relay 日志零 WARN/ERROR）；
- 跳过的两个用例是 external-relay 模式无法执行的、依赖 relay 重启的测试；开发期
  本地实验（未提交）跑过 reconnect/RPC，但本文件的可复现证据只有官方套件；
- 开发期手动 wire probe：覆盖 v1 转发/替换和 v2 control/data；probe 脚本/输出未提交，
  因此是补充材料，不是可复现证据；
- 覆盖：v2 control sync/connected/disconnected、生成的 `conn_` ID、data attach、
  大应用传输（分块，不是单条 relay 帧）、protected/legacy E2EE 协商、binary payload、relay ping/pong、
  `1008`/`1001` close code、SIGTERM drain 与重启。

带命令和摘要的日期快照在
[evidence/paseo-e2e-2026-11-07.md](evidence/paseo-e2e-2026-11-07.md)。

本地复现：

```sh
# 在已安装依赖并构建 workspace dist 的 Paseo checkout 中：
PASEO_RELAY_E2E_PORT=4000 FORCE_RELAY_E2E=1 \
  npx vitest run packages/server/src/server/daemon-e2e/relay-transport.e2e.test.ts \
  --maxWorkers=1
```

如果你改动了 wire 行为，请在 PR 中运行该套件（或说明为什么无法运行）。
