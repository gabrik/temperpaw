# ADR-0069: FactoryRepo — repository profiles are first-class entities

## Status

Accepted (2026-09-17) — implemented across factory-common, specs/CSDL/Cedar,
all six wasm modules, the console, and `bootstrap_factory_repo.py`; verified
end-to-end on the live pipeline (PR gabriele-baldoni_ddog/dark-factory-e2e#12
merged by the factory; see
`os-apps/dark-factory/.proofs/0069-factory-repo-profiles.md`).

## Context

ADR-0066 deliberately introduced `FactoryConfig` as an entity rather than an
imperative configuration process. That was the right first porting boundary,
but the entity now conflates two independent concerns:

1. **Factory-wide defaults and safety policy** — Pi defaults, Computer
   defaults, repair budgets, and global limits.
2. **A team's repository delivery contract** — source URL and branch,
   checkout/publish authority, validation, deployment, and observation
   commands, plus the credentials those operations need.

The New Request screen currently selects a `FactoryConfig`, although the
visible option is really a repository target (for example,
`https://github.com/gabriele-baldoni_ddog/dark-factory-e2e · github`). A
single global config means a team cannot safely define a second repository
with its own command contract or credentials. Editing the selected config can
also change the behavior of an in-flight task.

The factory must become a multi-team, multi-repository Temper app without
reintroducing a controller, local catalog, or ungoverned shell orchestration.
The entity-first rule applies: repository configuration is state, and work on
that state is performed by WASM integrations and governed `Exec` entities.

ADR-0066 D7 remains binding: an `Exec` row is an audit record, so secrets may
never be stored in entity fields, task action parameters, prompts, command
strings, or commits.

## Decision

### 1. Add `FactoryRepo` as the repository source of truth

Add a first-class `FactoryRepo` entity in `Temper.DarkFactory`. It represents
one team-owned repository delivery profile, not one task and not one deployed
instance.

Lifecycle:

```
Draft ──Activate──> Active ──Archive──> Archived
  │                     │
  └────Update───────────┘
```

- **Draft** profiles are editable but cannot be selected for a new task.
- **Active** profiles are selectable. Updates create a new effective profile
  revision for future tasks only.
- **Archived** profiles are never selectable, but remain readable for audit
  and for historical task snapshots.

Activation fails closed unless the source identity, branch, command contract,
and any required credential references are valid. Archiving an active profile
does not cancel existing tasks because those tasks use an immutable snapshot.

### 2. Define the repository profile contract

`FactoryRepo` has the following conceptual fields. Exact CSDL/IOA types and
names follow this ADR during implementation.

| Area | Fields | Notes |
|---|---|---|
| Identity | `repo_id`, `display_name`, `description`, `team_id` | Stable human and policy identity; the URL is not the sole identifier. |
| Source | `git_provider`, `git_url`, `base_branch`, `checkout_mode` | Provider-specific source adapter (`github_api`, later GitLab/etc.) or public clone. A private source must use an authenticated host-side adapter. |
| Publication | `publish_mode`, `publish_credential_ref` | Defines PR/ref creation and final merge behavior. `local` remains available for e2e. |
| Command contract | `validation_commands`, `build_commands`, `deploy_commands`, `observation_commands`, `preparation_commands` | Typed JSON `CommandSpec` arrays, detailed below. `validation_commands` replaces the current split test/lint shell strings. `build_commands` chain before validation in the Validating Exec — a compile failure is ordinary `ValidationFailed` evidence for the repair loop, and build artifacts persist in `/work/repo` for later phases. `preparation_commands` run on the Computer before the checkout baseline commit (install/configure what the repo needs, e.g. `rustup show` to materialize a pinned toolchain), fail-closed. Neither adds a pipeline state: preparation is part of checkout, build is part of validation. |
| Execution profile | `computer_image`, `setup_script`, `cpu_cores`, `memory_gb`, `storage_gb` | Repository-level override of global defaults: a Rust, Node, or service repository may require different tools and capacity. |
| Credentials | `source_credential_ref`, `command_secret_bindings` | Opaque references and non-secret environment-variable names only; never credential values. |
| Provenance | `profile_revision`, `profile_digest`, `created_by`, `updated_by` | Lets tasks prove exactly which non-secret profile was approved and used. |

