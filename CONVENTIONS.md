# CONVENTIONS.md — STT Server

## Scope

Applies to `stt-server` (https://github.com/mariuszRep/stt-server).

## Architecture

- Rust remains the server and CLI implementation direction.
- The server is a local provider-management control plane.
- Managed provider runtimes perform inference and expose the versioned local provider protocol.
- The server returns runtime connection descriptors; it does not proxy transcription.

## Required APIs

- Hardware and health discovery.
- Provider catalog, installation, update, removal, status, and runtime descriptor APIs.
- Model catalog, download/progress, verification, selection, removal, and compatibility APIs.
- Recommendations based on detected hardware and installed runtimes.

## Required Conventions

- Loopback is default; remote binding is explicit and authenticated.
- Provider/model identifiers are validated, not raw paths.
- Install/update/remove operations are observable and recover safely from partial failure.
- A provider install may target a specific hardware-variant build (e.g. CPU vs GPU); variants are cached independently (installing one never evicts another), and an install that requires a large download reports observable async progress rather than blocking silently.
- Runtime descriptors include provider identity, status, protocol/version, transport, endpoint, and capabilities.
- Consume the published, versioned `stt-sdk` library for shared provider communication and contract validation where it prevents duplication; never import SDK source by repository-relative path.
- Keep hardware detection, installation, model storage, and process supervision server-owned.

## Forbidden

- No normal batch-transcription endpoint, realtime transcription WebSocket, audio buffer, audio transcoder, or inference adapter in the server data path.
- No cloud provider API adapter in the server.
- No invisible model download during inference.
- No direct application coupling; the server API is usable independently.

## Provider Engine Architecture

- Provider/engine lifecycle logic is trait-based and pluggable (a `ProviderEngine` implementation
  per engine, dispatched through a registry), never a hardcoded per-engine `if`/`match` chain in
  `RuntimeManager`. Adding an engine is a catalog entry plus one new adapter module — not a
  parallel reimplementation of install/cache/uninstall machinery.
- **Engine selection criteria** — a new engine is only added when it has: an actively-maintained
  *official* upstream Git repository (not a fork, mirror, or single-maintainer experimental
  project); genuine, broad community adoption (not just technical merit in isolation); a
  redistribution-compatible license (MIT/Apache/BSD-style preferred), verified against the
  project's actual current `LICENSE` file at selection time, never assumed.
- **Minimize binaries the server itself builds and hosts.** An engine's adapter should fetch
  release assets from *its own upstream* project's official releases by default. The server only
  builds and hosts its own binary for an engine when no official upstream release exists at all —
  this is the exception (faster-whisper needs it because CTranslate2/faster-whisper ships only a
  pip package, no standalone executable), not the default expectation for every future engine.

## API Completeness

The HTTP API is the primary control surface and should expose as much of the server's
functionality as reasonably possible. The CLI is a thin convenience wrapper over the same
operations, not a separate capability surface. Consuming applications must drive the server via
the API; they must never shell out to CLI subcommands to reach a capability the API doesn't yet
cover — that gap is a signal to extend the API, not a reason for a client to depend on the CLI.
