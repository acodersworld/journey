FROM rust:1.99.0-trixie AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libclang-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY apps ./apps
COPY crates ./crates
COPY src ./src
RUN cargo build --locked --release -p journey-storage-service

FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates libavcodec61 libavformat61 libavutil59 libswscale8 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/journey-storage-service /usr/local/bin/journey-storage-service

USER 65532:65532

CMD ["/usr/local/bin/journey-storage-service", "--config", "/config.toml"]
