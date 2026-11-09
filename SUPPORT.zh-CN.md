# 支持

[English](SUPPORT.md) | **简体中文**

## 获取帮助

- **文档**：可搜索的[文档站](https://kfxhjz.github.io/paseo-relay/)、
  [README.zh-CN.md](README.zh-CN.md)、[docs/zh-CN/README.md](docs/zh-CN/README.md)
- **部署问题/社区帮助**：[GitHub Discussions](https://github.com/kfxhjz/paseo-relay/discussions)
  （Q&A / Show and tell）
- **Bug/功能请求**：使用 GitHub issue 模板。请提供 relay 版本/commit、部署拓扑和脱敏日志。
- **安全漏洞**：按 [SECURITY.zh-CN.md](SECURITY.zh-CN.md) 走私密报告；不要开公开 issue。
- **Paseo client/daemon/上游协议问题**：请走 [Paseo 项目](https://github.com/getpaseo/paseo)
  的渠道。本仓库是社区 relay，不是 Paseo 上游。

## 支持范围

这是志愿者维护的社区项目，**没有 SLA、付费支持或保证响应时间**。维护者会尽力
triage，但支持是 best-effort。

范围内：

- 本仓库文档化 runtime contract 内的 relay bug、协议行为、资源/限制、部署和升级问题；
- 可复现的 Paseo relay 传输兼容性失败；
- 通过私密渠道提交的安全报告。

范围外：

- 帮你运维 VPS/负载均衡/Kubernetes；
- Paseo client/daemon 的 bug 或上游功能请求；
- 调试自定义修改、fork 或不支持的多节点拓扑；
- 处理你公共 relay 上的滥用（见 [运营公共 relay](docs/zh-CN/operator-policy.md)）；
- relay 级认证或 per-user 配额功能（见 [完整中文设计](DESIGN.zh-CN.md)（英文规范 [DESIGN.md（英文）](DESIGN.md)）的非目标）。

## 提 issue 前

1. 在最新 release 或 `main` 上复现。
2. 运行 `paseo-relay --version` 和 `paseo-relay --check-config`。
3. 附上 `/ready`、相关 `/metrics` 计数器和 relay 结构化日志（脱敏
   `serverId`/`connectionId`/配对链接）。
4. 如果改了 wire 行为，附上 Paseo 兼容性命令和结果。

## 贡献

见 [CONTRIBUTING.zh-CN.md](CONTRIBUTING.zh-CN.md)。适合新贡献者的方向：文档、部署
示例、兼容性测试、fuzz/测试覆盖。
