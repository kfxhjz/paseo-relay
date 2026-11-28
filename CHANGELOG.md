# Changelog

**English** | [简体中文](CHANGELOG.zh-CN.md)

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Documentation

- Added a complete Simplified Chinese changelog (`CHANGELOG.zh-CN.md`).
- Updated the VPS quickstart's pinned Caddy image to `2.11.9-alpine` and
  documented how to reuse an existing Caddy when the VPS already serves other
  domains.
- Corrected the quickstart Caddyfile to use valid multi-line blocks and
  clarified that a bridge-network Caddy must join the `paseo-relay` network;
  loopback only works from the host or a host-network container.
- Made the Caddy example additive: `examples/sites-enabled/*.caddy` is imported
  so each new domain lives in its own file and can be reloaded independently.
- Added `examples/.env.example` so `RELAY_DOMAIN` is configured once in the
  git-ignored `examples/.env`; upgrades no longer need it inline.

## [0.1.4] - 2026-11-27

Fixes for the third (2026-11) review round. No wire-protocol change for normal
traffic; the v2 control plane now enforces the documented 64 KiB `sync` bound
and a few operational defaults have safer validation.

### Security

- **The relay's v2 control `sync` is bounded to 64 KiB.** A `sync` is built
  from client-chosen `connectionId`s, which may contain JSON-escaped characters;
  enough long ids could previously make the relay emit a control frame larger
  than the documented v2 control-frame limit (and, at an extreme
  `PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION`, larger than the writer queue). The
  session layer now computes the serialized size before accepting a v2 client
  (or an initial control socket) and refuses a client that would overflow it
  with `1013` "Relay session sync capacity"; initial and watchdog `sync`
  failures are fail-closed instead of being silently dropped, and are counted
  by the new `paseo_relay_control_send_failures_total`.
- **Control watchdog nudge coverage is tracked per client.** A client that
  attached after the watchdog's nudge used to be closed `1011` without ever
  receiving its own `sync`, while its wait window was still bounded by the
  oldest waiter. The watchdog now keeps the oldest waiter's `1011` deadline
  separate from a coverage clock, sends a supplementary `sync` for later
  arrivals, and never lets new clients postpone that deadline.
- **TLS healthcheck pins the relay certificate.** `paseo-relay healthcheck`
  previously accepted any certificate whose signature verified (trust chain and
  hostname were intentionally skipped), so a different local service or path
  MITM could report a broken relay healthy on a non-loopback probe target. With
  native TLS the probe now reads the leaf certificate at
  `PASEO_RELAY_TLS_CERT`, pins its DER SHA-256 and accepts only that
  certificate. If native TLS is configured but the certificate file cannot be
  read, it only probes loopback and fails closed for non-loopback targets. A
  plaintext listener has no certificate to pin: non-loopback probes are allowed
  but unauthenticated.
- **The TLS healthcheck pin follows the configured certificate file, not the
  certificate the listener is currently serving.** If a replacement pair is
  malformed or mismatched, the watcher keeps the previous certificate in use
  but the probe pins the new file and can report unhealthy until the pair is
  fixed. Repair or restore the files instead of restarting: startup also tries
  to load the broken pair and would fail.
- **Per-connection ingress share is configurable, with a residual-risk
  warning.** `PASEO_RELAY_PER_CONNECTION_LIMIT_BYTES` (0/unset = auto) clamps a
  configured cap to `[one maximum message, ingress budget]`; the automatic
  default remains `min(budget, 4 × one-max-message charge)`, which equals the
  whole budget at the shipped defaults. Startup now warns when that derived cap
  is the whole budget and `PASEO_RELAY_PER_IP_MAX_CONNECTIONS=0`. This reduces
  one slow source's share but does **not** eliminate the documented availability
  residual: several connections can still fill the budget, so edge per-source
  bandwidth limits remain required.
- **Release promotion is staged and version-checked.** The release workflow now
  refuses a tag that is not `vX.Y.Z[-prerelease]`, does not match `Cargo.toml`,
  or has no CHANGELOG section; it assembles the multi-arch index under a
  temporary `candidate-<sha>` tag, smoke-tests that digest, signs it and
  attaches build provenance, and only then promotes the version tag (and
  `latest`, only for a stable release at least as high as the highest stable tag
  on `main`). Publish runs in a `release` environment intended for required
  reviewers, and release verification now runs the same fuzz smoke as CI.
  Required status checks, tag protection and environment reviewers are GitHub
  repository settings; `SECURITY.md` lists the expected configuration instead of
  pretending code can enforce it.
