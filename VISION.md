# STT Server Next

> Prototype intent approved for local validation; a separate decision is required for a Voice
> Typer production cutover.

One Windows executable owns local GGUF speech-model installation, durable state, and batch
transcription. It exposes an authenticated OpenAI-compatible API and local management routes,
with no provider subprocesses or shipped inference DLLs. The first release target is Windows
x64 with CPU and Vulkan in the same binary, falling back to CPU if a chosen model cannot load
on Vulkan.

A fresh service presents a fixed Handy-informed model recommendation order and waits for a
user's explicit download choice. No hardware-detection endpoint ranks models. A per-model
capability matrix states which optional request fields the engine and API actually support.

The current STT SDK, server, Windows app, and Voice Typer gitlink pins stay unchanged while
this repository proves its own behavior. SDK and app integration belong to a separate cutover
goal after parity evidence and a replacement verdict.

