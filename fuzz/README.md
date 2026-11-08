# Fuzz targets

The hand-written RFC 6455 codec, handshake classifier, and query parser are the
untrusted-input boundaries. Run these with
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) on a nightly toolchain:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run frame_reader
cargo +nightly fuzz run handshake
cargo +nightly fuzz run protocol
cargo +nightly fuzz run upgrade
```

`frame_reader` feeds arbitrary bytes to `FrameReader` (header validation plus
chunked payload consumption); `upgrade` builds arbitrary HTTP upgrade headers
and calls `validate_upgrade`.

The fuzz crate is intentionally outside the root package so normal `cargo test`
does not require nightly. See `SECURITY.md` for reporting issues found this way.
