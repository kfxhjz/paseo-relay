# ADR 0001: Single node + edge consistent hashing

- Status: accepted
- Date: 2026-11-07

## Context

The Elixir relay uses Syn ownership, 409 reroute, and eventual consistency to
support multiple BEAM nodes. That model brings split-brain behavior, a
cluster-wide registry, and an Erlang distribution security surface.

## Decision

This relay is a **single-node** data plane. All sockets for a Paseo `serverId`
must reach the same process, enforced by an edge load balancer hashing the
`serverId` query parameter. There is no ownership registry, reroute response, or
session migration.

## Consequences

- Simpler code with no distributed registry, no split-brain, no EPMD/cookie.
- Operators must configure an edge with consistent hashing; this is documented
  in README/DEPLOYMENT and shown in Nginx/HAProxy/Kubernetes examples.
- Node removal is a reconnect, not a transparent handoff. Paseo clients already
  reconnect.
- Misconfigured edges can split a session; the relay cannot detect or repair it.
