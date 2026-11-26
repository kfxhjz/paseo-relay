# 安全策略

[English](SECURITY.md) | **简体中文**

英文 `SECURITY.md` 是规范版本；本文是中文说明。若两者冲突，以英文为准。

## 支持版本

安全修复会提供给最新发布的 minor 版本（当前 `0.1.x`）。开发在 `main`；如果漏洞
尚未发布修复，请也测试 `main`。

## 报告漏洞

**不要开公开 issue**。请使用本仓库 GitHub 的私密漏洞报告（Security → Report a
vulnerability），或通过仓库配置的安全联系方式联系维护者。

报告请包含：

- 受影响版本或 commit；
- 最小复现或协议序列；
- 你认为的影响（机密性、完整性、可用性或元数据泄漏）；
- 部署方式是直连还是经过边缘代理/负载均衡。

不要包含真实的 `serverId`、配对链接、凭证或客户 payload。需要展示协议字节时，
请使用隔离的测试 session。

我们会在 3 个工作日内确认，并与你协调披露时间线；请在修复发布前不要公开披露。

## 审计状态

本 relay **没有经过独立第三方安全审计**。仓库中有代码评审记录、真实 socket 测试、
fuzz targets（每个 pull request 都会构建并短时间运行，每周再长时间运行一次）和 Paseo
兼容性证据；这些不能替代审计。v0.1.3 是早期软件，请根据自己的部署阅读威胁模型。

## 威胁模型

Paseo 把 relay 当作 **不可信** 组件。消息内容由 Paseo client 与 daemon 之间的
端到端加密保护（Noise NK 或 legacy box）；relay 转发不透明帧，不需要也不能解密。
relay 能看到 IP、时序、帧大小、`serverId`/`connectionId` 以及明文握手中的公钥。

`serverId` 是 **bearer 路由秘密**，不是认证 token。知道它的人可以 attach 到该
session 并破坏可用性。它出现在 WebSocket URL query 中；请在边缘/代理日志中脱敏
query string，并把配对链接和二维码当作秘密。relay 按设计没有 relay-level 认证；
需要更强隔离的部署应放在 VPN/mTLS 或带认证的边缘之后。

**残余可用性风险。** relay 没有认证，所以能在配置上限内发送真实流量的 peer 仍然可以
占用共享的 ingress 预算。relay 能控制的是这样做的代价，以及它能拖垮节点的多大一部分。
（以下数字按默认值计算，会随预算和超时等比例变化。）

- *未完成的消息。* 一条开始了却不发完的消息必须持续有数据到达：静默的消息在
  `READ_STALL_TIMEOUT_MS`（30 秒）后被关闭，一旦有别的 source 在等预算，大约两秒内
  就会被关闭。持续慢速发送的消息在 `INGRESS_RECLAIM_AFTER_MS`（60 秒）后要让位给
  等待方。因此用这种方式占满主预算，意味着大约每分钟把它持有的数据重发一遍：默认
  512 MiB 预算约需 17 Mbit/s，Compose/Kubernetes 示例的 129 MiB 预算约需
  4.5 Mbit/s。在 v0.1.2 及之前，同样的预算只需发送一次然后保持空闲，就能占住十分钟。
- *永远不被投递的消息。* 已完整接收的消息不会被回收。如果 peer 把大消息发给一个不读取
  的 destination（它自己的另一个 socket），或作为 v2 client 发送而 data socket 始终
  不 attach，这些消息的预算会一直占到 `DELIVERY_TIMEOUT_MS`（30 秒）或
  `DATA_ATTACH_TIMEOUT_MS`（15 秒）到期——即使源连接已断开或被替换，队列中的每条待写帧
  仍持有其 ingress 预算 guard 与绝对投递 deadline，不会脱账也不会在慢消费端无限滞留。
  单个连接最多能持有它的 in-flight 上限：默认是
  `min(全局预算, 4 × 一条最大消息的计费)`（默认配置下就是整个预算），也可以用
  `PASEO_RELAY_PER_CONNECTION_LIMIT_BYTES` 显式设置。要持续维持，大约每 15–30 秒需要
  发送一整个预算量的 payload：默认约 35–70 Mbit/s，129 MiB 时约 9–18 Mbit/s。调低这两个
  超时可以缩短这个窗口。把每连接上限设到预算以下只能减少单个 source 的份额，多条协作
  连接仍可占满预算，而 `PASEO_RELAY_PER_IP_MAX_CONNECTIONS` 默认关闭；当派生上限等于
  预算且没有 per-IP 上限时，relay 会在启动时告警。主要缓解手段仍然是边缘带宽/速率限制。
