---
name: model-management-cli
title: Manage STT Server Next models from the CLI
description: Give standalone operators access to the existing model-management API without writing HTTP requests.
status: ready
type: feature
scope: stt-server-next only
attempt: 0
max_attempts: 8
last_result: none
next_action: Add CLI commands backed by the existing authenticated local API and verify them with an isolated server data folder.
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

None yet.

## Do Not Repeat

- Do not create a separate CLI implementation of model state or verification rules.

## Verification Log

2026-09-26: Created from the user's standalone CLI plus API product scope; no implementation started.

## Ready For Execution

The management API and discovery contract exist; this can be implemented without a Voice Typer client change.

## Final Outcome

Not started.
