# Benchmarks

**English** | [简体中文](zh-CN/benchmarks.md)

These are **loopback sanity measurements**, not a WAN benchmark and not a
head-to-head comparison. They exist so contributors can reproduce a regression
check and so users can see the order of magnitude.

## Environment

- Date: 2026-11-07
- Host: Apple M3, 8 vCPU, macOS 27.2 (arm64)
- Build: `cargo build --release --locked` (Rust 1.99.1)
- Relay: single process, plaintext loopback, default limits, no TLS
- Client/harness: `cargo run --release --example load`, same host
- Workload: 100 v1 server/client pairs, 500 request/response round trips per
  pair, 1 KiB text messages, relay forwards both directions

## Results

| Metric | Result |
|---|---|
| Round trips | 50,000 |
| Throughput | ~41,000–45,000 round-trip messages/s across runs |
| Forwarded payload | ~80–87 MiB/s across both directions |
| p50 round-trip latency | ~2.2–2.3 ms |
| p99 round-trip latency | ~3.3–3.6 ms |
| Max observed round trip | 6.8–14.8 ms across runs |
| Relay RSS after load | ~7.3 MiB |
| Relay binary | ~2.9 MiB (static musl) |
| Container image | ~3.4 MB (scratch, single platform) |
| Process start → `/health` 200 | ~31 ms (same host, warm release binary) |

Commands:

```sh
cargo build --release --locked
PASEO_RELAY_PORT=14221 PASEO_RELAY_LOG=warn ./target/release/paseo-relay &
cargo run --release --example load -- \
  --addr 127.0.0.1:14221 --pairs 100 --frames 500 --size 1024
ps -o rss= -p "$(pgrep -f target/release/paseo-relay)"
```

## Caveats

- Client and relay share one machine, so CPU, loopback networking, and the
  scheduler all affect the result.
- The harness sends text frames and waits for a full round trip per pair; this
  measures relay forwarding plus round-trip overhead, not unidirectional
  streaming capacity.
- No TLS, no edge proxy, no WAN latency, no packet loss.
- Connection-count/RSS at 1k/10k sessions, startup time, p99.9, and image size
  are useful additions; publish raw JSON alongside any future numbers.

Do not quote these as a comparison with another relay unless both were measured
on the same host, workload, and settings, with the raw data published.