- **CI/supply-chain hardening.** The Docker builder, CI and
  `rust-toolchain.toml` are aligned on Rust 1.100.0; `fuzz/Cargo.lock` drift is
  checked against the root lockfile and a seed corpus was added; the Pages build
  job no longer holds write credentials; `.dockerignore` excludes docs, examples,
  fuzz output, local review data and nested PEM/key files; the HAProxy example
  counts TCP connects (not only HTTP requests) for per-source limits; the Mermaid
  asset is pinned and self-hosted. The release verification job also runs
  `cargo audit` on both lockfiles and the fuzz smoke test, and the container
  image is smoke-tested by immutable digest before promotion.

### Changed

- **Rejection metrics are attributed to one cause each.**
  `paseo_relay_session_rejections_total` now counts only the global
  `MAX_SESSIONS` ceiling. The new
  `paseo_relay_session_connection_rejections_total` counts the per-session
  `connectionId` union cap and 64 KiB `sync` refusals,
  `paseo_relay_client_limit_rejections_total` counts the per-`connectionId`
  client cap, and `paseo_relay_ops_rejections_total` counts operations-listener
  quota refusals (which also log a rate-limited warning naming the peer).
- **Forwarding and delivery metrics have precise semantics.**
  `frames_forwarded_total`/`bytes_forwarded_total` count only relayed user
  frames/bytes, not relay-generated `sync`/`connected`/`disconnected`/pong;
  `delivery_timeouts_total` counts a missed delivery deadline once per shed
  delivery (source for a queue wait, writer for a write deadline), transport
  errors no longer count as delivery timeouts, and slow-consumer disconnects
  are no longer double-counted by the writer and the source.
- **The per-session `connectionId` cap counts the union of client ids and data
  routes.** One id now occupies one slot whether a client socket, a data socket,
  or both are attached; a new client is rejected immediately when the session's
  data routes are already full instead of being accepted and failing 15 s later
  with "Data route unavailable".
- **Query values are no longer trimmed.** `v` is matched verbatim (`v=+2` is
  `400`, not v2) and `connectionId=%20` is rejected instead of silently becoming
  the empty/control value. Omitted or empty `connectionId` still means a
  generated id for a v2 client and the control socket for a v2 daemon.
- **Configuration validation is stricter and more diagnosable.** An IPv6
  wildcard (`::`) conflicts with any same-port listener, and an IPv4 wildcard
  (`0.0.0.0`) conflicts with same-port IPv4 listeners; concrete addresses of
  different families can still coexist. A Kubernetes service-link value
  injected into `PASEO_RELAY_OPS_PORT` now gets the same actionable error as
  `PASEO_RELAY_PORT` (naming `paseo-relay-ops`).
  `--dump-config` shows `per_connection_limit=auto(N)` or the clamped value.
- **Control pong uses the 1 s control send timeout** instead of the 30 s data
  delivery deadline, so a control socket that stops reading cannot stall its
  own control plane for half a minute.
- **Reclaim no longer treats a stalled frame header as a full-frame need.** The
  ~1 KiB per-message overhead reservation now uses the small request weight as
  its reclaim hint; only the payload-chunk phase uses the remaining frame
  weight, so a peer that sends a large header and no payload cannot cause a
  wave of unrelated in-progress messages to be evicted.
- **Mutex-poison recovery and bounded shutdown.** Session/writer/capacity/server
  lock helpers now recover the guarded state instead of panicking every later
  caller; TLS reload refuses non-regular files (FIFO/socket/directory) instead
  of blocking on them, and shutdown waits at most 2 s for a stuck background
  watcher before exiting.
- **Documentation** is synchronised across English and Chinese: the multi-arch
  index attestation description (index build provenance exists; per-platform
  provenance/SBOM exist; no index-level SBOM), the new configuration and
  metrics, `sync`/64 KiB semantics, TLS healthcheck pinning, release ordering,
  and the residual availability risk.

### Fixed

- The initial v2 control `sync` used to discard send errors with `let _ = ...`;
  it now records a control-send failure and closes the control socket, matching
  the watchdog path.
- Operations-listener rejections were invisible; they now increment
  `paseo_relay_ops_rejections_total` and emit a rate-limited warning with the
  peer address.
- Ordinary destination transport errors could be reported as delivery timeouts
  and slow-consumer disconnects. They are now classified and counted once per
  shed delivery (source or destination writer), and only real deadline misses
  count against `delivery_timeouts_total`.
- `paseo-relay healthcheck` no longer trusts an arbitrary certificate on a
  non-loopback probe target (see Security above).

## [0.1.3] - 2026-11-26

