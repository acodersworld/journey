FROM rust:1.85-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY apps ./apps
COPY crates ./crates
COPY src ./src
RUN cargo build --locked --release -p journey-site -p journey-storage-service

FROM debian:bookworm-slim

RUN mkdir -p /var/lib/journey/site /var/lib/journey/objects /run/journey-site \
    && chown -R 65532:65532 /var/lib/journey /run/journey-site

COPY --from=build /src/target/release/journey-site /usr/local/bin/journey-site
COPY --from=build /src/target/release/journey-storage-service /usr/local/bin/journey-storage-service

USER 65532:65532
EXPOSE 8080 8081 8082

CMD ["/usr/local/bin/journey-site", "serve"]
