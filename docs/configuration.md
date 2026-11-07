# Configuration

**English** | [简体中文](zh-CN/configuration.md)

The relay reads configuration only from environment variables. Values are
validated at startup; an invalid value aborts startup with an explanatory
error. Use `paseo-relay --check-config` to validate a deployment environment
and `paseo-relay --dump-config` to print the effective settings (no secrets).

## Runtime contract

| Variable | Default | Meaning |
|---|---|---|
| `PASEO_RELAY_HOST` | `127.0.0.1` | Listener IP. The container image sets `0.0.0.0`. |
| `PASEO_RELAY_PORT` | `4000` | Listener port for HTTP, WebSocket, and ops endpoints. |
| `PASEO_RELAY_DRAIN` | `false` | Start already draining: `/ready` returns 503 and new `/ws` upgrades are rejected. SIGTERM/SIGINT always trigger a drain. |
| `PASEO_RELAY_MAX_CONNECTIONS` | `20000` | Maximum open WebSockets. Must be `<= PASEO_RELAY_MAX_TCP_CONNECTIONS`. |
| `PASEO_RELAY_MAX_TCP_CONNECTIONS` | `25000` | Maximum open TCP connections, including pre-upgrade HTTP. |
| `PASEO_RELAY_MAX_SESSIONS` | `20000` | Maximum live `serverId` sessions. |
| `PASEO_RELAY_MAX_CLIENTS_PER_CONNECTION` | `32` | Maximum v2 client sockets per `connectionId`. |
| `PASEO_RELAY_INGRESS_BUDGET_BYTES` | `536870912` | Node-wide weighted budget for in-flight payload bytes. Must admit one maximum message at the configured weight. |
| `PASEO_RELAY_INGRESS_WEIGHT` | `4` | Memory weight charged per payload byte, plus a fixed 256-byte per-message overhead. |
| `PASEO_RELAY_INGRESS_WAIT_TIMEOUT_MS` | `5000` | Maximum wait for a budget reservation before the source is closed `1013`. |
| `PASEO_RELAY_MAX_MESSAGE_BYTES` | `33554418` | Maximum data frame / reassembled message payload (`32 MiB - 14`). |
| `PASEO_RELAY_FRAME_READ_TIMEOUT_MS` | `300000` | Maximum time to receive one frame payload after its header; stalled payloads close `1013`. |
| `PASEO_RELAY_MESSAGE_READ_TIMEOUT_MS` | `600000` | Maximum time to assemble one fragmented message; exceeding it closes `1013`. |
| `PASEO_RELAY_DATA_ATTACH_TIMEOUT_MS` | `15000` | Maximum time a v2 client frame waits for its daemon data socket. |
| `PASEO_RELAY_DELIVERY_TIMEOUT_MS` | `30000` | Maximum wait for a destination writer permission/write; slow destinations are shed `1013`. |
| `PASEO_RELAY_CONTROL_QUEUE_BYTES` | `1048576` | Per-destination writer queue floor in bytes. The effective queue is at least `MAX_MESSAGE_BYTES + 64 KiB`. |
| `PASEO_RELAY_CONTROL_NUDGE_MS` | `10000` | While a v2 client waits for data, re-send `sync` to control after this delay. |
| `PASEO_RELAY_CONTROL_UNRESPONSIVE_MS` | `15000` | Close a still-unresponsive control socket with `1011` after this delay. Must be greater than the nudge delay. |
| `PASEO_RELAY_SESSION_IDLE_MS` | `30000` | Idle time before an empty session is swept. |
| `PASEO_RELAY_PER_IP_MAX_CONNECTIONS` | `0` | Open WebSocket cap per peer IP; `0` = unlimited. Set ~`256` for direct exposure; behind an LB leave `0` and limit at the edge. |
| `PASEO_RELAY_SHUTDOWN_GRACE_MS` | `10000` | Grace window after SIGTERM/SIGINT before sockets are closed `1012`. |
| `PASEO_RELAY_TCP_KEEPALIVE_MS` | `60000` | TCP keepalive idle time for accepted sockets; `0` disables it. |
| `PASEO_RELAY_HTTP_HEADER_TIMEOUT_MS` | `15000` | Maximum time to read HTTP headers (slowloris guard). |
| `PASEO_RELAY_TLS_CERT` | unset | Path to a PEM certificate chain. Must be set together with `PASEO_RELAY_TLS_KEY`. |
| `PASEO_RELAY_TLS_KEY` | unset | Path to the PEM private key. Both unset = plaintext (terminate TLS at the edge). |
| `PASEO_RELAY_ALLOWED_ORIGINS` | unset | Comma-separated exact `Origin` allowlist for `/ws`; empty = allow any. Requests without Origin (native clients) are allowed. |
| `PASEO_RELAY_LOG` | `info` | `tracing` `EnvFilter` directive, e.g. `info`, `paseo_relay=debug`. |
| `PASEO_RELAY_LOG_FORMAT` | `text` | `text` or `json` structured logs. |
| `PASEO_RELAY_HEALTHCHECK_HOST` | `127.0.0.1` | Host used by the `paseo-relay healthcheck` self-probe. |
| `PASEO_RELAY_BUILD_REVISION` | build-derived | Build-time revision embedded in `/version` and metrics; overrides the git-derived value. |

## CLI

```text
paseo-relay                      # run
paseo-relay --help               # usage
paseo-relay --version            # version + build revision
paseo-relay --check-config       # validate environment, exit 0/1
paseo-relay --dump-config        # print effective configuration
paseo-relay healthcheck          # container self-probe (TLS-aware)
```

The service itself has no command-line configuration; use environment
variables so the same contract works with Docker, Kubernetes, systemd, and
release images.

## Example minimal deployment

```sh
PASEO_RELAY_HOST=127.0.0.1 \
PASEO_RELAY_PORT=4000 \
PASEO_RELAY_LOG_FORMAT=json \
PASEO_RELAY_TLS_CERT=/tls/cert.pem \
PASEO_RELAY_TLS_KEY=/tls/key.pem \
./paseo-relay
```
