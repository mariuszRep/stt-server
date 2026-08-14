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
