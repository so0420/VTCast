FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN cargo build --release -p vtcast-relay --bin vtcast-relay

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -u 1000 -ms /bin/bash vtcast

WORKDIR /app

COPY --from=builder /build/target/release/vtcast-relay /app/vtcast-relay

RUN chmod +x /app/vtcast-relay && chown vtcast:vtcast /app/vtcast-relay

USER vtcast

# Change ports as desired

# HTTP/WS signalling port
EXPOSE 17239
# TURN/UDP port - UDP explicitly
EXPOSE 3478/udp

# Pass in ENV VARS or .env for configuration

ENTRYPOINT ["/app/vtcast-relay"]