A `CommandSpec` is an argv-based, structured command, not an arbitrary shell
string. For example:

```json
{
  "argv": ["cargo", "test", "--workspace"],
  "cwd": ".",
  "timeout_seconds": 600,
  "env": {
    "CARGO_TERM_COLOR": "never",
    "CARGO_NET_GIT_FETCH_WITH_CLI": "true"
  },
  "secret_binding_names": []
}
```

`env` is an explicit map of **non-secret** environment variables a command
needs (GB, 2026-09-17: commands legitimately carry their own env — build
flags, `CI=true`, toolchain hints). Values are static and auditable: they
appear in the profile snapshot and the Exec row, so anything sensitive is
forbidden here by construction. Names with the reserved `FACTORY_` prefix
are factory-injected context (e.g. `FACTORY_COMMIT_SHA`,
`FACTORY_DEPLOYMENT_REF`) and may not be set by a profile. Secret values are
never inlined into `env` — they are referenced through
`secret_binding_names` and injected via the 0600 env-file mechanism of §3.

A command list executes as one governed `Exec` per `CommandSpec`. This makes
output, exit status, timeout, and the command purpose independently auditable.
Checked-in repository scripts remain supported through an argv invocation
such as `["./scripts/validate.sh"]`; the factory must not use `sh -ec` to
interpret team-configured text.

### 3. Keep secret values outside entity state

`FactoryRepo` contains the *bindings required to obtain credentials*, not the
credentials themselves.

- **Source and publish credentials** are resolved only by the corresponding
  host-side provider adapter. For GitHub, this preserves ADR-0066 D7: no
  `GITHUB_TOKEN` reaches a Computer or an Exec command. A generic private
  Git provider requires a Temper-native source adapter before it can be
  activated; embedding a token in a clone URL is prohibited.
- **Deploy and observation credentials** are resolved by a platform secret
  resolver from the opaque `command_secret_bindings` references. Immediately
  before the relevant governed Exec, its integration writes only that command
  set's environment file with mode `0600`, runs the argv command, redacts
  known secret values from captured output, and deletes the file on success,
  failure, or cleanup.
- The implementation must extend the Temper secret/credential primitive where
  necessary. It must not create a local encrypted file, a side database, or a
  bespoke imperative secret broker in `crates/temperpaw/`.
- The Pi planner/implementer never receives deploy or observation bindings.
  A deployment secret exists only for the bounded deploy exec, not while the
  agent is running.

Cedar authorizes resolution by both module and purpose: source credentials to
the source/publisher adapter; deploy bindings only to the deploy integration;
observation bindings only to the observer/validator integration. Console
reads may expose profile metadata and opaque reference identifiers, but never
secret values.

### 4. Pin the selected profile onto each FactoryTask

Replace the user-facing `FactoryTask.factory_id` selection with
`FactoryTask.factory_repo_id`.

The New Request console sends only the selected active `FactoryRepo` ID and
the request text. It does **not** submit a URL, command data, or a profile
snapshot. On the first task transition, a WASM integration reads the active
profile, validates authorization, and records a non-secret immutable
`repo_profile_snapshot` plus `repo_profile_digest` and `repo_profile_revision`
on the task.

Every later phase reads the task snapshot, not the mutable `FactoryRepo` row.
Consequently:

- changing a profile affects future tasks only;
- a task cannot silently switch repository, branch, gates, or deployment
  target while a human gate is pending;
- the snapshot contains opaque credential references but no secret values;
- task/provenance evidence can identify the exact repository contract used.

