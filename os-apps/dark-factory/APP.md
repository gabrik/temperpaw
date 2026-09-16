# dark-factory

A dark factory for software tasks, built Temper-native on **paw-compute**
(ADR-0066). One `FactoryTask` plans → implements → validates → publishes →
merges → observes a single code change on its own dedicated **Computer**,
with two human gates (plan approval, merge approval) and bounded repair
loops. There is no external controller: every phase is driven by WASM
integrations wired to pollable re-check actions.

## Entities

### FactoryTask

The 11-state machine (ported from the external den-software-factory demo):

```
Requested
  → Planning            (StartPlanning)
  → AwaitingPlanApproval (SubmitPlan)        ← human gate: ApprovePlan / RejectPlan
  → Implementing        (ApprovePlan)
  → Validating          (SubmitImplementation)
  → PublishingPR        (ValidationPassed)
  → AwaitingMergeApproval (PublishPullRequest) ← human gate: ApproveMerge / RequestChanges
  → Merging             (ApproveMerge)
  → Observing           (RecordMerged)
  → Completed           (ObservationPassed)
  ↘ Failed              (FailTask / ExpireTask, from any working state)
```

Working states (`Planning`, `Implementing`, `Validating`, `PublishingPR`,
`Merging`, `Observing`) re-fire a `Check*` action every 30 s via
`state_timeout` + `reset_on` self-loop. The factory WASM module wired to that
tick performs one bounded step of the phase and reports an empty callback
when the phase is not done. Modules count their ticks and report
`ExpireTask` at the phase budget (130 ticks / 65 min for Planning and
Implementing — the Pi run budget; 60 for Validating/Observing; 30 for
PublishingPR/Merging). A module that fails hard fails its trigger and the
kernel dispatches `ExpireTask` via `on_failure`.

The task attaches 1:1 to a paw-compute **Computer** (`computer_id`). The
sandbox identity lives on the Computer row (`MachineId`); there is no
`sandbox_id` on the task.

The fencing contract is unchanged from the den demo: `operation_key` /
`operation_owner` identify the active operation; completion actions must
pass `expected_operation_key` / `expected_operation_owner` equal to the
stored fields; `plan_digest` binds plan approval to the exact submitted
plan; `head_sha` binds merge approval to the exact reviewed head.

### FactoryConfig

Config-as-entity (WebhookRoute pattern). One row per factory target, created
directly in `Active`; `Update` self-loop, `Archive` terminal.
`FactoryTask.factory_id` selects the row. Holds: `repo_url`, `base_branch`,
`checkout_mode` (`api` = GitHub Git Data API checkout from the planner WASM —
the GitHub token never reaches the Computer; `clone` = plain `git clone`
inside the sandbox, public repos only), `publish_mode` (`local` = simulate
publish/merge; `github` = real refs/PR/merge through the GitHub API),
`pi_provider` / `pi_model` / `model_env_var`, `test_commands` /
`lint_commands` / `observation_commands` (JSON argv arrays, run as governed
Exec rows), Computer sizing fields, and the bounds (`max_repair_rounds`,
`max_files_per_task`, `max_lines_per_task`).

## WASM modules

| Module | Wired to | Responsibility |
|---|---|---|
| `factory_planner` | `CheckPlanning` | Ensure the task's Computer exists and is Ready; write the model credential file (chmod 600); check out the repo (GitHub API or clone); start the Pi planning Exec (detached, marker-tagged); on completion read the plan and report `SubmitPlan` |
| `factory_implementer` | `CheckImplementing` | Start/poll the Pi implementation Exec (detached); on completion have the agent commit in the agent worktree and report `SubmitImplementation` with the frozen `head_sha` |
| `factory_validator` | `CheckValidating`, `CheckObserving` | Run `test_commands` + `lint_commands` (and `observation_commands` post-merge) one per tick as governed Execs; report `ValidationPassed`/`ValidationFailed` / `ObservationPassed`/`ObservationFailed` |
| `factory_publisher` | `CheckPublishing`, `CheckMerging` | Publish via the GitHub Git Data API with deterministic commit reproduction (exact author/committer/dates/message → identical SHA, so the `head_sha` binding survives); create ref + PR; on approval merge with the expected-sha parameter; report `PublishPullRequest` / `RecordMerged` |
| `factory_janitor` | `ObservationPassed`, `FailTask`, `ExpireTask` | Best-effort cleanup: dispatch `Computer.Destroy` for the task's computer; empty callback |

Long Pi runs are not one blocking Exec: the module starts a detached process
on the sandbox (`nohup … &`) through a short governed Exec whose command
embeds `# factory-op: <operation_key>`, then polls for the exit marker with
later short Execs. Every command is a Cedar-gated, audited Exec row; the
marker also lets an operator find the live Execs of a task with
`$filter=contains(Command, '<operation_key>')`.

## Secrets (trigger config → Cedar-scoped `access_secret`)

`temper_api_url`, `tensorlake_api_key` (+ Modal equivalents), `github_token`
(planner/implementer/publisher only), `anthropic_api_key` as `model_api_key`
(planner/implementer only — written into the sandbox env file; never passed
to GitHub). Cedar permits `http_call` / `access_secret` only when
`context.module` is one of the factory modules.

## Layout

- `specs/factory_task.ioa.toml` — the state machine, fencing contract,
  re-check actions, triggers, clocks.
- `specs/factory_config.ioa.toml` — FactoryConfig entity.
- `specs/model.csdl.xml` — OData surface (`Temper.DarkFactory`).
- `policies/factory.cedar` — Admin human gates, system-only module
  transitions, module-scoped host capabilities.
- `wasm/` — the five factory modules plus the shared `factory-common` crate.
- `adrs/` — app-scoped decisions.

## Verify

```bash
temper verify --specs-dir os-apps/dark-factory/specs   # IOA + CSDL + Cedar
os-apps/dark-factory/wasm/build.sh                     # wasm32 modules
```
