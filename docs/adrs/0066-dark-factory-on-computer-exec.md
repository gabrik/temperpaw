# ADR-0066: Dark factory on Computer and Exec — controller removal

Date: 2026-09-16
Status: Proposed
Written in ASD-STE100 Simplified Technical English.

## Context

The `den-software-factory` repository holds a Temper app and a demo stack.
The app has one entity: `FactoryTask`. It has eleven states, two human gates,
and four repair loops. The contract is good. We keep it.

The demo stack has a Node.js controller (about 1130 lines). The controller
polls Temper. It does all machine work:

- It makes one Tensorlake sandbox for each task (`tl sbx create`).
- It runs the Pi agent in the sandbox (plan and implementation).
- It runs git and `gh` commands (commit, push, pull request, merge).
- It runs the validation and observation command sets.
- It holds the secrets (Anthropic key, GitHub token, Tensorlake key).
- It fences each operation with `operation_key` and `operation_owner`.

The controller breaks the entity-first rule (ADR-0005). The orchestration
logic hides in imperative code. The audit test fails: you cannot understand
the flow from entity transitions alone. The controller is also a second
execution surface next to the platform surface.

TemperPaw already has the governed compute surface in `paw-compute`:

- `Computer`: the lifecycle of a long-lived VM. `Sleep`, `Wake`, and
  `Destroy` already have WASM triggers. Copies and leases exist.
- `Exec`: one governed command on a Computer. ADR-0004 made Exec
  asynchronous (`Created → Starting → Running → Succeeded|Failed`, with a
  poll loop and a deadline). A command can run longer than one WASM
  invocation. This removes the old limit for long Pi runs.
- `paw-patrol`'s `Effort` already attaches `computer_id` and does the work
  through Computer/Exec. The pattern is established.

One gap remains: `Computer.Provision` has no WASM trigger. Only paw-agent
session code provisions a fresh sandbox today. The factory must not depend
on session code.

In this checkout, `os-apps/den-software-factory` is only an untracked
symlink to the external repository. The port makes it a real application.
The application is generic: a dark factory for any repository. DEN is
only the first target repository. So the app name is `dark-factory`,
not `den-software-factory` (decision: GB, 2026-09-16).

## Decision

### D1. Port the factory app into TemperPaw as `dark-factory`

Create `os-apps/dark-factory/` as a real directory. Remove the symlink.
Port `factory_task.ioa.toml`, `model.csdl.xml`, and
`factory_task.cedar`. Set `name = "dark-factory"` in `app.toml`.
The entity name stays `FactoryTask`. Nothing in the app is DEN-specific:
the target repository is configuration (see D6).

Keep these contract parts unchanged:

- The eleven states and the two human gates.
- The `plan_digest` binding on plan approval.
- The `head_sha` binding on merge approval.
- The four repair loops and the terminal invariants.

### D2. Attach paw-compute: task 1:1 Computer, Computer 1:N Exec

Each `FactoryTask` owns exactly one `Computer` for its full lifetime.
Add a `computer_id` field to `FactoryTask`. The `sandbox_id` field moves
to the Computer row (`machine_id`, `sandbox_url` live there).

All commands on that Computer go through `Exec` entities. One Computer has
many Execs. The Exec rows are the audit trail: command, exit code, output
tails, full log path.

Duration warning (from operations): Exec had timeout problems in the
past. This ADR takes that risk seriously. ADR-0004 made Exec
asynchronous, so a command can outlive one WASM invocation. D4 and the
work plan set a bounded per-Exec timeout of 60 minutes for Pi runs
(decision: GB, 2026-09-16). The end-to-end proof must show a Pi run
longer than 120 seconds that still completes.

The task keeps its Computer across repair rounds. The Pi session id
(`operation_key`) stays valid. At `Completed` or `Failed`, a factory WASM
module dispatches `Computer.Destroy`. The Exec rows stay as the record.

### D3. Remove the controller. Phases become WASM integrations

Delete the controller process, its runtime file store, and
`FACTORY_EXECUTION_MODE`. Each controller phase becomes a WASM integration
on a `FactoryTask` action, in `os-apps/dark-factory/wasm/`:

