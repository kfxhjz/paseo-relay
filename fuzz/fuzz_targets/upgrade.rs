#![no_main]

//! Fuzz HTTP/1.1 WebSocket upgrade-request validation.
//!
//! Arbitrary bytes are turned into plausible `Upgrade`/`Connection`/
//! `Sec-WebSocket-Key`/`Sec-WebSocket-Version` header values (bytes that are
//! not legal header values are skipped), so the fuzzer can reach both the
//! rejection paths and the success path that returns the accept key.

use http::header::{
    HeaderMap, HeaderValue, CONNECTION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
};
use libfuzzer_sys::fuzz_target;
use paseo_relay::ws::validate_upgrade;

/// A valid `Sec-WebSocket-Key` from RFC 6455 section 1.3 (base64 of 16 bytes).
const SAMPLE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// Build a header value from fuzz bytes, or use `fallback` when the bytes are
/// not legal in an HTTP header value.
fn header_value(bytes: &[u8], fallback: &'static str) -> HeaderValue {
    HeaderValue::from_bytes(bytes).unwrap_or_else(|_| HeaderValue::from_static(fallback))
}

fuzz_target!(|data: &[u8]| {
    // The first byte selects which headers are forced to canonical values, so
    // some inputs exercise the fully valid handshake path; the remaining bytes
    // become header-value material for the other headers.
    let (selector, rest) = match data.split_first() {
        Some((first, rest)) => (*first, rest),
        None => (0, &[][..]),
    };

    // Four roughly equal slices; slicing is safe because the indices are
    // derived from `rest.len()` with integer division.
    let len = rest.len();
    let parts = [
        &rest[..len / 4],
        &rest[len / 4..len / 2],
        &rest[len / 2..len * 3 / 4],
        &rest[len * 3 / 4..],
    ];

    let mut headers = HeaderMap::new();

    let connection = if selector & 0b0001 != 0 {
        HeaderValue::from_static("keep-alive, Upgrade")
    } else {
        header_value(parts[0], "Upgrade")
    };
    headers.insert(CONNECTION, connection);

    let upgrade = if selector & 0b0010 != 0 {
        HeaderValue::from_static("websocket")
    } else {
        header_value(parts[1], "websocket")
    };
    headers.insert(UPGRADE, upgrade);

    let version = if selector & 0b0100 != 0 {
        HeaderValue::from_static("13")
    } else {
        header_value(parts[2], "13")
    };
    headers.insert(SEC_WEBSOCKET_VERSION, version);

    let key = if selector & 0b1000 != 0 {
        HeaderValue::from_static(SAMPLE_KEY)
    } else {
        header_value(parts[3], SAMPLE_KEY)
    };
    headers.insert(SEC_WEBSOCKET_KEY, key);

    // Duplicated headers must be rejected rather than taking the first value.
    if selector & 0b1_0000 != 0 {
        headers.append(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("8"));
    }
    if selector & 0b10_0000 != 0 {
        headers.append(SEC_WEBSOCKET_KEY, HeaderValue::from_static(SAMPLE_KEY));
    }

    // Validation must never panic; `Ok` only means the headers form a
    // well-formed handshake.
    let _ = validate_upgrade(&headers);
});
