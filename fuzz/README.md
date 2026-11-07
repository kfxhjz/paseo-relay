# Fuzz targets

The hand-written RFC 6455 codec, handshake classifier, and query parser are the
untrusted-input boundaries. Run these with
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) on a nightly toolchain:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run handshake
cargo +nightly fuzz run protocol
```

The fuzz crate is intentionally outside the root package so normal `cargo test`
does not require nightly. See `SECURITY.md` for reporting issues found this way.
