# 变更日志

[English](CHANGELOG.md) | **简体中文**

本项目所有重要变更都记录在这里。格式遵循
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/)，并且本项目遵循
[语义化版本（Semantic Versioning）](https://semver.org/spec/v2.0.0.html)。

## [Unreleased]

### 文档

- 新增完整的简体中文变更日志（`CHANGELOG.zh-CN.md`）。
- 将 VPS quickstart 固定的 Caddy 镜像更新到 `2.11.9-alpine`，并记录了当 VPS 已经在
  为其他域名提供服务时如何复用现有 Caddy。
- 修正 quickstart 中的 Caddyfile 为合法的多行 block，并明确 bridge 网络中的 Caddy
  必须接入 `paseo-relay` 网络；loopback 只适用于宿主机或 host 网络中的容器。
- Caddy 示例改为可累加结构：导入 `examples/sites-enabled/*.caddy`，每个新域名一个
  文件，可独立 reload。
- 新增 `examples/.env.example`：在 git 忽略的 `examples/.env` 里配置一次
  `RELAY_DOMAIN`，升级时不再需要内联变量。
- 新增 host-Caddy override，并明确两种反向代理地址：容器内 Caddy 使用
  `paseo-relay` 网络上的 `relay:4000`，宿主机 Caddy 使用 `127.0.0.1:4000`。
  新域名后端必须接入 `paseo-relay`，或通过 `host.docker.internal` 访问宿主机。

## [0.1.4] - 2026-11-27

针对第三轮（2026-11）评审的修复。正常流量没有线协议变更；v2 控制平面现在会强制执行
文档化的 64 KiB `sync` 上限，此外若干运维默认值也获得了更安全的校验。

### 安全

- **relay 的 v2 控制 `sync` 被限制在 64 KiB 以内。** `sync` 由客户端选择的
  `connectionId` 构建，这些 id 可能包含 JSON 转义字符；此前足够多的长 id 会让 relay
  发出的控制帧超过文档化的 v2 控制帧上限（在极端
  `PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION` 下，甚至超过 writer 队列）。session 层
  现在会在接受 v2 client（或初始 control socket）之前计算序列化后的大小，并以
  `1013` "Relay session sync capacity" 拒绝会溢出的 client；初始和 watchdog 的
  `sync` 失败现在 fail-closed，而不是被静默丢弃，并由新增的
  `paseo_relay_control_send_failures_total` 计数。
- **control watchdog 的 nudge 覆盖范围按 client 分别跟踪。** 在 watchdog 发送 nudge
  之后才 attach 的 client，以前会被以 `1011` 关闭，却从未收到属于自己的 `sync`，
  而它的等待窗口仍受最老等待者限制。watchdog 现在把最老等待者的 `1011` deadline 与
  覆盖时钟分开，为后到的 client 发送补充 `sync`，并且绝不让新 client 推迟该 deadline。
- **TLS healthcheck 固定 relay 证书。** `paseo-relay healthcheck` 以前会接受任何签名
  验证通过的证书（有意跳过信任链和主机名），因此，在非 loopback 探测目标上，
  另一个本地服务或路径上的 MITM 可以把坏掉的 relay 报告为健康。使用原生 TLS 时，
  探针现在读取 `PASEO_RELAY_TLS_CERT` 的 leaf 证书，固定其 DER SHA-256，并且只
  接受该证书。如果配置了原生 TLS 但证书文件无法读取，它只探测 loopback，并对非
  loopback 目标 fail-closed。明文 listener 没有可固定的证书：允许非 loopback
  探测，但不做认证。
- **TLS healthcheck 的 pin 跟随配置的证书文件，而不是 listener 当前正在提供的证书。**
  如果替换的一对证书/私钥格式错误或不匹配，watcher 会继续使用旧证书，但探针固定的是
  新文件，因此在证书对修复之前可能报告不健康。请修复或恢复文件，不要重启：启动时也会
  尝试加载坏掉的那一对并失败。
- **每连接 ingress 份额可配置，并附带残余风险警告。**
  `PASEO_RELAY_PER_CONNECTION_LIMIT_BYTES`（0/未设置 = 自动）把配置的上限 clamp 到
  `[one maximum message, ingress budget]`；自动默认值仍是
  `min(budget, 4 × one-max-message charge)`，在默认发布配置下等于整个预算。当派生上限
  等于整个预算且 `PASEO_RELAY_PER_IP_MAX_CONNECTIONS=0` 时，启动现在会发出警告。这
  减少了一个慢速 source 的份额，但 **不能** 消除文档化的残余可用性风险：多条连接
  仍可占满预算，因此仍需要边缘的 per-source 带宽限制。
- **发布提升是分阶段并带版本校验的。** release workflow 现在会拒绝不是
  `vX.Y.Z[-prerelease]`、与 `Cargo.toml` 不一致，或没有 CHANGELOG 段落的 tag；它会
  先在临时 `candidate-<sha>` tag 下组装多架构 index，对该 digest 做 smoke test，
  签名并附加构建来源证明，然后才提升版本 tag（以及 `latest`，仅当稳定 release 不低于
  `main` 上最高的 stable tag 时）。publish 在面向 required reviewers 的 `release`
  environment 中运行，release 校验现在也会运行与 CI 相同的 fuzz smoke。required
  status checks、tag protection 和 environment reviewers 属于 GitHub 仓库设置；
  `SECURITY.md` 列出预期的配置，而不是假装代码能够强制执行它们。
- **CI/供应链加固。** Docker builder、CI 和 `rust-toolchain.toml` 已统一到 Rust
  1.100.0；`fuzz/Cargo.lock` 的漂移会对照根 lockfile 检查，并新增了 seed corpus；
  Pages 构建 job 不再持有写凭证；`.dockerignore` 排除了 docs、examples、fuzz 输出、
  本地评审数据和嵌套的 PEM/key 文件；HAProxy 示例按 per-source 限制统计 TCP 连接数
  （而不只是 HTTP 请求）；Mermaid 资产已固定版本并改为自托管。release 校验 job 还会
  对两个 lockfile 运行 `cargo audit` 并运行 fuzz smoke test，容器镜像在提升前会按
  不可变 digest 做 smoke test。

### 变更

- **每类拒绝指标只归因于一个原因。** `paseo_relay_session_rejections_total` 现在只
  统计全局 `MAX_SESSIONS` 上限。新增的
  `paseo_relay_session_connection_rejections_total` 统计 session 级
  `connectionId` 并集上限和 64 KiB `sync` 拒绝，
  `paseo_relay_client_limit_rejections_total` 统计每个 `connectionId` 的 client
  上限，`paseo_relay_ops_rejections_total` 统计运维 listener 的配额拒绝（同时记录
  一条限速警告，点名 peer）。
- **转发和投递指标有精确语义。**
  `frames_forwarded_total`/`bytes_forwarded_total` 只统计 relay 转发的用户帧/字节，
  不统计 relay 生成的 `sync`/`connected`/`disconnected`/pong；
  `delivery_timeouts_total` 对每次被 shed 的投递只计一次错过的投递 deadline
  （队列等待计在 source，写入 deadline 计在 writer），transport 错误不再算作投递
  超时，慢 consumer 断开也不再由 writer 和 source 重复计数。
- **session 级 `connectionId` 上限统计 client id 与 data 路由的并集。** 一个 id
  现在只占一个名额，无论 attach 的是 client socket、data socket，还是两者；当
  session 的 data 路由已满时，新 client 会立即被拒绝，而不是先接受、15 秒后才以
  "Data route unavailable" 失败。
- **query 值不再做 trim。** `v` 按原样匹配（`v=+2` 返回 `400`，而不是 v2），
  `connectionId=%20` 会被拒绝，而不是静默变成空值/control 值。对 v2 client，省略
  或空的 `connectionId` 仍表示生成 id；对 v2 daemon，仍表示 control socket。
- **配置校验更严格，也更易诊断。** IPv6 通配地址（`::`）会与同一端口上的任何
  listener 冲突，IPv4 通配地址（`0.0.0.0`）会与同端口的 IPv4 listener 冲突；不同
  地址族的具体地址仍可共存。注入到 `PASEO_RELAY_OPS_PORT` 的 Kubernetes
  service-link 值现在会得到与 `PASEO_RELAY_PORT` 相同的可操作错误（点名
  `paseo-relay-ops`）。`--dump-config` 显示 `per_connection_limit=auto(N)` 或
  clamp 后的值。
- **control pong 使用 1 秒控制发送超时**，而不是 30 秒 data 投递 deadline，因此
  停止读取的 control socket 无法让自己的控制平面停顿半分钟。
- **回收不再把停滞的帧头当成需要整帧。** 约 1 KiB 的每消息开销预留现在使用小请求
  权重作为回收提示；只有 payload chunk 阶段才使用剩余的帧权重，因此发送大帧头但不
  发 payload 的 peer 无法引发一波无关的 in-progress 消息被驱逐。
- **mutex 中毒恢复与有界关闭。** session/writer/capacity/server 锁辅助函数现在会
  恢复受保护的状态，而不是让后续每个调用者都 panic；TLS reload 会拒绝非常规文件
  （FIFO/socket/目录），而不是阻塞在它们上面；关闭时最多等待卡住的后台 watcher
  2 秒，然后退出。
- **文档** 已在中英文之间同步：多架构 index attestation 的说明（存在 index 构建
  provenance；存在 per-platform provenance/SBOM；没有 index 级 SBOM）、新增配置
  和指标、`sync`/64 KiB 语义、TLS healthcheck 固定、发布顺序，以及残余可用性风险。

### 修复

- 初始 v2 control `sync` 以前用 `let _ = ...` 丢弃发送错误；现在会记录 control
  发送失败并关闭 control socket，与 watchdog 路径一致。
- 运维 listener 的拒绝以前不可见；现在会递增
  `paseo_relay_ops_rejections_total`，并发出带 peer 地址的限速警告。
- 普通的 destination transport 错误可能被报告为投递超时和 slow-consumer 断开。
  现在会分类，并按每次被 shed 的投递只计一次（source 或 destination writer），
  只有真正的 deadline 错过才计入 `delivery_timeouts_total`。
- `paseo-relay healthcheck` 在非 loopback 探测目标上不再信任任意证书（见上文安全）。

## [0.1.3] - 2026-11-26

针对 2026-11 安全与项目评审的修复。没有线协议变更：Paseo 的 `relay-transport`
E2E 套件原样通过（与之前一样，9 个通过、2 个跳过）。升级前请阅读“变更”；若干默认
行为有所不同。

### 安全

- **control socket 的 `sync` 放大攻击可能在 ingress 预算之外耗尽内存。** 以前，
  每个等待中的 v2 client 都会生成自己的 control watchdog，在 30 秒写超时内构建并
  排队完整的 `sync` JSON 列表，而单个 `serverId` session 对不同 `connectionId`
  没有上限——这造成二次方（$O(K^2)$）的内存增长（1,200 个空闲连接时约 810 MiB）。
  现在每个 session 强制执行 `PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION`（默认
  `512`，超出时以 `1013` "Relay session connection capacity" 关闭），并运行单个
  去重的 control watchdog，跨 await 只持 `Weak<Session>`，控制发送超时为 1 秒，
  使用预序列化的 `Bytes`。
- **替换或丢弃 source socket 时，即使帧仍排在 destination writer 中，ingress 预算
  也可能被释放。** 慢速 destination 的 writer channel 中排队的帧现在同时携带其
  ingress 预算 guard 的 `Arc<BudgetHold>` 和一个绝对投递 deadline；即使 source
  断开或被替换，writer 也会独立执行该 deadline，shed 停滞的 destination 并把占用
  的预算归还至零。
- **ingress 回收与停滞超时加固。**
  - 把 `HoldState` 跟踪拆分为 `bulk_held` 与 `small_held`，使主池等待者永远不会
    驱逐只占用小消息预留池的持仓；当慢速持仓释放的字节少于所需时，会继续越过它们，
    这样微小的停滞诱饵就无法让更老的 bulk 持仓躲过 60 秒年龄规则。
  - 分离回收节流时钟（`slow`、`patient_bulk`、`patient_small`），使急躁或 bulk
    等待者无法饿死符合条件的 patient/small 等待者。
  - 在 `IngressBudget` 中只索引正在接收消息的连接，而不是在全局锁下扫描所有空闲
    socket。
  - 分片消息（`!fin` 和续帧）现在始终从主池计费，而不是让 `small` 预留字节被一个
    起始分片锁占。
  - 停滞进展（`PASEO_RELAY_READ_STALL_TIMEOUT_MS`）现在要求累计至少 8 KiB 的
    payload 进展，封堵了 0 字节和 1 字节续帧 keepalive 两种绕过方式。
- **upgrade 前 HTTP 响应写超时与握手加固。**
  - upgrade 前的 HTTP 流现在对停滞的响应写入强制执行
    `PASEO_RELAY_HTTP_HEADER_TIMEOUT_MS`（例如 peer 停止读取的流水线化
    `/metrics` 请求），并在 WebSocket upgrade 完成后停用该计时器。
  - `accept_loop` 和 `ops_accept_loop` 现在会在接受新 socket 之前回收已完成的
    `JoinSet` 任务。
  - `/ws` 现在要求 HTTP/1.1（否则 `400`），并使用预计算的 `HashSet` 以 `403`
    拒绝重复的 `Origin` 头。
  - release 校验现在会对两个 lockfile 运行 `cargo audit --deny warnings`，并按
    不可变的 `@sha256:` digest 对已发布镜像做 smoke test。
- **watchdog nudge、写取消和停滞记账存在边界情况。**
  - 在 session 唯一的 control watchdog 已经发送 nudge 之后才 attach 的 v2
    client，可能会在没有收到自己的 `sync` 的情况下被以 `1011` 关闭。watchdog
    现在会记录 nudge 的发送时间，对后到者补发 nudge，并且始终给予完整的 nudge
    后宽限窗口。
  - 取消 destination writer 现在会在 flush close 帧之前清空其队列，因此即使 peer
    已停止读取，字节许可和 ingress 预算 guard 也会立即释放。close flush 上限为
    两秒；停滞写入期间取消会归还预算，而不是等完整个投递窗口。普通 control
    socket 断开不再计为 slow-consumer shed。
  - 控制帧 payload 读取（Ping/Pong/Close）受剩余停滞窗口约束，1 字节的续帧头不再
    重置它；relay 施加的预算等待不计入发送方的停滞时钟。压力下，回收会持续驱逐，
    直到满足等待方所需（最多到 victim 上限），因此微小的老持仓无法掩护大持仓。
  - upgrade 前的 TCP 和 per-IP 名额只在 HTTP `101` upgrade 完成后才释放，等待时间
    由 `HTTP_HEADER_TIMEOUT_MS` 约束，而不是固定的十秒。
- **ingress 预算可能被免费持有。** 两条连接各发送一个未完成的约 32 MiB 分片后
  进入空闲，就能把 Compose/Kubernetes 示例的整个 129 MiB 预算（默认 512 MiB
  预算下是四条连接）占用最多十分钟，而其他每个发送数据的 session 都会在 5 秒后
  以 `1013` 关闭。现在：
  - 在 `PASEO_RELAY_READ_STALL_TIMEOUT_MS`（30 秒）内没有 payload 进展的消息会被
    关闭；
  - 当另一个 source 在等待预算时，relay 会从其他连接的未完成消息中回收预算，而不是
    只关闭等待者：停滞或慢速的消息
    （`PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES`，64 KiB/s）、已传输超过
    `PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS`（60 秒）的消息，以及当等待中的消息
    不超过 64 KiB 时，至少比它大 8 倍的消息。被回收的 source 以 `1013`
    "Relay ingress reclaimed" 关闭；
  - 不超过 64 KiB 的消息有独立的预留池
    （`PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES`，16 MiB 加权），因此大传输占满
    主预算时，握手和普通帧仍能继续流动。

  不在覆盖范围内：已完整接收但从未投递的消息，仍会持有预算直到投递或 data
  attach 的 deadline。`SECURITY.md` 说明了占用预算的剩余成本。
- **session 名额曾比 socket 活得更久。** 空 session 会保留
  `PASEO_RELAY_SESSION_IDLE_MS`（30 秒），因此一个被限制为单个并发 WebSocket 的
  client 可以通过轮换 `serverId` 占满 `PASEO_RELAY_MAX_SESSIONS`，随后 `/ready`
  返回 503，新 daemon 被拒绝。现在，session 在最后一个 socket 关闭时立即移除。
- **per-IP 限制曾没有覆盖 upgrade 前 socket 或 IPv6 前缀。** 设置
  `PASEO_RELAY_PER_IP_MAX_CONNECTIONS` 后，一个 client 仍可用 keep-alive HTTP
  连接占满所有 TCP 名额（使 `/health` 失败），IPv6 client 还可以为每个连接使用
  其前缀中的新地址。现在该设置也会限制 client 的 TCP socket（WebSocket 上限加
  四分之一，至少多 8 个），IPv6 client 按 /64 计数，IPv4-mapped 地址按 IPv4 计数。
- **运维端点曾无法从公网端口移走。** `PASEO_RELAY_OPS_PORT` /
  `PASEO_RELAY_OPS_HOST` 在独立的明文 listener 上提供 `/ready`、`/metrics` 和
  `/version`；公网 listener 随后只回答 `/ws` 和 `/health`。运维 listener 有自己的
  连接额度，在 drain 期间仍会应答，`paseo-relay healthcheck` 也会跟随它，因此公网
  listener 已满的 relay 会被报告为未就绪，而不是让 liveness 探测失败。
- **release provenance 并不是 provenance。** v0.1.2 release 上的
  `provenance.intoto.jsonl` 资产是 workflow 里手写生成的，且没有签名，而 release
  workflow 只是列出它，却没有生成它。现在 release 会对多架构镜像签名（cosign，
  keyless）并附加 GitHub 构建来源证明；手动签名 workflow 从 registry 读取 digest，
  而不是从可编辑的 release notes 读取，不再写 provenance 文件，并会从它签名的
  release 中删除旧资产。验证命令在 `SECURITY.md` 和每个 release 的 notes 中。
- 握手 key 卫生已按其本来面目记录。v0.1.2 的 notes 称它“没有可绕过的字节
  预过滤”；relay 的 JSON 解析器拒绝、但更宽松的解析器能接受的帧（孤立
  代理项转义（surrogate escape）、超出 `f64` 范围的数字、极深的嵌套），或任何
  超过 64 KiB 的帧，都会被原样转发而不做检查。该检查从来不是安全边界，行为也未
  改变；现在它由测试固定，并在 `docs/protocol.md` 中说明。
- `examples/nginx.conf` 设置 `error_log ... crit`：nginx 的 error log 总是包含
  完整请求行，因此限流拒绝和上游错误会把 `serverId` 写入磁盘，尽管 access log
  已省略它。

### 新增

- `PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION`（默认 `512`）：限制每个 `serverId`
  session 的不同 v2 `connectionId` 数量。
- `PASEO_RELAY_WS_IDLE_TIMEOUT_MS`：可选的空闲超时（默认关闭）。如果一个 socket
  在该时长内没有收到任何帧，也没有任何 data 帧转发给它，就以 `1013`
  "Idle timeout" 关闭。
- 重新加载 TLS 证书而不断开 session：收到 `SIGHUP` 时，以及证书或私钥文件变化后
  30 秒内自动进行。如果证书/私钥对无法加载，旧证书会继续生效。
- 对未知 `PASEO_RELAY_*` 变量（拼错的名字以前会被静默忽略）、低于
  `PASEO_RELAY_MAX_TCP_CONNECTIONS` 的 open-file 上限（Linux），以及没有 per-IP
  上限的原生 TLS，启动和 `--check-config` 会发出警告。Kubernetes service-link
  变量会被识别，`PASEO_RELAY_PORT` 中的 service-link 值会得到一条解释它的错误。
- 限速的 `warn` 日志（每种原因每 10 秒一行，附带事件计数），在 relay shed 工作时
  记录：ingress 耗尽或被回收、读取或空闲超时、慢 consumer，以及
  WebSocket/session/TCP/per-IP 拒绝。
- 指标：`paseo_relay_ingress_reclaims_total`、
  `paseo_relay_ingress_rejections_total`、`paseo_relay_read_timeouts_total`、
  `paseo_relay_idle_timeouts_total`、`paseo_relay_tcp_rejections_total`、
  `paseo_relay_ingress_small_reserved_bytes` 和
  `paseo_relay_ingress_small_budget_bytes`；以及两条配套的示例告警。
- 测试：针对上述每个发现，都有基于真实 socket 的回归测试，另外还覆盖原生 TLS
  （正常服务、TLS healthcheck、证书重新加载）、运维 listener，以及随机化的 codec
  往返。CI 现在会构建全部四个 fuzz target，并在每个 pull request 上逐个短暂运行；
  每周 workflow 会运行更长时间（`scripts/fuzz.sh`）。

### 变更

- **共享端口 drain 在宽限窗口内保持运维端点可达。** 当
  `PASEO_RELAY_OPS_PORT` 未设置时，`SIGTERM`/`SIGINT` 会让 listener 在
  `PASEO_RELAY_SHUTDOWN_GRACE_MS` 期间保持打开，因此 `/ready`（`503`）、
  `/health`（`200`）和 `/metrics` 仍可观察，而 `/ws` 以 `503` 拒绝新 upgrade。
- **配置、TLS reload 和 healthcheck 修复。** `Config::validate` 现在会拒绝同一
  端口、同一地址族上的通配/具体 IP 冲突（与 Linux `EADDRINUSE` 一致）；
  `TlsReloader` 会串行化 `SIGHUP`（现在通过 `spawn_blocking` 运行）与 30 秒
  watcher 之间的 reload；`src/main.rs` 增加 `#![forbid(unsafe_code)]`；
  `paseo-relay healthcheck` 会忽略空的 `PASEO_RELAY_TLS_CERT`/`KEY` 值、忽略端口
  `0`、为 IPv6 `Host` 头加上方括号，并在绑定到非通配 IP 时默认使用
  `PASEO_RELAY_OPS_HOST`/`PASEO_RELAY_HOST`；`tokio` 的生产 feature 从 `full`
  收窄到实际使用的模块。
- **未完成消息可能在以前不会关闭的地方被关闭。** 停滞 30 秒的消息即使在空闲节点上
  也会被关闭；在 ingress 预算已满的节点上，慢于 64 KiB/s 或已传输超过 60 秒的
  大上传会让位给更新的上传（`1013`，可重试）。慢速链路请调高
  `PASEO_RELAY_READ_STALL_TIMEOUT_MS`、`PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS`
  或预算，或调低 `PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES`。
- **`SIGHUP` 重新加载 TLS，而不是终止进程。** 更早的版本不处理该信号。
- 小消息预留池是额外预算：最坏情况下的 in-flight 记账现在为
  `INGRESS_BUDGET_BYTES + INGRESS_SMALL_RESERVE_BYTES`（默认 16 MiB 加权，约
  4 MiB payload）。要恢复旧的总量，请把预留设为 `0`。
- `paseo_relay_active_sessions` 不再包含空 session，
  `PASEO_RELAY_SESSION_IDLE_MS` 只负责兜底清扫。
- 在原生 TLS listener 上于 accept 阶段被拒绝的连接会直接关闭且不发送响应；以前它
  会收到明文 `503`，而 TLS client 无法读取。
- 更快的转发：帧头和小的 payload 用一次 socket 读代替三次读取，一帧的头和 payload
  用一次写入代替两次写入。同机 A/B，100 对：1 KiB 下从 52–58k 提升到 58–70k
  往返/秒（`docs/benchmarks.md`）。空闲 WebSocket 多占用约 0.8 KiB（macOS/arm64
  上 13.5 → 14.3 KiB）；`× ~30 KiB` 的规划数字不变。
- 依赖：`url` 只被一个测试辅助函数使用，`serde` 的 derive feature 完全没用到；
  移除它们后，28 个包（ICU/IDNA 树）从构建中消失，135 → 107。`rcgen` 是 TLS
  测试新增的 dev-dependency。
- 库的支持面是 `Config`、`serve` 和 `ServerHandle`（加上 `config` 模块）。其他
  模块仍为 fuzz target 保持 public，但从文档中隐藏，且可能在任何 release 中变化。
  `ServerHandle` 新增 `ops_local_addr` 和 `reload_tls`。
- 示例：systemd unit 设置 `LimitNOFILE=65536`（默认软限制 1024 远低于 relay 的
  连接上限）和 `ExecReload`；Kubernetes 示例设置 `enableServiceLinks: false`，
  因为名为 `paseo-relay` 的 Service 否则会向 pod 注入
  `PASEO_RELAY_PORT=tcp://...`；nginx 用自己的 `429` 回应限流。运维 listener 的
  设置以注释形式包含，因为固定的 `v0.1.2` 镜像早于这些设置。

## [0.1.2] - 2026-11-08

### 安全

- ingress 预算记账现在改为增量式：data frame 的 payload 按 8 KiB chunk 读取，只在
  字节到达时计费，每 chunk 的记账也一并计入，并有每连接 in-flight 上限和 1024
  分片限制。声明了 payload 却从不发送的 peer（停滞 payload），或者连续发送空续帧
  的流，不再能预留全节点预算并以 `1013` 关闭其他 session。ingress 预算等待还会被
  剩余的帧/消息读取 deadline 封顶。
- upgrade 前 HTTP 内存有界：hyper 以 16 KiB 读缓冲、16 KiB header 上限和 64 个
  header 运行，因此未认证的 TCP 连接无法在 WebSocket admission 之前让 relay 为
  每个连接缓冲约 400 KiB。
- TCP 名额在整个 socket 生命周期内持有，因此 `PASEO_RELAY_MAX_TCP_CONNECTIONS`
  再次限制打开 socket 的总数。
- 加固：非法 close code 和非最小帧长度编码以 `1002` 拒绝；配置 allowlist 时，
  无法解析的 `Origin` 头会被拒绝；握手/控制 JSON 解析使用轻量字节预过滤，握手卫生
  上限从 1 MiB 降到 64 KiB；writer 投递使用一个绝对 deadline，而不是最多三个
  串行超时；构建 revision 在嵌入 `/version` 和指标之前会被净化。

### 新增

- 两版 README 和部署指南中，在前台 quickstart 检查旁边增加了生产用 `docker run`
  快速配方（detached、`--restart unless-stopped`、30 秒停止超时）。
- `paseo_relay_ingress_budget_bytes` gauge 指标，使运维人员可以按配置预算的比例
  对 ingress 饱和告警。
- 针对 RFC 6455 frame reader 和 upgrade 校验的 fuzz target。

### 变更

- release workflow 在匹配的 GitHub runner 上并行原生构建 `linux/amd64` 和
  `linux/arm64`，使用 GitHub Actions layer 缓存和发布后 smoke test。这取代了单个
  QEMU 多架构构建，把镜像阶段从约 25 分钟缩短到几分钟。该 workflow 仍然只在推送
  `v*` tag 时运行。
- Dependabot PR 按 ecosystem（cargo、GitHub Actions、Docker）分组，每个 workflow
  现在都固定当前 action release，因此每周依赖更新最多每个 ecosystem 一个 PR。
- `PASEO_RELAY_INGRESS_WEIGHT` 现在接受 4..=16（最小值是文档化的瞬态组装拷贝
  安全下限）。非法的 `PASEO_RELAY_LOG` 值，以及不是常规、可读文件的 TLS
  证书/私钥路径，现在会让启动和 `--check-config` 失败。
- 发布加固：只有 stable tag（不含 `-`）才会移动 GHCR 的 `latest` tag，并且
  release workflow 按 job 授予写权限，而不是全局授予。
- Quickstart 示例按其文档化的内存设置：Compose 把连接上限设为 4000
  sessions/5000 TCP socket，设置 129 MiB ingress 预算（512 MiB RAM）、
  `nofile`/内存限制；边缘示例不再记录含 `serverId` 的 query string，也不再默认
  暴露运维端点。
- Kubernetes 示例禁用 service-account token 自动挂载，增加 NetworkPolicy，并为
  其 1 Gi 内存限制设置连接上限。

### 修复

- Compose quickstart 无法启动：随附的 128 MiB ingress 预算低于所需的一条最大消息
  预留；现在改为 129 MiB。
- 一帧的 writer 投递 deadline 不再最多串行应用三次。

## [0.1.1] - 2026-11-07

### 新增

- 文档各处的图：`DESIGN.md`/`DESIGN.zh-CN.md` 中的 Mermaid 连接生命周期流程图；
  两版 README 中的单节点/多节点部署拓扑；v1 和 v2 协议时序图；部署指南中的多节点
  `serverId` 哈希拓扑和 drain/rollout 序列；配置指南中的 ingress 预算/有界队列
  流程；以及 quickstart 中的“什么在哪里运行”图（英文和中文）。
- 单节点 Compose 示例的源码构建 override `examples/docker-compose.build.yml`。
- `scripts/check-docs.py` 现在会对未对齐的带框 ASCII 图失败（East Asian 字符按
  两个显示列计算），因此 README/DESIGN 的图回归会在 CI 中被捕获。

### 变更

- 文档现在默认使用已发布的 release 镜像
  （`ghcr.io/kfxhjz/paseo-relay:v0.1.1`），而不是本地源码构建：README 30 秒快速
  开始、VPS quickstart、部署指南、Compose 示例和 Kubernetes 示例。源码构建和
  `cargo run` 仍作为清晰标注的开发者路径保留。部署指南和 release notes 中记录了
  可选的不可变 digest 固定方式。
- 依赖：`base64` 0.22 -> 0.23、`rand` 0.9 -> 0.10、dev-dependency
  `tokio-tungstenite` 0.28 -> 0.30、`smallvec` 1.16.4 -> 1.16.5。这从 lockfile
  中移除了遗留的 `digest` 0.10 / `generic-array` 树。

### 修复

- 对齐了 `README.md`、`README.zh-CN.md`、`DESIGN.md` 和 `DESIGN.zh-CN.md` 中的
  ASCII 架构图（包括 CJK 显示宽度和溢出的 `SessionRegistry` 行），以及
  `docs/zh-CN/design-summary.md` 中的续行线。
- 修正文档中的镜像 tag：GHCR release tag 带前导 `v`（例如 `v0.1.1`，而不是
  `0.1.1`）。短版本 tag 现在是文档化的默认值；release digest 只在可选的不可变
  固定场景中提及。

## [0.1.0] - 2026-11-07

首个公开的 Rust 重写版本。架构和设计不变量见 `DESIGN.md`。

### 新增

- 完整的中文 DESIGN 翻译（`DESIGN.zh-CN.md`），以及 docs/i18n CI 检查，防止
  中文页面静默链接到仅英文的文档。
- 单节点 Caddy quickstart、运营者策略、支持指南、对比指南、logo 和社交预览
  资产。
- 中文文档：`README.zh-CN.md`，翻译的
  protocol/configuration/deployment/compatibility/operations/migration/benchmarks/FAQ
  文档，中文安全与贡献指南，以及中文设计摘要。
- 已对照 Paseo v0.15.3（`ea30d261`）验证：Paseo 的 relay transport E2E 套件通过
  9/9 个可运行测试，0 失败；v1 路径还通过开发期手动 wire probe 做了额外验证
  （probe 脚本/记录未提交）。
- 原生 rustls TLS listener（`PASEO_RELAY_TLS_CERT` /
  `PASEO_RELAY_TLS_KEY`）。
- per-IP WebSocket 限制、节点级 WebSocket/TCP/session 限制，以及每连接 client
  上限。
- 带固定每消息开销的加权 ingress 字节预算，在分配 payload 之前预留。
- 帧和分片消息的读取 deadline。
- 与 Elixir relay 一致的 control nudge / `1011 Control unresponsive` watchdog。
- 优雅的 SIGTERM/SIGINT drain 和 `PASEO_RELAY_DRAIN`。
- Prometheus `/metrics`、liveness/readiness 端点、不含路由 ID 的结构化日志。
- Distroless 风格的 scratch 镜像、非 root UID、CI，以及可复现的负载生成器。

### 变更

- 文档树规范化：指南文件名小写、英文文档索引、ADR 索引，以及集中到
  `docs/zh-CN/` 下的中文翻译（根目录的本地化文件保留 `.zh-CN.md` 后缀）。
- 线协议兼容的 v1/v2 Paseo relay 语义，仅支持单节点路由。
- 对 `serverId` 和 `connectionId` 进行严格的 UTF-8/可打印字符校验。
- 握手 key 校验仅限于小型 JSON 形状的 client 帧，并记录为协议卫生，而不是安全
  边界。

### 安全

- 没有 Erlang VM、EPMD、分布式 Erlang、node cookie 或远程 shell。
- 失败的 WebSocket upgrade 无法创建 session 或容量状态。
- source 断开不会关闭 destination；慢 consumer 在自己的 deadline 被 shed。
- 非法 UTF-8 ID 以 HTTP 400 拒绝，绝不会被编码，也不会导致崩溃。

[Unreleased]: https://github.com/kfxhjz/paseo-relay/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.4
[0.1.3]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.3
[0.1.2]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.2
[0.1.1]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.1
[0.1.0]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.0
