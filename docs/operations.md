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

By default all endpoints share one listener. There is no authentication on ops
endpoints; do not expose `/metrics` or `/ready` to the public internet.

### Operations listener

Set `PASEO_RELAY_OPS_PORT` (and `PASEO_RELAY_OPS_HOST`, default `127.0.0.1`) to
move `/ready`, `/metrics` and `/version` to a second, plaintext listener. The
public listener then answers only `/ws` and `/health`, so the operations
endpoints cannot be reached through it even if an edge rule is missing. This
is the recommended layout when the relay is directly exposed (native TLS), and
it has two further effects:

- the operations listener has its own allowance of 64 connections, so probes,
  the container healthcheck (`paseo-relay healthcheck` follows the setting) and
  scrapes keep working when the public listener is at its TCP cap, instead of
  the orchestrator restarting a relay that is merely full;
- it keeps answering during the drain grace window, so `/ready` (503) and
  `/health` (200) stay observable after the public listener has closed.

Point readiness/liveness probes, load-balancer health checks and Prometheus at
the operations port once it is enabled: `/ready` and `/metrics` return 404 on
the public port.

## Logs

The relay logs little by design, and never `serverId`, `connectionId` or
payloads. When it sheds work it says so at `warn`, at most once per reason
every 10 seconds, with the number of events since the previous line:

| `event` | Meaning |
|---|---|
| `ingress_exhausted` | Sources were closed `1013` because no ingress budget could be reserved in time |
| `ingress_reclaimed` | Unfinished messages were closed to free ingress budget for a waiting source |
| `read_timeout` | A frame or message stalled or ran past its read deadline |
| `idle_timeout` | Sockets were closed by `PASEO_RELAY_WS_IDLE_TIMEOUT_MS` |
| `slow_consumer` | Destinations were shed at the delivery deadline |
| `websocket_capacity` / `session_capacity` / `tcp_capacity` | Upgrades or connections were refused at a configured cap |
| `per_ip_limit` | A client reached `PASEO_RELAY_PER_IP_MAX_CONNECTIONS` |

Each has a counter (below); the log line is there so a node without a metrics
pipeline still shows that it is under pressure. Operations-listener rejections
are logged separately with the offending peer address, at most once per 10
seconds, and counted as `paseo_relay_ops_rejections_total`; this is the only
warning that names a peer, because the operations allowance is deliberately
small and the source is what an operator needs to find.

## Rotating the TLS certificate

With native TLS, replace the files at `PASEO_RELAY_TLS_CERT` /
`PASEO_RELAY_TLS_KEY` and either send `SIGHUP` (`systemctl reload paseo-relay`
with the example unit) or wait: the relay checks the files every 30 seconds
and reloads them when they change. New connections get the new certificate and
established sessions continue. A pair that cannot be loaded (unreadable,
malformed, certificate and key not matching) is refused with one `warn` line
and the previous certificate stays in use. `v0.1.2` and earlier need a restart
and terminate on `SIGHUP`. `paseo-relay healthcheck` pins the certificate file,
not the certificate the listener is currently serving: while a malformed or
mismatched replacement pair is left on disk, the probe can report unhealthy even
though the old certificate still serves traffic. Repair or restore the files
instead of restarting; startup also tries to load that pair and would fail.

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
`paseo_relay_delivery_ack_timeouts_total`,
`paseo_relay_control_send_failures_total`,
`paseo_relay_connection_rejections_total`, `paseo_relay_session_rejections_total`,
`paseo_relay_session_connection_rejections_total`,
`paseo_relay_client_limit_rejections_total`,
`paseo_relay_ops_rejections_total`, `paseo_relay_per_ip_rejections_total`,
`paseo_relay_tcp_rejections_total`,
`paseo_relay_ingress_rejections_total`, `paseo_relay_ingress_reclaims_total`,
`paseo_relay_read_timeouts_total`, `paseo_relay_idle_timeouts_total`,
`paseo_relay_handshake_accepted_total`, `paseo_relay_handshake_rejected_total`.

