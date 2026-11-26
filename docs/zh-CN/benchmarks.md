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

## v0.1.2 之后的变化（0.1.3）

2026-11-26 同机 A/B 对比（Apple M3，8 核，macOS 27.2.1，Rust 1.99.1）：`main` 的
`4a60da1`（源码与 `v0.1.2` 完全相同）对比 `0.1.3` 的代码。两者都是 release 构建，交替
运行，各跑三次，命令同上，只改变 `--size`。

| 负载（100 个 pair × 500 次往返） | `v0.1.2` 源码 | 0.1.3 |
|---|---|---|
| 64 B 文本 | 52.8–55.3k msg/s | 56.3–68.2k msg/s |
| 1 KiB 文本 | 52.1–58.4k msg/s | 58.5–70.0k msg/s |
| 16 KiB 文本 | 35.6–36.6k msg/s | 40.1–44.1k msg/s |
| 1 KiB 的 p50 往返延迟 | 1.78–1.83 ms | 1.42–1.68 ms |
| 1 KiB 负载后的 relay RSS | 约 7.5 MiB | 约 7.8 MiB |
| 每个 idle WebSocket 的内存（4,000 个 idle socket） | 约 13.5 KiB | 约 14.3 KiB |

吞吐的提升来自 codec：帧头和小 payload 现在一次 socket 读取就能读到（原来三次），帧头
和 payload 一次写出（原来两次）。每个 idle socket 多出的约 0.8 KiB，是 reader 的
256 字节预读缓冲，加上预算回收和 idle 时钟所需的每连接记账。

这里 `v0.1.2` 一列比上面表格（2026-11-07 测得）更快：笔记本上的绝对数字会随系统版本
和机器状态变化。只有同一轮内的对比才有意义；上面的二进制大小、镜像大小和启动时间没有
重新测量。

## 注意事项

- 客户端和 relay 同机，CPU、loopback 网络和调度都会影响结果。
- Harness 每个 pair 串行等待完整往返；测的是 relay 转发 + 往返开销，不是单向流式
  吞吐上限。
- 无 TLS、无边缘代理、无 WAN 延迟、无丢包。
- 连接数/RSS（1k/10k session）、启动时间、p99.9 和镜像大小都值得继续补测；未来
  发布数据时建议同时提交原始 JSON。

除非在同一主机、同一负载和配置下测量并公布原始数据，否则不要把这些数字当作与
其他 relay 的对比结论。
