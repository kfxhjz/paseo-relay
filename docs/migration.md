# Migrating from the Elixir relay

**English** | [简体中文](zh-CN/migration.md)

This Rust relay is **wire-compatible for a single node** but not
deployment-compatible with the Elixir relay's cluster/ownership features.

## Behavior changes

- No Syn, no ownership registry, no 409 `x-reroute-target` reroute, no
  cross-node migration. Use an edge LB with `serverId` consistent hashing.
- No BEAM release, EPMD, distribution, `RELEASE_*` variables, or cookie.
- `serverId` / `connectionId` are validated as printable UTF-8; invalid values
  return HTTP 400 instead of being routed.
- New in-process limits: per-IP WebSocket cap, session cap, per-connection
  client cap, frame/message read deadlines.
- Native TLS via rustls is available; otherwise terminate at the edge.
- SIGTERM/SIGINT perform a bounded drain; `PASEO_RELAY_DRAIN` starts unready.

## Environment variable mapping

| Elixir | Rust |
|---|---|
| `PASEO_RELAY_HOST` / `PORT` | same |
| `PASEO_RELAY_ACCEPTORS`, `CONNECTIONS_PER_ACCEPTOR` | `PASEO_RELAY_MAX_CONNECTIONS`, `PASEO_RELAY_MAX_TCP_CONNECTIONS` |
| `PASEO_RELAY_INGRESS_BUDGET_BYTES`, `INGRESS_WEIGHT` | same; `INGRESS_WEIGHT` now only accepts `4..=16` |
| `PASEO_RELAY_DELIVERY_TIMEOUT_MS`, `DATA_ATTACH_TIMEOUT_MS`, `CONTROL_QUEUE_BYTES` | same |
| `PASEO_RELAY_WEBSOCKET_MAX_HEAP_WORDS`, `MEMORY_WATERMARK_BYTES`, `TCP_RECEIVE_BUFFER_BYTES` | removed (Rust memory model; explicit caps instead) |
| `PASEO_RELAY_CLUSTER_*`, `OWNERSHIP_TARGET`, `REROUTE_HEADER`, `MIN_CLUSTER_SIZE` | removed (single node; edge hashing) |
| `RELEASE_NODE`, `RELEASE_COOKIE`, `RELEASE_DISTRIBUTION` | removed (no Erlang) |
| `PASEO_RELAY_DRAIN` | same boot-time semantics |
| `PASEO_RELAY_LOG` | same (`tracing` EnvFilter), plus `PASEO_RELAY_LOG_FORMAT=text/json` |

Note: `PASEO_RELAY_INGRESS_WEIGHT` values `1..3` are rejected at startup; the
accepted range is `4..=16` (default `4`).

## Migration steps

1. Deploy one Rust relay node on a private address; verify `/health`, `/ready`,
   and a pairing through it.
2. Add the edge LB with TLS and `serverId` consistent hashing; verify a real
   Paseo client/daemon pair.
3. Shift traffic gradually. Existing sessions on the old relay need to drain;
   clients reconnect and the new relay creates fresh sessions (E2EE pairing
   remains valid because it is end-to-end, not relay state).
4. Update dashboards/alerts: metric families and names differ; see
   [operations.md](operations.md).
5. Remove the old relay, cluster config, and Erlang distribution exposure.

Migration is a client-visible reconnect, not a transparent handoff.
