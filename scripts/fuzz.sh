#!/usr/bin/env bash
# Build every fuzz target and run each one for a bounded time.
#
#   scripts/fuzz.sh [seconds-per-target]     (default: 20)
#
# Needs a nightly toolchain and cargo-fuzz (see fuzz/README.md). The repository
# pins stable in rust-toolchain.toml, so select nightly explicitly with
# RUSTUP_TOOLCHAIN or `rustup override`; CI sets the pinned nightly. Exits
# non-zero as soon as a target crashes; the reproducer is left in
# fuzz/artifacts/<target>/.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

readonly seconds="${1:-20}"
if ! [[ "${seconds}" =~ ^[0-9]+$ ]] || [ "${seconds}" -eq 0 ]; then
  echo "usage: scripts/fuzz.sh [seconds-per-target]" >&2
  exit 2
fi

# Fail early instead of letting cargo-fuzz report a cryptic `-Zsanitizer`
# error against the stable toolchain pinned by rust-toolchain.toml.
if ! rustc --version 2>/dev/null | grep -q 'nightly'; then
  echo "scripts/fuzz.sh needs a nightly toolchain; set RUSTUP_TOOLCHAIN=nightly (or run rustup override set nightly)" >&2
  rustc --version >&2 || true
  exit 2
fi

# cargo-fuzz defaults to the target its own binary was built for. The prebuilt
# binary installed in CI is a musl build, and ASan cannot link against musl's
# static libc; always build and fuzz for the host target instead.
host="$(rustc -vV | sed -n 's/^host: //p')"
if [[ -z "${host}" ]]; then
  echo "could not determine the host target from rustc -vV" >&2
  exit 2
fi

# The four untrusted-input boundaries. Keep in sync with fuzz/Cargo.toml.
readonly targets=(frame_reader handshake protocol upgrade)

for target in "${targets[@]}"; do
  echo "==> fuzz ${target} (${seconds}s)"
  # Inputs are capped at 64 KiB: every parser here is driven by a few header
  # bytes, and long inputs only slow the search down.
  cargo fuzz run --target "${host}" "${target}" -- \
    -max_total_time="${seconds}" -max_len=65536 -print_final_stats=1
done

# cargo-fuzz has no --locked. The fuzz crate has its own committed lockfile, so
# catch a build that had to change it (a dependency of the relay moved without
# fuzz/Cargo.lock being updated).
if command -v git >/dev/null 2>&1 && git rev-parse --git-dir >/dev/null 2>&1 \
    && git ls-files --error-unmatch fuzz/Cargo.lock >/dev/null 2>&1 \
    && ! git diff --quiet -- fuzz/Cargo.lock; then
  echo "fuzz/Cargo.lock is out of date; run a fuzz build locally and commit it" >&2
  exit 1
fi

echo "all ${#targets[@]} fuzz targets ran clean"