### 5. Make deployment a real governed phase

Add a repository-defined deployment path to the task state machine:

```
Plan → Implement → Validate → Publish PR → human code gate
     → Deploy → Observe → Finalize Git merge → Completed
```

This clarifies an existing semantic mismatch: the current `Merging` stage
records a reviewed head as deployed so it can be observed before final GitHub
merge. Under this ADR:

1. approval of the code gate starts **Deploying**, not an ambiguously named
   merge-like state;
2. a `factory_deployer` WASM integration executes the selected profile's
   `deploy_commands` as governed Execs and records an auditable deployment
   identity/reference on `FactoryTask`;
3. only successful deployment starts **Observing**;
4. `observation_commands` run against that deployment. The integration passes
   non-secret `FACTORY_COMMIT_SHA` and `FACTORY_DEPLOYMENT_REF` context;
5. only a successful observation authorizes the publisher to perform the
   actual final Git merge.

A deployment failure fails closed: it never merges the PR and preserves its
Exec evidence. Retrying a non-idempotent deploy, selecting a different
environment, or repairing code after a deployed failure requires an explicit
human transition; it is not an autonomous retry loop.

This ADR models one safe default deployment target per repository profile.
Multi-environment promotion (staging/production with separate approvals) is
explicitly deferred to a future `FactoryEnvironment` entity; it must not be
hidden in free-form command text.

### 6. Console behavior

The New Request selector lists active `FactoryRepo` rows rather than
`FactoryConfig` rows. Each option displays:

```
<display name> — <git URL> · <provider>
```

The screen does not accept an arbitrary URL. A team first creates and
activates a profile through a managed repository configuration surface, then
selects it for tasks. Task detail displays the pinned repository name, URL,
branch, profile revision, and digest so reviewers can identify the target.

Existing live activity behavior from ADR-0068 remains unchanged: repository
checkout, validation, deployment, and observation each appear as governed
Exec activity with in-flight output tails.

### 7. Retain FactoryConfig for global policy only

`FactoryConfig` remains as the factory-wide configuration entity, but it no
longer owns a repository target or repository-specific scripts. It retains:

- factory-wide Pi provider/model defaults and model credential binding;
- default Computer image/setup/resources and concurrency limits;
- repair/file/line budgets and global command/provider allowlists;
- factory-wide retention and safety policy.

Repository-specific compute fields override these defaults only when the
selected active `FactoryRepo` profile permits them under the global allowlist.

### 8. Migration and compatibility

1. Introduce `FactoryRepo`, its CSDL model, Cedar policy, managed UI, and
   profile snapshot action.
2. Convert each existing active `FactoryConfig` target into a corresponding
   `FactoryRepo`; for example, the current dark-factory-e2e cargo contract
   becomes one active profile. Declarative seed profiles live in
   `os-apps/dark-factory/scripts/profiles/*.json` and are applied by
   `os-apps/dark-factory/scripts/bootstrap_factory_repo.py` (idempotent:
   find-or-create, then update, then read-back verify). The script targets
   `FactoryConfig` during the compatibility window and switches to
   `FactoryRepo` when it lands. It seeds `dark-factory-e2e` (scratch test
   repo, cargo gates) and `den` (`ddoghq/den`, private, cargo workspace
   gates, 8 CPU / 16 GB — its rust-toolchain.toml pins nightly-2026-04-30,
   which rustup auto-installs on the Computer). The full local stack —
   secrets, WASM build, temperpaw server, console user, profiles, console
   preview — is one command: `os-apps/dark-factory/scripts/bootstrap.sh`
   (idempotent; reuses healthy services, registers the first account on a
   fresh deployment). After it finishes a user opens the console, picks a
   repository, and describes the task.
3. Add `factory_repo_id` and profile snapshot fields to new tasks. Existing
   tasks keep their legacy `factory_id` path until terminal, then remain
   historical records.
