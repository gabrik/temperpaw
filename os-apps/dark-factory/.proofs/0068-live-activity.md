# Proof — ADR-0068 live activity feed + ADR-0005 in-flight exec output (2026-09-17)

Two halves verified together end-to-end, exactly as designed:

- **paw-compute ADR-0005**: the Exec row gains live output mid-run
  (`CheckOutput` 5 s self-loop + `ReportOutput` from the new
  `computer_exec_tail` WASM module).
- **ADR-0068**: the console subscribes to the platform SSE feed
  (`GET /tdata/$events`) and refetches the open view (debounced 300 ms)
  on relevant `state_change` events; the 3 s poll stays as fallback.

## Environment

- temperpaw server: `target/debug/temperpaw-server`, port 3100, turso
  stores (log `/tmp/temperpaw-3100-sse4.log`).
- Console: vite preview 8081 (`npm run preview`), proxying `/tdata` +
  `/auth` → 3100.
- Factory: FCID8 (`en-01a0ae6c-f7aa-75f0-a0fa-c063a12d3017`), github
  mode, scratch repo `gabriele-baldoni_ddog/dark-factory-e2e`, gates
  updated for this run to `gb-e2e-0068.txt` / `live tails verified`.
- Task: **TID18** `en-01a0aece-c5fb-7830-89b9-b0f363550ac0`.

## 1. Exec-row in-flight output (ADR-0005, API level)

Exec `en-01a0aecb-b7f0-7941-a6b5-e7bc249b06ec` on a Ready tensorlake
computer (`/bin/sh -c 'for i in $(seq 1 8); do echo tick $i; sleep 2; done; echo ALL-DONE'`):

| t | status | stdout_bytes | tail |
|---|--------|--------------|------|
| +6 s | Running | 21 | ticks 1–3 |
| +10 s | Running | 35 | ticks 1–5 |
| +14 s | Running | 56 | ticks 1–8 |
| +16 s | Succeeded | 65 | ticks 1–8 + `ALL-DONE`, `exit_code=0` |

SSE capture (`curl -N /tdata/$events`, `/tmp/sse-stream3.log`) shows the
full sequence: `Created → Run → [CheckOutput → ReportOutput]×3 →
RunSucceeded` — every dispatch one `state_change` event.

Gotchas found and fixed during bring-up (recorded in ADR-0005
"Implementation notes"): `/tmp/.exec-out` absolute log path (sandbox file
API does no `~` expansion — computer_exec moved + tests updated);
`ctx.entity_id` for the exec id; Cedar gates WASM outbound HTTP per
module (`computer_exec_tail` added to compute.cedar `http_call` +
`access_secret` permits); failure paths now log `warn`.

## 2. Console live feed (ADR-0068, UI level)

Playwright probe `/tmp/console-e2e/sse-probe.mjs` + `sse-finish.mjs`
(logs `/tmp/console-e2e/sse-probe.log`), full task driven through the UI:

1. **Stream connected** — `window.__factoryStreamOpen === true` on the
   task page (EventSource does not surface in performance resource
   entries, hence the explicit flag; 10:02:31).
2. **Plan gate** — "Approve & implement" clicked in the UI (10:02:46).
3. **In-flight tail in the UI (the money shot)** — while the task was
   `Implementing`, the activity feed rendered the exec
   `run pi implementation agent` with status **Running** and a live tail
   (`Warning: No project session found with id '…-r0'; creating a new
   session…`) — output visible *before* completion, refreshed by
   `ReportOutput` events (10:03:52; shot `15-sse-inflight-tail.png`).
4. **Code gate** — reached `AwaitingMergeApproval`; gate card rendered;
   "Approve, deploy & observe" clicked in the UI (10:09:22; shot
   `16-sse-code-gate.png`).
5. **Terminal** — task `Completed` at 10:11:28 via the autonomous
   Merging → Published → Completed chain (shot `17-sse-final.png`).

Post-run entity check: `status=Completed`, `head_sha=3e972496…`,
`plan_digest=a7ec7cd…`; 7 Exec rows all `Succeeded exit=0` (setup, pi
bootstrap waits, commits, and both `gb-e2e-0068.txt` gate checks).

## 3. Regression

- `computer_exec_tail`: 7 unit tests green (red-green: decide/tail/
  sanitize parity with computer_exec's exact vectors).
- `computer_exec`: 17 tests green after the `/tmp/.exec-out` move.
- Console: 25 tests green (20 prior + 5 new `shouldRefreshForEvent`
  cases: FactoryTask/Exec-lifecycle/Computer/FactoryArtifact refresh;
  `Exec.CheckOutput` and platform chatter dropped; malformed tolerated).
- Full factory task regression = the section-2 run itself (all stages
  through Completed with the console open and the stream connected).

## Files

- paw-compute: `wasm/computer_exec_tail/` (new crate),
  `wasm/computer_exec/src/lib.rs` (log dir → `/tmp/.exec-out`),
  `specs/exec.ioa.toml` (CheckOutput/ReportOutput/state_timeout),
  `app.toml` + `wasm/build.sh` (module wiring),
  `policies/compute.cedar` (module allowlists),
  `adrs/0005-in-flight-exec-output.md` (status + implementation notes).
- dark-factory: `policies/factory.cedar` (`read_events` permit),
  `console/src/api.js` (`openFactoryEventStream`),
  `console/src/view-model.js` (`shouldRefreshForEvent` + 5 tests),
  `console/src/App.jsx` (stream wiring, 300 ms debounce, poll fallback).
- docs: `docs/adrs/0068-live-activity-feed.md` (status + final design).

## Deferred

- Range-read sandbox API (tail currently re-reads the whole log per
  tick; bounded at 256 KiB).
- EventSource across tabs: one connection per open tab — fine at
  single-user scale.
- GitHub token in `.env` is stale (401); unrelated to this work — the
  task's own credential path (token table) merged the PR fine.
