# Fuzz targets

The hand-written RFC 6455 codec, handshake classifier, and query parser are the
untrusted-input boundaries. Run them with
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) on a nightly toolchain:

```sh
cargo install cargo-fuzz
scripts/fuzz.sh 60                          # every target, 60 s each
cargo +nightly fuzz run frame_reader        # or one target, until stopped
```

| Target | What it exercises |
|---|---|
| `frame_reader` | Arbitrary bytes fed to `FrameReader`: header validation, the read-ahead buffer, chunked payload consumption |
| `handshake` | The `hello`/`e2ee_hello` classifier and X25519 key validation |
| `protocol` | `/ws` query parsing and route-id validation |
| `upgrade` | Arbitrary HTTP upgrade headers through `validate_upgrade` |

CI builds every target and runs each for 20 seconds on every pull request
(the `fuzz` job in `.github/workflows/ci.yml`), and for ten minutes each every
week (`.github/workflows/fuzz.yml`), on a pinned nightly
(`nightly-2026-11-20`). The date is pinned so a nightly regression cannot fail
an unrelated pull request; bump it in both workflows together. A crash leaves
its reproducer in `fuzz/artifacts/<target>/`, which the weekly workflow uploads.

The fuzz crate is intentionally outside the root package so normal `cargo test`
does not require nightly. It has its own `Cargo.lock`; cargo-fuzz has no
`--locked`, so `scripts/fuzz.sh` fails if a run had to change the committed
lockfile. See `SECURITY.md` for reporting issues found this way.
