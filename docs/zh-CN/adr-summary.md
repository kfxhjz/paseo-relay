# 架构决策记录（中文摘要）

[English ADRs](../adr/README.md) | **简体中文**

## ADR 0001：单节点 + 边缘一致性哈希

- **决策：** relay 是单节点数据面；同一个 `serverId` 的所有 socket 必须由边缘 LB
  按 query 参数一致性哈希到同一进程。
- **原因：** 避免 Syn 一致性、split-brain、集群注册表和 Erlang distribution 安全面。
- **代价：** 运维需要配置边缘哈希；节点轮换对客户端是重连；配置错误会把一个
  session 拆到两个节点，relay 无法修复。

## ADR 0002：自研 RFC 6455 codec

- **决策：** 直接基于 `AsyncRead`/`AsyncWrite` 实现服务端 RFC 6455 帧编解码，
  header 与 payload 分阶段读取。
- **原因：** 需要在 payload 字节到达时逐块计入 ingress 预算（按 8 KiB chunk，每个
  连接最多一个 chunk 在计费前分配，而不是按声明长度一次性预留），并精确执行 Paseo
  的 32 MiB、分片、control frame 限制；高层 WebSocket 库通常会先完整组装消息。
- **代价：** 维护协议代码；用单元测试、集成测试和 fuzz scaffold 覆盖；不支持压缩。

## ADR 0003：relay 不做认证

- **决策：** relay 没有 authentication/authorization；`serverId` 是 bearer routing
  secret。
- **原因：** Paseo 的威胁模型把 relay 当作不可信，内容安全由 E2EE 负责；relay 级
  认证仍不能保护内容，只改变资源滥用者。
- **代价：** 可用性风险由知道 `serverId` 的人承担；用边缘限流、VPN/mTLS、日志脱敏
  缓解。未来若增加 relay token，需要 Paseo 上游协同。

完整英文 ADR 在 [docs/adr/](../adr/README.md)，中文摘要与英文规范不一致时以英文为准。