Fixes for the 2026-11 security and project review. No wire-protocol change:
Paseo's `relay-transport` E2E suite passes unchanged (9 passed, 2 skipped, as
before). Read "Changed" before upgrading; a few defaults behave differently.

### Security

- **Control-socket `sync` amplification could exhaust memory outside the
  ingress budget.** Previously, every waiting v2 client spawned its own control
  watchdog that built and queued a full `sync` JSON list over a 30 s write
  timeout, and a single `serverId` session had no cap on distinct
  `connectionId`s—producing quadratic ($O(K^2)$) memory growth (~810 MiB at
  1,200 idle connections). Now each session enforces
  `PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION` (default `512`, excess closes
  `1013` "Relay session connection capacity") and runs a single deduplicated
  control watchdog holding only a `Weak<Session>` across awaits, with a 1 s
  control send timeout and pre-serialized `Bytes`.
- **Replacing or dropping a source socket could release ingress budget while
  frames stayed queued in the destination writer.** Queued frames in a slow
  destination's writer channel now carry both an `Arc<BudgetHold>` of their
  ingress budget guards and an absolute delivery deadline; the writer enforces
  that deadline independently even after the source disconnects or is replaced,
  shedding stalled destinations and returning the budget to zero.
- **Ingress reclaim and stall-timeout hardening.**
  - Split `HoldState` tracking into `bulk_held` vs `small_held` so main-pool
    waiters never evict small-reserve-only holds, and continue past slow holds
    when they free fewer bytes than needed so tiny stalled decoys cannot shield
    older bulk holds from the 60 s age rule.
  - Separated reclaim throttle clocks (`slow`, `patient_bulk`, `patient_small`)
    so impatient or bulk waiters cannot starve eligible patient/small waiters.
  - Indexed only connections actively receiving a message in `IngressBudget`
    rather than scanning all idle sockets under the global lock.
  - Fragmented messages (`!fin` and continuations) now always charge the main
    pool rather than locking `small` reserve bytes behind an opening fragment.
  - Stall progress (`PASEO_RELAY_READ_STALL_TIMEOUT_MS`) now requires at least
    8 KiB of cumulative payload progress, closing both 0-byte and 1-byte
    continuation keepalive bypasses.
- **Pre-upgrade HTTP response write timeout and handshake hardening.**
  - Pre-upgrade HTTP streams now enforce `PASEO_RELAY_HTTP_HEADER_TIMEOUT_MS`
    on stalled response writes (such as pipelined `/metrics` requests whose peer
    stops reading) and disable the timer once a WebSocket upgrade completes.
  - `accept_loop` and `ops_accept_loop` now reap completed `JoinSet` tasks ahead
    of accepting new sockets.
  - `/ws` now requires HTTP/1.1 (`400` otherwise) and rejects duplicate `Origin`
    headers with `403` using a precomputed `HashSet`.
  - Release verification now runs `cargo audit --deny warnings` on both lockfiles
    and smoke-tests the published image by immutable `@sha256:` digest.
- **Watchdog nudges, write cancellation, and stall accounting had edge cases.**
  - A v2 client that attached after the session's single control watchdog had
    already sent its nudge could be closed `1011` without a `sync` of its own.
    The watchdog now records when the nudge was sent, nudges later arrivals,
    and always grants a full post-nudge grace window.
  - Cancelling a destination writer now drains its queue before flushing the
    close frame, so byte permits and ingress budget guards are released at once
    even when the peer stopped reading. The close flush is capped at two
    seconds, and a cancel during a stalled write returns the budget instead of
    waiting out the delivery window. An ordinary control-socket disconnect is
    no longer counted as a slow-consumer shed.
  - Control-frame payload reads (Ping/Pong/Close) are bounded by the remaining
    stall window, and a one-byte continuation header no longer restarts it;
    relay-imposed budget waits do not count against the sender's stall clock.
    Under pressure, reclaim keeps evicting until the waiter's need is met, up
    to the victim cap, so tiny old holds cannot shield a large one.
  - The pre-upgrade TCP and per-IP slot is released only after the HTTP `101`
    upgrade completes, with the wait bounded by `HTTP_HEADER_TIMEOUT_MS`
    instead of a fixed ten seconds.
