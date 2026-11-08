# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security

- Ingress budget accounting is now incremental: data-frame payloads are read in
  8 KiB chunks and charged only as bytes arrive, with per-chunk bookkeeping
  charged too, a per-connection in-flight cap and a 1024-fragment limit. A peer that declares a payload and never sends
  it (stalled payload), or a stream of empty continuation frames, can no longer
  reserve the node-wide budget and close other sessions with `1013`. Ingress
  budget waits are also capped by the remaining frame/message read deadline.
- Pre-upgrade HTTP memory is bounded: hyper runs with a 16 KiB read buffer,
  16 KiB header limit, and 64 headers, so unauthenticated TCP connections cannot
  make the relay buffer ~400 KiB each before WebSocket admission.
- TCP slots are held for the whole socket lifetime, so
  `PASEO_RELAY_MAX_TCP_CONNECTIONS` again bounds total open sockets.
- Hardening: invalid close codes and non-minimal frame length encodings are
  rejected with `1002`; an unparseable `Origin` header is denied when an
  allowlist is configured; handshake/control JSON parsing uses cheap byte
  prefilters and the handshake hygiene cap drops from 1 MiB to 64 KiB; writer
  delivery uses one absolute deadline instead of up to three sequential
  timeouts; the build revision is sanitized before being embedded in
  `/version` and metrics.

### Added

- Quick production `docker run` recipes (detached, `--restart unless-stopped`,
  30 s stop timeout) next to the foreground quickstart check, in both READMEs
  and the deployment guide.
- `paseo_relay_ingress_budget_bytes` gauge metric so operators can alert on
  ingress saturation as a ratio of the configured budget.
- Fuzz targets for the RFC 6455 frame reader and upgrade validation.

### Changed

- The release workflow builds `linux/amd64` and `linux/arm64` natively on
  matching GitHub runners in parallel, with GitHub Actions layer caching and a
  post-publish smoke test. This replaces the single QEMU multi-arch build and
  cuts the image stage from ~25 minutes to a few minutes. The workflow still
  runs only for `v*` tag pushes.
- Dependabot PRs are grouped per ecosystem (cargo, GitHub Actions, Docker), and
  every workflow now pins the current action releases, so weekly dependency
  updates arrive as at most one PR per ecosystem.
- `PASEO_RELAY_INGRESS_WEIGHT` now accepts 4..=16 (the minimum is the documented
  safe floor for transient assembly copies). Invalid `PASEO_RELAY_LOG` values
  and TLS certificate/key paths that are not regular, readable files now fail
  startup and `--check-config`.
- Release hardening: only stable tags (no `-`) move the GHCR `latest` tag, and
  the release workflow grants write permissions per job instead of globally.
- Quickstart examples are sized for their documented memory: Compose caps
  connections at 4000 sessions/5000 TCP sockets, sets a 129 MiB ingress budget
  (512 MiB RAM), `nofile`/memory limits, and the edge examples no longer log
  `serverId` query strings or expose operations endpoints by default.
- The Kubernetes example disables service-account token automounting, adds a
  NetworkPolicy, and caps connections for its 1 Gi memory limit.

### Fixed

- Compose quickstart could not start: the shipped 128 MiB ingress budget was
  below the required one-max-message reservation; it is now 129 MiB.
- Writer delivery deadlines are no longer applied up to three times
  sequentially for one frame.

## [0.1.1] - 2026-11-07

### Added

- Diagrams throughout the documentation: Mermaid connection-lifecycle flowchart
  in `DESIGN.md`/`DESIGN.zh-CN.md`, a single-node/multi-node deployment topology
  in both READMEs, v1 and v2 protocol sequence diagrams, a multi-node `serverId`
  hashing topology and a drain/rollout sequence in the deployment guide, an
  ingress-budget/bounded-queue flow in the configuration guide, and a
  what-runs-where diagram in the quickstart (English and Chinese).
- `examples/docker-compose.build.yml` source-build override for the single-node
  Compose example.
