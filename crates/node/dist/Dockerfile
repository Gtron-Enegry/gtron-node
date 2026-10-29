# syntax=docker/dockerfile:1
# gtron-node alone, for shops that run it in Docker. Build from the repository root:
#   docker build -f crates/node/dist/Dockerfile -t gtron-node .
# Prebuilt images: ghcr.io/gtron-enegry/gtron-node (dist/Dockerfile.release).

FROM rust:1.99.1-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 AS builder
# Build with the image's own toolchain (see the main Dockerfile).
ENV RUSTUP_TOOLCHAIN=1.99.1
WORKDIR /app
COPY . .
RUN cargo build --release --locked -p gtron-node --bin gtron-node \
    && cp target/release/gtron-node /gtron-node

FROM debian:bookworm-20261020-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /var/lib/gtron-node --shell /usr/sbin/nologin gtron-node \
    # A fresh named volume copies this ownership, so the non-root agent can write to it.
    && install -d -o gtron-node -g gtron-node -m 0700 /var/lib/gtron-node
COPY --from=builder /gtron-node /usr/local/bin/gtron-node
USER gtron-node
WORKDIR /var/lib/gtron-node
ENV NODE_STATE_DIR=/var/lib/gtron-node \
    NODE_PRIVATE_KEY_FILE=/run/secrets/wallet_key
VOLUME /var/lib/gtron-node
# `docker stop` sends SIGTERM: the job in hand finishes first. Give it time:
#   docker stop -t 120 gtron-node
STOPSIGNAL SIGTERM
ENTRYPOINT ["gtron-node"]