- **Ingress budget could be held for free.** Two connections that each sent an
  unfinished ~32 MiB fragment and then went idle reserved the whole 129 MiB
  budget of the Compose/Kubernetes examples (four connections for the 512 MiB
  default) for up to ten minutes, and every other session that sent data was
  closed `1013` after 5 s. Now:
  - a message that makes no payload progress for
    `PASEO_RELAY_READ_STALL_TIMEOUT_MS` (30 s) is closed;
  - while another source waits for budget, the relay reclaims budget from
    unfinished messages of other connections instead of only closing the
    waiter: stalled or slow ones (`PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES`,
    64 KiB/s), ones that have been arriving for longer than
    `PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS` (60 s), and, for a waiting message
    of at most 64 KiB, one that is at least 8× larger. Reclaimed sources are
    closed `1013` "Relay ingress reclaimed";
  - messages of at most 64 KiB have a separate reserve
    (`PASEO_RELAY_INGRESS_SMALL_RESERVE_BYTES`, 16 MiB weighted), so handshakes
    and ordinary frames keep flowing while large transfers fill the main
    budget.

  Not covered: completely received messages that are never delivered still
  hold budget for the delivery or data-attach deadline. `SECURITY.md` states
  the remaining cost of occupying the budget.
- **Session slots outlived their sockets.** An empty session was kept for
  `PASEO_RELAY_SESSION_IDLE_MS` (30 s), so one client limited to a single
  concurrent WebSocket could fill `PASEO_RELAY_MAX_SESSIONS` by cycling through
  `serverId`s, after which `/ready` returned 503 and new daemons were refused.
  A session is now removed when its last socket closes.
- **Per-IP limit did not cover pre-upgrade sockets or IPv6 prefixes.** With
  `PASEO_RELAY_PER_IP_MAX_CONNECTIONS` set, one client could still hold every
  TCP slot with keep-alive HTTP connections (making `/health` fail), and an
  IPv6 client could use a new address from its prefix for every connection.
  The setting now also caps a client's TCP sockets (the WebSocket cap plus a
  quarter, at least 8 more), IPv6 clients are counted per /64, and IPv4-mapped
  addresses count as IPv4.
- **Operations endpoints could not be taken off the public port.**
  `PASEO_RELAY_OPS_PORT` / `PASEO_RELAY_OPS_HOST` serve `/ready`, `/metrics`
  and `/version` on a separate plaintext listener; the public listener then
  answers only `/ws` and `/health`. The operations listener has its own
  connection allowance and keeps answering during a drain, and
  `paseo-relay healthcheck` follows it, so a relay whose public listener is
  full is reported unready instead of failing its liveness probe.
- **Release provenance was not provenance.** The `provenance.intoto.jsonl`
  asset on the v0.1.2 release was written by hand in a workflow and is
  unsigned, and the release workflow listed it without producing it. Releases
  now sign the multi-arch image (cosign, keyless) and attach a GitHub build
  provenance attestation to it; the manual signing workflow reads the digest
  from the registry instead of from the editable release notes, no longer
  writes a provenance file, and removes the old asset from a release it signs.
  Verification commands are in `SECURITY.md` and in each release's notes.
- Handshake key hygiene is documented for what it is. The v0.1.2 notes said it
  had "no bypassable byte prefilters"; a frame the relay's JSON parser rejects
  but a more lenient parser accepts (lone surrogate escape, a number outside
  `f64` range, very deep nesting), or any frame above 64 KiB, is forwarded
  uninspected. The check was never a security boundary and the behaviour is
  unchanged; it is now pinned by a test and stated in `docs/protocol.md`.
- `examples/nginx.conf` sets `error_log ... crit`: nginx's error log always
  contains the full request line, so rate-limit rejections and upstream errors
  wrote `serverId` to disk even though the access log omitted it.

### Added

- `PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION` (default `512`): caps distinct v2
  `connectionId`s per `serverId` session.
- `PASEO_RELAY_WS_IDLE_TIMEOUT_MS`: optional idle timeout (off by default). A
  socket with no frame received and no data frame relayed to it for that long
  is closed `1013` "Idle timeout".
- TLS certificate reload without dropping sessions: on `SIGHUP`, and
  automatically within 30 s of the certificate or key file changing. A pair
  that cannot be loaded leaves the previous certificate in use.
- Startup and `--check-config` warnings for an unknown `PASEO_RELAY_*`
  variable (a misspelled name used to be ignored silently), for an open-file
  limit below `PASEO_RELAY_MAX_TCP_CONNECTIONS` (Linux), and for native TLS
  without a per-IP cap. Kubernetes service-link variables are recognised, and
  a service-link value in `PASEO_RELAY_PORT` gets an error that explains it.
- Rate-limited `warn` logs (one line per reason per 10 s, with an event count)
  when the relay sheds work: ingress exhausted or reclaimed, read or idle
  timeouts, slow consumers, and WebSocket/session/TCP/per-IP rejections.
