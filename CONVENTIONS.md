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
- A missing `model` field means the selected model.
- Responses report diagnostics that exist and omit fields the engine did not produce.

## Operation

- Default mode: launched by the client app and stopped with it. Standalone mode: started by the
  CLI, at Windows sign-in, or as a Windows Service; clients attach and never stop it.
- Loopback by default. Binding beyond loopback requires the bearer token; health is the only
  unauthenticated route. Shutdown accepts only local callers.
- One server per data folder, discoverable through its `server.json` file.
- State lives in SQLite with versioned forward migrations and a backup before each migration.
- Long-running model work is a durable operation that survives restarts.

## Engineering

- Build and test locally first. Candidate and release builds are dispatch-only, and a release
  promotes the exact tested binary and its checksum; rebuilding is not reproducible.
- Code adapted from Handy (MIT) keeps an attribution comment and the notice in
  `THIRD_PARTY_NOTICES.md`.
- Personal dictation audio and transcripts used for testing never enter the repository.
