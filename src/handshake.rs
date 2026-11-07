//! Stateless validation of client-originated `hello` / `e2ee_hello` frames.
//!
//! This is a direct port of `PaseoRelay.HandshakeValidation` from the Elixir
//! relay. Client-originated JSON frames whose `type` is `hello` or
//! `e2ee_hello` are the narrow exception to opaque frame forwarding: their
//! `key` must be canonical padded Base64 for a canonical 32-byte X25519
//! public key. A matching frame with an invalid key must never be forwarded;
//! the caller closes the client with `1008 Invalid handshake key`.
//!
//! Most forwarded frames are not handshakes, so callers should first run the
//! cheap [`looks_like_handshake`] prefilter and only then run the JSON-parsing
//! [`check`].

use std::cmp::Ordering;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value;

use crate::ws::OpCode;

/// Largest payload that [`looks_like_handshake`] considers worth parsing.
///
/// Real handshake frames are a few hundred bytes; the cap keeps a pathological
/// near-JSON frame from being parsed at the full 32 MiB data-frame limit.
pub const MAX_HANDSHAKE_JSON: usize = 1024 * 1024;

/// Handshake message kinds that receive validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeType {
    /// Plain `{"type":"hello", ...}` handshake.
    Hello,
    /// End-to-end-encrypted `{"type":"e2ee_hello", ...}` handshake.
    E2eeHello,
}

/// Result of inspecting one client frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// Not a `hello` / `e2ee_hello` message; forward it opaquely.
    NotHandshake,
    /// A matching handshake with a valid key; forward it.
    Accept(HandshakeType),
    /// A matching handshake with an invalid key; do not forward, close 1008.
    Reject(HandshakeType),
}

/// Cheap prefilter: `payload` is a JSON object candidate small enough to parse.
///
/// True when the payload is at most [`MAX_HANDSHAKE_JSON`] bytes and its first
/// non-ASCII-whitespace byte is `{`. This never parses JSON and is safe to run
/// on every forwarded frame.
pub fn looks_like_handshake(payload: &[u8]) -> bool {
    payload.len() <= MAX_HANDSHAKE_JSON
        && payload
            .iter()
            .find(|byte| !byte.is_ascii_whitespace())
            .is_some_and(|byte| *byte == b'{')
}

/// Validate one text or binary client frame against the handshake contract.
///
/// Returns [`HandshakeOutcome::NotHandshake`] for control frames, non-object
/// JSON, JSON whose `type` is absent or not a handshake type, and invalid
/// JSON. A recognized handshake type with a missing or invalid `key` yields
/// [`HandshakeOutcome::Reject`].
pub fn check(opcode: OpCode, payload: &[u8]) -> HandshakeOutcome {
    if !matches!(opcode, OpCode::Text | OpCode::Binary) {
        return HandshakeOutcome::NotHandshake;
    }

    let message: Value = match serde_json::from_slice(payload) {
        Ok(message) => message,
        Err(_) => return HandshakeOutcome::NotHandshake,
    };
    let Value::Object(message) = message else {
        return HandshakeOutcome::NotHandshake;
    };

    let handshake_type = match message.get("type").and_then(Value::as_str) {
        Some("hello") => HandshakeType::Hello,
        Some("e2ee_hello") => HandshakeType::E2eeHello,
        _ => return HandshakeOutcome::NotHandshake,
    };

    match message.get("key").and_then(Value::as_str) {
        Some(key) if validate_x25519_key(key) => HandshakeOutcome::Accept(handshake_type),
        _ => HandshakeOutcome::Reject(handshake_type),
    }
}

/// Validate a canonical padded Base64 encoding of a canonical X25519 key.
///
/// `encoded` must be standard padded Base64 whose canonical re-encoding equals
/// the input and that decodes to exactly 32 bytes. The little-endian 256-bit
/// coordinate must be below the field prime `2^255 - 19`, and the seven
/// low-order/unsupported encodings from the Elixir implementation are
/// rejected.
pub fn validate_x25519_key(encoded: &str) -> bool {
    let decoded = match STANDARD.decode(encoded.as_bytes()) {
        Ok(decoded) => decoded,
        Err(_) => return false,
    };
    if decoded.len() != 32 {
        return false;
    }
    if STANDARD.encode(&decoded) != encoded {
        return false;
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&decoded);
    is_canonical_coordinate(&key) && !is_unsupported_public_key(&key)
}

/// Field prime `p = 2^255 - 19` in big-endian byte order.
const FIELD_PRIME_BE: [u8; 32] = {
    let mut bytes = [0xFFu8; 32];
    bytes[0] = 0x7F;
    bytes[31] = 0xED;
    bytes
};

/// A canonical X25519 coordinate is a little-endian 256-bit integer `< p`.
fn is_canonical_coordinate(key: &[u8; 32]) -> bool {
    // Big-endian comparison: the last byte of the key is the most significant.
    key.iter().rev().cmp(FIELD_PRIME_BE.iter()) == Ordering::Less
}

