# 运营公共 relay

[English](../operator-policy.md) | **简体中文**

本页面向想把 relay 开放给其他 Paseo 用户（而不只是自己用）的运营者。这是运维建议，
不构成法律意见。

## 你继承的安全模型

relay 按设计 **没有认证/授权**。`serverId` 是 bearer 路由秘密；知道它的人可以
attach 到该 session 并破坏可用性。Paseo payload 端到端加密且对 relay 不透明，
所以你无法检查、审核或按内容过滤消息。

这意味着：

- 公共 relay 是一个 **开放的 signaling/rendezvous 服务**，任何持有有效
  `serverId` 的人都可以使用；它不是匿名网络；
- 你可以按元数据/流量限流和监控，但不能按内容分类流量；
- 如果无法承担滥用/可用性风险，请只在私网运行（VPN、mTLS 或带认证的边缘），
  不要公开暴露。

## 最低控制

- 终止 TLS；不要公开明文 `ws://`。
- 前面放限流边缘（CDN/WAF/LB）：per-IP 连接数、建连速率、消息速率、带宽。
  仓库提供的 Nginx/HAProxy 示例已经包含 per-client 连接/速率限制、拒绝运维端点、
  不记录 query string 的日志；请按自己的流量调整阈值。Caddy 示例没有进程内
  per-IP 限流，需要配合外部 CDN/WAF。LB 终止连接时，进程内
  `PASEO_RELAY_PER_IP_MAX_CONNECTIONS` 看到的是代理 IP；请在边缘后保持 `0`，
  在边缘限制。
- 按主机规模设置 `PASEO_RELAY_MAX_CONNECTIONS`、`MAX_TCP_CONNECTIONS`、
  `MAX_SESSIONS` 和 ingress 预算；监控 `/ready` 和 `/metrics`。一个 idle 已接受
  socket 仍占约 23 KiB，因此 `MAX_TCP_CONNECTIONS × ~30 KiB` 是内存下限，
  512 MB 主机不能使用内置 25,000 默认值。
- 客户端应只发送一个未编码的 `serverId`。重复 query key 在 relay 侧是 last-wins，
  而边缘一致性哈希通常使用原始 query 值，因此重复或 percent-encoded 的值可能把
  同一 session 的两端路由到不同 relay 节点。
- `/metrics` 只放内网，并限制访问。
- 使用提供的 scratch 镜像：read-only、非 root、drop capabilities；保持工具链/
  镜像更新。
- 不要添加 Erlang distribution；本 relay 没有。

## 日志、隐私与保留

- relay 刻意不记录 `serverId`、`connectionId` 或 payload。
- 你的边缘/代理/CDN 可能记录完整 URL；**query string 含 `serverId`**。请脱敏或
  关闭 query 日志，并设置保留期。
- 内存中的 session 拓扑和 per-IP 状态是临时的。请决定边缘是否记录 client IP，
  并向用户说明。
- 不要承诺匿名。你仍然能在边缘观察 IP、时序、帧大小和 session ID。
- 如果受数据保护法规约束，请发布简短隐私说明：边缘日志、保留期、滥用处理。

## 滥用处理

- 公布 abuse 联系方式和简短政策（禁止什么、如何响应、保留什么数据）。
- 优先用容量/连接限制；不要尝试检查 payload——协议不允许，也破坏 E2EE 信任模型。
- 针对性滥用可在边缘封 IP/网段、降低全局上限、临时关闭 listener。不要记录超过
  必要的标识符。
- 保留 incident 记录（时间、指标、动作）；不要公开他人的 `serverId` 或配对链接。

## 容量与可用性

- relay 节点很小，但带宽和连接数才是实际限制。idle 内存主要由已接受 socket
  决定（每个约 23 KiB），请按 `MAX_TCP_CONNECTIONS × ~30 KiB` 加 ingress 预算
  规划，并给突发和 Caddy/proxy 留余量。按主机设置 `PASEO_RELAY_MAX_CONNECTIONS`、
  `MAX_SESSIONS` 和 ingress 预算。
- 多节点必须用边缘按 `serverId` query 一致性哈希，并用 `/ready` 摘除 draining 节点。
  普通 round-robin 会拆分 session。
- SIGTERM drain，等待 grace window，再移除节点；不要先 SIGKILL。
- 为计划维护提供状态页/公告；即使优雅 drain，用户也会看到重连。

## 公开前检查

- [ ] 只提供 TLS + HTTPS/WSS，考虑 HSTS。
- [ ] 边缘 per-IP/请求/带宽限制。
- [ ] query string 日志关闭或脱敏。
- [ ] abuse 联系方式和隐私/保留说明已发布。
- [ ] `/metrics` 和 `/ready` 仅内网。
- [ ] 容量限制和告警已配置。
- [ ] 镜像按 digest 固定，并在安全更新时升级。
- [ ] 演练过 incident/回滚流程（SIGTERM drain、恢复旧镜像）。
- [ ] 明确声明：社区 relay，不是 Paseo 托管 relay，无 SLA。
