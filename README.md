# STT Server

Local-first speech-to-text transcription server using [faster-whisper](https://github.com/SYSTRAN/faster-whisper). This is the standalone local-model runtime component of [Voice Typer](https://github.com/mariuszRep/whisper-vibes), sideloaded by the desktop app as a PyInstaller sidecar. It owns model execution and the public transcription API contract; the app owns UI, capture, and display.

## Quick install

Windows (PowerShell):
```powershell
irm https://raw.githubusercontent.com/mariuszRep/stt-server/main/install.ps1 | iex
```

Linux:
```bash
curl -sSL https://raw.githubusercontent.com/mariuszRep/stt-server/main/install.sh | bash
```

Both download only the CPU-only binary and hand off to `install` (see CLI below) — nothing GPU-related is fetched until `install` itself detects a CUDA-capable GPU and pulls the runtime it needs (see CUDA Behavior). Set `STT_SERVER_VERSION` to pin a specific release instead of the latest.

## Setup (from source)

```bash
python -m venv venv
source venv/bin/activate  # venv\Scripts\activate on Windows
pip install -r requirements.txt
```

## Run

```bash
./run.sh
```

Or directly:

```bash
uvicorn app.main:app --host 127.0.0.1 --port 8000
```

Consumers (e.g. the desktop app) should check `GET /health` before starting a new process, and reuse an already-running compatible server on the configured host/port instead of double-starting.

## CLI

The packaged binary (or `python run_sidecar.py`) is a CLI with these subcommands:

| Command | Does |
|---|---|
| `serve` (or no subcommand) | Run in the foreground. This is the original, argument-free behavior consumers like Tauri's sidecar spawn rely on — unaffected by everything below. |
| `install` | Copy the binary to a stable per-user location, auto-detect and swap in the GPU-accelerated build if applicable (see CUDA Behavior below), register it to start at login (Windows: Scheduled Task; Linux: systemd user unit), and start it immediately. |
| `uninstall` | Stop the server, unregister auto-start, and remove the installed binary/logs/pid file. |
| `start` | Start the server in the background (detached, PID-tracked, logs to a file). |
| `stop` | Stop the running server — tries a clean shutdown via `POST /v1/admin/stop` first, falls back to a direct kill. |
| `status` | Report whether the server is running and responding to `/health`. |
| `logs [-n N]` | Print the last `N` lines (default 200; `0` = all) of the server's log file. |
| `detect` | Print GPU/CPU/RAM facts as one JSON line and exit — does **not** start the server. GPU fields (`cuda_available`, `cuda_runtime_ok`, `cuda_error`, `cuda_supported_compute_types`) are the same signals `GET /v1/config` reports, just available before any server is running (useful for a caller — e.g. a desktop app's onboarding — deciding what to start with, before starting anything). Also reports `cpu_count` and `total_ram_mb` for model-size recommendations. |

`install`'s startup registration is best-effort: if Task Scheduler/systemd-user-session access is unavailable (locked-down environments, some CI/sandboxes), it logs that and still starts the server for the current session — only auto-start-at-login is affected.

## Configuration

All settings are environment variables:

| Variable | Default | Description |
|---|---|---|
| `VOICE_TYPER_HOST` | `127.0.0.1` | Bind address |
| `VOICE_TYPER_PORT` | `8000` | Listen port |
| `VOICE_TYPER_MODEL` | `Systran/faster-whisper-small` | Whisper model ID |
| `VOICE_TYPER_DEVICE` | `auto` | Device selection. Auto uses CUDA when CTranslate2 detects a CUDA device, otherwise CPU. Set `cpu` or `cuda` to force a device. |
| `VOICE_TYPER_COMPUTE_TYPE` | `auto` | faster-whisper compute type. Auto uses `float16` on CUDA and `int8` on CPU. |
| `VOICE_TYPER_LANGUAGE` | _(auto)_ | Language code or auto-detect |

## CUDA Behavior

CUDA is optional. When `VOICE_TYPER_DEVICE` is unset, the backend asks CTranslate2 whether CUDA is available. If CUDA is detected, Voice Typer starts with `device=cuda` and `compute_type=float16`; otherwise it starts with `device=cpu` and `compute_type=int8`.

If CUDA is requested but the runtime is incomplete or inference fails, the backend falls back to CPU and exposes the error through `GET /v1/config`. On Windows, CTranslate2 CUDA support requires the CUDA runtime DLLs to be loadable by the backend process, including `cublas64_12.dll`.

If `cublas64_12.dll` is installed but not on `PATH`, set `VOICE_TYPER_CUDA_DLL_DIR` to the directory containing the CUDA DLLs, for example a CUDA Toolkit `bin` directory. The packaged backend also searches common Python NVIDIA package DLL folders and CUDA Toolkit v12 install folders automatically.

**CUDA runtime auto-pull:** `stt-server install` checks the same signal `GET /v1/config` reports (`cuda_available` + `cuda_runtime_ok`). If a CUDA-capable GPU is present but the installed binary can't yet load the CUDA runtime, it downloads a small `stt-server-<platform>-cuda-runtime.zip` (just the cuBLAS/cuDNN/cuda_runtime DLLs — not a second copy of the binary) and extracts it flat into the install directory, right next to the binary, which is already one of the directories searched above — no separate binary swap, no env var wiring needed. Fetched via `curl`/`tar` (both ship built into Windows since the 2018 update). Override the download URL with `STT_SERVER_GPU_ASSET_URL` (useful for local testing against a locally-served file). Windows-only today — no Linux GPU build is published yet.

## API Contract

### `GET /health`

Returns server status and configured model.

```json
{ "status": "ok", "model": "Systran/faster-whisper-small" }
```

### `POST /v1/audio/transcriptions`

OpenAI-compatible transcription endpoint.

**Request:** multipart form data with field `file` containing an audio file (webm, wav, mp3, etc.). Optional `Authorization: Bearer <token>` header (accepted but not validated — for OpenDora compatibility).

**Response:**

```json
{ "text": "transcribed text here" }
```

### `GET /v1/config`

Returns runtime configuration and diagnostics, including requested device, active device, compute type, CUDA availability, CUDA runtime status, CUDA-supported compute types, model load state, and recent timing metrics.

### `WS /v1/audio/stream`

Realtime streaming transcription with partial and final transcript events, backed by a LocalAgreement2 adapter over faster-whisper. JSON control messages and events in text frames; raw PCM in binary frames. The full wire protocol and the Live Transcription Provider Standard it implements are specified in the app repo: [`protocol.md`](https://github.com/mariuszRep/whisper-vibes/blob/main/protocol.md) and [`CONVENTIONS.md`](https://github.com/mariuszRep/whisper-vibes/blob/main/CONVENTIONS.md).

`GET /v1/config` reports `schema_version: 4` and includes a `streaming` block describing the endpoint, accepted encodings/sample rates, and channel count. Clients must treat a missing `streaming` block (`schema_version < 4`) as "streaming unavailable" and fall back to batch transcription.

### `POST /v1/admin/restart`

Restarts the standalone backend process with optional overrides:

```json
{
  "host": "127.0.0.1",
  "port": 8000,
  "model": "Systran/faster-whisper-small",
  "device": "cuda",
  "compute_type": "float16"
}
```

Omit `device` and `compute_type` to keep automatic CUDA/CPU selection. In desktop mode, Tauri manages the sidecar process directly and forwards the same settings as environment variables.

### `POST /v1/admin/stop`

Stops the standalone backend process. Starting the backend is handled by the process manager: shell command, standalone binary, dev script, or Tauri sidecar.

## OpenDora Compatibility

Voice Typer is designed as a drop-in replacement for the local Whisper provider in OpenDora.

In OpenDora: **Settings → Voice → Speech-to-Text → Local Whisper Server URL** → set to `http://127.0.0.1:8000`.

OpenDora appends `/v1/audio/transcriptions` to this URL and sends multipart audio with field name `file` (filename `recording.webm`, format `audio/webm;codecs=opus`). The response must be JSON containing `text`.

No OpenDora changes are required — the contract matches exactly.

## Building the sidecar binary

```bash
python -m PyInstaller --clean --distpath dist voice-typer-backend.spec
```

Set `VOICE_TYPER_BUILD_VARIANT=gpu` before building to bundle NVIDIA cuBLAS/cuDNN/cuda_runtime DLLs directly into the binary instead (larger binary, CUDA works out of the box, useful for manual/offline builds). Default (`cpu`, or unset) omits them for a smaller CPU-only build. CI (`.github/workflows/build.yml`) only ever builds the `cpu` variant — the CUDA runtime is published separately as `stt-server-windows-cuda-runtime.zip` and pulled on demand by `install` instead (see CUDA Behavior above).
