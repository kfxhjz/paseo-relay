# Fuzz seed corpus

Small, reviewable inputs that give the fuzzers a starting point in the parser
paths that matter. `scripts/fuzz.sh` copies every file in `fuzz/seeds/<target>/`
into the gitignored `fuzz/corpus/<target>/` directory with `cp -n` before a run,
so the fuzzer keeps growing a corpus locally and in the CI cache without the
seeds ever overwriting it.

The seeds are not golden tests: they only have to land near valid input and
near the rejection paths listed in the per-target descriptions below. Keep them
small (a few hundred bytes) and add one when a new parser branch is introduced.

## `frame_reader`

Raw bytes fed to `paseo_relay::ws::FrameReader` exactly as if a client had sent
them after the upgrade. Client-to-server frames must be masked.

| File | Path exercised |
|---|---|
| `masked_text_hello.bin` | smallest masked text frame (FIN, 7-bit length) |
| `masked_binary_hello.bin` | masked binary frame |
| `fragmented_text.bin` | fragmented text message (opcode 1 then 0) |
| `ping_pong_close.bin` | ping, pong, and close control frames |
| `extended_16bit_len.bin` | 16-bit extended payload length |
| `extended_64bit_len.bin` | 64-bit extended length with a missing payload |
| `unmasked_text.bin` | illegal unmasked client frame |
| `reserved_bits.bin` | RSV1 set without a negotiated extension |
| `unknown_opcode.bin` | reserved opcode 3 |

## `handshake`

Client text/binary frame payloads passed to
`paseo_relay::handshake::{looks_like_handshake, check, validate_x25519_key}`.

| File | Path exercised |
|---|---|
| `hello_valid.json` | accepted canonical 32-byte X25519 key |
| `e2ee_hello_valid.json` | accepted `e2ee_hello` handshake |
| `hello_missing_key.json` | handshake without a key (rejected) |
| `hello_bad_key.json` | zero low-order point (rejected) |
| `hello_noncanonical_b64.json` | Base64 that is not re-encoding-canonical |
| `not_handshake.json` | unrelated JSON object |
| `upgrade_request.http` | raw RFC 6455 upgrade request bytes |

## `protocol`

Raw query strings after `/ws?` passed to
`paseo_relay::protocol::Connection::from_query`.

| File | Path exercised |
|---|---|
| `v1_client.txt`, `v1_server.txt` | v1 client/server routes |
| `v2_control.txt` | v2 control route (empty `connectionId`) |
| `v2_data.txt` | v2 data route with a `connectionId` |
| `v2_client_empty_id.txt` | v2 client with a generated id |
| `percent_encoded.txt` | percent-encoded id containing whitespace |
| `invalid_utf8.bin` | percent-encoded invalid UTF-8 |
| `unknown_keys.txt` | unknown parameters are ignored |
| `lowercase_keys.txt` | parameter names are case-sensitive |
| `empty.txt` | empty query (missing role/serverId) |

## `upgrade`

The target uses the first byte as a selector (bits force canonical
`Connection`/`Upgrade`/`Sec-WebSocket-Version`/`Sec-WebSocket-Key` values) and
splits the remaining bytes into four header-value slices.

| File | Path exercised |
|---|---|
| `canonical.bin` | selector `0x0f`, fully valid upgrade |
| `canonical_with_duplicates.bin` | valid headers plus duplicated key/version |
| `weird_values.bin` | selector `0x00` with arbitrary header values |
