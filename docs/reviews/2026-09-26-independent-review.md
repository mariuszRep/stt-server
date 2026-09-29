# Independent review: stt-server-next (dae590d) and the migration goals, 2026-09-26

Scope: read-only review. Nothing was built, run, or modified. The review focused on the main risk areas and did not read all 11.8k lines line by line. Anything I could not trace end to end is marked "unconfirmed".

## High

### H1. `stop` can force-kill an unrelated process tree (stale server.json plus PID reuse)
`src/bin/server.rs:204-262`, `src/discovery.rs:76-80`.
`server.json` is removed only on a clean exit (`api.rs:1245`). If the server crashes, is killed, or the machine loses power, the file stays behind. `cmd_stop` only checks `pid_is_alive(info.pid)`, which runs `tasklist` and admits in its own doc comment that it "does not distinguish ... a different process reused the PID". The shutdown POST then fails because no server is listening, and the code falls through to `taskkill /PID <pid> /F /T`.
Scenario: the server crashes, Windows reuses the PID for (say) the user's editor, and the app or the user runs `stt-server-next stop`. The editor and its child processes are killed.
Fix: treat the single-instance lock as the source of truth. If `acquire_lock(data_dir)` succeeds, nothing is running, so delete server.json and exit 0. Only fall back to taskkill when the lock is held and the endpoint failed. Optionally also compare the process image name or start time. Drop `/T`.

### H2. Every startup re-hashes every managed model before binding; `start` gives up after 30 s
`src/app.rs:176-249` (called at `app.rs:309` from `open_app_at_full`), `src/bin/server.rs:164`.
`reconcile_installed` runs a full SHA-256 over every managed GGUF on every start (`app.rs:228`). This happens before the listener binds.
Scenario: with several models installed (the Whisper large and Canary GGUFs are GBs each), cold-cache hashing takes tens of seconds, so:
- `start` reports "did not become healthy within 30s" and exits 1 while the server is still coming up;
- the app-owned launch in phase 4 sees a slow or failed start;
- autostart at logon competes for disk.
Second effect: any read error (antivirus lock, sharing violation) makes `sha256_file` fail. The model is then treated as unverified: it is unregistered, deselected, and moved to `quarantine/`. A transient lock silently uninstalls a good model.
Fix: at startup check size plus mtime (as is already done for drop-ins) and defer the full hash to explicit verify, or run it in the background after bind. Distinguish an I/O error (keep the model, report it) from a hash mismatch (quarantine).

## Medium

### M1. One unreadable drop-in file aborts the whole refresh
`src/dropin.rs:328-330` (and the similar loop at about 228). `hash_and_probe(...)` is chained with `??`, so a single file that is locked, still being copied, or has an ACL problem fails the whole refresh operation. The remaining files are then neither registered nor unregistered.
Fix: record a per-file error in the result and continue the loop.

### M2. Contract says 422, code returns 409 for selecting a model that needs verification
`docs/client-contract.md` "Select (load)" says `422 unsupported_capability`. `src/api.rs:602-607` returns `409 needs_verification`. The SDK (phase 3) will be written against the contract.
Fix: change the contract to match the code, and add a router test that pins the status and code.

### M3. Import writes to SQLite once per multipart chunk
`src/import.rs` (streaming loop, `update_operation(...)` for every chunk). A 1-3 GB import means hundreds of thousands of synchronous SQLite writes, each taken under the global `app.db` mutex that every request also takes. Expect a slow import and latency spikes on concurrent transcription requests.
Fix: throttle progress updates, for example every 1 MB or every 250 ms.

### M4. A removed drop-in model comes back on the next refresh (unconfirmed as a problem)
`src/api.rs:674-680` only deletes the `installed` row and never touches the file (which is correct). There is no ignore list (grep found none), so the next `POST /v1/local/models/refresh` registers the file again. The contract should say "remove the file from the folder to remove it permanently", or the server needs a dismissed list.

### M5. An older binary silently accepts a newer schema
`src/store.rs:429-431`: `version >= CURRENT_SCHEMA_VERSION` returns Ok. Phase 2's "rollback on failed start" will run N-1 against an N-migrated DB with no warning. This is fine while migrations stay additive, but it is not tested or documented.
Fix: log a warning, and add a rollback test in phase 2.

### M6. The token file gets no explicit ACL in the user-mode data dir
`src/app.rs:109-130` creates `auth.token` with inherited permissions. The build goal (line 112) says to "protect it with restrictive Windows ACLs". Only `service.rs:111` runs `icacls`, and only in service mode. Under `%LOCALAPPDATA%` the inherited ACL is normally user-only, so the practical risk is low. A custom `--data-dir` (for example on D:\) may be readable by other users, which matters because a token-holder has LAN reach.
Fix: apply the same icacls step to `auth.token` in every mode.

## Low

- L1. `src/auth.rs:7-12`: the token comparison is not constant-time. Only relevant with LAN mode on. Fix: use the `subtle` crate or a manual constant-time compare.
- L2. `start`/`status` treat any HTTP 200 from `/health` on the port as "our server" (`server.rs:164-175`). Another service on 54321 would be misreported as a running server. Check a field in the `/health` body, or check the PID from server.json.
- L3. CORS defaults to `*` (`api.rs:302-318`). This is acceptable with bearer-header auth (no cookies, and a browser page cannot read the token file). It does let any web page probe `/health` and learn the server exists. Document it, and consider narrowing the default before the repo goes public.
- L4. `.gitignore` does not cover `auth.token` or `server.json`. Neither is tracked today; add them defensively, since test dirs like `/t/` sit in the repo root.
- L5. The local ref `remotes/origin/HEAD` still points to `origin/codex/prototype`. The goal says the default branch is `main`. This may only be a stale local ref (`git remote set-head origin -a`). Unconfirmed on GitHub.
- L6. The request body limit is 40 MB (`api.rs:1107,1111`), about 21 minutes of 16 kHz mono PCM16. Whisper Vibes has backlog merging (`wav-concat.ts`, `merge-backlog`). Confirm that merged uploads never exceed this. Unconfirmed.