| Controller phase (old) | New mechanism |
|---|---|
| Make a sandbox for each task | `factory_planner` creates and configures the Computer, dispatches `Provision` |
| Run Pi for the plan | Exec on the task Computer: `pi --print --approve --tools read,grep,find,ls --session-id <operation_key> <prompt>` |
| Run Pi for the implementation | Exec on the task Computer: `pi --print --approve --session-id <operation_key> <prompt>` |
| Import agent files, commit in the trusted worktree | Execs that run git on the Computer (the two-worktree boundary stays, inside the sandbox) |
| Run the validation command set | One Exec for each command in `FactoryConfig.validation_commands` |
| Push, create the pull request | `factory_publisher` reads the exact commits from the trusted worktree (Exec), then calls the GitHub API with `http_call` (see D7) |
| Merge the approved head | `factory_publisher` calls the GitHub merge API with the expected `head_sha` (`http_call`) |
| Run the observation command set | One Exec for each command in `FactoryConfig.observation_commands` |
| Fence and retry operations | `operation_key`/`operation_owner` stay as entity fields (see D5) |
| Tear down the sandbox | `factory_janitor` dispatches `Computer.Destroy` at terminal states |

Self-reporting rule: the factory WASM modules dispatch the next
`FactoryTask` action (`SubmitPlan`, `SubmitImplementation`,
`ValidationPassed`, and so on). When a module starts an Exec, it arms a
bounded re-check action on the FactoryTask (delay, `check_count`,
`max_checks` — the platform pattern). The re-check reads the Exec row.
On `Succeeded`, it dispatches the next action. On `Failed`, it dispatches
the matching repair or `FailTask`. No external watcher exists.

Suggested modules (small, one phase each, `*_lifecycle` naming as in
paw-patrol): `factory_planner`, `factory_implementer`,
`factory_validator`, `factory_publisher`, `factory_janitor`.

### D4. Close the provisioning gap in paw-compute

Add a `computer_provision` WASM trigger on `Computer.Provision`. The
module does what the controller's provisioner did, as a platform
primitive:

1. Create the sandbox through the provider abstraction
   (`wasm_helpers::sandbox`, the same path Exec uses).
2. Run the Computer's `setup_script` (agent user, repository clone, Pi
   install, environment file).
3. Dispatch `ProvisionComplete(machine_id, sandbox_url, ssh_host)`.

This change is in `paw-compute`, not in the factory app. Other apps get
the same primitive.

Also in `paw-compute`: extend `computer_exec_start` with a bounded
`timeout_seconds` parameter. The factory sets 60 minutes for Pi Execs
and a smaller value for command Execs (see Resolved questions).

### D5. Keep the fencing fields for the port

Keep `operation_key`, `operation_owner`, and the
`param_equals_field` constraints in the ported spec. The WASM modules set
the key at phase start and pass the expected values at phase end. The key
also serves as the Pi session id. A later ADR can remove fencing that the
WASM dispatch path makes redundant. Do not mix that simplification into
the port.

### D6. New config entity: `FactoryConfig`

Add a `FactoryConfig` entity (the config-entity pattern, like
`WebhookRoute`). One row holds the factory settings:

- `repo_url`, `base_branch`
- `computer_image`, `computer_cpus`, `computer_memory_mb`,
  `computer_disk_mb`
- `pi_package`, `pi_model`
- `validation_commands`, `observation_commands` (JSON arrays of argv
  arrays; no shell evaluation)
- `publish_mode` (`local` or `github`), `max_repair_rounds`

The factory WASM modules read this row. There is no imperative config
process.

### D7. Secrets stay host-side and on the Computer

The Exec row is an audit record. A secret inside an Exec command is a
leak. So:

- `TENSORLAKE_API_KEY` goes through the existing trigger config overlay
  (`{secret:tensorlake_api_key}`) for `computer_provision`,
  `computer_exec_start`, and `computer_exec_poll`.
- The model provider credential (Codex subscription/OAuth first, per the
  project rules; `ANTHROPIC_API_KEY` only when told) is written by
  `computer_provision` into an environment file on the Computer
  (`chmod 600`, agent user only). It never appears in an entity field or
  an Exec command. The Pi Exec sources the file.
