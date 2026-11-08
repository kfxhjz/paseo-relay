# Security Policy

**English** | [简体中文](SECURITY.zh-CN.md)

## Supported versions

Security fixes are provided for the latest published minor release (currently
the `0.1.x` line). Development happens on `main`; please test `main` for a fix
if you are reporting a vulnerability that has not yet been released.

## Reporting a vulnerability

Please **do not open a public issue**. Use GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository (Security → Report a vulnerability), or contact the
maintainers through the repository's security contact if one is configured.

Include:

- the affected version or commit;
- a minimal reproduction or protocol sequence;
- the impact you believe it has (confidentiality, integrity, availability, or
  metadata leakage);
- whether the deployment was direct or behind an edge proxy/load balancer.

Never include live `serverId`s, pairing links, credentials, or customer
payloads. If you need to show protocol bytes, generate a fresh isolated test
session.

We aim to acknowledge reports within 3 business days and will coordinate a
disclosure timeline with you. Please allow a fix to be released before public
disclosure.

## Audit status

There has been **no independent third-party security audit** of this relay.
The repository contains code review notes, real-socket tests, fuzzing targets,
and Paseo compatibility evidence; those are not a substitute for an audit.
Treat v0.1.1 as early software and review the threat model for your deployment.

## Threat model

Paseo treats the relay as **untrusted**. Message content is end-to-end
encrypted between the Paseo client and daemon (Noise NK or the legacy box
construction); the relay forwards opaque frames and must not need to decrypt
them. The relay can observe IP addresses, timing, frame sizes, `serverId` /
`connectionId`, and the plaintext handshake public keys.

`serverId` is a **bearer routing secret**, not an authentication token. Anyone
who learns it can attach to that session and disrupt availability. It appears
in the WebSocket URL query string; redact query strings in edge/proxy logs and
treat pairing links and QR codes as secrets. There is no relay-level
authentication by design; deployments that need stronger isolation should put
the relay behind a VPN/mTLS or an authenticated edge.

**Residual availability risk.** A peer that can send real traffic within the
configured caps can still saturate the shared ingress budget: roughly four
connections each sending a ~32 MiB fragmented message and leaving it unfinished
can hold about 512 MiB weighted for up to `MESSAGE_READ_TIMEOUT_MS`. This is
bounded by the documented budget and read deadlines, not eliminated; mitigate
with edge bandwidth/rate limits. The default 512 MiB global budget is slightly
smaller than four maximum messages' weighted charge, so a maximal four-message
pipeline can see normal `1013` budget backpressure.

In scope:

- memory-safety or panic bugs reachable from network input;
- resource exhaustion that bypasses the documented caps or the ingress budget;
- protocol-confusion, cross-session delivery, or close-code changes that could
  corrupt a Paseo session;
- TLS/configuration bugs that silently disable a documented protection;
- accidental logging of IDs, payloads, or secrets.

Out of scope (by design, documented in README/DESIGN):

- the relay being able to observe metadata or drop/delay/reorder frames;
- availability attacks that rely on consuming documented, configured capacity
  (use edge rate limits and monitoring);
- the absence of relay-side authentication;
- security of Paseo's client/daemon E2EE implementation;
- `serverId` leakage caused by deployment logs you control.

## Hardening baseline

See the hardening checklist in [README.md](README.md) and the deployment
examples in [docs/deployment.md](docs/deployment.md). At minimum: terminate TLS,
do not expose `/metrics` or `/ready` publicly, apply per-IP limits at the edge,
run the container read-only as the provided non-root user, and keep the relay
and Rust toolchain up to date. Size the connection caps to host memory: an idle
accepted socket costs roughly 23 KiB when the relay is plaintext behind an edge,
or ~33 KiB with native TLS, so plan for
`PASEO_RELAY_MAX_TCP_CONNECTIONS × ~30 KiB` (plaintext) or `× ~35 KiB` (native
TLS); the built-in 25,000-socket default needs ~700–800 MiB plaintext or
~850–900 MiB with native TLS, so lower the caps on small nodes.
