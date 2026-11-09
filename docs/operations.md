# Operations

**English** | [简体中文](zh-CN/operations.md)

## Runtime endpoints

| Path | Purpose | Exposure |
|---|---|---|
| `/health` | Liveness; 200 while the process is alive (including draining) | Orchestrator/LB |
| `/ready` | 200 only while admitting new work; 503 while draining, shutting down, or at WebSocket/session capacity | LB readiness, monitoring |
| `/metrics` | Prometheus text (aggregate-only, no IDs) | Internal monitoring only |
| `/version` | Build version and git revision | Internal/optional |
| `/ws` | Paseo WebSocket protocol | Public (behind TLS/LB) |

All endpoints share the listener. There is no authentication on ops endpoints;
do not expose `/metrics` or `/ready` to the public internet.

## Draining a node

1. Remove the node from the load balancer (or set it unready first).
2. Wait until the LB no longer sends new connections (readiness observed).
3. Send `SIGTERM`. The relay stops accepting, flips `/ready` to 503, keeps
   existing sessions for `PASEO_RELAY_SHUTDOWN_GRACE_MS`, then closes them with
   `1012` and exits.
4. Ensure the orchestrator grace period is larger than the relay's grace
   (`terminationGracePeriodSeconds`/`TimeoutStopSec` > `PASEO_RELAY_SHUTDOWN_GRACE_MS`).
5. Only use `SIGKILL` as a last resort; it skips the close frames and leaves
   clients to reconnect immediately.

`PASEO_RELAY_DRAIN=true` starts the process already unready. This is useful
when a node is brought up only to receive no new work, or while an operator
prepares a rotation. It is a boot-time flag; the process has no runtime
"cancel drain" control.

## Metrics reference

Counters: `paseo_relay_frames_forwarded_total`, `paseo_relay_bytes_forwarded_total`,
`paseo_relay_slow_consumer_disconnects_total`, `paseo_relay_delivery_timeouts_total`,
`paseo_relay_connection_rejections_total`, `paseo_relay_session_rejections_total`,
`paseo_relay_per_ip_rejections_total`, `paseo_relay_handshake_accepted_total`,
`paseo_relay_handshake_rejected_total`.

Rejection counters are split by cause:

- `paseo_relay_connection_rejections_total` counts upgrades rejected for
  WebSocket capacity, draining, an invalid upgrade/handshake, a
  disallowed/unparseable Origin, or an upgrade failure/timeout;
- `paseo_relay_per_ip_rejections_total` counts the per-IP WebSocket cap (it is
  **not** part of `connection_rejections_total`);
- `paseo_relay_session_rejections_total` counts the `MAX_SESSIONS` cap.

Query-parameter validation failures that return `400` before admission are not
counted by `connection_rejections_total`; upgrade/handshake validation failures
that return `400`/`426` are.

Gauges: `paseo_relay_ready`, `paseo_relay_draining`,
`paseo_relay_active_websockets`, `paseo_relay_active_sessions`,
`paseo_relay_ingress_reserved_bytes`, `paseo_relay_ingress_budget_bytes`,
`paseo_relay_max_frame_bytes`, `paseo_relay_uptime_seconds`, and
`paseo_relay_build_info{version,revision}`.

Suggested alerts: node unready for >2 minutes, rising connection rejections,
slow-consumer disconnects, ingress reservations above ~80% of the configured
budget (use the `paseo_relay_ingress_reserved_bytes /
paseo_relay_ingress_budget_bytes` ratio, which works for any configured budget),
and an instance disappearing from scrape targets. See
[examples/alerts.yml](https://github.com/kfxhjz/paseo-relay/blob/main/examples/alerts.yml).

## Capacity and debugging

- `paseo_relay_ingress_reserved_bytes / paseo_relay_ingress_budget_bytes`
  approaching `0.8` means large in-flight messages or slow destinations; look at
  slow-consumer/delivery metrics. The ratio works for any configured budget,
  unlike an absolute byte threshold.
- Idle memory is dominated by accepted sockets, not by payload: an idle socket
  costs roughly 23 KiB plaintext or ~33 KiB with native TLS, so
  `PASEO_RELAY_MAX_TCP_CONNECTIONS × ~30 KiB` (plaintext) or `× ~35 KiB` (native
  TLS) is the floor. The built-in 25,000-socket default needs ~700–800 MiB
  plaintext (or ~850–900 MiB with native TLS) before traffic; lower the caps on
  small nodes.
- Because the TCP slot is held for the whole socket lifetime, keep
  `PASEO_RELAY_MAX_TCP_CONNECTIONS` above `PASEO_RELAY_MAX_CONNECTIONS` with
  headroom (at least 64–128 slots, more for probe bursts). At equal caps a full
  WebSocket load can reject `/health`, `/ready`, `/metrics`, and probes at accept
  even though the process is healthy.
- `active_sessions` includes empty sessions until the idle sweep removes them;
  this is expected after a reconnect storm.
- `connection_rejections_total` covers WebSocket capacity, draining, invalid
  upgrades/Origin, and upgrade failure/timeout; per-IP rejections are counted
  separately by `per_ip_rejections_total`. Pair the counters with edge logs for
  attribution.
- The relay does not log `serverId`, `connectionId`, or payloads. Use edge
  logs (with query strings redacted where possible) for per-user attribution.
- `/version` and `build_info` identify the deployed commit. Set
  `PASEO_RELAY_BUILD_REVISION` during builds if the checkout has no `.git`.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| All nodes report `503 /ready` | Draining, capacity full, session cap reached, or a node-wide capacity failure; check `/metrics` and logs |
| Sessions connect to different nodes | Edge is not hashing on `serverId`; fix the LB hash policy |
| One user cannot connect after a node restart | Client reconnects; verify the edge ring did not map `serverId` elsewhere |
| Continuous 1012/1013 closes | Edge/LB idle or write timeouts too short for long-lived sessions; check `PASEO_RELAY_DELIVERY_TIMEOUT_MS` and proxy tunnel timeouts |
| TLS handshake failures in native TLS mode | Certificate/key mismatch or missing read permission for UID 65532 |
