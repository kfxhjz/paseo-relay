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

## Change since v0.1.2 (0.1.3)

Same-host A/B on 2026-11-26 (Apple M3, 8 cores, macOS 27.2.1, Rust 1.99.1):
`main` at `4a60da1`, whose sources are identical to `v0.1.2`, against
`0.1.3`. Both are release builds, the runs are interleaved, three runs
each, with the command above and `--size` varied.

| Workload (100 pairs × 500 round trips) | `v0.1.2` sources | 0.1.3 |
|---|---|---|
| 64 B text | 52.8–55.3k msg/s | 56.3–68.2k msg/s |
| 1 KiB text | 52.1–58.4k msg/s | 58.5–70.0k msg/s |
| 16 KiB text | 35.6–36.6k msg/s | 40.1–44.1k msg/s |
| p50 round trip, 1 KiB | 1.78–1.83 ms | 1.42–1.68 ms |
| Relay RSS after the 1 KiB run | ~7.5 MiB | ~7.8 MiB |
| Memory per idle WebSocket (4,000 idle sockets) | ~13.5 KiB | ~14.3 KiB |

The throughput gain comes from the codec: a frame header and a small payload
are now read with one socket read instead of three, and a frame's header and
payload are written with one write instead of two. The extra ~0.8 KiB per idle
socket is the reader's 256-byte read-ahead buffer plus the per-connection
bookkeeping for budget reclaim and the idle clock.

The `v0.1.2` column is faster here than in the table above, which was measured
on 2026-11-07: absolute numbers on a laptop move with the OS version and
machine state. Only the same-run comparison is meaningful, and the binary
size, image size and start-up figures above were not re-measured.

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
