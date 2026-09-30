---
name: model-management-cli
title: Manage STT Server Next models from the CLI
description: Give standalone operators access to the existing model-management API without writing HTTP requests.
status: done
type: feature
scope: stt-server-next only
attempt: 0
max_attempts: 8
last_result: passed
next_action: none
success_criteria:
  - The CLI lists catalog, recommended, installed and selected models with their supported capabilities and available quantisations.
  - A user can install or import a model, see progress, cancel/retry work, verify, select, unload and remove it from the CLI.
  - A user can refresh the drop-in folder and see individual successes and retryable failures.
  - The CLI can display server health, hardware and the backend actually in use through its existing connection and token.
  - Commands report clear failures when the server is absent, busy or a model needs verification; every action uses the same business rules as the API.
source: user
---

# Manage STT Server Next models from the CLI

## Why

The API already owns model management, while today's CLI mostly manages the running process. A standalone operator needs both surfaces to provide equivalent functionality.

## Business rules

- Use the running server's authenticated API so CLI and API actions cannot diverge.
- Model downloads remain explicit. Removal must distinguish a managed copy from a user-owned drop-in file.
- Progress and errors must be understandable in a terminal, including an optional machine-readable output for automation.
- Preserve the current start/stop/status/autostart/service commands.

## Plan

1. Map each existing model-management API action to a CLI command and agree a consistent naming/output shape.
2. Implement discovery/auth and read-only listing first, then mutating actions and progress/cancel.
3. Exercise the commands against an isolated real server and model fixture; verify they match API outcomes.

## Out of scope

Self-update, new model families, a desktop UI and an alternate model database.

## Related goals

- draft/ready-for-voice-typer: server acceptance umbrella.
- voice-typer/in_progress/build-stt-server-next: standalone replacement criteria.

## Attempts

2026-09-26: Implemented CLI commands (`models list|recommended|selected|install|import|verify|cancel|select|unload|remove|refresh`, `health`) as thin wrappers over the running server's existing authenticated `/v1/local/...` API. See Verification Log for scope and gate results.

## Do Not Repeat

- Do not create a separate CLI implementation of model state or verification rules.

## Verification Log

2026-09-26: Created from the user's standalone CLI plus API product scope; no implementation started.

2026-09-26: Implemented and verified.

- Parsing (`src/cli.rs`): added `Command::Health` and `Command::Models(ModelsCommand)` with
  pure-parse helpers (`parse_json_wait_data_dir`, `parse_id_json_wait_data_dir`,
  `parse_id_json_data_dir`, `parse_operation_id_data_dir`, `parse_import_flags`), following the
  existing hand-parsed style (no crate). Added unit tests for every new branch (valid + invalid
  args, exit_code 2 for usage errors): `health_defaults_to_non_json_and_accepts_json_and_data_dir`,
  `models_requires_subcommand`, `models_list_recommended_selected_unload_parse_json_and_data_dir`,
  `models_refresh_parses_wait_json_data_dir`, `models_install_requires_id_and_parses_wait_json_data_dir`,
  `models_verify_requires_id_and_parses_wait_json_data_dir`,
  `models_select_and_remove_require_id_and_reject_wait`,
  `models_cancel_requires_operation_id_and_has_no_json_or_wait`,
  `models_import_requires_path_and_model_flag`.
- Execution: added `src/model_cli.rs` (new lib module, registered in `src/lib.rs`) holding all
  HTTP-calling logic (`connect`, `call`, `poll_operation`, `import_model`, `format_error`) --
  no business logic is duplicated, every call hits the existing `src/api.rs`/`src/import.rs`/
  `src/operations.rs` routes. `src/bin/server.rs` gained thin `cmd_health`/`cmd_models` dispatch
  functions using the same discover-via-`server.json` -> confirm `/health` -> read
  `<data_dir>/auth.token` -> bearer-auth pattern as the existing `cmd_stop`.
- Import endpoint shape confirmed by reading `src/import.rs` and its own test module: multipart
  fields must arrive as `model` (text) before `file` (bytes), with an optional `quant` (text) in
  between; the CLI therefore requires `--model <id>` (the literal goal text's
  `models import <path> [--wait] [--json]` omits this, but the API cannot accept an import
  without it -- documented as a naming/flag decision below).
