#!/usr/bin/env bash
# Local CI entry point. Mirrors .github/workflows/ci.yml so contributors can
# reproduce the pipeline locally. The image smoke test needs Docker but does
# not need network access beyond the Docker daemon.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo '==> documentation links'
python3 scripts/check-docs.py

echo '==> cargo fmt'
cargo fmt --all --check

echo '==> cargo clippy'
cargo clippy --all-targets -- -D warnings

echo '==> cargo test'
cargo test --locked

echo '==> cargo build --release'
cargo build --release --locked

if ! command -v docker >/dev/null 2>&1; then
  echo '==> docker not found; skipping image build' >&2
  exit 0
fi

echo '==> docker build'
docker build --tag paseo-relay:ci .

# The scratch image has no curl, so probe /health from the host. This also
# verifies the container binds 0.0.0.0 and serves the readiness contract.
echo '==> docker smoke test'
readonly container="paseo-relay-ci-$$"
readonly port="${PASEO_RELAY_CI_PORT:-14000}"

cleanup() {
  docker rm --force "${container}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run --detach --name "${container}" \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --publish "127.0.0.1:${port}:4000" paseo-relay:ci >/dev/null

for _attempt in $(seq 1 100); do
  if curl --fail --silent --show-error "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then
    echo 'container healthy'
    exit 0
  fi
  sleep 0.1
done

echo 'container did not become healthy' >&2
docker logs "${container}" >&2 || true
exit 1
