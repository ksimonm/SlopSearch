FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release && cp target/release/slopensearch /slopensearch

FROM debian:bookworm-slim
COPY --from=build /slopensearch /usr/local/bin/slopensearch
ENV HOST=0.0.0.0 PORT=9200
EXPOSE 9200
USER nobody
ENTRYPOINT ["slopensearch"]
