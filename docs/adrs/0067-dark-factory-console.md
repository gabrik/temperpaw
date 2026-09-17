# ADR-0067: Dark Factory Console — browser-native UI on OData, no BFF

**Status**: Proposed
**Date**: 2026-09-16
**Deciders**: GB
**Supersedes**: none (builds on ADR-0066)

## Context

ADR-0066 delivered the dark-factory pipeline (Planning → … → Completed)
driven entirely by entities, WASM modules and governed Execs, verified
end-to-end via curl. The DEN-era demo UI
(`den-software-factory/demo/`, React SPA + Express BFF) is the UX we want
to keep: task sidebar, stage rail, plan/code decision cards with comments,
activity view, diff view.

That demo's data plane splits in two:

1. **Temper client** — list/get/create `FactoryTasks`, dispatch
   `Temper.DenFactory.{ApprovePlan, RejectPlan, ApproveMerge, RequestChanges}`.
   This maps almost 1:1 onto dark-factory (`Temper.DarkFactory`).
2. **Sidecar store** (`.factory-data/`, Express `runtime-store.js`) —
   prompts, chat messages, activity logs, diff artifact — populated by the
   Node controller that ADR-0066 eliminated. Nothing writes it anymore.

Per the Entity-First Rule, UI state must come from entities. A sidecar
store with no author is dead weight; reviving it would re-introduce a
stateful Node process alongside the entity model — the anti-pattern
ADR-0065/0066 removed.

## Decision

**D1 — The console lives in temperpaw.** Port the React SPA (rewired) to
`os-apps/dark-factory/console/` (vite build, static assets). No DEN-repo
dependency at runtime. (Decision: GB, 2026-09-16)

**D2 — No BFF. The browser talks to Temper OData directly.** (Decision:
GB, 2026-09-16) The SPA uses the same endpoints the e2e used:

- `GET /tdata/FactoryTasks?tenant=default` / `('<id>')` — task list/detail
- `POST /tdata/FactoryTasks` `{created_by}` then
  `POST .../Temper.DarkFactory.StartPlanning` `{task_prompt, factory_id,
  computer_id: "", operation_key, operation_owner, phase_ticks: 0}` — create+start
- `GET /tdata/FactoryConfigs` — factory picker (create flow selects a
  config; if exactly one exists, preselect it)
- Human gates (read the row first, then dispatch with the CAS values):
  - `ApprovePlan {plan_digest, operation_key, operation_owner, phase_ticks: 0}`
  - `ApproveMerge {head_sha, operation_key, operation_owner, phase_ticks: 0}`
  - `FailTask {failure_reason, operation_result, expected_operation_key,
    expected_operation_owner}` — exposed as "Abort"
- `GET /tdata/Execs?$filter=...&$orderby=...` — activity view (see D4)

`operation_owner` = the logged-in user's identity; `operation_key` is
minted console-side as `{task_id}:{next-phase}:r{repair_round}:t0`
(matching the module mint pattern; CAS only requires uniqueness).

**D3 — Spec delta: human feedback actions.** The demo's reject flows have
no dark-factory equivalent; add two input actions to `factory_task.ioa.toml`
(+ CSDL + `temper verify`):

- `RejectPlan(plan_digest /* CAS vs field */, repair_context /* nonempty */,
  operation_key, operation_owner, phase_ticks)` —
  `AwaitingPlanApproval → Planning`, effect `repair_round += 1`, mint
  `:plan:` key. The planner folds `repair_context` into the re-plan prompt
  (it currently only reads `task_prompt` — module change).
- `RequestChanges(head_sha /* CAS */, repair_context /* nonempty */,
  operation_key, operation_owner, phase_ticks)` —
  `AwaitingMergeApproval → Implementing`, effect `repair_round += 1`, mint
  `:implement:` key. The implementer already consumes `repair_context` as
  failure evidence; no module change beyond prompt assembly confirming the
  human feedback is included.

Both actions are Admin-only (Cedar human path), same as Approve*.

**D4 — Sidecar features map to entities, not a store.**

| Demo feature | Dark-factory console source |
|---|---|
| Task list/detail | `FactoryTasks` OData (status, fields, counters) |
| Stage rail | task `status` (+ `repair_round` counter) |
| Plan text | task `plan_text` field |
| Plan/code decision cards | `ApprovePlan`/`RejectPlan`, `ApproveMerge`/`RequestChanges` (D3) |
| Chat | dropped; the decision-card comment *is* the feedback (`repair_context`) |
| Activity log | poll task row + `Execs` filtered by `computer_id`, ordered by created; rows carry `command`, `exit_code`, `stdout_tail`, `stderr_tail` |
| Diff view | github mode: link out to `pull_request_url` (real diff UI on GitHub); local mode: `FactoryArtifact` entity holding the patch (D8) |
| Events stream | client-side polling (2–5 s) of task + execs; SSE is a GB-endorsed follow-up |

