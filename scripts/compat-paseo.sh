#!/usr/bin/env bash
# Run Paseo's own relay transport E2E suite against this relay.
#
# Requirements: git, node/npm, a Rust toolchain, and network access. This is a
# heavyweight check used by the compatibility workflow, not by every local CI
# run. It clones Paseo at a pinned commit (or $PASEO_SHA), installs the server
# workspace, builds the relay, starts it, and runs relay-transport.e2e.test.ts.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
readonly repo_root="$PWD"
readonly work="${PASEO_COMPAT_WORKDIR:-/tmp/paseo-relay-compat}"
readonly paseo_sha="${PASEO_SHA:-ea30d2616a6ea1ad51447cde6e2fc6da4904fe21}"
readonly port="${PASEO_RELAY_E2E_PORT:-4399}"

if ! command -v node >/dev/null || ! command -v npm >/dev/null; then
  echo "node and npm are required" >&2
  exit 2
fi

if [[ ! -d "${work}/.git" ]]; then
  rm -rf "${work}"
  git init -q "${work}"
  git -C "${work}" remote add origin https://github.com/getpaseo/paseo.git
fi
git -C "${work}" fetch -q --depth 1 origin "${paseo_sha}"
git -C "${work}" checkout -q FETCH_HEAD

if [[ ! -d "${work}/node_modules" ]]; then
  # Server workspace + root dev dependencies (vitest) only. Ignore lifecycle
  # scripts: this harness never needs Electron/desktop native builds.
  (cd "${work}" && npm ci --ignore-scripts --include-workspace-root \
    --workspace packages/server \
    --workspace packages/protocol \
    --workspace packages/client \
    --workspace packages/relay \
    --workspace packages/plugin \
    --workspace packages/highlight)
fi

# The server test imports workspace dist packages; build the needed ones in
# dependency order (these are exactly what passed in the verified run). Always
# run these: a cached node_modules directory may still lack dist/.
(cd "${work}" && npm run build --workspace=@getpaseo/protocol)
(cd "${work}" && npm run build --workspace=@getpaseo/relay)
(cd "${work}" && npm run build --workspace=@getpaseo/client)
(cd "${work}" && npm run build --workspace=@getpaseo/highlight)
(cd "${work}" && npm run build --workspace=@getpaseo/plugin)

(cd "${repo_root}" && cargo build --release --locked)
PASEO_RELAY_HOST=127.0.0.1 PASEO_RELAY_PORT="${port}" PASEO_RELAY_LOG=warn \
  "${repo_root}/target/release/paseo-relay" >/tmp/paseo-relay-compat.log 2>&1 &
relay_pid=$!
cleanup() { kill "${relay_pid}" >/dev/null 2>&1 || true; wait "${relay_pid}" 2>/dev/null || true; }
trap cleanup EXIT

for _ in $(seq 1 100); do
  if curl --fail --silent "http://127.0.0.1:${port}/health" >/dev/null; then break; fi
  sleep 0.1
done
curl --fail --silent "http://127.0.0.1:${port}/ready" >/dev/null

cd "${work}"
PASEO_RELAY_E2E_PORT="${port}" FORCE_RELAY_E2E=1 \
  npx vitest run packages/server/src/server/daemon-e2e/relay-transport.e2e.test.ts --maxWorkers=1
