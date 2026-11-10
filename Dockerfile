FROM rust:1.90-slim AS builder
WORKDIR /app

# Compile the dependencies in a layer of their own. It depends only on
# Cargo.toml and Cargo.lock, so a change under src/ reuses it instead of
# recompiling every crate. The stub binary exists only to give cargo a target;
# cleaning the package afterwards makes the real build below recompile it
# whatever the file timestamps say.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && cargo clean --release -p hydra-server

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