- `GH_TOKEN` never reaches the Computer (decision: GB, 2026-09-16).
  All GitHub control-plane work (create refs, create the pull request,
  read the pull request, merge with the expected head SHA) runs as
  `http_call` from `factory_publisher`. The token lives only in the
  module config overlay (`{secret:github_token}`). Cedar scopes
  `http_call` to `context.module == "factory_publisher"`. The Git Data
  API reproduces the reviewed `head_sha` exactly, so the merge gate
  binding still holds. In `local` publish mode no GitHub credential
  exists at all.
- Local development reads these values from `demo/.env` in the external
  repository and puts them into the trigger config overlays and the
  provisioning step. Nothing secret enters a spec, a row, or a commit.

### D8. Cedar split

The human (`Customer`) creates the task, dispatches `StartPlanning`, and
holds the two gates (`ApprovePlan`, `RejectPlan`, `ApproveMerge`,
`RequestChanges`). The machine actions are dispatched only through the
WASM path (system principal), as with the Exec callbacks in ADR-0002.
The `factory-controller` agent type disappears from the policy.

## Consequences

Good:

- The audit test passes. FactoryTask rows, Computer rows, and Exec rows
  show the complete flow. No logic hides in a process.
- Every command is Cedar-gated at `Exec.Run` and auditable after the
  fact.
- Provisioning becomes a platform primitive, not demo JavaScript.
- Async Exec (ADR-0004) already supports Pi runs longer than one WASM
  invocation.
- The two-worktree trust boundary stays: the in-sandbox agent has no
  Temper credential. Only the factory WASM modules create Execs.

Costs and risks:

- One Computer per task costs more than one shared sandbox. Sleep and
  Destroy at terminal states bound the cost.
- The long Pi run needs a per-Exec timeout larger than the current
  default: 60 minutes for Pi Execs (decision: GB, 2026-09-16).
  `computer_exec_start` must accept a bounded timeout value. This is a
  small `paw-compute` extension. Past Exec timeout problems make the
  step-3 proof mandatory, not optional.
- `paw-compute` changes (`computer_provision`, Exec timeout) affect all
  users of the app. They need their own tests and proofs.
- The React console and the loopback UI proxy are ported into TemperPaw
  (decision: GB, 2026-09-16). They keep calling the same Temper
  HTTP/OData endpoints, now served by TemperPaw.

## Work plan (red-green TDD, end-to-end proof)

1. **Port.** Move the specs into `os-apps/dark-factory/`. Remove
   the symlink. `temper verify` passes. No behavior change.
2. **Provision.** Add `computer_provision` to `paw-compute` (failing
   test first). Prove: create Computer, `Provision`, row becomes `Ready`
   with a real Tensorlake sandbox.
3. **Exec timeout.** Add the bounded timeout (60 minutes for Pi) to
   `computer_exec_start`. Prove: a Pi run longer than 120 seconds
   completes within the bound.
4. **Planner.** `factory_planner` with the config entity. Prove: dispatch
   `StartPlanning`, get `AwaitingPlanApproval` with a real plan from Pi.
5. **Implementer and validator.** Prove: approve the plan, get a commit
   and a validation result, all visible as Exec rows.
6. **Publisher and janitor.** Prove the full path to `Completed` in
   `local` publish mode, then in `github` mode (token in the module
   config overlay, sourced from `demo/.env`). Computer is destroyed at
   the end.
7. **Console.** Port the React console and the loopback UI proxy into
   TemperPaw. Prove: approve a plan and a merge from the console, with
   the calls served by TemperPaw endpoints.
8. **Record.** `.proofs/` report with OData queries of the state
   transitions. Publish the apps to Genesis. Verify the pinned refs.
   Verify live behavior through Datadog.

## Resolved questions (GB, 2026-09-16)

1. Pi Exec timeout: **60 minutes**. The FactoryTask re-check cadence is
   30 seconds, bounded to 130 checks (65 minutes), then `FailTask`.
2. GitHub operations: **`http_call` from WASM**, so the token stays
   under platform control (see D7). Not `gh` Execs on the Computer.
