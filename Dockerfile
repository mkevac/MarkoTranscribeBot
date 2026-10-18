# Stage 1: build the Rust binary
FROM rust:1-bookworm AS builder
WORKDIR /app

# Cache dependencies separately from the source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release \
 && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

# Stage 2: runtime with ffmpeg/ffprobe
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
ENV HOME=/app DATA_DIR=/app/data
COPY --from=builder /app/target/release/markotranscribebot /app/markotranscribebot
ENTRYPOINT ["/app/markotranscribebot"]