The forwarding counters include **only relayed user frames and bytes**. Relay-generated
control frames (`sync`, `connected`, `disconnected`, and the v2 control `pong`) do not
increment `frames_forwarded_total`/`bytes_forwarded_total`, so a control-heavy session
with no user traffic stays at zero. `delivery_timeouts_total` counts a missed
delivery deadline once per shed delivery: at the source for a queue wait
(`QueueTimeout`) and at the destination writer for a write deadline
(`WriteTimeout`). A control notification that misses its 1 s send timeout is
counted once too, alongside `control_send_failures_total`.
`delivery_ack_timeouts_total` counts the caller's acknowledgement wait expiring
at the delivery deadline. It can overlap with `delivery_timeouts_total` (the
source timer and the writer's deadline can both fire for one delivery), and when
the writer completed exactly at the boundary the destination is still closed as
a slow consumer without a `delivery_timeouts_total` or
`slow_consumer_disconnects_total` increment. Do not sum it with those counters;
use it to explain `1013` closes that are not attributed to a shed.
`slow_consumer_disconnects_total` counts the destinations shed at that deadline.
Ordinary transport/write errors are counted as neither.
`control_send_failures_total` counts
`sync`/`connected`/`disconnected`/pong notifications that could not be queued or
written to a control socket; a failed initial or watchdog `sync` is fail-closed and
closes that control socket.

Rejection counters are split by cause:

- `paseo_relay_connection_rejections_total` counts upgrades rejected for
  WebSocket capacity, draining, an invalid upgrade/handshake, a
  disallowed/unparseable Origin, or an upgrade failure/timeout;
- `paseo_relay_per_ip_rejections_total` counts the per-IP WebSocket cap (it is
  **not** part of `connection_rejections_total`);
- `paseo_relay_session_rejections_total` counts **only** the global
  `MAX_SESSIONS` cap;
- `paseo_relay_session_connection_rejections_total` counts sockets refused at
  the per-session `PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION` cap (the union of
  client ids and daemon data routes), or because accepting the socket would
  push the control `sync` past its 64 KiB limit;
- `paseo_relay_client_limit_rejections_total` counts clients refused at the
  per-`connectionId` `PASEO_RELAY_MAX_CLIENTS_PER_CONNECTION` cap;
- `paseo_relay_ops_rejections_total` counts operations-listener connections
  refused by its 64-connection allowance (see
  [Operations listener](#operations-listener));
- `paseo_relay_tcp_rejections_total` counts sockets refused when they were
  accepted: at `MAX_TCP_CONNECTIONS`, or by the per-IP TCP cap.

These counters attribute capacity precisely: a global session ceiling, one
abusive session, one hot `connectionId`, and a full operations listener no
longer share `session_rejections_total`.

The ingress counters tell pressure from loss:

- `paseo_relay_ingress_reclaims_total` counts unfinished messages closed to
  free budget for a waiting source. Some are normal under load;
- `paseo_relay_ingress_rejections_total` counts sources closed because
  reclaiming did not free enough in time. These are users being dropped;
- `paseo_relay_read_timeouts_total` counts sources closed because a frame or
  message stalled or exceeded its read deadline;
- `paseo_relay_idle_timeouts_total` counts sockets closed by the optional idle
  timeout.

Query-parameter validation failures that return `400` before admission are not
counted by `connection_rejections_total`; upgrade/handshake validation failures
that return `400`/`426` are.

Gauges: `paseo_relay_ready`, `paseo_relay_draining`,
`paseo_relay_active_websockets`, `paseo_relay_active_sessions`,
`paseo_relay_ingress_reserved_bytes`, `paseo_relay_ingress_budget_bytes`,
`paseo_relay_ingress_small_reserved_bytes`,
`paseo_relay_ingress_small_budget_bytes`, `paseo_relay_max_frame_bytes`,
`paseo_relay_uptime_seconds`, and `paseo_relay_build_info{version,revision}`.
The two `ingress_small_*` gauges describe the small-message reserve;
`ingress_reserved_bytes` and `ingress_budget_bytes` describe the main budget
only.

Suggested alerts: node unready for >2 minutes, rising connection rejections,
slow-consumer disconnects, ingress reservations above ~80% of the configured
budget (use the `paseo_relay_ingress_reserved_bytes /
paseo_relay_ingress_budget_bytes` ratio, which works for any configured budget),
any increase of `paseo_relay_ingress_rejections_total`, a sustained increase of
`paseo_relay_ingress_reclaims_total`, any increase of
`paseo_relay_ops_rejections_total` (the operations listener is saturated and
`/ready`/`/metrics` may be unreachable), and an instance disappearing from scrape
targets. Rising
`paseo_relay_session_connection_rejections_total` or
`paseo_relay_client_limit_rejections_total` points at one session or one
`connectionId` hitting its cap rather than node-wide capacity; use them to tell
a hot session from a genuinely full node. See
[examples/alerts.yml](https://github.com/kfxhjz/paseo-relay/blob/main/examples/alerts.yml)
for the ingress, connection, slow-consumer, per-session, per-client and
operations-listener rules.

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
- `active_sessions` counts sessions that somebody is attached to: a session is
  removed as soon as its last socket closes. (`v0.1.2` and earlier kept empty
  sessions for `PASEO_RELAY_SESSION_IDLE_MS`, so the gauge stayed high after a
  reconnect storm.)
- Every socket is a file descriptor. If logs show `accept failed` with "too
  many open files", the process's open-file limit is below
  `PASEO_RELAY_MAX_TCP_CONNECTIONS`; raise it (`LimitNOFILE=`, `ulimits.nofile`).
  The relay warns about this at startup on Linux.
- `connection_rejections_total` covers WebSocket capacity, draining, invalid
  upgrades/Origin, and upgrade failure/timeout; per-IP rejections are counted
  separately by `per_ip_rejections_total`, and the per-session / per-client limits
  by `session_connection_rejections_total` / `client_limit_rejections_total`.
  Pair the counters with edge logs for attribution.
- If `/ready` and `/metrics` return `503` only on the operations listener, its
  64-connection allowance is exhausted (slow header floods, too many scrapers,
  or a probe misconfiguration). `ops_rejections_total` grows and the warn line
  names the peer; keep probes short-lived and limit scrape concurrency.
- The default derived per-connection cap can equal the whole ingress budget, so
  one fast source may hold it for the delivery window. Lower
  `PASEO_RELAY_PER_CONNECTION_LIMIT_BYTES`, set `PASEO_RELAY_PER_IP_MAX_CONNECTIONS`,
  or shape per-source bandwidth at the edge; the startup warning points at this
  when both settings are at their default/off values.
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
| `/ready` or `/metrics` returns 404 | `PASEO_RELAY_OPS_PORT` is set: they are served on the operations port only |
| `/ready` and `/metrics` return 503 on the operations port | The operations listener's 64-connection allowance is full; `paseo_relay_ops_rejections_total` rises and the warn line names the peer. Reduce scrape/probe concurrency or block the source |
| TLS healthcheck fails with "not loopback" / "could not be read" | Native TLS is configured but the probe target is non-loopback and `PASEO_RELAY_TLS_CERT` could not be pinned: fix the certificate path or use a loopback target. A plaintext listener allows non-loopback probes but cannot authenticate them |
| Large uploads closed `1013` "Relay ingress reclaimed" | The ingress budget was full and another source was waiting; the upload was stalled, slower than `PASEO_RELAY_INGRESS_RECLAIM_MIN_RATE_BYTES`, or older than `PASEO_RELAY_INGRESS_RECLAIM_AFTER_MS`. Raise the budget for the host, or those two settings for slow links |
| Sources closed `1013` "Relay read timeout" mid-message | No payload progress for `PASEO_RELAY_READ_STALL_TIMEOUT_MS` (30 s by default), or the frame/message deadline passed |
| A setting has no effect | Check the startup log / `--check-config` for an "is not a relay setting" warning: the variable name is misspelled |
