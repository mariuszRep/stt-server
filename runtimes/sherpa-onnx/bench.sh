#!/usr/bin/env bash
# Throwaway benchmark harness for validate-parakeet-performance.
# Not production code -- provider-conformance-test-suite owns the durable version.
#
# Usage: ./bench.sh <wav-dir>
#   Runs every *.wav in <wav-dir> through every model below and prints a
#   latency / real-time-factor table. Assumes:
#     - sherpad is running on 127.0.0.1:7891 with every SHERPAD_MODEL already
#       pulled AND loaded (POST /v1/models/<id>/load) so timings exclude load.
#     - faster-whisper's managed runtime is running and FW_BASE_URL points at it
#       (get it from `stt provider start faster-whisper` / descriptor.baseUrl).
#
# Excludes model load time from every measurement by design (see goal's
# success_criteria). Reports wall-clock request latency, which includes HTTP
# + multipart overhead + WAV decode + inference -- not pure inference time --
# since that's what a real caller experiences.

set -euo pipefail

WAV_DIR="${1:?usage: ./bench.sh <wav-dir>}"
SHERPAD_URL="${SHERPAD_URL:-http://127.0.0.1:7891}"
FW_BASE_URL="${FW_BASE_URL:-}"

SHERPAD_MODELS=("parakeet-tdt-0.6b-v2" "sense-voice-multi")

wav_duration_secs() {
  python -c "import wave,sys; w=wave.open(sys.argv[1]); print(w.getnframes()/w.getframerate())" "$1"
}

# Times a transcription request via curl's own %{time_total} (avoids any
# external timing/race issues) and prints one report row. $1=clip $2=label
# $3=dur, remaining args = the curl invocation (must include -F fields).
bench_one() {
  local clip="$1" label="$2" dur="$3"
  shift 3
  local resp wall text rtf
  resp=$(mktemp)
  wall=$("$@" -s -o "$resp" -w '%{time_total}')
  text=$(python -c "import json,sys
try:
    print(json.load(open(sys.argv[1])).get('text','<error>'))
except Exception as e:
    print(f'<error: {e}>')" "$resp")
  rtf=$(python -c "print(f'{$wall/$dur:.3f}')")
  rm -f "$resp"
  printf "%-30s %-20s %8.2f %8.3f %6s  %s\n" "$clip" "$label" "$dur" "$wall" "$rtf" "$text"
}

printf "%-30s %-20s %8s %8s %6s  %s\n" "clip" "model" "dur(s)" "wall(s)" "rtf" "text"
printf '%s\n' "----------------------------------------------------------------------------------------------------"

for wav in "$WAV_DIR"/*.wav; do
  [ -f "$wav" ] || continue
  clip=$(basename "$wav")
  dur=$(wav_duration_secs "$wav")

  for model in "${SHERPAD_MODELS[@]}"; do
    bench_one "$clip" "$model" "$dur" curl -X POST "$SHERPAD_URL/v1/audio/transcriptions" \
      -F "model=$model" -F "file=@$wav"
  done

  if [ -n "$FW_BASE_URL" ]; then
    bench_one "$clip" "faster-whisper" "$dur" curl -X POST "$FW_BASE_URL/v1/audio/transcriptions" -F "file=@$wav"
  fi
done