- `scripts/check-docs.py` now fails on misaligned framed ASCII diagrams
  (East Asian characters counted as two display columns), so README/DESIGN
  diagram regressions are caught in CI.

### Changed

- Documentation now defaults to the published release image
  (`ghcr.io/kfxhjz/paseo-relay:v0.1.1`) instead of a local source build: README
  30-second quickstart, VPS quickstart, deployment guide, Compose examples, and
  the Kubernetes example. Source builds and `cargo run` remain as a clearly
  labelled developer path. Optional immutable digest pinning is documented in
  the deployment guide and release notes.
- Dependencies: `base64` 0.22 -> 0.23, `rand` 0.9 -> 0.10, dev-dependency
  `tokio-tungstenite` 0.28 -> 0.30, and `smallvec` 1.16.4 -> 1.16.5. This
  removes the legacy `digest` 0.10 / `generic-array` tree from the lockfile.

### Fixed

- Aligned the ASCII architecture diagrams in `README.md`, `README.zh-CN.md`,
  `DESIGN.md`, and `DESIGN.zh-CN.md` (including CJK display widths and the
  overflowing `SessionRegistry` line), and the continuation bar in
  `docs/zh-CN/design-summary.md`.
- Corrected the image tag in the docs: GHCR release tags carry a leading `v`
  (for example `v0.1.1`, not `0.1.1`). The short version tag is now the
  documented default; the release digest is only mentioned for optional
  immutable pinning.

## [0.1.0] - 2026-11-07

Initial public Rust rewrite. See `DESIGN.md` for the architecture and design
invariants.

### Added

- Full Chinese DESIGN translation (`DESIGN.zh-CN.md`) and a docs/i18n CI check
  that prevents Chinese pages from silently linking English-only documents.
- Single-node Caddy quickstart, operator policy, support guide, comparison
  guide, logo and social-preview assets.
- Chinese documentation: `README.zh-CN.md`, translated protocol/configuration/
  deployment/compatibility/operations/migration/benchmarks/FAQ docs, Chinese
  security and contributing guides, and a Chinese design summary.
- Verified against Paseo v0.15.3 (`ea30d261`): Paseo's relay transport E2E
  suite passed 9/9 runnable tests with 0 failures; v1 paths were additionally
  exercised with a manual development-time wire probe (the probe
  script/transcript is not committed).
- Native rustls TLS listener (`PASEO_RELAY_TLS_CERT` / `PASEO_RELAY_TLS_KEY`).
- Per-IP WebSocket limits, node-wide WebSocket/TCP/session limits, and a
  per-connection client cap.
- Weighted ingress byte budget with fixed per-message overhead, reserved before
  payload allocation.
- Frame and fragmented-message read deadlines.
- Control nudge / `1011 Control unresponsive` watchdog matching the Elixir
  relay.
- Graceful SIGTERM/SIGINT drain and `PASEO_RELAY_DRAIN`.
- Prometheus `/metrics`, liveness/readiness endpoints, structured logs without
  routing IDs.
- Distroless-style scratch image, non-root UID, CI, and a reproducible load
  generator.

### Changed

- Documentation tree normalized: lower-case guide filenames, an English
  documentation index, an ADR index, and Chinese translations consolidated under
  `docs/zh-CN/` (root localized files keep the `.zh-CN.md` suffix).
- Wire-compatible v1/v2 Paseo relay semantics, with single-node-only routing.
- Strict UTF-8 / printable validation for `serverId` and `connectionId`.
- Handshake key validation is bounded to small JSON-shaped client frames and
  documented as protocol hygiene, not a security boundary.

### Security

- No Erlang VM, EPMD, distributed Erlang, node cookie, or remote shell.
- Failed WebSocket upgrades cannot create session or capacity state.
- Source disconnects do not close destinations; slow consumers are shed at
  their own deadline.
- Invalid UTF-8 IDs are rejected with HTTP 400, never encoded or crashed on.

[Unreleased]: https://github.com/kfxhjz/paseo-relay/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.1
[0.1.0]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.0
