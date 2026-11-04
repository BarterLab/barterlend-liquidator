# syntax=docker/dockerfile:1
FROM rust:1.98-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git pkg-config libssl-dev python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY scripts/check-release.py ./scripts/check-release.py
RUN python3 scripts/check-release.py
COPY src ./src
COPY abi ./abi
RUN cargo +1.98.0 build --release --locked --bin barterlend-liquidator

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/barterlend-liquidator /usr/local/bin/barterlend-liquidator
USER 10001:10001
WORKDIR /app
ENV LOG_FILTER=info
EXPOSE 9090
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/barterlend-liquidator"]
