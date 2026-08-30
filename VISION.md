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

The server is a control plane. It does not own normal batch/realtime transcription APIs, audio processing, model inference, or an audio proxy. Those belong to a managed local provider runtime, which the SDK contacts directly. The server may consume the published SDK library for shared provider contracts and runtime validation, but never SDK source by repository path.

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

## Key Decisions

- 2026-08-30 — Provider engines are added via a pluggable architecture (see `CONVENTIONS.md`'s
  selection criteria and Provider Engine Architecture sections), not one-off hardcoded
  integrations. Planned roster: faster-whisper (priority, already shipped), whisper.cpp, and
  sherpa-onnx, extensible to further engines beyond those three. faster-whisper continues to be
  the engine serving Whisper models specifically — sherpa-onnx's own ONNX-exported Whisper path
  has a documented accuracy regression versus faster-whisper on identical audio
  ([k2-fsa/sherpa-onnx#2900](https://github.com/k2-fsa/sherpa-onnx/issues/2900)), so it is
  additive for model families neither other engine can run (NVIDIA Parakeet/Canary, Moonshine,
  SenseVoice, Zipformer/Paraformer), not a replacement for faster-whisper.
