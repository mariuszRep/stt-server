#!/usr/bin/env bash
# Smoke test for stt-server's control-plane API.
# Starts `stt run`, hits the control-plane endpoints, confirms 200s.
# This only exercises the control plane itself — it never starts a managed
# runtime or touches transcription traffic, matching the "server never
# carries audio" boundary the routes are supposed to enforce.

set -euo pipefail

PORT="${PORT:-8080}"
HOST="${HOST:-127.0.0.1}"
STT_BIN="${1:-target/release/stt}"

if [ ! -f "$STT_BIN" ]; then
    echo "ERROR: stt binary not found at $STT_BIN"
    echo "Build with: cargo build --release"
    exit 1
fi

echo "Starting stt-server on $HOST:$PORT..."
"$STT_BIN" run --host "$HOST" --port "$PORT" &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true; wait $SERVER_PID 2>/dev/null || true' EXIT

# Wait for server to start
sleep 2

check() {
    local path="$1"
    local http_code
    http_code=$(curl -s -o /dev/null -w "%{http_code}" "http://$HOST:$PORT$path" || echo "000")
    if [ "$http_code" = "200" ]; then
        echo "PASS: $path returned 200"
    else
        echo "FAIL: $path returned $http_code (expected 200)"
        exit 1
    fi
}

echo "Testing /v1/health..."
check "/v1/health"

echo "Testing /v1/readiness..."
check "/v1/readiness"

echo "Testing /v1/hardware..."
check "/v1/hardware"

echo "Testing /v1/providers..."
check "/v1/providers"

echo "Testing /v1/models..."
check "/v1/models"

echo "Testing /v1/recommendations..."
check "/v1/recommendations"

echo ""
echo "All smoke tests passed!"
