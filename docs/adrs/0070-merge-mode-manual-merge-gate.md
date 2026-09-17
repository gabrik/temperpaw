# ADR-0070: Merge mode — auto-merge only when the repo profile allows it

## Status

Accepted (2026-09-17). Implemented red-green TDD; verified live end-to-end in
both modes — manual: task `en-01a0afa0-f09f-7560-972c-1f324117eea3` pinned
rev 4 and completed with `merge_disposition=manual`, PR #15 left open on
GitHub, console banner rendered (live DOM probe); auto: task
`en-01a0afb0-2fca-7670-845a-eb1ff8693fda` pinned rev 5 and merged PR #16
(`merge_commit_sha=9c463a58e717…`). Two integration bugs were found by the
live e2e and fixed before acceptance (snapshot builder dropping `merge_mode`;
console `toTaskView` not carrying `fields`). See
`os-apps/dark-factory/.proofs/0070-merge-mode.md`.

## Context

The ADR-0069 pipeline ends like this:

```
ApproveMerge → Deploying → Observing → FinalizingMerge → (publisher merges
the PR via the GitHub API) → Completed
```

`FinalizingMerge` **always** merges the pull request itself once the human has
approved at the merge gate. That is the wrong default for repositories where a
bot must not land code: branch-protection or compliance rules, team norms that
require a human merge, or simply a profile the operator doesn't fully trust
yet. Today there is no way to express "the factory may do everything except
press merge".

ADR-0069 made the `FactoryRepo` profile the repository's delivery contract —
it already carries the sibling authority fields `checkout_mode` (local |
github) and `publish_mode` (local | github). Merge authority is the same kind
of decision and belongs in the same place.

## Decision

**D1 — `merge_mode` joins the profile contract.** New `FactoryRepo` param
field `merge_mode` with values `"auto" | "manual"`, default `"auto"`. It is
the 22nd entry in `REPO_PROFILE_PARAM_KEYS`, so it is covered by the ADR-0069
pin snapshot and the profile digest rule unchanged; seeding a profile with a
new merge_mode bumps `profile_revision` like any other contract change. The
legacy `FactoryConfig` path has no such field and defaults to `"auto"`
(`field_or`), preserving current behavior for unmigrated configs.

**D2 — the FinalizingMerge tick branches on the pinned mode.** The existing
`CheckFinalizing` tick (`factory_publisher`) reads `merge_mode` from the
pinned profile snapshot:

- `auto` — exactly today's behavior: merge via the GitHub API, then
  `MergeFinalized(merge_commit_sha)` → `Completed`.
- `manual` — do **not** call the merge API. Transition `ManualMergeHandoff`
  (`FinalizingMerge → Completed`) and stop: the pipeline is done and the PR
  is left unmerged for a human. The task row already carries everything the
  human needs: `pull_request_url`, `head_sha`, `merge_sha`.

**D3 — manual mode completes the task; no new state, no new gate.** One new
action, mirroring `MergeFinalized` minus the merge SHA:

```
ManualMergeHandoff (FinalizingMerge → Completed)
params: merge_disposition, operation_result, expected_operation_key, expected_operation_owner
constraints: param_equals_field(expected_operation_key), param_equals_field(expected_operation_owner)
effect: trigger factory_janitor
```

The publisher dispatches it with `merge_disposition = "manual"`;
`merge_commit_sha` stays empty. Janitor cleanup runs exactly as on the auto
path. The entity trail reads: pipeline completed, merge deliberately left to
a human, PR link attached.

**D4 — the console tells the user to merge.** When a task is `Completed`
with `merge_disposition = "manual"`, the console shows a persistent banner
on the task detail: "The factory did not merge this PR — merge it manually
on GitHub: <link>". No confirm button, no SHA input, no gate: merging is a
human process step outside the factory's authority. The banner reads the
flat task field, not the pinned snapshot JSON.

**D5 — pipeline before the gate is unchanged.** Deploying and Observing still
run on the approved head in both modes; only the terminal merge step differs.
`merge_mode` does not affect `publish_mode`: the factory still opens the PR.

## State machine delta

```
add action: ManualMergeHandoff  FinalizingMerge → Completed  (factory_publisher, manual mode)
unchanged:  MergeFinalized      FinalizingMerge → Completed  (auto mode)
unchanged:  no new states, no new gates, FailTask from-list
```

## Implementation touch list

- `factory_repo.ioa.toml` + `model.csdl.xml`: `merge_mode` field/param
- `factory-common`: `REPO_PROFILE_PARAM_KEYS` → 22 keys
- `factory_task.ioa.toml`: the `ManualMergeHandoff` action above
- `factory_publisher`: mode branch in the CheckFinalizing handler
- console: manual-merge banner on Completed tasks (`merge_disposition` +
  `pull_request_url`); no STAGES/STATUS_COPY growth
- `scripts/profiles/*.json` + `bootstrap_factory_repo.py`: seed `merge_mode`
  (`"auto"` for dark-factory-e2e; `"manual"` available for den)

## Consequences

- Repositories where a bot must not merge are first-class, with minimal
  machinery: one profile field, one action, one banner.
- **`Completed` no longer implies merged.** In manual mode the trail shows
  `FinalizingMerge → Completed` with `merge_disposition = "manual"`, an
  empty `merge_commit_sha`, and the PR link. If the human never merges,
  nothing landed — later tasks branch from base without the change. That is
  the accepted trade-off of the simplified design; the evidence to detect
  it is on the task row.
- Upgrade paths remain backward-compatible if the trust trade-off ever
  needs tightening (verified confirm, or a real gate — both build on the
  same `merge_mode` field).

## Alternatives considered

- **Third gate (`AwaitingManualMerge` + `ConfirmMerged`)** — the original
  D3/D4 of this ADR: a new state where the human confirms after merging on
  GitHub, carrying the claimed merge SHA. Rejected in operator review: too
  heavy for a step the factory has no authority over; the banner +
  disposition field carry the same information with no state-machine
  growth.
- **GitHub-verified confirm** — moot under the no-gate design; a compatible
  upgrade if ever needed.
- **Bounded GitHub poll in FinalizingMerge** — moot: polling over
  event-driven, and there is no gate outcome to wait for.
- **Reuse AwaitingMergeApproval** — skip Deploying/Observing in manual mode
  and let the existing gate mean "merge it yourself". Rejected: deploy +
  observe evidence is valuable in both modes, and overloading one gate with
  two meanings makes the trail ambiguous.
