FROM rust:1.85-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release --bins

FROM debian:bookworm-slim

COPY --from=build /src/target/release/gateway /usr/local/bin/gateway
COPY --from=build /src/target/release/home /usr/local/bin/home

USER 65532:65532
EXPOSE 8080 9000

CMD ["/usr/local/bin/gateway"]
