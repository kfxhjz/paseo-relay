# Choosing a relay deployment

**English** | [简体中文](zh-CN/comparison.md)

This is a decision guide, not a benchmark. It describes which deployment model
fits which user, and what this project does **not** do.

## Quick decision

| You are... | Recommended |
|---|---|
| One person running Paseo on a laptop/NAS/VPS, wants no Erlang | **This relay**, single node behind Caddy/edge TLS |
| A small team with one daemon and several clients | **This relay**, single node; monitor capacity |
| A team that needs multiple relay nodes without extra edge work | **Elixir relay** (Syn ownership/409 reroute) or the managed relay |
| Someone who does not want to operate a public endpoint at all | **Managed Paseo relay** |
| Security-sensitive and wants to inspect/audit the relay | **This relay** (small Rust code, bounded model), but read [SECURITY.md](../SECURITY.md) first |
| A relay operator serving the public | **This relay** is usable, but read [operator-policy.md](operator-policy.md) and provide edge limits/privacy policy |

## Comparison dimensions

- **Runtime/ops:** one static Rust binary vs BEAM release + Cowboy vs managed.
- **Cluster model:** edge consistent hashing (this relay) vs Syn ownership and
  reroute (Elixir) vs provider-managed.
- **Failure model:** node drain = reconnect; no transparent migration.
- **Memory/limits:** explicit configured caps and pre-payload accounting here;
  BEAM/capacity ledger in Elixir.
- **TLS:** native rustls or edge; Elixir typically edge.
- **Authentication:** none in either relay by design; Paseo E2EE protects
  content, and `serverId` is a bearer secret.
- **Ecosystem:** Elixir relay has built-in cluster behavior; this project aims at
  a smaller, auditable single-node deployment and a plain wire contract.

## What this relay does not do

- no built-in clustering, ownership handoff, reroute, or split-brain resolution;
- no relay-level authentication, per-user quotas, or token revocation;
- no payload inspection/decryption/storage/replay protection;
- no transparent session migration or HA persistence;
- no administrative HTTP API beyond health/ready/metrics/version.

If those are requirements, do not deploy this relay; use the Elixir relay or the
managed relay. This is an intentional scope, documented in
[DESIGN.md](../DESIGN.md).

## Migration

If you are moving from the Elixir relay, see [migration.md](migration.md). Expect Paseo clients to reconnect once.
