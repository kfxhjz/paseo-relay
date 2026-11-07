## Summary

<!-- What changed and why? -->

## Risk / security impact

<!-- Does this touch wire behavior, resource accounting, IDs, logs, TLS, or
     the public runtime contract? If yes, describe the bounds and failure
     modes. -->

## Testing

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --locked`
- [ ] `cargo build --release --locked`
- [ ] Paseo relay-transport E2E (if wire behavior changed)
- [ ] Docs updated for runtime-contract/compatibility changes

## Evidence

<!-- Commands, results, or a short red/green description for behavior changes. -->

## Checklist

- [ ] No secrets, live `serverId`s, or customer payloads in logs/tests/docs
- [ ] New limits/timeouts have a rationale and a test
- [ ] License of new dependencies is compatible with Apache-2.0
