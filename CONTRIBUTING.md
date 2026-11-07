# Contributing

**English** | [简体中文](CONTRIBUTING.zh-CN.md)

Thanks for helping improve Paseo Relay. This is a small, security-sensitive
data-plane component: correctness, bounded memory, and clear failure modes
matter more than feature count.

## Development setup

- Rust `1.99.1` (pinned in `rust-toolchain.toml`; `rustup` installs it).
- `cargo fmt`, `clippy`, and the test toolchain from the pin.
- Docker only for the optional container checks.

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
python3 scripts/check-docs.py   # local links + Chinese/English pairing
scripts/ci.sh          # includes the same checks plus a container smoke test
```

The test suite uses real listeners and real WebSocket clients
(`tokio-tungstenite`), not mocks. Please keep that property: new protocol
behavior should be covered by an integration test in
`tests/relay_integration.rs`, and pure logic by unit tests next to the module.

## Change guidelines

- **Security and behavior changes need a failing test first.** For a protocol
  or resource-accounting change, add the red test, then the fix, and record the
  evidence in `DESIGN.md` or the PR description.
- Do not remove a bound, timeout, or cap without explaining the new bound.
- Do not log `serverId`, `connectionId`, payloads, or TLS key material.
- Keep the payload path opaque: the only protocol inspection allowed is the
  bounded `hello`/`e2ee_hello` hygiene check.
- Preserve Paseo wire behavior. If you intentionally differ from the Elixir
  relay, document the difference in `docs/compatibility.md` and README.
- Any new dependency needs a reason, a license compatible with Apache-2.0, and
  should survive `cargo audit` / review.

## Testing against Paseo

The strongest compatibility test is Paseo's own relay end-to-end suite. It can
point at an already-running relay:

```sh
# In a shallow Paseo checkout with dependencies installed:
PASEO_RELAY_E2E_PORT=4000 FORCE_RELAY_E2E=1 \
  npx vitest run packages/server/src/server/daemon-e2e/relay-transport.e2e.test.ts --maxWorkers=1
```

See [docs/compatibility.md](docs/compatibility.md) for details. Please run it
when changing wire behavior if you have the environment available.

## Documentation and translation

See [docs/translation.md](docs/translation.md) and [docs/zh-CN/translation.md](docs/zh-CN/translation.md).

## Pull requests

- Describe the user-visible change and the risk it addresses.
- Include the commands and results you ran.
- Keep refactors separate from behavior changes.
- Update README/DESIGN/docs when the runtime contract, compatibility, or
  security model changes.
- By contributing you agree that your contribution is licensed under the
  repository's Apache-2.0 license.

## Code of conduct

This project follows [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md). Be kind,
assume good faith, and keep discussion technical.
