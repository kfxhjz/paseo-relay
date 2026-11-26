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
The repository contains code review notes, real-socket tests, fuzzing targets
(built and run briefly on every pull request, and for longer weekly), and Paseo
compatibility evidence; those are not a substitute for an audit. Treat v0.1.4
as early software and review the threat model for your deployment.

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

**Residual availability risk.** The relay has no authentication, so a peer
that sends real traffic within the configured caps can still occupy the shared
ingress budget. What the relay controls is the price of doing so and how much
of the node it takes down. (Figures are for the defaults; they scale with the
budget and the timeouts.)

- *Unfinished messages.* A message that is started and never finished must be
  kept arriving: one that goes silent is closed after
  `READ_STALL_TIMEOUT_MS` (30 s), and within about two seconds once another
  source is waiting for budget. One that is kept trickling gives way to a
  waiter after `INGRESS_RECLAIM_AFTER_MS` (60 s). Keeping the main budget full
  this way therefore means re-sending what it holds about once a minute:
  roughly 17 Mbit/s for the 512 MiB default, 4.5 Mbit/s for the 129 MiB budget
  of the Compose/Kubernetes examples. Up to and including v0.1.2 the same
  budget could be held for ten minutes by sending it once and going idle.
- *Messages that are never delivered.* Completely received messages are not
  reclaimed. A peer that sends large messages to a destination that does not
  read them (its own second socket), or as a v2 client whose data socket never
  attaches, holds their budget until `DELIVERY_TIMEOUT_MS` (30 s) or
  `DATA_ATTACH_TIMEOUT_MS` (15 s) expires—including after the source socket
  disconnects or is replaced, because each queued outbound frame carries both
  its ingress budget guard and its absolute delivery deadline. One connection
  may hold up to its in-flight cap: `min(global budget, 4 × one-max-message
  charge)` by default (the whole budget at the defaults), or
  `PASEO_RELAY_PER_CONNECTION_LIMIT_BYTES` when set. Sustaining that costs about
  one budget of payload every 15–30 s: roughly 35–70 Mbit/s at the default,
  9–18 Mbit/s at 129 MiB. Lowering those two timeouts shortens the window.
  Setting the per-connection cap below the budget reduces one source's share,
  but several coordinated connections can still fill the budget, and
  `PASEO_RELAY_PER_IP_MAX_CONNECTIONS` is off by default; the relay warns at
  startup when the derived cap equals the budget and no per-IP cap is set.
  Edge bandwidth/rate limits remain the primary mitigation.
- *What it denies.* While the main budget is full, other sources' messages
  larger than 64 KiB (and fragmented messages) wait and may be closed `1013`
  (retryable). Complete single-frame messages of at most 64 KiB, which is every
  handshake and most ordinary traffic, are served from the separate
  small-message reserve and, if that is full too, displace the largest
  unfinished message. Denying them as well requires filling both pools with
  messages that are never delivered. Control-socket `sync` notifications cannot
  bypass this budget: each session caps the **union** of distinct v2
  `connectionId`s (client ids and daemon data routes share one
  `PASEO_RELAY_MAX_CONNECTIONS_PER_SESSION`, default `512`), runs a single
  deduplicated control watchdog, and bounds every generated `sync` to 64 KiB.
  A client that would make the next `sync` exceed that limit is refused `1013`
  "Relay session sync capacity" before it is attached, so the relay never emits
  an oversized control frame; a failed initial or watchdog `sync` closes the
  control socket instead of leaving a stale routing table.

This is bounded by the documented budgets and deadlines, not eliminated.
Mitigate with edge bandwidth/rate limits, and alert on
`paseo_relay_ingress_rejections_total` (sources actually dropped) and on a
sustained rise of `paseo_relay_ingress_reclaims_total`. The default 512 MiB
budget is slightly smaller than four maximum messages' weighted charge, so a
maximal four-message pipeline can see normal `1013` budget backpressure.

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
and Rust toolchain up to date. In releases after v0.1.2, set
`PASEO_RELAY_OPS_PORT` so the operations endpoints are not on the public port
at all, and when the relay is directly exposed (native TLS, no proxy in front)
set `PASEO_RELAY_PER_IP_MAX_CONNECTIONS`: it is off by default because behind a
proxy every client shares the proxy's address. Keep the process's open-file
limit above `PASEO_RELAY_MAX_TCP_CONNECTIONS`. Size the connection caps to host memory: an idle
accepted socket costs roughly 23 KiB when the relay is plaintext behind an edge,
or ~33 KiB with native TLS, so plan for
`PASEO_RELAY_MAX_TCP_CONNECTIONS × ~30 KiB` (plaintext) or `× ~35 KiB` (native
TLS); the built-in 25,000-socket default needs ~700–800 MiB plaintext or
~850–900 MiB with native TLS, so lower the caps on small nodes.

