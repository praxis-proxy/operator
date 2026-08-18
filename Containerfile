# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Stage 1: Build
# ---------------------------------------------------------------------------

FROM rust:1.96-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /src

# ---------------------------------------------------------------------------
# Cache Build
# ---------------------------------------------------------------------------

# Cache dependency builds: copy only manifests first, then
# create a stub source file so `cargo build` resolves and
# compiles all dependencies without the real source code.
# See: https://shaneutt.com/blog/rust-fast-small-docker-image-builds/

# The manifest declares an explicit [[bench]], so cargo refuses to parse
# it unless that file exists. Stub it alongside src/ or this layer fails
# before a single dependency is compiled. The `xtask` workspace member is
# a dev-only tool never shipped in this image; `-p praxis-operator` keeps
# it out of the build target, but cargo still needs its manifest and a
# target file present to resolve the workspace, so it gets a permanent
# stub rather than being swapped for real source later.
COPY Cargo.toml Cargo.lock ./
COPY xtask/Cargo.toml xtask/Cargo.toml
RUN mkdir -p src benches xtask/src \
    && printf '//! stub\nfn main() {}\n' > src/main.rs \
    && printf 'fn main() {}\n' > benches/config_generation.rs \
    && printf 'fn main() {}\n' > xtask/src/main.rs \
    && cargo build --release --locked -p praxis-operator \
    && rm -rf src

# ---------------------------------------------------------------------------
# Cache Tricks
# ---------------------------------------------------------------------------

# Replace the stub with real source, then rebuild. Only the
# project crate recompiles; all dependencies are cached.

COPY src src
COPY benches benches
RUN touch src/main.rs \
    && cargo build --release --locked -p praxis-operator \
    && cp target/release/praxis-operator /usr/local/bin/

# ---------------------------------------------------------------------------
# Stage 2: Runtime
# ---------------------------------------------------------------------------

FROM alpine:3.23

LABEL org.opencontainers.image.source="https://github.com/praxis-proxy/praxis-operator" \
      org.opencontainers.image.description="Praxis Gateway API operator" \
      org.opencontainers.image.licenses="MIT"

RUN apk add --no-cache ca-certificates \
    && addgroup -S operator \
    && adduser -S -G operator -h /nonexistent -s /sbin/nologin operator

COPY --from=builder --chown=root:root --chmod=0555 \
    /usr/local/bin/praxis-operator /usr/local/bin/praxis-operator

USER operator:operator

ENTRYPOINT ["praxis-operator"]
