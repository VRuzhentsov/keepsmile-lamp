# syntax=docker/dockerfile:1

FROM rust:1.88-bookworm AS builder

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libdbus-1-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first so dependency builds can be cached.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo test --locked
RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends libdbus-1-3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/keepsmile-lamp /usr/local/bin/keepsmile-lamp

ENTRYPOINT ["keepsmile-lamp"]
CMD ["--help"]
