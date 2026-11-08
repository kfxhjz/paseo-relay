# Operating a public relay

**English** | [简体中文](zh-CN/operator-policy.md)

This page is for people who want to expose a relay to other Paseo users, not
just themselves. It is operational guidance, not legal advice.

## The security model you are inheriting

The relay has **no authentication or authorization by design**. `serverId` is a
bearer routing secret; anyone who learns it can attach to that session and
disrupt availability. Paseo payloads are end-to-end encrypted and opaque to the
relay, so you cannot inspect, moderate, or filter message content.

That has consequences:

- a public relay is an **open signaling/rendezvous service** for anyone with a
  valid `serverId`; it is not an anonymity network;
- you can rate-limit and monitor metadata/volume, but you cannot classify
  traffic by content;
- if you cannot accept abuse/availability risk, run the relay privately (VPN,
  mTLS, or an authenticated edge) instead of exposing it publicly.

## Minimum controls

- Terminate TLS; do not serve plaintext `ws://` publicly.
- Put a rate-limiting edge (CDN/WAF/LB) in front: per-IP connection count,
  connect rate, message rate, and bandwidth. The provided Nginx and HAProxy
  examples include per-client connection/rate limits, deny the operations
  endpoints, and log without query strings; size their thresholds for your
  traffic. The Caddy example has no in-process per-IP limiting, so pair it with
  an external CDN/WAF. In-process `PASEO_RELAY_PER_IP_MAX_CONNECTIONS` sees the
  proxy IP when an LB terminates connections; leave it at `0` behind a proxy and
  limit at the edge.
- Set `PASEO_RELAY_MAX_CONNECTIONS`, `MAX_TCP_CONNECTIONS`, `MAX_SESSIONS`, and
  the ingress budget for your host size; monitor `/ready` and `/metrics`. An idle
  accepted socket still costs ~23 KiB, so `MAX_TCP_CONNECTIONS × ~30 KiB` is the
  memory floor and a 512 MB host cannot use the built-in 25,000 default.
- Clients should send exactly one unencoded `serverId`. Duplicate query keys are
  last-wins at the relay, while edge consistent hashing normally uses the raw
  query value, so duplicate or percent-encoded values can route the two sides of
  a session to different relay nodes.
- Keep `/metrics` on an internal network and rotate/restrict access.
- Run the provided scratch image read-only, non-root, drop capabilities; keep
  the toolchain/image updated.
- Do not add Erlang distribution; there is none in this relay.

## Logs, privacy, and retention

- The relay deliberately does **not** log `serverId`, `connectionId`, or
  payloads.
- Your edge/proxy/CDN may log full URLs; **query strings contain `serverId`**.
  Redact or disable query logging, and set a retention period.
- In-memory session topology and IP-derived limits are ephemeral. Decide
  whether your edge logs client IPs; document that for users.
- Do not promise anonymity. You can still observe IPs, timing, frame sizes, and
  session IDs at the network edge.
- If you operate in a jurisdiction with data-protection rules, publish a short
  privacy notice covering edge logs, retention, and abuse handling.

## Abuse handling

- Publish an abuse contact and a short policy (what is not allowed, how you
  respond, what data you keep).
- Use volume/connection limits first; do not attempt payload inspection, which
  the protocol does not allow and would break the E2EE trust model.
- For targeted abuse, you can block source IPs/ranges at the edge, lower global
  caps, or temporarily disable the listener. Avoid logging more identifiers
  than necessary.
- Keep an incident note with timestamps, metrics, and actions; do not publish
  another user's `serverId` or pairing link.

## Capacity and availability

- A relay node is small, but bandwidth and connection counts are the real
  limits. Idle memory is dominated by accepted sockets (each ~23 KiB), so plan
  for `MAX_TCP_CONNECTIONS × ~30 KiB` plus the ingress budget and leave headroom
  for bursts and Caddy/proxy. Size `PASEO_RELAY_MAX_CONNECTIONS`,
  `MAX_SESSIONS`, and the ingress budget for the host.
- For multiple nodes, use an edge that hashes the `serverId` query parameter and
  removes draining nodes using `/ready`. Plain round-robin will split sessions.
- Drain a node with SIGTERM, wait the grace window, then remove it. Do not
  SIGKILL first.
- Maintain a status page/issue channel for planned maintenance; users will see
  reconnects even for a graceful drain.

## Before making it public

- [ ] TLS + HTTPS/WSS only, HSTS considered.
- [ ] Edge per-IP/request/bandwidth limits.
- [ ] Query-string logging disabled or redacted.
- [ ] Abuse contact and privacy/retention note published.
- [ ] `/metrics` and `/ready` internal only.
- [ ] Capacity limits and alerts configured.
- [ ] Image pinned by digest and updated on security releases.
- [ ] Incident/rollback procedure tested (SIGTERM drain, restore previous image).
- [ ] Clear statement: community relay, not the managed Paseo relay; no SLA.
