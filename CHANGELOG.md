# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
  (`ghcr.io/kfxhjz/paseo-relay:v0.1.0`) instead of a local source build: README
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
- Corrected the published image tag in the docs: GHCR publishes `v0.1.0` (with
  the `v`), not `0.1.0`. The short version tag is now the documented default;
  the release digest is only mentioned for optional immutable pinning.

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

[Unreleased]: https://github.com/kfxhjz/paseo-relay/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.0
