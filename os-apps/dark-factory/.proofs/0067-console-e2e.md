# ADR-0067 — Console live e2e (work-plan step 3)

Date: 2026-09-17. Console: vite preview on 8081 (production build + proxy).
Server: temperpaw on 3100 (`/tmp/temperpaw-3100-adr67b.log`).
Driver: playwright (headless Chromium) against the real UI — no mocks.
Log: `/tmp/console-e2e/resume.log`; screenshots `/tmp/console-e2e/shots/*.png`.

## Scenario (all human interaction through the browser UI)

- **TID17** = `en-01a0ae4a-47c2-7f40-89ff-4522ba7d5d01`, factory **FCID7**
  (`en-01a0ae46-c2d1-7942-abf3-f1e1f45eac38`, github mode, scratch repo
  `gabriele-baldoni_ddog/dark-factory-e2e`, test/observation gates on
  `console-e2e.txt`).

1. **Login** — UI login form → temperpaw cookie session (`/auth/login` via
   console proxy). ✓ (01-logged-in.png)
2. **Create** — "New request" → factory picker (select FCID7) → prompt →
   "Start factory". UI POSTs create + StartPlanning. Task → Planning. ✓
   (02-new-task.png)
3. **Plan gate** — gate card rendered with plan text + digest
   (03-plan-gate.png). "Reject plan" with feedback. ✗→✓ see F19 below.
   RejectPlan completed via the same proxy path (curl, identical request
   shape) → Planning, `repair_round=1`; round-1 plan incorporated the
   feedback (`console-note.txt` mentioned in plan_text). Client-side reject
   validation verified in the UI: empty comment → banner "Please include
   feedback so the agent knows what to change.", no POST fired.
4. **Plan gate round 1** — "Approve & implement" from the UI → Implementing.
   ✓ (04-plan-gate-round1.png)
5. **Code gate** — patch pane showed `changes-r1.patch`; "Request changes"
   from the UI (README touch) → Implementing round 2, **same PR/branch**
   (`darkfactory/en01a0ae4a47c27f-r1`, PR #6). ✓ (05-code-gate.png)
6. **Code gate round 2** — "Approve, deploy & observe" from the UI.
   ✓ (06-code-gate-round2.png)
7. **Observe-before-merge visible in the UI** — `Observing` status pill
   rendered while PR #6 was still **open** on GitHub. ✓ (07-observing.png)
8. **Completed** — pill reached Completed. ✓ (08-completed.png)

## Post-state (server + GitHub)

- `Status: Completed`, `merge_sha=a61ac98a…` ≠ `merge_commit_sha=e68d04ac…`
  (post-observation squash merge — D7).
- PR #6 `merged=true`, `merge_commit_sha` starts `e68d04acdc` — matches the
  task field exactly.
- PR files: `README.md`, `console-e2e.txt`, `console-note.txt` — **all
  repair rounds carried through one reused PR** (F17 cumulative-diff fix
  exercised under a real RequestChanges round; round r2 produced the README
  delta without dropping r1's files).
- FactoryArtifacts: `changes-r1.patch`, `changes-r2.patch` (D8, one per
  publish round).
- `npm test`: 12/12 view-model tests green; `npm run build` green.

## F19 — gate decisions never dispatched from the UI (found & fixed)

First browser run: clicking "Reject plan" surfaced error banner
"C is not a function" and no POST reached the server. Root cause:
`gateDecision`'s default parameter was `uuid = crypto.randomUUID()` —
evaluated once to a string, then invoked as `uuid()` for `operation_key`.
Every unit test passed an explicit uuid provider, so the default path was
never exercised. The production minified build surfaced it as "C is not a
function".

Fix (red-green): regression test calling `gateDecision` without the uuid
argument (fails with `uuid is not a function`, then with
`ERR_INVALID_THIS` for the bare method reference), default changed to
`uuid = () => crypto.randomUUID()`. 12/12 green; resume run then drove all
four gate actions from the UI successfully.

Lesson recorded: default-parameter fallbacks in view-model helpers must be
covered by an explicit no-arg test — injected seams hide exactly the path
production takes.

---

## Follow-up (2026-09-17): exit-code gates (F20/F21/F22) + activity tails

**Console additions (GB feedback, red-green):**
- Provisioning-phase activity steps derived from task+Computer state
  (register → prepare → provision → ready → attach, destroy when
  terminal); 7 view-model tests. Live-verified on TID18: all steps done +
  6 exec lines (shot 11).
- Exec output tails: `toActivityEvents` carries `stdoutTail`/`stderrTail`;
  Activity renders each exec with output as an expandable `<details>`; 2
  tests (24/24 console green). Playwright probe on the completed F22 task:
  6 step lines, 5 exec lines, expansion shows real output (shot 12).

**F20 — gates were no-ops for command failures (bug found via TID18):**
TID18 reached Completed with validation AND observation execs at exit 1.
Root cause: Exec status Succeeded (RunSucceeded) maps to Passed regardless
of `fields.exit_code`. Fix: `factory_common::exec_succeeded` (status AND
exit_code "0", fail-closed) + `exec_failure_summary`; decide functions in
validator/planner/implementer gate on it. RED: 2 validator + 1 planner + 1
implementer tests; GREEN: all pass.

**F21 — latent benign exit 1 caught by the new gate:** implementer base
checkout commit is an idempotency no-op (planner pre-commits the shared
/work/repo); both checkout commands now `git diff --cached --quiet ||
git commit`. RED→GREEN per module (command-shape tests).

**F22 — repair evidence names the failing gate:** `failure_evidence`
includes the exec `command` (silent gates leave empty tails). RED→GREEN.

**Live end-to-end (task en-01a0aea0-4556, FCID8, server with F20-22):**
Implementing → Validating (exit 1) → Implementing round 1 (autonomous
fix-forward — previously impossible) → Validating r1 passed (agent read
the named gate and created `gb-e2e.txt` = "verified by a human") →
PublishingPR → AwaitingMergeApproval → ApproveMerge → Merging → Observing
→ FinalizingMerge → **Completed**. PR #8 merged; `gb-e2e.txt` on main with
exact required content. TID19 (pre-F22) demonstrated the failure mode:
3 evidence-blind repair rounds → Failed with loud `failure_reason`.

**Stage rail 10 steps concluding with Finalize merge (GB feedback):**
dropped the "Complete" pseudo-step; Completed/Failed render via the
rail's done rule + status pill. Merge gate unchanged (human-approved,
spec-level). RED→GREEN 2 tests (25/25 console green). Playwright probe on
the completed F22 task: 10 steps all ✓, last "Finalize merge", header
"Completed" (shot 13).
