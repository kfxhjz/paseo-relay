# syntax=docker/dockerfile:1
#
# Paseo Relay image: a single static musl binary in a shell-free scratch image.
#
# TLS is terminated at the edge (recommended, e.g. a load balancer or ingress
# controller) or configured natively with PASEO_RELAY_TLS_CERT /
# PASEO_RELAY_TLS_KEY mounted as PEM files. No Erlang, EPMD, cookies, secrets,
# or package manager exist in the runtime image.
#
# Build: docker build -t paseo-relay .
# Run:   docker run --rm -p 127.0.0.1:4000:4000 paseo-relay

FROM rust:1.99.1-alpine@sha256:44819b27be4c97b0e4d7203bd2e02c4820f97700211b6a37fb4c246a2de59383 AS builder

# The Rust target is musl, so the Alpine host toolchain is enough. build-base
# supplies the C compiler and headers used by ring/socket2; ca-certificates are
# needed to fetch crates over HTTPS.
RUN apk add --no-cache build-base ca-certificates

# The Docker build context does not include .git; the release workflow passes the
# commit SHA so `/version` and paseo_relay_build_info can identify the build.
ARG PASEO_RELAY_BUILD_REVISION=unknown
ENV PASEO_RELAY_BUILD_REVISION=${PASEO_RELAY_BUILD_REVISION}

WORKDIR /app

# TARGETARCH is populated by BuildKit (amd64/arm64); fall back to uname so a
# plain `docker build` without buildx metadata still works.
ARG TARGETARCH

# Dependency-cache layer: compile with placeholder sources so a source-only
# change does not invalidate the crates.io build cache.
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
RUN set -eux; \
    case "${TARGETARCH:-$(uname -m)}" in \
      amd64 | x86_64) target=x86_64-unknown-linux-musl ;; \
      arm64 | aarch64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture: ${TARGETARCH:-$(uname -m)}" >&2; exit 1 ;; \
    esac; \
    rustup target add "${target}"; \
    mkdir -p src; \
    printf 'fn main() {}\n' > src/main.rs; \
    printf '\n' > src/lib.rs; \
    cargo build --release --locked --target "${target}"; \
    rm -rf src

COPY src ./src
# The dependency-cache layer compiled a placeholder crate with the same package
# name/version. Docker COPY preserves source mtimes, which can otherwise make
# Cargo believe the real sources are older than the placeholder artifact. Touch
# the entry files so the real binary is always rebuilt.
RUN touch src/main.rs src/lib.rs

RUN set -eux; \
    case "${TARGETARCH:-$(uname -m)}" in \
      amd64 | x86_64) target=x86_64-unknown-linux-musl ;; \
      arm64 | aarch64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture: ${TARGETARCH:-$(uname -m)}" >&2; exit 1 ;; \
    esac; \
    cargo build --release --locked --target "${target}"; \
    cp "target/${target}/release/paseo-relay" /paseo-relay; \
    chmod 0555 /paseo-relay

# Runtime stage. `scratch` has no shell, package manager, or curl. The binary
# implements a `healthcheck` subcommand that performs the probe itself (including
# native TLS), so the image can use a normal Docker/Kubernetes healthcheck.
FROM scratch

LABEL org.opencontainers.image.title="paseo-relay" \
      org.opencontainers.image.description="Single-node, protocol-compatible WebSocket relay for Paseo" \
      org.opencontainers.image.licenses="Apache-2.0"

COPY --from=builder /paseo-relay /paseo-relay

# Unprivileged, deterministic UID/GID (the conventional distroless nonroot ID).
# 65532 is numeric so it works in scratch without /etc/passwd.
USER 65532:65532

# The relay listens on one HTTP/WebSocket port; /health, /ready, and /metrics
# share it and should be kept on an internal network.
EXPOSE 4000

# Bind to all interfaces inside the container; the documented process default
# (127.0.0.1) would make the container unreachable. Operators can still
# override these at run time.
ENV PASEO_RELAY_HOST=0.0.0.0 \
    PASEO_RELAY_PORT=4000

# The binary is its own healthcheck client, so this works in a scratch image
# with no shell, curl, or wget. It probes /health over loopback.
HEALTHCHECK --interval=30s --timeout=5s --start-period=3s --retries=3 \
    CMD ["/paseo-relay", "healthcheck"]

# SIGTERM starts the graceful drain (ready 503, stop accepting, grace window,
# then close with 1012). Never send SIGKILL as the first stop signal.
STOPSIGNAL SIGTERM

ENTRYPOINT ["/paseo-relay"]