4. Migrate current `test_commands`, `lint_commands`, and
   `observation_commands` shell strings to typed argv command lists. Reject
   profiles that retain arbitrary shell text after the compatibility window.
5. Add `Deploying` and the deploy integration before enabling
   `deploy_commands` on any Active profile.
6. Remove repository/source/publish/gate fields from `FactoryConfig` only
   after every active task and profile has migrated.

No migration may retarget an active task. All conversion is recorded by
entity actions and included in the proof/evidence report.

## Alternatives considered

### Keep one FactoryConfig per repository

Rejected. The current selector already approximates this, but the name and
schema still mingle global and team-specific policy. It does not provide a
first-class team/repository identity, a profile lifecycle, per-profile
credentials, immutable task pinning, or a clean deployment contract.

### Put a git URL and scripts directly on FactoryTask

Rejected. It allows ad hoc targets and command injection at request time,
makes secret authorization impossible to govern centrally, and removes the
team-maintained configuration entity that must be reviewed before use.

### Store tokens and deploy keys encrypted in FactoryRepo fields

Rejected. Encryption at rest does not prevent entity/API/Exec disclosure and
violates ADR-0066 D7. The entity stores opaque references; the platform secret
primitive owns values and access control.

### Keep shell strings for flexibility

Rejected. Team-owned scripts are supported, but they must be checked into the
repository and called as argv. Typed commands provide deterministic quoting,
per-command evidence, and a practical Cedar/allowlist surface.

### Make a single generic deploy command run after final merge

Rejected. The factory's safety property is observe-before-final-merge.
Deploying the reviewed PR head, observing it, and merging only after health is
proven preserves that property and gives deploy failure a safe fail-closed
outcome.

## Consequences

### Positive

- Teams can independently register and maintain repository delivery profiles.
- Each task is auditable and reproducible against an immutable selected
  profile, even as the team updates its profile for future work.
- Real validation, deploy, and observation commands are first-class governed
  work rather than convention hidden in factory-wide fields.
- Per-purpose secret scoping improves on a single global GitHub token while
  retaining the no-secret-in-Exec boundary.
- The console's repository selector matches what users believe it selects.

### Costs and risks

- This is a material model and workflow migration: IOA/CSDL, factory WASM,
  Cedar, console, secret resolver, and proof coverage all change.
- A provider-neutral private-source adapter and dynamic secret references may
  require a coordinated Temper platform extension. That extension is a
  dependency, not a reason to bypass the entity/trigger boundary.
- Structured argv migration is intentionally less permissive than the current
  shell-string implementation and will require teams to move glue logic into
  checked-in scripts.
- Deployment is safety-critical; it needs dedicated failure, retry, approval,
  and environment policy tests before any production profile is activated.

## Verification required before implementation is complete

1. **Red-green specs/tests:** entity lifecycle/invariants, active-only task
   selection, snapshot immutability, command-spec validation, and Cedar
   denial tests for every forbidden secret/module/purpose combination.
2. **Secret-boundary tests:** prove secret values never occur in FactoryRepo,
   FactoryTask, Exec rows, prompts, logs, patches, or browser responses;
   prove deploy/observe environment files are scoped and deleted.
3. **End-to-end flow:** configure two active repositories with different Rust
   and Node command contracts; create tasks from each; prove the correct
   profile and gate commands run; deploy/observe only the selected repository
   contract; reject a failing deployment without Git merge.
4. **Snapshot regression:** update/archive a selected profile while a task is
   at each human gate; prove the task continues on its recorded snapshot and
   no new task can select an archived profile.
5. **Console:** use Playwright to select a repo, confirm its pinned profile in
   task detail, and observe live checkout/validation/deploy/observation Exec
   tails through SSE.
6. **Proof and production:** record the workflow/OData transitions in
   `os-apps/dark-factory/.proofs/` and, after merge/publish to Genesis, verify
   the deployed profile behavior on Railway with Datadog.