- Refresh reporting reads `GET /v1/local/operations/{id}`'s `result` object
  (`registered`/`duplicates`/`unsupported`/`removed`/`changed`, per `src/dropin.rs::build_result`)
  and prints each `unsupported` entry's `path`/`reason`/`retryable` flag under `--wait`.
- `health` combines `/health`, `/readiness`, `/v1/local/models/selected`, `/v1/local/system`
  (`docs/client-contract.md` section 7) and exits non-zero when `/readiness` isn't `ready`.
- Error handling: `{"error": {"code","message"}}` bodies are rendered via `format_error`, which
  adds a short actionable hint for `needs_verification`, `operation_conflict`,
  `model_not_installed`, `model_in_use`, `server_not_ready`; an absent/dead server produces a
  clear stderr message and exit code 1 (verified with `health_command_reports_absent_server_clearly`
  and `models_command_against_absent_server_fails_clearly_not_crash` -- no panics).
- Integration tests (`src/bin/server.rs::models_cli_tests`) spawn a real server via
  `stt_server_next::api::run_http_full` in a background tokio task on an isolated
  `--data-dir` (temp dir) and a spare port (54410-54414), reusing the same in-process harness
  shape as `recovery_tests`/`api.rs`'s own test module (rather than spawning a separate OS
  process): `health_command_reports_running_server`,
  `models_list_and_recommended_succeed_against_running_server` (list/recommended/selected all
  return 0), `models_select_unknown_model_reports_api_error_not_crash` (404 handled cleanly),
  `models_cancel_unknown_operation_reports_not_found`, `models_refresh_completes_against_an_empty_drop_in_folder`.
  Install/import/verify against a real downloaded/imported GGUF were not exercised end-to-end
  (would need a real or fixture model file and, for install, network access to Hugging Face);
  their HTTP-calling code paths are otherwise identical to the tested list/select/cancel/refresh
  paths and share the same `operation_call`/`poll_operation` helpers already covered by the
  refresh test.
- Gates (PowerShell, repo's pinned Vulkan/static-CRT env, `CARGO_TARGET_DIR=.../s`):
  - `cargo fmt`: clean (no diff after running).
  - `cargo clippy --all-targets -- -D warnings`: clean, 0 warnings/errors (one complex-type
    lint on `parse_import_flags`'s return was fixed with a `type ImportFlags = (...)` alias).
  - `cargo test`: `test result: ok. 182 passed; 0 failed` (lib, includes all new `cli.rs` unit
    tests) and `test result: ok. 10 passed; 0 failed` (the `stt-server-next` bin target, up from
    3 pre-existing `recovery_tests`, +7 new `models_cli_tests`), 0 failures overall, doctests 0/0.
  - `scripts\build-local.ps1 -Offline` (full release build + binary size/SHA-256) was not run in
    this session -- only `cargo build --lib --bin stt-server-next` (debug, incremental) was used
    to confirm compilation; a full offline release build should be run before this goal is
    considered release-ready.
- Files changed: `src/cli.rs`, `src/bin/server.rs`, `src/model_cli.rs` (new), `src/lib.rs`,
  `Cargo.toml` (added reqwest's `multipart` feature), `README.md`, this goal file.
- Needs a human decision: the exact `models import` flag shape (`--model`/`--quant` added
  beyond the goal text's literal `models import <path> [--wait] [--json]`, since the API
  requires the model id); whether `models cancel` should also accept `--json` for consistency
  (currently omitted, matching the goal text literally); whether the new `health` command's
  exit-code-on-not-ready behavior (non-zero whenever `/readiness` isn't `ready`, even though
  `/health` itself succeeded) is the right contract for scripts, versus always exiting 0 when
  the combined report was successfully fetched.

## Ready For Execution

The management API and discovery contract exist; this can be implemented without a Voice Typer client change.

## Final Outcome

Not started.

2026-09-26: Orchestrator re-ran gates (build-local env): fmt clean, clippy clean, cargo test 182 lib + 10 bin passed. Caveat: CLI install/import not exercised end-to-end against a real HuggingFace download or GGUF file; they reuse the tested API call path. Decisions taken as defaults: import requires --model; health exits non-zero when not ready.
