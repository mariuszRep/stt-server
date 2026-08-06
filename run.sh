#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

HOST="${VOICE_TYPER_HOST:-127.0.0.1}"
PORT="${VOICE_TYPER_PORT:-8000}"
MODEL="${VOICE_TYPER_MODEL:-Systran/faster-whisper-small}"

echo "Starting Voice Typer backend on http://${HOST}:${PORT}"
echo "Model: ${MODEL}  (first run downloads it)"
echo "Press Ctrl+C to stop."
echo ""

exec uvicorn app.main:app --host "$HOST" --port "$PORT" --app-dir "$SCRIPT_DIR"