3. Human approval surface: **port the React console**. It keeps calling
   the same Temper HTTP/OData endpoints, now served by TemperPaw (see
   work plan, step 7).
4. Two-worktree (agent/trusted) split inside one sandbox: **keep it**.
5. Application name: **`dark-factory`**. DEN is the first target
   repository, not part of the app identity.
6. Source of truth for the app after the port: **TemperPaw holds it**
   (decision: GB, 2026-09-16). Genesis holds the installs (project
   rule). The external repository keeps the original demo for history.
   Nothing syncs back.

## Open questions

None. Implementation details live in the work plan.

## Implementation Deviations & Findings (2026-09-16, post-implementation)

Implemented and live-e2e-verified end-to-end (both publish modes) on a local
temperpaw server with real tensorlake computers and a real GitHub scratch repo.
Deviations from the design above, all behaviour-preserving unless noted:

1. **Checkout via Git Data API, not codeload tarball.** The WASM SDK `http_call`
   surface is String-only, so tarball bytes cannot cross the boundary. Planner
   and implementer fetch the tree recursively + base64 blobs and write files via
   the sandbox file API. Same resulting `/work/repo` (git init + add + commit).
2. **Publish replays full file contents, not a diff.** The implementer records
   `git diff --name-status HEAD~1 HEAD` into `/work/changed-files.txt`; the
   publisher re-reads those files from the sandbox and replays them through the
   Git Data API (blob/tree with `base_tree` -> commit on base -> ref create with
   one force-update retry -> PR create, reusing an existing open PR for the
   branch). `/work/changes.patch` remains the provenance artifact, truncated
   into the PR body.
3. **`published_sha` pins the GitHub-side head (F16).** The publisher's commit
   sha cannot equal the approved sandbox head sha (same tree, different
   metadata), so `ApproveMerge`'s `head_sha` CAS alone cannot protect the merge.
   New FactoryTask field `published_sha`; the merge tick merges the PR's live
   head but FailTasks when it diverges from `published_sha`. Merge is squash.
4. **Repair loops re-base on `merge_sha` when set (F15).** Post-merge repairs
   start from the merged content, not the frozen `base_sha`.
5. **Planner creates its own Computer** (deterministic name) when none is
   attached and binds it via a new `AttachComputer` CAS action; FactoryConfig
   gained `computer_image` (tensorlake requires a non-empty base_image).
6. **WASM guest contract (F11):** entry is `run(ctx_ptr: i32, ctx_len: i32)`;
   a 1-arg extern traps in ~2 ms with zero guest logs. `ctx.entity_state` wraps
   state as `{"fields": {...}, "counters": {...}}`; the status key is lowercase
   `"status"` (a wrong-case read once defaulted the publisher into a
   force-push republish loop).
7. **Module error logging (F9):** guest `set_error_result` writes NO log line;
   every factory module logs the error text via `ctx.log("error", ...)` first.
   (Also F10: every guest_log line is emitted exactly twice - cosmetic.)
8. **pi install race (F12):** implementer/planner prefix pi invocations with a
   bounded `command -v pi` wait loop; FactoryConfig field is `setup_script`
   (unknown keys are silently dead).
9. **Observing runs in the validator**, merge runs in the publisher (both
   branch on `ctx.entity_state.status`). `ObservationFailed` re-enters
   Implementing with `repair_round` incremented and a fresh operation key.
10. **Model API key naming:** modules read the key from the config secret slot
    `model_api_key` and write it into `/run/factory/env` under the FactoryConfig
    `model_env_var` name; GH_TOKEN never reaches the computer (github mode uses
    module-side `http_call` only).

Open platform findings (temper gaps, follow-up candidates): **F13**
state_timeout does not survive a server restart (phases stall until a Check
action is nudged); **F14** tensorlake sandbox death is invisible to the Computer
row (no liveness probe) and nested exec dispatch errors propagate synchronously
into the calling module (a dead sandbox hard-expired a mid-repair task).
Factory-side reprovision-on-dead-sandbox is future work; today it fails loudly.

E2E evidence: `os-apps/dark-factory/.proofs/0066-e2e.md`.
