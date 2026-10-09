FROM rust:alpine3.24 AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY apps ./apps
COPY crates ./crates
COPY src ./src
ARG BUILD_COMMIT
RUN test -n "$BUILD_COMMIT" || (echo "BUILD_COMMIT build argument is required" >&2; exit 1)
RUN BUILD_COMMIT="$BUILD_COMMIT" cargo build --locked --release -p journey-site

FROM alpine:3.24

RUN apk add --no-cache tzdata
COPY --from=build /src/target/release/journey-site /usr/local/bin/journey-site

USER 65532:65532

CMD ["/usr/local/bin/journey-site", "serve", "--config", "/app/config.toml"]
