# ADR 0002: Custom RFC 6455 codec

- Status: accepted
- Date: 2026-11-07

## Context

The relay needs to reserve ingress budget **before** allocating a frame
payload, enforce precise Paseo size limits, and account for fragmented messages.
A high-level WebSocket library normally assembles a message before the
application sees it, which leaves the largest staging allocation outside the
ledger.

## Decision

Implement a small server-side RFC 6455 frame codec directly over any
`AsyncRead`/`AsyncWrite` (`src/ws.rs`). It separates header parsing from payload
reading so the connection task can reserve budget first. Control frames,
masking, fragmentation, close payloads, and size limits are explicit and
unit-tested. `tokio-tungstenite` remains a dev-dependency for integration tests.

## Consequences

- Pre-payload accounting is possible; the read path never allocates an
  unaccounted payload.
- More protocol code to maintain; covered by focused tests and a fuzz target
  scaffold.
- No compression extension; Paseo does not require it.
- Any future WebSocket extension must be implemented deliberately.
