#![no_main]

//! Fuzz the RFC 6455 server-side frame reader.
//!
//! Arbitrary bytes are fed to [`FrameReader`] exactly as hyper would feed an
//! upgraded client connection. The reader has two phases (header, then
//! payload), and the fuzz target exercises both: it only charges payload
//! budget after a header validates, and consumes payload bytes through the
//! chunked API used by the production server. Errors are expected and simply
//! end the input; the property under test is "no panic and no unbounded work
//! for untrusted frame bytes".

use futures::executor::block_on;
use libfuzzer_sys::fuzz_target;
use paseo_relay::ws::{FrameReader, WsLimits};

/// Bound on frames decoded from one input so a single test case cannot loop
/// forever on many tiny frames.
const MAX_FRAMES: usize = 64;

/// Payload chunks are read in small pieces, matching the server's incremental
/// budget accounting rather than allocating the declared length at once.
const CHUNK_LEN: usize = 64;

fuzz_target!(|data: &[u8]| {
    block_on(async {
        let mut reader = FrameReader::new(data, WsLimits::default());
        for _ in 0..MAX_FRAMES {
            let header = match reader.read_header().await {
                Ok(header) => header,
                Err(_) => break,
            };

            // TODO(payload-api): this uses the chunked API
            // (`begin_payload` + `read_payload_chunk`) added for incremental
            // ingress budget charging. If that API changes again, adapt here;
            // `read_payload` remains a convenience wrapper with the same
            // validation semantics.
            let mut remaining = match reader.begin_payload(&header) {
                Ok(remaining) => remaining,
                Err(_) => break,
            };
            let mut chunk = [0u8; CHUNK_LEN];
            while remaining > 0 {
                let want = remaining.min(chunk.len());
                match reader.read_payload_chunk(&mut chunk[..want]).await {
                    Ok(0) => break,
                    Ok(read) => remaining -= read.min(remaining),
                    // Truncated/oversized input: both are rejected by the
                    // codec, which is the behaviour we want to fuzz.
                    Err(_) => return,
                }
            }
        }
    });
});
