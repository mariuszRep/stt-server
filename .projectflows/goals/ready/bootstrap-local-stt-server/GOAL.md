---
name: bootstrap-local-stt-server
title: Bootstrap Local STT Provider Management Server
description: Establish the Rust control plane for hardware detection, provider/model lifecycle, runtime supervision, and connection descriptors.
status: ready
type: refactor
scope: stt-server repository
attempt: 0
max_attempts: 5
last_result: none
next_action: Re-plan the workspace around provider-management APIs, replace inference-path responsibilities with provider lifecycle contracts, and preserve only reusable control-plane components.
success_criteria:
  - Server exposes provider, model, hardware, recommendation, health, and runtime-descriptor APIs.
  - Server does not accept or proxy normal transcription audio traffic.
  - Server can install/start a local provider and return a versioned connection descriptor.
  - Provider runtimes, not the server, expose transcription APIs.
source: user
---

# Bootstrap Local STT Provider Management Server

## Goal

Convert the existing Rust server direction from embedded STT execution into a local provider-management control plane.

## Scope

- Hardware detection and compatibility/recommendations.
- Provider and model lifecycle.
- Runtime supervision, health, logs, and connection descriptors.
- CLI/API parity for management operations.
- Migration assessment for reusable existing Rust types/routes.

## Out of Scope

- Whisper.cpp/faster-whisper inference implementation in this server.
- Batch/realtime audio API or audio proxy.
- Cloud provider adapters.
- App/Electron implementation.

## Acceptance Criteria

1. Server management APIs make a local provider available without accepting transcription audio.
2. A running provider has a descriptor usable by `stt-sdk`.
3. Provider/model lifecycle is explicit and observable.
4. Existing execution-oriented adapter code is removed from the planned server data path or documented as migration-only legacy.

## Verification Expectations

- Server API/CLI tests cover provider/model lifecycle and descriptor output.
- An SDK contract fixture connects directly to a managed runtime descriptor.
- Architecture review confirms no normal audio flow crosses the server.

## Ready For Execution

- Status: yes
- Reason: The control-plane boundary is confirmed.
