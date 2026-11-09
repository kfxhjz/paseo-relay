# Paseo Relay

**Self-host the relay for [Paseo](https://github.com/getpaseo/paseo) in five minutes.** A single-node, wire-compatible relay written in Rust: one static musl binary in a `scratch`, non-root container, opaque end-to-end-encrypted payloads, no Erlang/OTP.

[Five-minute VPS quickstart](quickstart.md){ .md-button .md-button--primary }
[Why this relay](comparison.md){ .md-button }
[GitHub repository](https://github.com/kfxhjz/paseo-relay){ .md-button }

## What it is

Paseo's daemon connects **outbound** to a relay; clients meet it there. This project is a community, self-hostable implementation of that relay for Paseo v1/v2 sessions. It forwards encrypted payloads opaquely and never sees plaintext content or long-term keys.

- **One static binary + scratch image** — no BEAM, no EPMD, no node cookie, no cluster.
- **Bounded by default** — chunked ingress accounting, connection/session/frame limits, bounded queues, read deadlines.
- **Fail closed** — explicit HTTP status and WebSocket close codes; no half-open state.
- **Operations ready** — health/readiness/metrics/version endpoints, graceful drain, built-in healthcheck, multi-arch image with per-platform provenance and SBOM attestations.

## Start here

- [Quickstart](quickstart.md) — one VPS, Caddy automatic HTTPS, Docker Compose.
- [Deployment](deployment.md) — TLS, edge `serverId` hashing, multiple nodes, Kubernetes, drain.
- [Configuration](configuration.md) — full environment contract and CLI.
- [Protocol](protocol.md) — `/ws` query, v1/v2 semantics, frame limits, close codes.
- [Compatibility](compatibility.md) — Paseo feature matrix and E2E evidence.

## 简体中文

- [中文文档](zh-CN/README.md) — 快速开始、部署、配置、协议与运维。
- [五分钟快速开始](zh-CN/quickstart.md)
- [项目 README（中文）](https://github.com/kfxhjz/paseo-relay/blob/main/README.zh-CN.md)

## Project

- [GitHub repository](https://github.com/kfxhjz/paseo-relay)
- [Releases](https://github.com/kfxhjz/paseo-relay/releases)
- [Container image](https://github.com/kfxhjz/paseo-relay/pkgs/container/paseo-relay)
- [Design](https://github.com/kfxhjz/paseo-relay/blob/main/DESIGN.md)
- [Security model](https://github.com/kfxhjz/paseo-relay/blob/main/SECURITY.md)
- [Contributing](https://github.com/kfxhjz/paseo-relay/blob/main/CONTRIBUTING.md)

> **Community implementation.** This is not the managed relay behind `relay.paseo.sh:443`, not affiliated with or endorsed by the Paseo project, and not built-in clustering/ownership failover. Affinity for multiple nodes is provided by your edge (consistent hashing on `serverId`). See [Compatibility](compatibility.md).

---

## All documentation

### Getting started

- [Quickstart](quickstart.md) — one VPS, Caddy automatic HTTPS, Docker Compose
- [Comparison](comparison.md) — choosing a relay deployment
- [Deployment](deployment.md) — TLS, edge `serverId` hashing, Kubernetes, drain
- [Configuration](configuration.md) — full environment contract and CLI

### Reference

- [Protocol](protocol.md) — `/ws` query, v1/v2 semantics, frame limits, close codes
- [Compatibility](compatibility.md) — Paseo feature matrix and verified E2E
- [Operator policy](operator-policy.md) — running a public relay safely

### Operations

- [Operations](operations.md) — metrics, alerts, troubleshooting
- [Migration](migration.md) — moving from the Elixir relay
- [Benchmarks](benchmarks.md) — reproducible load test and caveats
- [FAQ](faq.md) — common questions

### Project

- [Design](https://github.com/kfxhjz/paseo-relay/blob/main/DESIGN.md)
- [ADRs](adr/README.md)
- [Changelog](https://github.com/kfxhjz/paseo-relay/blob/main/CHANGELOG.md)
- [Contributing](https://github.com/kfxhjz/paseo-relay/blob/main/CONTRIBUTING.md)
- [Security](https://github.com/kfxhjz/paseo-relay/blob/main/SECURITY.md)
- [Support](https://github.com/kfxhjz/paseo-relay/blob/main/SUPPORT.md)
- [Translation policy](translation.md)
- [E2E evidence](evidence/paseo-e2e-2026-11-07.md)