## Checked and found sound
- `queue.rs`: the CAS-bounded waiting count with a drop guard is correct. Tokio's Semaphore is fair, so the `try_acquire` fast path cannot jump queued waiters. The tests cover FIFO order, a full queue, timeout, and a dropped waiter.
- Model swap: `select`/`remove`/`deselect` are serialized by `app.selection`. Loading happens outside the lock. Requests clone the `Model` handle before queueing, so a swap never breaks queued requests. A failed load leaves the old model in place.
- Cancellation: `CancelWhenDropped` fires on client disconnect and on inference timeout. The permit is held inside the blocking closure until the engine returns.
- Auth: every route except `/health` calls `authorized`. That covers 17 call sites plus operations, import, and install. Shutdown is loopback-only and authenticated, with tests (`api.rs:1950-2030`).
- The LAN guard fails closed on an empty token (`api.rs:1203-1214`).
- Drop-in, remove, and quarantine never delete user files. Managed removal canonicalizes against `data/models`. Startup quarantine only moves files that are under `models/`.
- Git history: no audio, transcript, db, gguf, or token files were ever committed (`git log --all --name-only`). Build and smoke logs in the working tree are gitignored. `THIRD_PARTY_NOTICES.md` carries Handy's MIT text, and the Handy-derived modules cite the source.
- Test count: I counted 166 `#[test]`/`#[tokio::test]` functions, which matches the recorded "166 tests". I did not re-run them.

## Unsubstantiated claims
1. Migration goal: "phase 1 done / feature-complete". The goal's own phase-1 exit evidence requires "LAN reachable from a second device only with a token" and "system endpoint on CPU-only and Vulkan hosts". Only same-machine LAN-IP reachability and a Vulkan host (Iris Xe) are recorded. There is no second-device test and no CPU-only host run. CPU-only startup without `vulkan-1.dll` is still listed as an open gate in the build goal (lines 676, 694).
2. Service install/uninstall/run: I found no record of a real-machine test in the phase-1a log. Only start/stop/restart/status/autostart were exercised.
3. "start/stop/restart idempotent": verified by hand once. No automated test exercises the stale-server.json path (see H1).
4. "Every capability the shipping clients use has a tested equivalent": the compatibility audit lives in an orchestrator scratchpad (`compat-audit.md`) that is not committed anywhere, so it cannot be checked.
5. Binary SHA-256 values and the `dumpbin` audit are recorded but no artifact is kept. Acceptable as log entries, but not reproducible.

## Decision conflicts
1. The frontmatter `next_action` in build-stt-server-next/GOAL.md (line 11) is stale ("End-to-end test the local model-verification API and commit/push the current prototype; then generalize the catalog..."). The log shows that work finished long ago.
2. The build goal log (lines 742-746) records a HuggingFace-then-mirror fallback implementation. It is superseded by the "HuggingFace only" user decision (line 326, removal at 769). The history is consistent, but readers skimming the log may take it as current. Mark it superseded inline.
3. The migration goal says a missing `model` is treated as "`default`". The code and contract say "the selected model", and `default` means the selected model, so the two are consistent. The SDK note "always sends one" plus the "default" string should be stated once, in the contract only.
4. The contract says select returns 422; the code returns 409 (M2).
5. The build goal requires restrictive token ACLs, but user mode applies none (M6).
6. No live leftovers were found for vocabulary field, fixed queue limits, strict language errors, or "manually dropped files never trusted". The contract says there is no vocabulary field, the queue is unbounded by default, and drop-ins are hashed and probed then trusted.

## Migration blockers
- Phase 3 (SDK):
  - Fix the contract/code mismatch (M2).
  - Document the error-code catalog in one table (the codes found include `model_not_active`, `server_not_ready` with `operation_id`, `queue_full`, `queue_timeout`, `inference_timeout`, `needs_verification`, `model_in_use`, and `operation_conflict`).
  - The old SDK's `/v1/admin/model` switch semantics (`local-runtime.ts:126`) map to `select` but are missing from the gap table.
- Phase 4 (app):
  - H2 (slow startup) directly affects app-owned launch.
  - H1 affects "the app stops the server on quit".
  - The desktop app currently passes `--idle-timeout-secs 0 --allow-remote` (`lib.rs:697-717`). The new CLI has no idle timeout; the contract should say explicitly that there is none.
  - The app-side settings migration (saved provider IDs and pins mapped to catalog IDs) has no mapping table yet.
  - The root pin for whisper-vibes is dirty (`M whisper-vibes`: pinned `a217543`, head `87329ea` with untested sound-theme work). Resolve this before phase 4 branches from it.
- Phase 2 (update) is not started and needs M5 handled.

## Verdict
Phase 1 server work is substantially implemented and of good quality. The queue, swap, auth, LAN guard, and file-safety logic hold up, and history hygiene is clean. It is not "done" by the goal's own exit criteria: second-device LAN, a CPU-only host, and a real service install are all unverified. Two real defects should be fixed before any client depends on the CLI: the PID-reuse kill in `stop` (H1) and hash-everything-at-startup with its transient-error quarantine (H2). The contract needs a small correction pass (M2, error catalog, no idle timeout). With those fixed, the project is on track for phase 3.