- *它能拒绝什么。* 主预算被占满期间，其他 source 大于 64 KiB 的消息（以及分片消息）
  需要等待，并可能以 `1013`（可重试）关闭。不超过 64 KiB 的完整单帧消息（所有握手和大
  多数普通流量）由独立的小消息预留池服务；如果预留池也满了，它们会挤掉最大的那条未完成
  消息。要把它们也拒绝掉，必须用"永远不被投递的消息"同时占满两个池。控制通道的 `sync`
  通知也不能绕过这套预算：每个 session 限制不同 v2 `connectionId` 的 **并集**（client
  id 与 daemon data 路由共用 `PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION`，默认 `512`），
  只运行单个去重的 control watchdog，并把每条生成的 `sync` 限制在 64 KiB 内。会让下一
  条 `sync` 超限的 client 会在接入前以 `1013` "Relay session sync capacity" 拒绝，所以
  relay 不会发出超大的控制帧；初始或 watchdog `sync` 发送失败会关闭 control socket，
  而不是留下过期路由表。

这些都受文档化的预算和 deadline 约束，但不会消失。请用边缘带宽/速率限制缓解，并对
`paseo_relay_ingress_rejections_total`（确实有 source 被丢弃）以及
`paseo_relay_ingress_reclaims_total` 的持续上升设置告警。默认 512 MiB 预算略小于
四条最大消息的加权计费，因此最大的 4 消息流水线可能看到正常的 `1013` 预算反压。

范围内：

- 网络输入可触发的内存安全/panic；
- 绕过文档化上限或 ingress budget 的资源耗尽；
- 协议混淆、跨 session 投递或会破坏 Paseo session 的 close code 变化；
- 静默禁用文档化保护措施的 TLS/配置 bug；
- 意外记录 ID、payload 或秘密。

范围外（按设计，文档已说明）：

- relay 能观察元数据，或丢弃/延迟/重排帧；
- 依赖消耗已配置、已文档化容量的可用性攻击（请用边缘限流和监控）；
- 没有 relay 认证；
- Paseo client/daemon 的 E2EE 实现安全；
- 由你自己的部署日志导致的 `serverId` 泄漏。

## 加固基线

见 README 的 hardening checklist 和 [docs/zh-CN/deployment.md](docs/zh-CN/deployment.md)。
至少：终止 TLS、不要公开 `/metrics` 和 `/ready`、在边缘做 per-IP 限制、用提供的
非 root 用户以 read-only 运行容器，并保持 relay 和 Rust 工具链更新。v0.1.2 之后的
版本请设置 `PASEO_RELAY_OPS_PORT`，让运维端点根本不在公网端口上；relay 直接暴露
（原生 TLS、前面没有代理）时请设置 `PASEO_RELAY_PER_IP_MAX_CONNECTIONS`——它默认
关闭，是因为在代理之后所有客户端共用代理的地址。进程的 open-file 上限要高于
`PASEO_RELAY_MAX_TCP_CONNECTIONS`。连接上限要按
主机内存设置：relay 在边缘后为明文时一个 idle 已接受 socket 约占 23 KiB，原生 TLS
时约 33 KiB，按 `PASEO_RELAY_MAX_TCP_CONNECTIONS × ~30 KiB`（明文）或
`× ~35 KiB`（原生 TLS）规划；内置 25,000 socket 默认值约需 700–800 MiB（明文）或
约 850–900 MiB（原生 TLS），小节点必须调低上限。

