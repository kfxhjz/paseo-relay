# Fuzz targets

The hand-written RFC 6455 codec, handshake classifier, and query parser are the
untrusted-input boundaries. Run them with
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) on a nightly toolchain:

```sh
cargo install cargo-fuzz
RUSTUP_TOOLCHAIN=nightly scripts/fuzz.sh 60   # every target, 60 s each
cargo +nightly fuzz run frame_reader          # or one target, until stopped
```

`rust-toolchain.toml` pins the stable toolchain for this repository, and rustup
prefers it over the installed default. The script therefore needs
`RUSTUP_TOOLCHAIN=nightly` (or `rustup override set nightly`); `cargo +nightly`
only applies to that one invocation.

| Target | What it exercises |
|---|---|
| `frame_reader` | Arbitrary bytes fed to `FrameReader`: header validation, the read-ahead buffer, chunked payload consumption |
| `handshake` | The `hello`/`e2ee_hello` classifier and X25519 key validation |
| `protocol` | `/ws` query parsing and route-id validation |
| `upgrade` | Arbitrary HTTP upgrade headers through `validate_upgrade` |

CI builds every target and runs each for 20 seconds on every pull request
(the `fuzz` job in `.github/workflows/ci.yml`), and for ten minutes each every
week (`.github/workflows/fuzz.yml`), on a pinned nightly
(`nightly-2026-11-20`, exported as `RUSTUP_TOOLCHAIN` for the run). The date is
pinned so a nightly regression cannot fail an unrelated pull request; bump it
in both workflows together. A crash leaves its reproducer in
`fuzz/artifacts/<target>/`, which the weekly workflow uploads.

The fuzz crate is intentionally outside the root package so normal `cargo test`
does not require nightly. It has its own `Cargo.lock`, which must agree with the
root lockfile on every shared crate: otherwise the fuzzed dependency graph (and
`cargo audit --file fuzz/Cargo.lock`) diverges from the released binary. Run
`scripts/check-fuzz-lock.sh` after any dependency change and bump the fuzz
lockfile in the same commit:

```sh
cargo update --manifest-path fuzz/Cargo.toml
# or pin a shared crate to the root version:
cargo update --manifest-path fuzz/Cargo.toml -p <crate> --precise <version>
```

CI also runs `cargo metadata --manifest-path fuzz/Cargo.toml --locked` so a
lockfile that cannot satisfy the manifests fails before the fuzz build. Because
cargo-fuzz has no `--locked`, `scripts/fuzz.sh` additionally fails if a run had
to rewrite the committed lockfile.

The tracked seed corpus lives in `fuzz/seeds/<target>/` (see
[`fuzz/seeds/README.md`](seeds/README.md)); `scripts/fuzz.sh` copies it with
`cp -n` into the gitignored `fuzz/corpus/<target>/`, which CI restores from a
cache so coverage accumulates across runs. See `SECURITY.md` for reporting
issues found this way.
