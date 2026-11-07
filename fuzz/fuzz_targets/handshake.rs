#![no_main]
use libfuzzer_sys::fuzz_target;
use paseo_relay::handshake::{check, looks_like_handshake, validate_x25519_key};
use paseo_relay::ws::OpCode;

fuzz_target!(|data: &[u8]| {
    let _ = looks_like_handshake(data);
    let _ = check(OpCode::Text, data);
    let _ = check(OpCode::Binary, data);
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = validate_x25519_key(text);
    }
});
