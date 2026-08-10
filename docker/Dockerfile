# ── Build stage ───────────────────────────────────────────────
FROM rust:1.76-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    g++ \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/common/Cargo.toml crates/common/Cargo.toml
COPY crates/adapter/Cargo.toml crates/adapter/Cargo.toml
COPY crates/server/Cargo.toml crates/server/Cargo.toml
COPY crates/cli/Cargo.toml crates/cli/Cargo.toml

# Create dummy source files for dependency caching
RUN mkdir -p crates/common/src crates/adapter/src crates/server/src crates/cli/src && \
    echo "pub fn dummy() {}" > crates/common/src/lib.rs && \
    echo "pub fn dummy() {}" > crates/adapter/src/lib.rs && \
    echo "fn main() {}" > crates/server/src/main.rs && \
    echo "fn main() {}" > crates/cli/src/main.rs

# Build dependencies (cached)
RUN cargo build --release 2>/dev/null || true

# Copy actual source
COPY . .

# Remove dummy files and build
RUN touch crates/common/src/lib.rs crates/adapter/src/lib.rs crates/server/src/main.rs crates/cli/src/main.rs && \
    cargo build --release --bin stt-server && \
    cargo build --release --bin stt

# ── Runtime stage ─────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Copy binaries
COPY --from=builder /app/target/release/stt-server /usr/local/bin/stt-server
COPY --from=builder /app/target/release/stt /usr/local/bin/stt

# Copy smoke test
COPY smoke-test.sh /usr/local/bin/smoke-test.sh
RUN chmod +x /usr/local/bin/smoke-test.sh

# Create model directory
RUN mkdir -p /var/lib/stt-server/models

EXPOSE 8080

ENTRYPOINT ["stt-server"]
CMD ["--host", "0.0.0.0", "--port", "8080"]
