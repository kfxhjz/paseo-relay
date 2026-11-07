# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