**D5 — Auth.** Browser logs in via the existing temper session/cookie flow
as an Admin human (same flow the e2e used). Cedar already maps
`User with role Admin` to the human actions and `agent_type == "system"`
to module actions; no policy change needed. The console never sees
secrets: `GH_TOKEN`/model keys remain server-side module config.

**D6 — Serving & ports.** Temper does not serve static assets (confirmed
by GB), so the built console is served by a small static file server on
**8081** (planned console port; `vite preview` or equivalent — a new tiny
static server is acceptable per GB). Dev: `vite dev` proxying `/tdata` to
the temperpaw server on 3100. CORS: dev uses the same-origin vite proxy;
if cross-origin access to 3100 is ever needed we add a CORS allow-list
then, not preemptively.

**D7 — Merge ordering: observe-before-merge (aligns with the DEN
controller).** Review finding (GB): the DEN flow merges the GitHub PR at a
different point than ADR-0066 implemented. The DEN controller's `merge()`
**deploys the exact approved head and records `RecordMerged` with the PR
left open**; `observe()` runs the observation suite against that deployed
head and **only after observation passes** merges the PR
(`gh pr merge --match-head-commit`, inside the same operation receipt so a
restart can never merge twice or skip the merge). The DEN spec hint on
`RecordMerged` ("Observe only after GitHub reports the exact PR as
merged") is stale relative to this deliberate controller behavior; we
follow the controller. Rationale: the PR merge is the only irreversible
step, and code whose post-deploy observation fails should never reach the
base branch — a failed observation must leave the PR open for a
fix-forward repair round.

Concretely, dark-factory gains one state and two actions:

- `Merging` phase (publisher) becomes **deploy-record**: verify nothing,
  merge nothing — report `RecordMerged(merge_sha = head_sha)` with
  operation_result "deployed approved head; PR left open". (`merge_sha`
  regains DEN semantics: the deployed/observed head, not a GitHub merge
  commit.) The F15 repair rebase (`merge_sha` when nonempty) is unchanged
  and still correct.
- `ObservationPassed` re-targets `Observing → FinalizingMerge` (new
  state). Validator module code is unchanged — only the spec target
  moves.
- New self-loop `CheckFinalizing` (budget 30 ticks) on `FinalizingMerge`
  triggers the **publisher's finalize arm**: github mode GETs the PR,
  enforces the `published_sha` tamper guard, merges (squash), waits for
  GitHub to report MERGED (bounded), then reports the new input action
  `MergeFinalized(merge_commit_sha /* nonempty */, operation_result,
  expected k/o)` → `Completed` → janitor. Local mode reports
  `MergeFinalized(merge_commit_sha = merge_sha)` immediately.
- New nullable field `merge_commit_sha` holds the real GitHub merge
  commit (the DEN keeps it as a sidecar artifact; ours belongs on the
  entity).
- `ObservationFailed` is unchanged (`Observing → Implementing`,
  `repair_round += 1`); the PR is still open at that point. Branch naming
  is DEN-parity (implementer change): pre-deploy repairs (RequestChanges,
  ValidationFailed — `merge_sha` empty) KEEP the same branch, so the
  publisher force-updates the same ref and reuses the same open PR;
  post-deploy repairs (ObservationFailed — `merge_sha` set) mint a fresh
  `-r<round>` branch and open a new PR, exactly like DEN's post-merge
  repair loop. A superseded PR from a post-deploy repair is left open
  (DEN behaves the same); closing it automatically is a follow-up.

This supersedes the merge-then-observe ordering in ADR-0066 (addendum
added there). The TID13/TID14 proofs remain valid as historical evidence
of the old ordering.

**D8 — `FactoryArtifact` entity** (GB review decision, replacing the
deferred local-mode diff option). New entity in the dark-factory spec:
`FactoryArtifact { task_id, kind, name, content, created_by }` with
`kind ∈ {"patch", "plan"}` for now. The publisher creates a
`kind="patch"` artifact (full `/work/changes.patch` text) via loopback
entity create at publish time — the same mechanism the planner already
uses to create its Computer. The console's diff pane reads artifacts
filtered by `task_id` + `kind="patch"` (both publish modes; github mode
keeps the PR link as the primary view). Spec + CSDL + `temper verify`
in work-plan step 1.

## Consequences

Positive:
- Zero new backend processes; the entity model stays the single source of
  truth (Audit Test passes for UI-driven flows too).
- Human feedback loops (`RejectPlan`, `RequestChanges`) become
  first-class, audited transitions — an improvement over the DEN chat
  side-channel.
- The console works against any dark-factory deployment with zero config
  beyond the API origin.

Negative / costs:
- dark-factory spec + module changes (D3 feedback actions, D7 merge
  ordering, D8 artifact entity) land before the console is fully wired.
  D7 reworks the already-verified publisher merge path (TID13/TID14 ran
  the old ordering).
- Chat as a free-form channel disappears; multi-round human steering
  happens only through gate feedback. Accepted: gates are the designed
  interaction points.
- One more state (`FinalizingMerge`) lengthens the happy path by one
  phase; in return the PR can never merge with failing post-deploy
  observation.

## Implementation Findings (2026-09-17)

Work-plan step 1 is complete and curl-verified on a live server (tasks
TID15 + TID16 against `gabriele-baldoni_ddog/dark-factory-e2e`): PR stays
OPEN through Observing and merges only after observation passes;
`RequestChanges` fix-forward reuses the same open PR and branch;
`merge_commit_sha` set; `FactoryArtifact` patches recorded; Computer
Destroyed by the janitor. Two bugs were found by the live run and fixed
red-green:

- **F17 — repair-round extract lost earlier files.** `extract_command`
  diffed `HEAD~1..HEAD` (last commit only). Repair rounds stack another
  base+implement commit pair on the persisted sandbox git, so TID15's
  fix-forward publish replayed only the round-1 delta onto `base_sha` —
  the merged PR silently dropped the round-0 file. Fixed by diffing
  `git rev-list --max-parents=0 HEAD`..HEAD (cumulative vs the original
  checkout), correct for fresh and persisted sandboxes alike; test pins
  the range.
- **F18 — `FactoryArtifact` create 403.** factory.cedar had no permits
  for the new entity; the publisher's loopback POST is an Agent-principal
  HTTP call (unlike result-envelope transitions, which dispatch
  internally). Added Admin / Agent-create / read-list permits, plus the
  D7 action names (`MergeFinalized`, `CheckFinalizing`) to the existing
  lists.
- **Test-design note:** e2e scratch repos accumulate merged files across
  runs; a repair-context request for a file a previous run already merged
  is a no-op by construction. TID16's repair round still exercised F17's
  fix meaningfully: a round producing zero new changes republished the
  complete cumulative tree (the old code would have failed the round on
  an empty patch).

## Work plan

1. **Spec + module deltas (red-green)**, in order: — **DONE (2026-09-17,
   see Implementation Findings)**
   a. `factory_task.ioa.toml` + CSDL: add `RejectPlan`, `RequestChanges`
      (D3); add state `FinalizingMerge`, re-target `ObservationPassed`,
      add `CheckFinalizing` self-loop + `MergeFinalized` input action +
      nullable `merge_commit_sha` field (D7); add `FactoryArtifact`
      entity (D8). `temper verify` PASS (L0–L3 + composite).
   b. Planner folds `repair_context` into the re-plan prompt (failing
      test first); publisher splits its Merging arm into deploy-record
      (`RecordMerged(merge_sha = head_sha)`, no GitHub merge) and a new
      FinalizingMerge arm (tamper-guarded squash merge →
      `MergeFinalized`), creates the `kind="patch"` FactoryArtifact at
      publish time. Validator unchanged. Rebuild WASMs; module suites
      green.
   c. Curl-level re-verify of the reordered tail on a live server before
      the console touches it: PR must stay OPEN through Observing and
      merge only after observation passes; one `RequestChanges`
      fix-forward round reusing the same open PR.
   → GB: "if we look at the flow it may also be a bit different on where
   the PR merge happens so please have a look at that in the other
   repo." — confirmed and adopted: the DEN controller deploys the
   approved head and merges the PR only AFTER observation passes (its
   spec hint is stale); D7 realigns dark-factory to that ordering.
2. **Console port**: copy `src/` SPA into `os-apps/dark-factory/console/`,
   replace `api.js`/`temper-client.js` with a direct OData client
   (session auth, tenant header), rewire create flow (config picker +
   StartPlanning), decision cards (4 actions), activity view (Execs),
   diff view (PR link + FactoryArtifact patch pane). Port server tests
   that still apply to a thin client-side test layer.
3. **Live e2e through the browser**: boot on 3100, serve console on 8081,
   run a full task from the UI — create → plan approve → code approve →
   deploy-record → observe → finalize-merge → Completed; exercise one
   `RejectPlan`, one `RequestChanges`, and one `ObservationFailed`
   fix-forward round; record in `.proofs/0067-console-e2e.md`.
4. Follow-ups (separate ADRs if material): SSE/entity change feed
   (GB-endorsed follow-up), multi-factory dashboards, non-Admin reviewer
   role.

## Open questions

All resolved in review (GB, 2026-09-16):

- **Static serving** → temper does not serve static assets; a small
  static server on 8081 is acceptable (D6).
- **Local-mode diff surfacing** → `FactoryArtifact` entity (D8), not
  deferred. # GB: FactoryArtifcatg entity
- **Polling vs SSE** → 2–5 s polling for v1; SSE is a follow-up
  (GB-endorsed). # GB: SSE can be a follow up

## References

- ADR-0066 (dark-factory on Computer/Exec) — pipeline, CAS/operation-key
  model, module set
- `den-software-factory/demo/` — source UX (React SPA + Express BFF)
- `os-apps/dark-factory/.proofs/0066-e2e.md` — the curl-driven flow this
  console replaces with a UI
