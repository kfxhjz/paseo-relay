# FAQ

**English** | [简体中文](zh-CN/faq.md)

## Is this the relay behind `relay.paseo.sh`?

No. This is a community, single-node implementation. The managed relay used by
Paseo is a separate deployment (currently the Elixir service). This project is
wire-compatible for a single relay endpoint and is intended for self-hosting.

## Do I need the Elixir relay too?

No. This is a standalone relay. Paseo's daemon/client connect to it through the
normal relay settings (`PASEO_RELAY_ENDPOINT`, TLS, pairing). You do not run
Erlang/OTP.

## Can I run multiple nodes?

Yes, behind an edge LB that consistently hashes the `serverId` query parameter.
There is no built-in cluster or transparent failover; clients reconnect when a
node drains. See [deployment.md](deployment.md).

## Does the relay need TLS if Paseo is E2E encrypted?

Yes for public deployments. E2EE protects payload content, not `serverId`,
metadata, or availability. Terminate TLS at the edge or use native rustls.

## Does it decrypt or log messages?

No. Payloads are forwarded opaquely. The only content inspection is the bounded
`hello`/`e2ee_hello` key-hygiene check. `serverId`, `connectionId`, and payloads
are never logged.

## Why is there no relay authentication?

Paseo's threat model treats the relay as untrusted; content security is
end-to-end. `serverId` is a bearer secret. See
[ADR 0003](adr/0003-no-relay-authentication.md).

## How do I point Paseo at it?

Set the daemon's relay endpoint/TLS environment (or `config.json`) to your relay
and pair with `paseo daemon pair --relay`. See the README "Use it with Paseo"
section.

## How do I know it is compatible?

Paseo ships a relay transport E2E suite (`relay-transport.e2e.test.ts`) that can
run against an external relay. The compatibility workflow pins a Paseo commit
and runs it; see [compatibility.md](compatibility.md).

## Why Rust instead of Elixir?

One static binary, no BEAM/EPMD/cookie/distribution, native TLS, explicit
bounded memory accounting, and a small container. The trade-off is no built-in
cluster ownership; the edge provides affinity. See
[ADR 0001](adr/0001-single-node-edge-hashing.md).

## What are the limits?

Default: 20,000 WebSockets, 20,000 sessions, 512 MiB weighted ingress, 32 MiB−14
per message, 64 KiB v2 control frames, 30 s delivery timeout, and at most 1024
fragments per message. Payload bytes are charged in 8 KiB chunks as they arrive;
the 256-byte per-message overhead is charged once, and one connection can hold
at most `min(budget, 4 × one-max-message charge)` of the ingress budget.
Everything is configurable except the fragment cap; see the runtime contract in
[configuration.md](configuration.md).

## How much memory do I need?

Idle accepted sockets dominate: each costs roughly 23 KiB (hyper read/write
buffers plus the task), so plan for `PASEO_RELAY_MAX_TCP_CONNECTIONS × ~30 KiB`
plus the in-flight ingress budget. The built-in 25,000 TCP / 20,000 WebSocket
defaults need roughly 700–800 MiB before payload traffic; a 512 MB host must
lower the caps (the Compose examples use 5,000 / 4,000 / 4,000). The
pre-upgrade HTTP path is additionally capped by hyper at 16 KiB buffers/headers
and 64 headers.
