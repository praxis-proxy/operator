# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Stage 1: Build
# ---------------------------------------------------------------------------

FROM rust:1.94-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /src

# ---------------------------------------------------------------------------
# Cache Build
# ---------------------------------------------------------------------------

# Cache dependency builds: copy only manifests first, then
# create a stub source file so `cargo build` resolves and
# compiles all dependencies without the real source code.
# See: https://shaneutt.com/blog/rust-fast-small-docker-image-builds/

COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && printf '//! stub\nfn main() {}\n' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# ---------------------------------------------------------------------------
# Cache Tricks
# ---------------------------------------------------------------------------

# Replace the stub with real source, then rebuild. Only the
# project crate recompiles; all dependencies are cached.

COPY src src
RUN touch src/main.rs \
    && cargo build --release \
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
