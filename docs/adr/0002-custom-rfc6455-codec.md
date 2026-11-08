# ADR 0002: Custom RFC 6455 codec

- Status: accepted
- Date: 2026-11-07

## Context

The relay needs to charge ingress budget as payload bytes arrive, enforce precise
Paseo size limits, and account for fragmented messages. A high-level WebSocket
library normally assembles a message before the application sees it, which
leaves the largest staging allocation outside the ledger.

## Decision

Implement a small server-side RFC 6455 frame codec directly over any
`AsyncRead`/`AsyncWrite` (`src/ws.rs`). It separates header parsing from payload
consumption so the connection task can read a data-frame payload in 8 KiB chunks
and charge each chunk after it arrives (at most one 8 KiB chunk per connection
is allocated before charging). Control frames,
masking, fragmentation, close payloads, and size limits are explicit and
unit-tested. `tokio-tungstenite` remains a dev-dependency for integration tests.

## Consequences

- Chunked accounting is possible; the read path never allocates an unaccounted
  full payload, and a stalled frame cannot reserve its declared length.
- More protocol code to maintain; covered by focused tests and a fuzz target
  scaffold.
- No compression extension; Paseo does not require it.
- Any future WebSocket extension must be implemented deliberately.
