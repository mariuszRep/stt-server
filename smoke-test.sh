#!/usr/bin/env bash
# Smoke test for stt-server
# Starts the server, hits health endpoint, confirms 200

set -euo pipefail

PORT="${PORT:-8080}"
HOST="${HOST:-127.0.0.1}"
SERVER_BIN="${1:-target/release/stt-server}"

if [ ! -f "$SERVER_BIN" ]; then
    echo "ERROR: Server binary not found at $SERVER_BIN"
    echo "Build with: cargo build --release"
    exit 1
fi

echo "Starting stt-server on $HOST:$PORT..."
$SERVER_BIN --host "$HOST" --port "$PORT" &
SERVER_PID=$!

# Wait for server to start
sleep 2

# Hit health endpoint
echo "Testing /v1/health..."
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "http://$HOST:$PORT/v1/health" || echo "000")

if [ "$HTTP_CODE" = "200" ]; then
    echo "PASS: /v1/health returned 200"
    HEALTH_BODY=$(curl -s "http://$HOST:$PORT/v1/health")
    echo "Response: $HEALTH_BODY"
else
    echo "FAIL: /v1/health returned $HTTP_CODE (expected 200)"
    kill $SERVER_PID 2>/dev/null || true
    exit 1
fi

# Hit readiness endpoint
echo "Testing /v1/readiness..."
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "http://$HOST:$PORT/v1/readiness" || echo "000")

if [ "$HTTP_CODE" = "200" ]; then
    echo "PASS: /v1/readiness returned 200"
    READINESS_BODY=$(curl -s "http://$HOST:$PORT/v1/readiness")
    echo "Response: $READINESS_BODY"
else
    echo "FAIL: /v1/readiness returned $HTTP_CODE (expected 200)"
    kill $SERVER_PID 2>/dev/null || true
    exit 1
fi

# Hit models endpoint
echo "Testing /v1/models..."
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "http://$HOST:$PORT/v1/models" || echo "000")

if [ "$HTTP_CODE" = "200" ]; then
    echo "PASS: /v1/models returned 200"
else
    echo "FAIL: /v1/models returned $HTTP_CODE (expected 200)"
    kill $SERVER_PID 2>/dev/null || true
    exit 1
fi

# Test non-loopback rejection (if we can bind to 0.0.0.0)
echo ""
echo "All smoke tests passed!"

# Cleanup
kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true

echo "Server stopped."
exit 0