- Metrics: `paseo_relay_ingress_reclaims_total`,
  `paseo_relay_ingress_rejections_total`, `paseo_relay_read_timeouts_total`,
  `paseo_relay_idle_timeouts_total`, `paseo_relay_tcp_rejections_total`,
  `paseo_relay_ingress_small_reserved_bytes` and
  `paseo_relay_ingress_small_budget_bytes`; two matching example alerts.
- Tests: regression tests for every finding above against real sockets,
  native TLS (serving, the TLS healthcheck, certificate reload), the
  operations listener, and randomised codec round trips. CI now builds all
  four fuzz targets and runs each briefly on every pull request, and a weekly
  workflow runs them for longer (`scripts/fuzz.sh`).

### Changed

- **Shared-port drain keeps operations endpoints reachable during the grace
  window.** When `PASEO_RELAY_OPS_PORT` is unset, `SIGTERM`/`SIGINT` keeps the
  listener open for `PASEO_RELAY_SHUTDOWN_GRACE_MS` so `/ready` (`503`),
  `/health` (`200`), and `/metrics` remain observable while `/ws` rejects new
  upgrades with `503`.
- **Configuration, TLS reload, and healthcheck fixes.** `Config::validate` now
  rejects wildcard/specific IP collisions on the same port and address family
  (matching Linux `EADDRINUSE`); `TlsReloader` serialises reloads across
  `SIGHUP` (now run via `spawn_blocking`) and the 30 s watcher; `src/main.rs`
  adds `#![forbid(unsafe_code)]`; `paseo-relay healthcheck` ignores empty
  `PASEO_RELAY_TLS_CERT`/`KEY` values, ignores port `0`, brackets IPv6 `Host`
  headers, and defaults to `PASEO_RELAY_OPS_HOST`/`PASEO_RELAY_HOST` when bound
  to a non-wildcard IP; `tokio` production features are narrowed from `full` to
  the exact modules used.
- **Unfinished messages can be closed where they were not before.** A message
  that stalls for 30 s is closed even on an idle node, and on a node whose
  ingress budget is full a large upload that is slower than 64 KiB/s or has
  been arriving for more than 60 s gives way to a newer one (`1013`,
  retryable). Raise `PASEO_RELAY_READ_STALL_TIMEOUT_MS`,
  `PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS` or the budget, or lower
  `PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES`, for slow links.
- **`SIGHUP` reloads TLS instead of terminating the process.** Earlier
  versions did not handle the signal.
- The small-message reserve is extra budget: worst-case in-flight accounting
  is now `INGRESS_BUDGET_BYTES + INGRESS_SMALL_RESERVE_BYTES` (16 MiB weighted,
  about 4 MiB of payload, by default). Set the reserve to `0` for the old
  total.
- `paseo_relay_active_sessions` no longer includes empty sessions, and
  `PASEO_RELAY_SESSION_IDLE_MS` only governs a safety-net sweep.
- A connection refused at accept on a native-TLS listener is closed without a
  response; it used to get a plaintext `503` that a TLS client cannot read.
- Faster forwarding: a frame header and a small payload are read with one
  socket read instead of three, and a frame's header and payload are written
  with one write instead of two. Same-host A/B, 100 pairs: 52–58k → 58–70k
  round trips/s at 1 KiB (`docs/benchmarks.md`). An idle WebSocket costs about
  0.8 KiB more (13.5 → 14.3 KiB on macOS/arm64); the `× ~30 KiB` planning
  figure is unchanged.
- Dependencies: `url` was used only by one test helper and `serde`'s derive
  feature not at all; removing them takes 28 packages (the ICU/IDNA tree) out
  of the build, 135 → 107. `rcgen` is a new dev-dependency for the TLS tests.
- The library's supported surface is `Config`, `serve` and `ServerHandle`
  (plus the `config` module). The other modules remain public for the fuzz
  targets but are hidden from the documentation and may change in any
  release. `ServerHandle` gains `ops_local_addr` and `reload_tls`.
- Examples: the systemd unit sets `LimitNOFILE=65536` (the default soft limit
  of 1024 is far below the relay's connection caps) and `ExecReload`; the
  Kubernetes example sets `enableServiceLinks: false`, because a Service named
  `paseo-relay` otherwise injects `PASEO_RELAY_PORT=tcp://...` into the pod;
  nginx answers its own rate limits with `429`. Operations-listener settings
  are included commented out, because the pinned `v0.1.2` image predates
  them.

## [0.1.2] - 2026-11-08

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

[Unreleased]: https://github.com/kfxhjz/paseo-relay/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.4
[0.1.3]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.3
[0.1.2]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.2
[0.1.1]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.1
[0.1.0]: https://github.com/kfxhjz/paseo-relay/releases/tag/v0.1.0
