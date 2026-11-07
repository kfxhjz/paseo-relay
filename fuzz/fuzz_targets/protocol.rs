#![no_main]
use libfuzzer_sys::fuzz_target;
use paseo_relay::protocol::Connection;

fuzz_target!(|data: &[u8]| {
    if let Ok(query) = std::str::from_utf8(data) {
        let _ = Connection::from_query(query);
    }
});
