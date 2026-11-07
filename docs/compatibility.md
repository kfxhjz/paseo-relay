# Paseo compatibility

**English** | [简体中文](zh-CN/compatibility.md)

This relay implements the wire contract of the Paseo relay described by Paseo's
[relay endpoint security](https://github.com/getpaseo/paseo/blob/main/docs/relay-endpoint-security.md)
and [security](https://github.com/getpaseo/paseo/blob/main/SECURITY.md) docs.
Payloads are opaque and end-to-end encrypted; the relay never decrypts them.

## Positioning

- **Wire-compatible** with Paseo v1 and v2 relay sessions for a single relay
  endpoint.
- **Community implementation**, not the managed relay behind
  `relay.paseo.sh:443`, and not built-in clustering/ownership failover.
- For multi-node deployments, affinity is provided by the edge (consistent hash
  on `serverId`), not by the relay.

## Supported protocol surface

| Feature | Status |
|---|---|
| v1 `role=server` / `role=client` pair | Supported; newest socket replaces the old one (`1008`) |
| v2 daemon control socket (`role=server`, empty `connectionId`) | Supported: initial `sync`, replacement, control ping/pong |
| v2 daemon data sockets (non-empty `connectionId`) | Supported: replacement, client fan-out, disconnect propagation |
| v2 clients with generated `conn_<16 hex>` IDs | Supported (relay assigns when the client omits `connectionId`) |
| `connected` / `disconnected` / `sync` messages | Supported |
| Last-client detach → data socket `1001` | Supported |
| Data socket death → clients `1012` | Supported |
| Control nudge + `1011 Control unresponsive` | Supported |
| Protected / legacy Paseo E2EE | Opaque pass-through; the relay does not negotiate or inspect E2EE records |
| Legacy `hello` / `e2ee_hello` key hygiene | Supported: small JSON-shaped client frames only, `1008` on invalid keys |
| Maximum masked client frame / message | `32 MiB - 14` bytes (`1009` on overflow) |
| v2 control frame cap | 64 KiB |
| WebSocket ping/pong | Supported (relay answers pings) |
| WebSocket compression | Not negotiated (Paseo does not require it) |

## Known differences from the Elixir relay

| Area | Elixir relay | This relay |
|---|---|---|
| Clustering | Syn ownership + 409 `x-reroute-target` reroute, partition behavior | Single node only; edge consistent hashing required |
| Config | `PASEO_RELAY_CLUSTER_*`, `RELEASE_*`, acceptors, watermark, ownership target | Different, smaller env contract; see README |
| IDs | Any bytes up to 256 for `serverId`/`connectionId` | Valid UTF-8, printable, no whitespace; invalid → HTTP 400 |
| Per-IP limits | None in-process | Optional `PER_IP_MAX_CONNECTIONS`, default 0 (edge recommended) |
| Client cap per connectionId | Unbounded broadcast set | Configurable cap, default 32 |
| Close on read stall | No source frame read deadline | `FRAME_READ_TIMEOUT_MS` / `MESSAGE_READ_TIMEOUT_MS` → `1013` |
| TLS | Typically terminated at edge; listener is plaintext | Edge termination or native rustls |
| Runtime | BEAM release + Cowboy/Ranch | Static Rust binary + hyper/custom RFC 6455 codec |
| Metrics | Different metric families/names | Documented in README/OPERATIONS |

### Single-node semantics

All sockets for a `serverId` must be routed to the same relay node. During a
rolling deployment, a node's sessions are closed in the drain window and Paseo
clients reconnect through the new hash ring. If the edge maps one `serverId` to
two nodes, Paseo will see two independent sessions and pairing will not
connect. There is no transparent migration.

## Evidence: Paseo's own E2E suite

Paseo ships an end-to-end relay transport test:
`packages/server/src/server/daemon-e2e/relay-transport.e2e.test.ts`. It supports
an external relay through `PASEO_RELAY_E2E_PORT` and `FORCE_RELAY_E2E=1`.

The compatibility workflow pins a Paseo commit and runs that file against a
built relay; see `.github/workflows/paseo-compat.yml` and
`scripts/compat-paseo.sh`.

**Verified 2026-11-07 against Paseo v0.15.3
(`ea30d2616a6ea1ad51447cde6e2fc6da4904fe21`; reproduced at
`f21699a37a3d49e8c61bd6d8b65f8a7070f0470c`):**

- official external-relay run: **9 passed, 2 skipped, 0 failed**, run twice
  with identical results (27.9 s/31.1 s; relay logs had zero WARN/ERROR);
- the two skips are restart-dependent tests that external-relay mode cannot
  run; a local development experiment (not committed) exercised the
  reconnect/RPC path, but the committed evidence is the official suite only;
- evidence: Paseo commit `ea30d261`, `relay-transport.e2e.test.ts` with
  `PASEO_RELAY_E2E_PORT`, `FORCE_RELAY_E2E=1`; the same suite also passed against
  the earlier `f21699a` checkout;
- manual development-time wire probe: v1 forwarding/replacement and v2
  control/data behavior were exercised; the probe script/transcript is not
  committed, so this is supplementary, not reproducible evidence;
- exercised paths: v2 control sync/connected/disconnected, generated `conn_`
  IDs, data attachment, large application transfers (chunked; not single relay
  frames), protected/legacy E2EE negotiation,
  binary payloads, relay ping/pong, `1008`/`1001` close codes, SIGTERM drain and
  restart.

A dated snapshot of the run, including the exact command and summary, is
committed at [evidence/paseo-e2e-2026-11-07.md](evidence/paseo-e2e-2026-11-07.md).

Local reproduction:

```sh
# In a Paseo checkout with dependencies installed:
PASEO_RELAY_E2E_PORT=4000 FORCE_RELAY_E2E=1 \
  npx vitest run packages/server/src/server/daemon-e2e/relay-transport.e2e.test.ts \
  --maxWorkers=1
```

If you change wire behavior, run this suite (or explain why you could not) in
your PR.
