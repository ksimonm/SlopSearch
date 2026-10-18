FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release && cp target/release/slopsearch /slopsearch

FROM debian:bookworm-slim
COPY --from=build /slopsearch /usr/local/bin/slopsearch
ENV HOST=0.0.0.0 PORT=9200
EXPOSE 9200
USER nobody
ENTRYPOINT ["slopsearch"]
