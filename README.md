# stt-server

A local, self-hosted, vendor-neutral control plane for speech-to-text provider runtimes.

`stt-server` never transcribes audio and never sees transcription traffic. It detects
local hardware capability, installs/starts/stops curated provider runtimes (faster-whisper
today; whisper.cpp and others are typed seams), and hands back a versioned
**runtime connection descriptor** that a client uses with [`@open-vibe-ai/stt-sdk`](../stt-sdk)
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
| GET | `/v1/providers` | Curated provider catalog + hardware compatibility, incl. per-variant `compatible`/`recommended` |
| POST | `/v1/providers/:id/install` | Install `{"variant": "cpu"\|"gpu"}` (default `"cpu"`) — instant if already local, else `202` + `operationId` |
| DELETE | `/v1/providers/:id/install/:variant` | Remove a previously-downloaded variant's cached copy (never the vendored dev copy) |
| GET | `/v1/install-operations/:operation_id` | Poll an in-flight/finished variant download (`downloading`/`complete`/`failed`) |
| POST | `/v1/providers/:id/update` | Alias of `install` (no versioned releases yet) |
| DELETE | `/v1/providers/:id` | Uninstall (stops it first if running) |
| POST | `/v1/providers/:id/start` | Start, block until healthy, return the descriptor. Body: `{device, computeType, bindHost, authToken}`, all optional |
| POST | `/v1/providers/:id/stop` | Stop |
| GET | `/v1/providers/:id/status` | Current `RuntimeStatus` |
| GET | `/v1/providers/:id/logs?tail=N` | Recent captured stdout/stderr lines |
| GET | `/v1/providers/:id/descriptor` | Re-fetch the descriptor without restarting |
| POST | `/v1/providers/:id/heartbeat` | Reset the idle-shutdown clock for a long session |
| GET | `/v1/models` | Curated model catalog across all providers |
| POST | `/v1/models/select` | `{providerId, modelId}` — picked up on next `start` |
| GET | `/v1/models/selected?provider=:id` | Currently selected model |
| POST | `/v1/models/pull?provider=:id&model=:id` | Download a model's weights into stt-server's own model directory — instant `200` if already cached, else `202` + `operationId` to poll |
| POST | `/v1/models/verify?provider=:id&model=:id` | Pure filesystem check: are this model's weights actually present on disk |
| DELETE | `/v1/models/remove?provider=:id&model=:id` | Delete a model's cached weights, reclaiming disk space |
| GET | `/v1/recommendations` | Hardware-driven provider/model/device suggestion |

Model weights are stored under `<data root>/models/<provider-id>/<model-id>/` (see
`stt_common::default_model_dir`/`default_data_root`) — an explicit, stt-server-owned
location passed to the managed runtime as `download_root`, not the OS-default HuggingFace
cache. `pull`/`verify`/`remove` are real filesystem operations, not stubs; `provider`/`model`
are query params (not `:id` path segments) because model ids like
`"Systran/faster-whisper-small"` contain their own `/`.

### GPU vs CPU variant installs

Every install targets a specific build: `"cpu"` (default — small, always compatible) or
`"gpu"` (bundles CUDA/cuDNN, much larger). Installing one never evicts the other — they're
cached side by side, so switching back and forth doesn't force a re-download. A caller that
never sends a body gets today's instant, network-free `"cpu"` behavior unchanged.

```bash
# Kicks off a background download if the gpu build isn't cached yet
curl -X POST http://127.0.0.1:8080/v1/providers/faster-whisper/install \
  -H 'Content-Type: application/json' -d '{"variant":"gpu"}'
# => 202 {"status":"downloading","providerId":"faster-whisper","variant":"gpu","operationId":"..."}

curl http://127.0.0.1:8080/v1/install-operations/<operationId>
# => {"status":"downloading","downloadedBytes":...,"totalBytes":...} until "complete"/"failed"
```

### LAN mode (non-loopback binding)

Loopback is the default and requires nothing extra. Binding a managed runtime to the network
needs two explicit opt-ins — one at the control plane's own launch, one per start request —
so a caller of the (still loopback-only) admin API can't unilaterally expose anything the
operator didn't sanction:

1. Launch the daemon itself with `stt run --allow-remote` (the control plane's own HTTP API
   still binds loopback — this flag only sanctions the *managed runtime* binding non-loopback
   later).
2. Request it per-start: `POST /v1/providers/:id/start {"bindHost": "0.0.0.0", "authToken": "..."}`
   — `authToken` is required whenever `bindHost` isn't loopback. The returned descriptor's
   `baseUrl` always stays `127.0.0.1` (that's what the *local* caller — e.g. a desktop app on
   the same machine — uses); the LAN-reachable address is a separate fact for a separate
   audience that this control plane deliberately doesn't try to guess.

### Runtime connection descriptor

What `start`/`descriptor` return, and what `@open-vibe-ai/stt-sdk`'s `createProvider()`
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
stt run [--port 8080] [--idle-timeout-secs 600] [--allow-remote] [--auth-token <token>]

stt hardware
stt recommend

stt provider list
stt provider install faster-whisper [--variant cpu|gpu]   # default cpu; blocks until any download completes
stt provider remove-variant faster-whisper --variant gpu  # reclaim disk space; never touches a vendored dev copy
stt provider start faster-whisper [--device ...] [--compute-type ...] [--bind-host 0.0.0.0] [--auth-token <token>]
stt provider status faster-whisper
stt provider logs faster-whisper --tail 50
stt provider stop faster-whisper

stt model list
stt model select --provider faster-whisper --model Systran/faster-whisper-tiny
stt model selected --provider faster-whisper
stt model pull --provider faster-whisper --model Systran/faster-whisper-tiny    # blocks until downloaded; requires a provider variant already installed
stt model verify --provider faster-whisper --model Systran/faster-whisper-tiny
stt model remove --provider faster-whisper --model Systran/faster-whisper-tiny  # reclaim disk space

stt descriptor faster-whisper

stt reset --yes   # wipe every on-disk artifact stt-server manages (provider binaries + model weights) — no daemon needed
```

`hardware`, `recommend`, `provider|model list`, and `reset` run entirely in-process (no
daemon needed) — `reset` is a pure filesystem operation, deliberately independent of a live
`stt run`, so an installer's uninstall hook can call the equivalent cleanup without spinning
one up first. Every other subcommand talks over HTTP to an already-running `stt run`
(`--server-url`, default `http://127.0.0.1:8080`) — that daemon process is the only place a
provider's running state actually lives.

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
