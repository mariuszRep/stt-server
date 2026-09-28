# STT Server Next — Vision

> Approved by the user on 2026-09-26; "Runs the way people need it" updated with the user's
> decisions of 2026-09-27. Change only on explicit human instruction.

## What it is

One self-contained local speech-to-text server. A person installs a single program, and it
manages speech models and turns audio into text for any application that asks, through the
OpenAI audio API that many tools already understand. Voice Typer is its first client, not its
only one.

## What it promises

- **One program, one engine.** No helper processes, no Python, no separately shipped inference
  libraries. It runs on the CPU everywhere and uses the GPU through Vulkan when it can, and it
  always says which one it actually used.
- **Every model Handy supports.** The full Handy GGUF catalog is available in every published
  quantisation. People can also drop their own GGUF files into a folder and refresh.
- **The person decides.** Nothing downloads until someone chooses a model. Recommendations
  follow a fixed curated order, never a guess about the user's hardware.
- **Honest capabilities.** Each loaded model states which options it really supports (prompt,
  language hint, translation, temperature, timestamps). Clients use that to show or hide
  controls. The server never pretends an option worked.
- **Transcription stays simple.** Applications send finished audio and get text back. The
  client owns the dictation session: microphone, chunking, prompts and vocabulary, and editing
  the result. The server passes a prompt through unchanged and never rewrites a transcript.
- **Runs the way people need it.** By default it starts and stops with the app that uses it,
  installed just for that person. It can also run on its own: at Windows sign-in for one person,
  or installed for the whole machine as a shared server (a Windows Service) that other users,
  devices on a private network, or the person's own devices over Tailscale can use. Each install
  keeps its models in one place, and shared use is always behind a token, with ordinary users
  able to transcribe and only an administrator able to change models or settings.
- **Open source.** It will be published as an open-source project, with public releases the
  server can update itself from.

## Not in scope

Live streaming transcripts, server-side microphone capture or voice-activity detection,
holding a user's dictation session, and cloud or multi-tenant hosting.
