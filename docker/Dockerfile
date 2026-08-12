# ── Build stage ───────────────────────────────────────────────
FROM rust:1.76-bookworm AS builder

WORKDIR /app

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/common/Cargo.toml crates/common/Cargo.toml
COPY crates/runtime/Cargo.toml crates/runtime/Cargo.toml
COPY crates/server/Cargo.toml crates/server/Cargo.toml
COPY crates/cli/Cargo.toml crates/cli/Cargo.toml

# Create dummy source files for dependency caching
RUN mkdir -p crates/common/src crates/runtime/src crates/server/src crates/cli/src && \
    echo "pub fn dummy() {}" > crates/common/src/lib.rs && \
    echo "pub fn dummy() {}" > crates/runtime/src/lib.rs && \
    echo "pub fn dummy() {}" > crates/server/src/lib.rs && \
    echo "fn main() {}" > crates/cli/src/main.rs

# Build dependencies (cached)
RUN cargo build --release 2>/dev/null || true

# Copy actual source
COPY . .

# Remove dummy files and build the single `stt` binary (daemon + CLI)
RUN touch crates/common/src/lib.rs crates/runtime/src/lib.rs crates/server/src/lib.rs crates/cli/src/main.rs && \
    cargo build --release --bin stt

# ── Runtime stage ─────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    python3 \
    python3-venv \
    && rm -rf /var/lib/apt/lists/*

# Vendored managed-runtime source + its own isolated venv, so `stt` finds a
# working Python interpreter without depending on the control plane's own
# (much lighter) dependency footprint.
COPY runtimes/faster-whisper /opt/runtimes/faster-whisper
RUN python3 -m venv /opt/runtimes/faster-whisper/venv && \
    /opt/runtimes/faster-whisper/venv/bin/pip install --no-cache-dir \
        -r /opt/runtimes/faster-whisper/requirements.txt
ENV PATH="/opt/runtimes/faster-whisper/venv/bin:${PATH}"

# Copy binary
COPY --from=builder /app/target/release/stt /usr/local/bin/stt

# Copy smoke test
COPY smoke-test.sh /usr/local/bin/smoke-test.sh
RUN chmod +x /usr/local/bin/smoke-test.sh

# Create model directory
RUN mkdir -p /var/lib/stt-server/models

EXPOSE 8080

ENTRYPOINT ["stt"]
# Loopback-only by default (CONVENTIONS.md); remote binding needs explicit
# --host/--allow-remote flags passed by the operator, not baked in here.
CMD ["run", "--host", "127.0.0.1", "--port", "8080"]