In releases after v0.1.3, the built-in `paseo-relay healthcheck` no longer
accepts an arbitrary certificate. With native TLS it reads the leaf certificate
at `PASEO_RELAY_TLS_CERT`, pins its DER SHA-256 and accepts only that
certificate from the probe target, so another local service or a path MITM
cannot answer `200` while the relay is unhealthy. If native TLS is configured
but the certificate file cannot be read, the probe only trusts a loopback
target and fails closed for any non-loopback `PASEO_RELAY_HEALTHCHECK_HOST`. A
plaintext listener has no certificate to pin: a non-loopback probe is still
allowed but cannot be authenticated, so keep it on a trusted network.

In releases after v0.1.3, the derived per-connection ingress cap
(`min(budget, 4 × one-max-message charge)`) can be lowered with
`PASEO_RELAY_PER_CONNECTION_LIMIT_BYTES`. This reduces how much of the shared
budget one slow source can hold, but it is not a complete fix for the residual
availability risk above: several connections can still fill the budget, and
the per-IP cap is off by default. Use edge per-source bandwidth/rate limits as
the primary control.

## Verifying a release

Releases after v0.1.2 are signed and attested when they are built. Verify the
image you are about to run against this repository's release workflow:

```sh
IMAGE=ghcr.io/kfxhjz/paseo-relay@sha256:<digest>   # from the release notes

cosign verify "$IMAGE" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github\.com/kfxhjz/paseo-relay/\.github/workflows/release\.yml@refs/tags/v'

gh attestation verify "oci://$IMAGE" --repo kfxhjz/paseo-relay
```

The first command checks a keyless Sigstore signature on the multi-arch image;
the second checks GitHub's build provenance attestation, which records the
commit and workflow that produced that digest.

Publication is ordered so a tag never points at unverified bits. The release
workflow first validates that the tag is a strict `vX.Y.Z[-prerelease]` string,
matches `Cargo.toml` and has a CHANGELOG section, and that its commit is on
`main`; it then builds both platforms, assembles the multi-arch index under a
temporary `candidate-<sha>` tag, smoke-tests that digest, signs it (cosign,
keyless) and creates the GitHub build provenance attestation. Only after all of
those succeed is the version tag promoted to the same digest — and `latest`
only when the release is stable and at least the highest stable tag on `main`.
A failed build, smoke test or signing step therefore leaves the previous
`latest` in place, and an older pending release cannot take over `latest`.

### Repository settings (maintainers)

Some release controls are GitHub repository settings, not workflow code, and a
compromised workflow cannot enforce them on itself. Maintainers should:

- protect `v*` tags (tag ruleset) against deletion and force-update, and
  restrict who can create them (with a bypass for the release actor), so the
  release tags are not editable by a compromised or mistaken run;
- require the full CI context set on `main`: `Format`, `Clippy`, `Test`,
  `Release build`, `Dependency audit`, plus **`Fuzz targets (build + smoke
  run)`** and **`Container image`** (with CodeQL as a code-scanning ruleset), so
  a broken fuzz target or image cannot merge;
- configure the `release` environment used by the publish job with required
  reviewers, so publication needs a human approval after the candidate digest
  exists but before the version tag or `latest` is promoted.

The fuzz suite also runs in the release verification job, so a release cannot
skip it even if the branch-protection setting is missed.

v0.1.2 and earlier cannot be verified this way. The v0.1.2 release has an
`image-digest.txt` asset with a detached signature, but that signs a text file
containing the digest, not the image. Its `provenance.intoto.jsonl` asset was
written by hand in a workflow, is unsigned, and is **not** build provenance; do
not rely on it. v0.1.0 and v0.1.1 have no signature assets. (The release
workflow did ask buildx to attach per-platform provenance and SBOM
attestations to the images in the registry; those are produced by the build
itself.)
