# 文档与翻译策略

[English](../translation.md) | **简体中文**

英文是 runtime contract、协议、安全模型和兼容性声明的规范语言。中文（`zh-CN`）翻译
用于让中文用户不必先读英文就能采用和运维 relay。

## 文件命名

- 英文：`README.md`、`docs/protocol.md`、`SECURITY.md` ...
- 简体中文：`README.zh-CN.md`、`docs/zh-CN/protocol.md`、`SECURITY.zh-CN.md` ...
- 中文文档索引：[docs/zh-CN/README.md](README.md)。
- 完整中文设计：[DESIGN.zh-CN.md](../../DESIGN.zh-CN.md)；
  [docs/zh-CN/design-summary.md](design-summary.md) 是较短的架构/不变量摘要。

## 规则

1. **先改英文**；如果改动影响用户可见行为、限制、命令、安全声明或兼容性，在同一个
   PR 里更新中文文件。
2. **不要翻译标识符**：环境变量名、路径、HTTP 路径、close code、指标名、Rust/TOML
   片段、Paseo 协议字段名保持原样。
3. **中文文档不能静默链接到英文文档。** 有中文翻译就链中文；没有就标注
   `English`/`英文`，或链接中文摘要，让读者知道打开的是什么。
4. **保留语言切换器**，放在成对文档顶部。
5. **不要复制会与英文产生冲突的安全/限制声明**；未完整翻译时使用摘要 + 明确英文链接。
6. 新增翻译文档时更新 [docs/zh-CN/README.md](README.md) 和根 README。

## 本地检查

```sh
python3 scripts/check-docs.py
```

脚本会检查相对 Markdown 链接，并标记“中文文档链接了已知英文规范文档，却没有链接
中文版本或标注 English/英文”的情况。CI 会运行它。

## 当前覆盖

中文：README、完整设计、协议、配置、部署、quickstart、兼容性、运维、迁移、benchmark、
FAQ、安全、贡献、支持、设计摘要、ADR 摘要、E2E 证据、行为准则。

仅英文：ADR 原文、changelog、许可证文本。从中文文档链接时已明确标注。
DESIGN 有完整中文翻译，英文版仍是规范来源。
