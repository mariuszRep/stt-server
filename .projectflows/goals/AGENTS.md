# Goal Execution Instructions

This file is installed at `.projectflows/goals/AGENTS.md` when a project goals tree is initialized.

## Directory Layout

```text
.projectflows/goals/
├── draft/        # incomplete or not executable yet
├── ready/        # executable and waiting for an attempt
├── in_progress/  # an attempt is currently underway or more work remains
├── blocked/      # cannot safely continue without human input or a dependency
├── done/         # success criteria satisfied with evidence
└── cancelled/    # intentionally abandoned
```

Canonical goal path:

```text
.projectflows/goals/<status>/<goal-slug>/GOAL.md
```

The folder name and `GOAL.md` frontmatter `status` must match.

## Status Values

Use these statuses for new goals:

- `draft` — not executable yet; missing requirements, constraints, scope, or approval.
- `ready` — executable; no active attempt is underway.
- `in_progress` — an execution attempt has started and work remains active.
- `blocked` — cannot safely continue without human input, external dependency, or a max-attempt/no-progress decision.
- `done` — success criteria are satisfied and verification evidence is recorded.
- `cancelled` — intentionally abandoned by user/operator decision.

Legacy `needs-clarification` means `draft`; migrate it to `draft` on the next edit.

## Execution Template

When executing a goal:

1. Read the target `GOAL.md` and any applicable repository context files before editing implementation files.
2. Confirm the goal is executable. If not, update it to `status: draft` or `status: blocked`, record why, and move its folder accordingly.
3. Before starting implementation from `ready`, update frontmatter to `status: in_progress`, increment `attempt`, set `last_result` to `partial`, append an entry under `## Attempts`, and move the whole goal folder to `in_progress/`.
4. Execute the smallest safe implementation/verification cycle that can satisfy the goal.
5. Update `## Verification Log` with commands, checks, manual evidence, or why verification could not run.
6. Decide final state:
   - success: set `status: done`, `last_result: passed`, `next_action: null`, complete `## Final Outcome`, and move to `done/`.
   - more work needed but safe to continue later: set `status: ready` or `in_progress` based on whether an active attempt remains, record `next_action`, and move to the matching folder.
   - blocked: set `status: blocked`, `last_result: blocked`, record the blocker and required human decision, and move to `blocked/`.
   - cancelled by explicit instruction: set `status: cancelled`, record reason, and move to `cancelled/`.
7. Never leave duplicated copies of the same goal in multiple status folders.
8. Never leave a goal in a folder whose name disagrees with its frontmatter `status`.

## Movement Rules

Move the entire `<goal-slug>/` directory, not only `GOAL.md`.

Typical transitions:

```text
draft -> ready
ready -> in_progress
in_progress -> ready
in_progress -> blocked
in_progress -> done
ready -> blocked
any -> cancelled
```

If a move would overwrite an existing goal folder, stop and report the collision instead of merging silently.

## Goal File Maintenance

Keep these sections current during execution:

- frontmatter: `status`, `attempt`, `last_result`, `next_action`
- `## Attempts`
- `## Do Not Repeat`
- `## Verification Log`
- `## Final Outcome`
- `## Ready For Execution`

Use `## Do Not Repeat` to record failed approaches so future attempts do not loop.
