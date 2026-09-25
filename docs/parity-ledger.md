# Voice Typer parity ledger (prototype)

The references are the `voice-typer-windows` nested worktrees under
`D:\Users\mariu\Projects\voice-typer`, not the sibling `main` clones. This ledger records
required behavior and current evidence; it does not imply a replacement verdict.

| Current client need | Prototype evidence | Evidence still required |
|---|---|---|
| Provider/model discovery and progress | Fixed recommendations; offline, model-free first start; durable install progress | Failure and retry contract tests |
| Runtime connection descriptor | Stable loopback URL, protected token, LocalSystem service tested | Clean-machine client handoff |
| Download, verify, selection, load, removal | Pinned Parakeet download/import, hash, selection, restart reload, deselection, removal tested; durable `verify` operation end-to-end on the installed copy, including restart reconciliation and live corruption quarantine | Interrupted resume, disk-full and upgrade tests |
| Batch audio | OpenAI-style multipart, same-file transcript, and tested stereo 48 kHz downmix/resample | Other containers and dictation corpus |
| Prompt and vocabulary | Parakeet matrix says unsupported; prompt rejected with `unsupported_capability` | Admit and test a model with supported decode hints |
| Language and translation | Unsupported Parakeet controls rejected | Model-specific multilingual tests |
| Streaming | Native catalog metadata is distinct from server batch support | API contract test |
| Diagnostics | CPU, Vulkan0, and forced-failure CPU fallback tested | More GPU/driver combinations |
| Current app settings | App preserves global settings, omits unavailable optional fields, explains disabled controls | Separate SDK/app cutover goal |

The current SDK sends `file`, optional `prompt`, `language`, and `model` to
`/v1/audio/transcriptions`. The current Windows app management client uses provider lifecycle,
hardware, recommendations, model pull/verify/remove/select/load, and operation polling. Provider
processes, descriptors, and hardware-ranked recommendations intentionally have no direct
replacement in the new server API.

The completed nested server `validate-parakeet-performance` goal records four same-machine
short English dictation clips. Its recorded Parakeet RTFs are 0.230–0.255 and its
faster-whisper-small.en RTFs are 0.809–2.607. The underlying exported WAV clips could not be
found in current app data on 2026-09-25, so those results are historical baseline evidence,
not a replayable corpus yet. Add consented long dictation, technical vocabulary, and multiple
languages before a replacement verdict.
