# VISION.md — STT Server

> This repository is the local control-plane component of the Voice Typer workspace. Root product intent lives at `../VISION.md`.

## Purpose

`stt-server` makes compatible local STT provider runtimes available on a machine. It is self-hosted, local-first, and vendor-neutral.

## Responsibilities

- Hardware, driver, and runtime capability detection.
- Provider catalog, compatibility evaluation, and recommendations.
- Provider install, update, removal, start, stop, health, and logs.
- Model catalog, download, verification, selection, and removal.
- Runtime connection descriptors for clients.

## Boundaries

The server is a control plane. It does not own normal batch/realtime transcription APIs, audio processing, model inference, or an audio proxy. Those belong to a managed local provider runtime, which the SDK contacts directly.

## Core Workflow

1. A client asks the server what local providers/models are compatible.
2. The server installs and starts a selected provider runtime.
3. The server returns a versioned connection descriptor.
4. The client uses `stt-sdk` to communicate directly with the runtime.

## Rules

- Local runtime binding defaults to loopback.
- Provider/model installation is explicit and observable.
- Only curated compatible provider/runtime/model combinations are offered.
- The server may manage, but must not carry, normal transcription data traffic.
