# stt-server

A local, self-hosted, vendor-neutral control plane for speech-to-text provider runtimes.

`stt-server` never transcribes audio and never sees transcription traffic. It detects
local hardware capability, installs/starts/stops curated provider runtimes (faster-whisper
today; whisper.cpp and others are typed seams), and hands back a versioned
**runtime connection descriptor** that a client uses with [`@voice-typer/stt-sdk`](../stt-sdk)
to talk to the runtime directly. See the root [`VISION.md`](../VISION.md) and this repo's own
[`VISION.md`](VISION.md)/[`CONVENTIONS.md`](CONVENTIONS.md) for the full architectural boundary.

## Architecture

```
stt-server/
├── crates/
│   ├── common/    # Shared types: RuntimeConnectionDescriptor, config, errors
│   ├── runtime/    # Hardware detection, provider catalog, process supervision,
│   │                 idle auto-shutdown, model selection, faster-whisper install/launch
│   ├── server/    # HTTP control-plane API (axum) — no transcription routes
│   └── cli/       # `stt` binary: daemon + CLI subcommands
├── runtimes/
│   └── faster-whisper/  # Vendored copy of the managed runtime's Python source
└── docker/        # Container packaging
```

The managed faster-whisper runtime is a separate OS process this control plane spawns,
health-checks, and supervises — it is never linked into `stt-server` itself. Its own wire
contract (`GET /health`, `GET /v1/config`, `POST /v1/audio/transcriptions`,
`WS /v1/audio/stream`) is untouched by this repo; `stt-server` only starts it and reports
back where it's listening.

## Quick Start

```bash
# Build
cargo build --release

# Run the control plane (also auto-registers the vendored faster-whisper
# runtime if it's found next to the binary or at ./runtimes/faster-whisper)
./target/release/stt run --port 8080

# Health check
curl http://127.0.0.1:8080/v1/health

# In another shell: start the managed runtime and get its connection descriptor
./target/release/stt provider start faster-whisper
```

## API

All routes are served by `stt-server` itself (the control plane), never the managed
runtime. Provider ids are validated strings (no path traversal).

| Method | Path | Description |
|---|---|---|
| GET | `/v1/health` | Control-plane health |
| GET | `/v1/readiness` | Control-plane readiness |
| GET | `/v1/hardware` | Detected hardware capability |
| GET | `/v1/providers` | Curated provider catalog + hardware compatibility |
| POST | `/v1/providers/:id/install` | Confirm/register a provider's artifact |
| POST | `/v1/providers/:id/update` | Alias of `install` (no versioned releases yet) |
| DELETE | `/v1/providers/:id` | Uninstall (stops it first if running) |
| POST | `/v1/providers/:id/start` | Start, block until healthy, return the descriptor |
| POST | `/v1/providers/:id/stop` | Stop |
| GET | `/v1/providers/:id/status` | Current `RuntimeStatus` |
| GET | `/v1/providers/:id/logs?tail=N` | Recent captured stdout/stderr lines |
| GET | `/v1/providers/:id/descriptor` | Re-fetch the descriptor without restarting |
| POST | `/v1/providers/:id/heartbeat` | Reset the idle-shutdown clock for a long session |
| GET | `/v1/models` | Curated model catalog across all providers |
| POST | `/v1/models/select` | `{providerId, modelId}` — picked up on next `start` |
| GET | `/v1/models/selected?provider=:id` | Currently selected model |
| GET | `/v1/recommendations` | Hardware-driven provider/model/device suggestion |

`POST /v1/models/:id/pull`, `/verify`, and `DELETE /v1/models/:id` exist for API
completeness but return an explicit "automatic" response for faster-whisper — that runtime
downloads/caches its own models via HuggingFace on first use, so there is no separate file
for the control plane to manage.

### Runtime connection descriptor

What `start`/`descriptor` return, and what `@voice-typer/stt-sdk`'s `createProvider()`
consumes:

```json
{
  "schemaVersion": 1,
  "provider": "faster-whisper",
  "protocol": "voice-typer-v1",
  "transport": "http",
  "baseUrl": "http://127.0.0.1:51234",
  "streaming": { "enabled": true, "endpoint": "/v1/audio/stream", "protocolVersion": 1, "..." : "..." },
  "auth": { "type": "token", "value": "..." }
}
```

### Idle auto-shutdown

A managed runtime nobody is using doesn't stay resident: `stt run --idle-timeout-secs N`
(default 600, `0` disables it) stops a provider automatically once `N` seconds pass with no
`start`/`status`/`descriptor`/`heartbeat` call against it.

## CLI

```bash
stt run [--port 8080] [--idle-timeout-secs 600]

stt hardware
stt recommend

stt provider list
stt provider install faster-whisper
stt provider start faster-whisper
stt provider status faster-whisper
stt provider logs faster-whisper --tail 50
stt provider stop faster-whisper

stt model list
stt model select --provider faster-whisper --model Systran/faster-whisper-tiny
stt model selected --provider faster-whisper

stt descriptor faster-whisper
```

`hardware`, `recommend`, and `provider|model list` run entirely in-process (no daemon
needed). Every other subcommand talks over HTTP to an already-running `stt run` (`--server-url`,
default `http://127.0.0.1:8080`) — that daemon process is the only place a provider's running
state actually lives.

## Development

The managed faster-whisper runtime's Python source is vendored at `runtimes/faster-whisper/`.
To run it locally:

```bash
cd runtimes/faster-whisper
python3 -m venv venv
./venv/bin/pip install -r requirements.txt
```

With that venv's `bin`/`Scripts` directory on `PATH` (or activated), `stt run`/`stt provider
install` will find and use it automatically.

## License

MIT
