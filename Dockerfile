# syntax=docker/dockerfile:1
#
# CCTV Bandar Lampung — unified dashboard image.
#
# Two stages: a Rust builder that compiles the API and the data pipeline, and a
# slim runtime that carries only the two binaries, the CA bundle, and the static
# client. The dataset is copied in as well, so the image runs standalone.

# ------------------------------------------------------------------ builder
FROM rust:1.99-slim-bookworm AS builder

WORKDIR /build

# rustls means no OpenSSL, but `ring` still compiles C, and Debian's `gcc`
# does not pull in `libc6-dev` under --no-install-recommends.
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/*

# Layer 1 — the dependency graph. Manifests plus stub sources only, so editing
# a real source file below reuses this layer instead of rebuilding every crate.
COPY Cargo.toml Cargo.lock ./
COPY crates/web-core/Cargo.toml crates/web-core/
COPY crates/api/Cargo.toml      crates/api/
COPY crates/cli/Cargo.toml      crates/cli/
RUN set -eux; \
    mkdir -p crates/web-core/src crates/api/src crates/cli/src; \
    echo '' > crates/web-core/src/lib.rs; \
    echo 'fn main() {}' > crates/api/src/main.rs; \
    echo 'fn main() {}' > crates/cli/src/main.rs; \
    cargo build --release --locked --bin cctv-api --bin cctv-data; \
    rm -rf crates/*/src

# Layer 2 — the real sources.
COPY crates ./crates

# Cargo fingerprints a local crate by comparing each source's mtime against its
# artifact's, and COPY restores the *context's* mtimes — which are older than
# the stub artifacts just built above. Cargo therefore calls the real sources
# fresh, the build below is a no-op, and the image ships the `fn main() {}`
# stubs. Bumping every source past the artifacts forces the rebuild; touching in
# a layer of its own keeps the cache intact when `crates/` is unchanged.
RUN find crates -name '*.rs' -exec touch {} +

RUN cargo build --release --locked --bin cctv-api --bin cctv-data

# ------------------------------------------------------------------ runtime
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="CCTV Bandar Lampung" \
      org.opencontainers.image.description="Unified CCTV dashboard — Seribu Wajah + Polresta Bandar Lampung" \
      org.opencontainers.image.licenses="MIT"

# curl exists solely to give HEALTHCHECK a real HTTP probe: a bare TCP connect
# would report the container healthy while /api/health was still 500ing.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --create-home --shell /usr/sbin/nologin cctv

WORKDIR /app

COPY --from=builder /build/target/release/cctv-api  /usr/local/bin/cctv-api
COPY --from=builder /build/target/release/cctv-data /usr/local/bin/cctv-data

# The client is plain HTML/CSS/JS plus the generated dataset — there is no
# asset build step, so the tree is copied verbatim. Owned by `cctv` so the
# pipeline can rewrite it in place when invoked inside this image.
COPY --chown=cctv:cctv static ./static

USER cctv

# 0.0.0.0 because the loopback default is unreachable from outside the container.
ENV CCTV_BIND=0.0.0.0:3447 \
    RUST_LOG=info,tower_http=warn

EXPOSE 3447

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:3447/api/health || exit 1

# Spawned directly rather than through a shell or an init shim: the server
# installs its own SIGTERM/SIGINT handlers, which need it to be PID 1 to
# receive them.
ENTRYPOINT ["/usr/local/bin/cctv-api"]
CMD ["--data-dir", "/app/static/data", "--static-dir", "/app/static"]
