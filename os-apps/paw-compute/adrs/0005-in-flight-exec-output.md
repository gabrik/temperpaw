# ADR-0005: In-flight Exec output via CheckOutput self-loop

## Status

Accepted — implemented and live-verified (2026-09-17, GB asked for live
exec output / "tackle the SSE").

## Context

`computer_exec` (ADR-0002/0004) runs one blocking sandbox call per Exec:
the Exec sits in `Running` with no field updates until the command exits
and the module reports `RunSucceeded` (exit_code + bounded tails). The
dark-factory console (ADR-0067) therefore shows exec output only after
completion. Temper's `/tdata/$events` SSE stream emits one
`EntityStateChange` per dispatch, but during a run there are no
dispatches — streaming transport alone cannot surface in-flight output.

The full combined output is already tee'd to `/tmp/.exec-out/<exec_id>.log`
on the sandbox in real time (`wrap_command`, ADR-0004), and
`sandbox_file_read` (ADR-0003 era file API) can read it mid-run.

## Decision

Entity-first in-flight output, mirroring the dark-factory Check* pattern:

1. **`CheckOutput`** — input action `Running → Running`, effect: trigger
   `computer_exec_tail`. No `on_failure`: a tailing failure must never
   kill the run. The module is written to never error (every failure
   path degrades to the empty stay callback).
2. **`[[state_timeout]]`** on `Running`: `after_seconds = 5`,
   `on_timeout = "CheckOutput"`, `reset_on = ["CheckOutput"]` — the
   kernel re-fires `CheckOutput` every 5 s while the exec runs. 5 s
   (not the factory's 30 s) because the consumer is a live console.
3. **`computer_exec_tail`** WASM module: resolves the Computer row from
   the exec's `computer_id`, reads `/tmp/.exec-out/<exec_id>.log` via
   `sandbox_file_read`, and reports `ReportOutput` with truncated
   `stdout_tail` / `stderr_tail` (same 256 KiB bound as
   `computer_exec`). Missing file / not-yet-flushed output / sandbox
   hiccup → `set_success_result("", {})` (stay, no update). Because the
   log file holds *combined* output, the tail reports it as
   `stdout_tail` and leaves `stderr_tail` untouched mid-run; the final
   `RunSucceeded` report still carries the true separated streams.
4. **`ReportOutput`** — input action `Running → Running`, params
   `stdout_tail`, `stderr_tail` (optional), `stdout_bytes` (current log
   size). Updates the row; each dispatch emits an SSE `state_change`.

## Consequences

- The Exec row carries live-ish output (≤ ~5 s lag) with zero changes to
  the blocking run path; the console's existing polling picks it up
  immediately, and `/tdata/$events` subscribers get a nudge per tick.
- Event-store cost: ~12 extra dispatches/min per running exec
  (CheckOutput + ReportOutput). Acceptable at dark-factory concurrency
  (≤ 10 batch jobs); revisit with adaptive cadence if volume matters.
- `sandbox_file_read` pulls the whole log each tick. Pi-run logs can
  reach MBs; bounded by cadence and per-exec size. A range-read sandbox
  API is future work if profiling objects.
- Cedar: `CheckOutput`/`ReportOutput` are input actions — permitted for
  the system service like `Run`/`RunSucceeded` (compute.cedar unchanged;
  the kernel dispatches CheckOutput, the module reports ReportOutput as
  the same authority class as RunSucceeded — verified live).

## Implementation notes (discovered during bring-up)

- **Log dir is absolute: `/tmp/.exec-out`** (not `~/.exec-out`). The
  sandbox file API does no shell expansion, so the tail module must read
  the exact absolute path `computer_exec::wrap_command` writes.
  computer_exec's wrapper + tests moved accordingly; the sanitize
  vectors are parity-tested in both crates.
- **Exec id comes from `ctx.entity_id`** — `ctx.entity_state["Id"]` is
  not populated on self-loop dispatches.
- **WASM outbound HTTP is Cedar-gated per module** (`http_call` on
  `HttpEndpoint` matched by `context.module`). compute.cedar permits
  `["computer_exec", "computer_exec_tail"]` for `http_call` and
  `access_secret`; a module missing from the list fails every loopback
  read with `WASM host authorization denied outbound HTTP call`.
- Every failure path is **loud in the server log** (`ctx.log("warn")`)
  while still degrading to the stay callback — the first implementation
  was silent and cost a debug round-trip.

## Verification

Red-green unit tests in `computer_exec_tail` (decide/tail truncation/
sanitize parity with computer_exec's exact vectors; 7 tests) plus
computer_exec's 17 tests updated for the `/tmp` log dir.

Live (2026-09-17, local 3100, tensorlake computer):
`for i in $(seq 1 8); do echo tick $i; sleep 2; done; echo ALL-DONE`
showed the Exec row's `stdout_bytes` grow 21 → 35 → 56 with `stdout_tail`
(ticks 1–3 → 1–5 → 1–8) **while Running**; `RunSucceeded` then wrote the
authoritative final tail (65 bytes incl. `ALL-DONE`, `exit_code=0`).
`/tdata/$events` carried
`Created → Run → [CheckOutput → ReportOutput]×3 → RunSucceeded`.
Console-level evidence: os-apps/dark-factory/.proofs/0068-live-activity.md.
