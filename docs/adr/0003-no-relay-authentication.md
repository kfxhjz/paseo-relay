# ADR 0003: No relay-level authentication

- Status: accepted
- Date: 2026-11-07

## Context

Paseo treats the relay as untrusted and protects message content with
end-to-end encryption. A relay-visible authentication scheme would require
client/daemon protocol changes and would still not protect against a malicious
relay; it only shifts who can consume relay resources.

## Decision

The relay has no authentication or authorization. `serverId` is a bearer
routing secret; anyone who learns it can attach to the session and disrupt
availability. Deployments that need stronger isolation use a VPN, mTLS, or an
authenticated edge, and treat pairing links/logs as secrets.

## Consequences

- Content confidentiality/integrity do not depend on relay authentication; they
  are Paseo E2EE properties.
- Availability is the residual risk; edge rate limits, per-IP controls, and
  monitoring are the intended defenses.
- An optional future relay token would require upstream Paseo coordination and
  a new deployment contract; it is not a drop-in setting.
