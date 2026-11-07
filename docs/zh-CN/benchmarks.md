# 性能基准

[English](../benchmarks.md) | **简体中文**

这些是 **loopback  sanity 测量**，不是 WAN benchmark，也不是跨实现对比。它们用于
回归检查，并让用户看到数量级。

## 环境

- 日期：2026-11-07
- 主机：Apple M3，8 vCPU，macOS 27.2（arm64）
- 构建：`cargo build --release --locked`（Rust 1.99.1）
- relay：单进程，明文 loopback，默认限制，无 TLS
- 客户端/压测：`cargo run --release --example load`，同机
- 负载：100 个 v1 server/client pair，每对 500 次请求/响应往返，1 KiB 文本消息，
  relay 双向转发

## 结果

| 指标 | 结果 |
|---|---|
| 往返次数 | 50,000 |
| 吞吐 | 不同运行约 41,000–45,000 往返 msg/s |
| 转发 payload | 双向约 80–87 MiB/s |
| p50 往返延迟 | 约 2.2–2.3 ms |
| p99 往返延迟 | 约 3.3–3.6 ms |
| 最大往返 | 不同运行 6.8–14.8 ms |
| load 后 relay RSS | 约 7.3 MiB |
| relay 二进制 | 约 2.9 MiB（静态 musl） |
| 容器镜像 | 约 3.4 MB（scratch，单平台） |
| 进程启动 → `/health` 200 | 约 31 ms（同机、warm release binary） |

命令：

```sh
cargo build --release --locked
PASEO_RELAY_PORT=14221 PASEO_RELAY_LOG=warn ./target/release/paseo-relay &
cargo run --release --example load -- \
  --addr 127.0.0.1:14221 --pairs 100 --frames 500 --size 1024
ps -o rss= -p "$(pgrep -f target/release/paseo-relay)"
```

## 注意事项

- 客户端和 relay 同机，CPU、loopback 网络和调度都会影响结果。
- Harness 每个 pair 串行等待完整往返；测的是 relay 转发 + 往返开销，不是单向流式
  吞吐上限。
- 无 TLS、无边缘代理、无 WAN 延迟、无丢包。
- 连接数/RSS（1k/10k session）、启动时间、p99.9 和镜像大小都值得继续补测；未来
  发布数据时建议同时提交原始 JSON。

除非在同一主机、同一负载和配置下测量并公布原始数据，否则不要把这些数字当作与
其他 relay 的对比结论。
