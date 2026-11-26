#!/usr/bin/env bash
# Fail when fuzz/Cargo.lock disagrees with the root Cargo.lock.
#
# The fuzz crate is a separate package with its own lockfile, so a root
# dependency bump (for example a Dependabot PR editing Cargo.lock) can leave the
# fuzz build resolving different versions of shared crates. That makes
# `cargo audit --file fuzz/Cargo.lock` and the fuzzed dependency graph diverge
# from the published binary without any CI signal. This check compares every
# crate that appears in both lockfiles and fails on any version mismatch.
#
# Missing crates are not detected here: `cargo metadata --manifest-path
# fuzz/Cargo.toml --locked` in CI catches a lockfile that cannot satisfy the
# manifests (see `.github/workflows/ci.yml`). Run this script after updating
# either lockfile; fix drift with e.g.:
#
#   cargo update --manifest-path fuzz/Cargo.toml
#   # or, to pin a specific shared crate to the root version:
#   cargo update --manifest-path fuzz/Cargo.toml -p <crate> --precise <version>
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! command -v python3 >/dev/null 2>&1; then
  echo "scripts/check-fuzz-lock.sh needs python3" >&2
  exit 2
fi

python3 - <<'PY'
from __future__ import annotations

import sys

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    raise SystemExit("scripts/check-fuzz-lock.sh requires Python 3.11+ (tomllib)")

from pathlib import Path


def load(path: Path) -> dict[str, set[str]]:
    with path.open("rb") as handle:
        lock = tomllib.load(handle)
    packages: dict[str, set[str]] = {}
    for package in lock.get("package", []):
        packages.setdefault(package["name"], set()).add(package["version"])
    return packages


def main() -> int:
    root = load(Path("Cargo.lock"))
    fuzz = load(Path("fuzz/Cargo.lock"))
    shared = sorted(root.keys() & fuzz.keys())
    drift = [(name, root[name], fuzz[name]) for name in shared if root[name] != fuzz[name]]
    if not drift:
        print(f"fuzz/Cargo.lock matches Cargo.lock on {len(shared)} shared crates")
        return 0
    print(
        "fuzz/Cargo.lock drifts from Cargo.lock for "
        f"{len(drift)} shared crate(s):",
        file=sys.stderr,
    )
    for name, root_versions, fuzz_versions in drift:
        print(
            f"  {name}: Cargo.lock={','.join(sorted(root_versions))} "
            f"fuzz/Cargo.lock={','.join(sorted(fuzz_versions))}",
            file=sys.stderr,
        )
        if len(root_versions) == 1 and len(fuzz_versions) == 1:
            print(
                "    cargo update --manifest-path fuzz/Cargo.toml "
                f"-p {name}@{next(iter(fuzz_versions))} "
                f"--precise {next(iter(root_versions))}",
                file=sys.stderr,
            )
    print(
        "\nUpdate fuzz/Cargo.lock in the same change as Cargo.lock. The exact",
        file=sys.stderr,
    )
    print(
        "commands above pin each shared crate to the root version; a plain",
        file=sys.stderr,
    )
    print(
        "`cargo update --manifest-path fuzz/Cargo.toml` also work when the root",
        file=sys.stderr,
    )
    print("lockfile is already on the newest compatible versions.", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
PY
