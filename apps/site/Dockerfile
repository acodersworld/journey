FROM rust:alpine3.24 AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY apps ./apps
COPY crates ./crates
COPY src ./src
RUN cargo build --locked --release -p journey-site

FROM alpine:3.24

RUN apk add --no-cache tzdata
COPY --from=build /src/target/release/journey-site /usr/local/bin/journey-site

USER 65532:65532

CMD ["/usr/local/bin/journey-site", "serve", "--config", "/app/config.toml"]

