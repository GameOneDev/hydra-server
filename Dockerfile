FROM rust:1.90-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY static ./static
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/hydra-server /usr/local/bin/hydra-server
ENV HYDRA_SERVER_DATA_DIR=/data
VOLUME /data
EXPOSE 8788
CMD ["hydra-server"]
