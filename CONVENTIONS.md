# Conventions

Architecture and behaviour rules. They reflect the user's decisions recorded in the workspace
goals `build-stt-server-next` and `migrate-voice-typer-to-stt-server-next`.

## Shape

- One executable, one process, one engine: statically linked transcribe-cpp with CPU and
  Vulkan. The GPU driver, the Vulkan loader, and Windows system libraries are the only external
  runtime dependencies. No provider processes, connection descriptors, or ONNX runtime.
- One resident model. Transcriptions run one at a time in arrival order. The queue has no
  default length or wait limit; limits are optional settings. A request uses the model that was
  loaded when it entered the queue, so switching models never strands queued work.

## Models

- Nothing downloads on first start, and transcription never triggers a download or a model
  change.
- Downloads come from HuggingFace only, at pinned revisions, and are accepted only after the
  size and SHA-256 match the catalog.
- Files dropped into the user model folder become models only after an explicit refresh hashes
  them and reads their GGUF header. Refresh and removal never delete a user's file.
- Recommendations use the fixed Handy-informed order. Hardware is reported, never used to rank.
- Keep source benchmarks, editorial rank, and local measurements as separately labelled facts.

## Requests

- The API follows OpenAI's audio endpoints: transcriptions, and translations to English.
- The prompt is opaque: passed verbatim to models that accept one, rejected by models that do
  not. The server never composes, trims, or stores prompt text.
- A language hint the model cannot honour falls back as Handy does (auto-detect, then English,
  then the model's first language), and the response reports what was applied.
- Other optional fields a model cannot honour are rejected with a capability error. Clients
  read the capability matrix and omit what is not supported.
- Capabilities come from what the engine reports for the loaded model, cross-checked against
  its real behaviour, never from a hand-kept list or another project's rules. The catalog list
  gives the best static answer before loading; the selected-model view is authoritative.
- A missing `model` field means the selected model.
- Responses report diagnostics that exist and omit fields the engine did not produce.

## Operation

- Two roles. Part of an application (default): launched by the client app, stopped with it,
  managed through the API; the CLI covers only what the API cannot (process start, update,
  autostart, service). Shared server: runs on its own and clients attach without stopping it.
- Two install scopes, each with exactly one data folder used by every mode: per user
  (`%LOCALAPPDATA%\OpenVibeAI\STT Server`, no admin) and machine-wide (Program Files plus
  `%ProgramData%\OpenVibeAI\STT Server`, admin once). Scope follows where the executable
  lives. The Windows Service exists only for machine-wide installs.
- One server per data folder, discoverable through its `server.json` file. Clients always read
  the port from it: a per-user server whose preferred port is taken picks a free one; a
  machine-wide server keeps its fixed port.
- Two tokens: the admin token (administrators only) reaches everything; the user token
  (readable by every local user on a machine-wide install) reaches transcription, translation,
  model listing, readiness and system information. A user token on an admin route gets
  `403 admin_required`.
- Network modes: `local` (default, loopback only), `lan` (other devices only while every active
  Windows network is Private or Domain), `tailscale` (other devices only from Tailscale
  addresses). Anything beyond health from another device needs a token. Shutdown accepts only
  local callers.
- Browsers are refused by default (no CORS origins); origins are allowed only when explicitly
  configured.
- The server answers immediately at startup and loads the selected model in the background;
  status and stop always work during a load.
- Health reports the version and an `api_level` that increases whenever clients must change.
- State lives in SQLite with versioned forward migrations and a backup before each migration.
- Long-running model work is a durable operation that survives restarts.

## Engineering

- Build and test locally first. Candidate and release builds are dispatch-only, and a release
  promotes the exact tested binary and its checksum; rebuilding is not reproducible.
- Code adapted from Handy (MIT) keeps an attribution comment and the notice in
  `THIRD_PARTY_NOTICES.md`.
- Personal dictation audio and transcripts used for testing never enter the repository.