/// Unsupported X25519 public-key encodings, copied from the Elixir relay.
///
/// The list contains the two low-order encodings `0` and `1`, two points of
/// small order, and the three encodings `p-1`, `p`, `p+1` (once again as raw
/// little-endian byte strings).
const UNSUPPORTED_PUBLIC_KEYS: [[u8; 32]; 7] = [
    hex_public_key("0000000000000000000000000000000000000000000000000000000000000000"),
    hex_public_key("0100000000000000000000000000000000000000000000000000000000000000"),
    hex_public_key("E0EB7A7C3B41B8AE1656E3FAF19FC46ADA098DEB9C32B1FD866205165F49B800"),
    hex_public_key("5F9C95BCA3508C24B1D0B1559C83EF5B04445CC4581C8E86D8224EDDD09F1157"),
    hex_public_key("ECFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF7F"),
    hex_public_key("EDFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF7F"),
    hex_public_key("EEFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF7F"),
];

fn is_unsupported_public_key(key: &[u8; 32]) -> bool {
    UNSUPPORTED_PUBLIC_KEYS.contains(key)
}

/// Decode a 64-character hex string into a 32-byte public key at compile time.
const fn hex_public_key(hex: &str) -> [u8; 32] {
    let hex = hex.as_bytes();
    assert!(hex.len() == 64, "X25519 key hex must be 64 characters");
    let mut key = [0u8; 32];
    let mut index = 0;
    while index < 32 {
        key[index] = (hex_nibble(hex[index * 2]) << 4) | hex_nibble(hex[index * 2 + 1]);
        index += 1;
    }
    key
}

const fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("invalid hex digit in an X25519 public key"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arbitrary canonical point, not on the unsupported list.
    const VALID_KEY: [u8; 32] = [0x02; 32];

    fn encoded(key: &[u8]) -> String {
        STANDARD.encode(key)
    }

    /// The coordinate `p - delta` as little-endian bytes.
    fn p_minus(delta: u8) -> [u8; 32] {
        let mut key = [0xFFu8; 32];
        key[0] = 0xED - delta;
        key[31] = 0x7F;
        key
    }

    fn hello_payload(key: &str) -> Vec<u8> {
        format!(r#"{{"type":"hello","key":"{key}"}}"#).into_bytes()
    }

    #[test]
    fn accepts_canonical_32_byte_key() {
        assert!(validate_x25519_key(&encoded(&VALID_KEY)));
        // The largest canonical coordinates are p-1 and p-2; p-1 is on the
        // unsupported list, p-2 is accepted.
        assert!(!validate_x25519_key(&encoded(&p_minus(1))));
        assert!(validate_x25519_key(&encoded(&p_minus(2))));
    }

    #[test]
    fn rejects_non_canonical_base64() {
        let key = encoded(&[0xFB; 32]);
        assert!(key.contains('+') && key.contains('/'), "test key encoding");

        let unpadded = key.trim_end_matches('=');
        assert!(!validate_x25519_key(unpadded));

        let url_safe = key.replace('+', "-").replace('/', "_");
        assert!(!validate_x25519_key(&url_safe));

        assert!(!validate_x25519_key(&format!(" {key} ")));
        assert!(!validate_x25519_key(&format!("{key}\n")));
        assert!(!validate_x25519_key(""));
    }

    #[test]
    fn rejects_wrong_lengths() {
        assert!(!validate_x25519_key(&encoded(&[0x02; 31])));
        assert!(!validate_x25519_key(&encoded(&[0x02; 33])));
        assert!(!validate_x25519_key(&encoded(&[])));
    }

    #[test]
    fn rejects_all_unsupported_low_order_keys() {
        assert_eq!(UNSUPPORTED_PUBLIC_KEYS.len(), 7);
        for key in &UNSUPPORTED_PUBLIC_KEYS {
            assert!(
                !validate_x25519_key(&encoded(key)),
                "unsupported key must be rejected: {key:02X?}"
            );
        }
    }

    #[test]
    fn rejects_coordinates_at_or_above_field_prime() {
        // p and p+1 and 2^255-ish values are non-canonical.
        assert!(!validate_x25519_key(&encoded(&p_minus(0))));
        assert!(!validate_x25519_key(&encoded(&[0xFFu8; 32])));
        let mut above = [0xFFu8; 32];
        above[0] = 0xEE;
        assert!(!validate_x25519_key(&encoded(&above)));
    }

    #[test]
    fn hello_with_valid_key_is_accepted() {
        let key = encoded(&VALID_KEY);
        let payload = hello_payload(&key);
        assert_eq!(
            check(OpCode::Text, &payload),
            HandshakeOutcome::Accept(HandshakeType::Hello)
        );
        assert_eq!(
            check(OpCode::Binary, &payload),
            HandshakeOutcome::Accept(HandshakeType::Hello)
        );
    }

    #[test]
    fn e2ee_hello_with_valid_key_is_accepted() {
        let key = encoded(&VALID_KEY);
        let payload = format!(r#"{{"type":"e2ee_hello","key":"{key}"}}"#).into_bytes();
        assert_eq!(
            check(OpCode::Text, &payload),
            HandshakeOutcome::Accept(HandshakeType::E2eeHello)
        );
        assert_eq!(
            check(OpCode::Binary, &payload),
            HandshakeOutcome::Accept(HandshakeType::E2eeHello)
        );
    }

    #[test]
    fn hello_with_invalid_key_is_rejected() {
        let payload = hello_payload("not-base64");
        assert_eq!(
            check(OpCode::Text, &payload),
            HandshakeOutcome::Reject(HandshakeType::Hello)
        );
        let payload = format!(
            r#"{{"type":"e2ee_hello","key":"{}"}}"#,
            encoded(&UNSUPPORTED_PUBLIC_KEYS[0])
        )
        .into_bytes();
        assert_eq!(
            check(OpCode::Text, &payload),
            HandshakeOutcome::Reject(HandshakeType::E2eeHello)
        );
        // Missing and non-string keys are rejects, matching the Elixir port.
        assert_eq!(
            check(OpCode::Text, br#"{"type":"hello"}"#),
            HandshakeOutcome::Reject(HandshakeType::Hello)
        );
        assert_eq!(
            check(OpCode::Text, br#"{"type":"hello","key":123}"#),
            HandshakeOutcome::Reject(HandshakeType::Hello)
        );
        assert_eq!(
            check(OpCode::Text, br#"{"type":"hello","key":null}"#),
            HandshakeOutcome::Reject(HandshakeType::Hello)
        );
    }

    #[test]
    fn other_messages_are_not_handshakes() {
        let key = encoded(&VALID_KEY);
        assert_eq!(
            check(OpCode::Text, br#"{"type":"goodbye"}"#),
            HandshakeOutcome::NotHandshake
        );
        assert_eq!(
            check(OpCode::Text, br#"{"type":123}"#),
            HandshakeOutcome::NotHandshake
        );
        assert_eq!(check(OpCode::Text, b"{}"), HandshakeOutcome::NotHandshake);
        assert_eq!(
            check(OpCode::Text, br#"[]"#),
            HandshakeOutcome::NotHandshake
        );
        assert_eq!(
            check(OpCode::Text, br#"{"key":"abc"}"#),
            HandshakeOutcome::NotHandshake
        );
        assert_eq!(
            check(
                OpCode::Text,
                format!(r#"{{"type":"hello","key":"{key}"}} trailing"#).as_bytes()
            ),
            HandshakeOutcome::NotHandshake
        );
    }

    #[test]
    fn invalid_json_is_not_handshake() {
        assert_eq!(
            check(OpCode::Text, b"not json at all"),
            HandshakeOutcome::NotHandshake
        );
        assert_eq!(
            check(OpCode::Text, b"{\"type\":\"hello\""),
            HandshakeOutcome::NotHandshake
        );
        assert_eq!(
            check(OpCode::Text, b"{\"type\":\"hello\",\"key\":\xFF}"),
            HandshakeOutcome::NotHandshake
        );
    }

    #[test]
    fn control_opcodes_are_never_handshakes() {
        let payload = hello_payload(&encoded(&VALID_KEY));
        for opcode in [
            OpCode::Ping,
            OpCode::Pong,
            OpCode::Close,
            OpCode::Continuation,
        ] {
            assert_eq!(
                check(opcode, &payload),
                HandshakeOutcome::NotHandshake,
                "opcode {opcode:?}"
            );
        }
    }

    #[test]
    fn looks_like_handshake_accepts_json_objects_with_leading_whitespace() {
        assert!(looks_like_handshake(br#"{"type":"hello"}"#));
        assert!(looks_like_handshake(b" \t\r\n{\"type\":\"hello\"}"));
        assert!(looks_like_handshake(b"{}"));
    }

    #[test]
    fn looks_like_handshake_rejects_non_objects_and_empty_input() {
        assert!(!looks_like_handshake(b""));
        assert!(!looks_like_handshake(b"   "));
        assert!(!looks_like_handshake(b"[]"));
        assert!(!looks_like_handshake(b"\"hello\""));
        assert!(!looks_like_handshake(b"not json"));
        assert!(!looks_like_handshake(&[0xFF, b'{']));
    }

    #[test]
    fn looks_like_handshake_enforces_size_cap() {
        let mut exactly_max = vec![b'{'; MAX_HANDSHAKE_JSON];
        assert!(looks_like_handshake(&exactly_max));

        exactly_max.push(b'}');
        assert_eq!(exactly_max.len(), MAX_HANDSHAKE_JSON + 1);
        assert!(!looks_like_handshake(&exactly_max));
    }

    #[test]
    fn handshake_types_are_copy_and_comparable() {
        let outcome = HandshakeOutcome::Accept(HandshakeType::Hello);
        assert_eq!(outcome, HandshakeOutcome::Accept(HandshakeType::Hello));
        assert_ne!(outcome, HandshakeOutcome::Accept(HandshakeType::E2eeHello));
        assert_eq!(HandshakeType::Hello, HandshakeType::Hello);
    }
}