`v0.1.3` 之后的版本中，内置 `paseo-relay healthcheck` 不再接受任意证书。原生 TLS 下
它会读取 `PASEO_RELAY_TLS_CERT` 的 leaf 证书，固定其 DER SHA-256 并只接受探测目标提供
的这一张证书，因此其他本地服务或路径 MITM 无法在 relay 不健康时返回 `200`。如果配置了
原生 TLS 但证书文件不可读，探针只信任 loopback 目标，对任何非 loopback 的
`PASEO_RELAY_HEALTHCHECK_HOST` fail-closed。明文监听没有可固定证书：仍允许非 loopback
探测，但无法认证，请只在可信网络使用。

`v0.1.3` 之后的版本可以用 `PASEO_RELAY_PER_CONNECTION_LIMIT_BYTES` 调低自动派生的每连接
ingress 上限（`min(budget, 4 × 一条最大消息的计费)`）。这能减少单个慢 source 能占的共享
预算，但不是上条残余可用性风险的完整修复：多条连接仍可占满预算，且 per-IP 上限默认关闭。
主要控制手段仍然是边缘的 per-source 带宽/速率限制。

## 验证发布产物

v0.1.2 之后的 release 在构建时即完成签名和证明。运行镜像之前，请针对本仓库的 release
workflow 验证它：

```sh
IMAGE=ghcr.io/kfxhjz/paseo-relay@sha256:<digest>   # 见 release notes

cosign verify "$IMAGE" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github\.com/kfxhjz/paseo-relay/\.github/workflows/release\.yml@refs/tags/v'

gh attestation verify "oci://$IMAGE" --repo kfxhjz/paseo-relay
```

第一条命令校验多架构镜像上的 Sigstore keyless 签名；第二条校验 GitHub 的构建来源证明
（build provenance attestation），其中记录了产出该 digest 的 commit 和 workflow。

发布顺序保证 tag 不会指向未验证的内容。release workflow 先校验 tag 必须是严格的
`vX.Y.Z[-prerelease]`、与 `Cargo.toml` 一致、CHANGELOG 有对应段落、且 commit 在
`main` 上；然后构建两个平台，在临时 `candidate-<sha>` tag 下组装多架构 index，按
digest 做 smoke test，再用 cosign keyless 签名并生成 GitHub build provenance
attestation。只有这些全部成功，才把版本 tag 提升到同一个 digest——`latest` 还要求
release 稳定且不低于 `main` 上最高 stable tag。因此构建、smoke 或签名任一步失败都会
保留原来的 `latest`，乱序排队中的旧 release 也无法接管 `latest`。

### 仓库设置（维护者）

部分发布控制属于 GitHub 仓库设置而不是 workflow 代码，被攻破的 workflow 也无法对自己
强制执行。维护者应：

- 保护 `v*` tag（tag ruleset），防止删除或强制移动，并限制创建（为发布者留 bypass）；
- 在 `main` 上要求完整 CI check 集：`Format`、`Clippy`、`Test`、`Release build`、
  `Dependency audit`，再加上 **`Fuzz targets (build + smoke run)`** 和
  **`Container image`**（CodeQL 用 code-scanning ruleset），这样坏掉的 fuzz target
  或镜像无法合并；
- 给 publish job 使用的 `release` environment 配置 required reviewers，让发布在
  candidate digest 已存在、但版本 tag/`latest` 尚未提升时等待人工批准。

release verify job 也会跑 fuzz smoke，所以即使分支保护设置遗漏，发布也无法跳过它。

v0.1.2 及更早的版本无法用这种方式验证。v0.1.2 的 release 有一个带分离签名的
`image-digest.txt` 资产，但它签的是一个写着 digest 的文本文件，而不是镜像本身。它的
`provenance.intoto.jsonl` 资产是 workflow 里手写生成的，没有签名，**不是**构建来源
证明，请不要依赖它。v0.1.0 和 v0.1.1 没有任何签名资产。（release workflow 确实让
buildx 给 registry 中的镜像附加了 per-platform provenance 和 SBOM attestation；那些是
构建过程自己产出的。）
