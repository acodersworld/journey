FROM rust:1.85-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY apps ./apps
COPY crates ./crates
COPY src ./src
RUN cargo build --locked --release -p journey-gateway -p journey-home

FROM debian:bookworm-slim

COPY --from=build /src/target/release/journey-gateway /usr/local/bin/gateway
COPY --from=build /src/target/release/journey-home /usr/local/bin/home

USER 65532:65532
EXPOSE 8080 9000

CMD ["/usr/local/bin/gateway"]
