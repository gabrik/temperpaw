# Proof — ADR-0070 merge_mode: manual merge authority

Date: 2026-09-17. Branch: `claude/paw-compute-access` (local; unpushed).

## What was built

- **Profile contract**: `merge_mode` (`"auto"` default | `"manual"`) is the 22nd
  `REPO_PROFILE_PARAM_KEYS` entry — pinned into the task snapshot and covered by
  the single digest rule exactly like every other profile param.
  - `specs/factory_repo.ioa.toml`: `merge_mode` field (initial `"auto"`), added to
    Revise/Update params.
  - `specs/model.csdl.xml`: `FactoryRepo.merge_mode` property + Revise/Update
    parameters; `FactoryTask.merge_disposition` property.
  - `scripts/bootstrap_factory_repo.py`: `merge_mode` in `REPO_PARAMS` (drift
    detection + digest payload).
  - `scripts/profiles/dark-factory-e2e.json` → `"auto"`;
    `scripts/profiles/den.json` → `"manual"`.
- **Spec**: `FactoryTask.merge_disposition` field; `ManualMergeHandoff`
  (FinalizingMerge → Completed) — params `merge_disposition, operation_result,
  expected_operation_key, expected_operation_owner`, `param_equals_field` fences,
  janitor trigger. No new state, no new gate (operator direction).
- **factory_publisher** (red-green, +2 tests → 9): `merge_mode_is_manual`
  (absent/empty/`auto` → auto; only `"manual"` diverts — legacy FactoryConfig
  rows unchanged), `handoff_params` (disposition + fences; no merge SHA, no
  fresh op key). `CheckFinalizing` branches on the **pinned** profile's
  merge_mode after the local-publish early return: manual logs the handoff and
  dispatches `ManualMergeHandoff`; auto falls through to the existing
  merge + `MergeFinalized` path.
- **Console** (red-green, +1 test → 46): `manualMergeBanner(fields, status)`
  returns the PR URL only for `Completed` + `merge_disposition == "manual"`;
  `StageRail` renders a persistent amber banner linking the PR on GitHub.

## Test evidence (red-green)

- factory-common 26 green (`repo_profile_digest_payload_includes_merge_mode`
  failed red before the key existed).
- factory_publisher 9 green (mode helper + handoff params failed red as
  compile errors before implementation).
- Console 46 green (`manualMergeBanner` suite failed red on import).
- Full factory wasm suite: 91 green; `sh wasm/build.sh` clean; server restart
  reconciled `updated=["FactoryRepo","FactoryTask"]`, all 6 wasm modules loaded.

## Live e2e — manual mode (dark-factory-e2e profile temporarily seeded manual)

Profile re-seeded with merge_mode drift → **rev 4**, digest `sha256:7d8c51e4f5dba505…`.

### Bug found by e2e #1 (unit tests missed it)

First manual run (task `en-01a0af95-488e-7c31-861e-3d0b60a441b3`, prompt:
`pub fn farewell`) **merged PR
[#14](https://github.com/gabriele-baldoni_ddog/dark-factory-e2e/pull/14)
(`merge_commit_sha=24aa886a10a5…`) instead of handing off**. Root cause: the
planner pinned rev 4 with the correct 22-key digest, but
`build_profile_snapshot` copies a **hardcoded key list** that did not include
`merge_mode` — so the publisher read merge_mode as absent → defaulted `auto`
→ merged. Adding the key to `REPO_PROFILE_PARAM_KEYS` (digest coverage) was
not enough; the snapshot builder is a second, independent enumeration.

Fix (red-green): both snapshot tests extended with merge_mode assertions
(failed red), then `"merge_mode": or(r("merge_mode"), "auto")` added to
`build_profile_snapshot` after `publish_mode`. factory-common 26 green;
all modules rebuilt; server restarted.

### Run 2 (post-fix)

Task `en-01a0afa0-f09f-7560-972c-1f324117eea3` (prompt: add `pub fn shout` + unit test),
driven by `/tmp/e2e-0070-manual.py` (auto-approves both gates). Completed in **585s**:

```
Status:             Completed
merge_disposition:  manual
merge_commit_sha:   (empty)
pull_request_url:   https://github.com/gabriele-baldoni_ddog/dark-factory-e2e/pull/15
operation_result:   manual merge: PR left unmerged for a human (ADR-0070)
published_sha:      da3e94542c18e4fd844743ae6359898f3cdfe992
pin:                rev 4 · sha256:7d8c51e4f5dba505…  (snapshot carries merge_mode=manual)
```

GitHub-side: **PR #15 OPEN and unmerged**, `headRefOid == published_sha` — the
factory opened the PR, observed head, and deliberately did not merge.

### Bug found by live console e2e #2 (unit tests missed it)

First console check showed **neither the banner nor the profile pin** on the
Completed task. Root cause: `toTaskView` flattened individual fields but never
carried `fields` itself onto the view, so StageRail's `task.fields ?? {}` was
always `{}` — pin and banner both dead. (This also retro-fixes the ADR-0069
pinned-profile header, which had only ever been unit-tested, never rendered live.)
Fix (red-green): `assert.deepEqual(task.fields, taskRow.fields)` in the
toTaskView test (failed red), then `fields` added to the view. Console 46 green.

Live DOM probe after rebuild (`/tmp/console-e2e/dom-probe.mjs`):

```
profile-pin: "profile rev 4 · 7d8c51e4f5db"
merge-banner: "⚠ Manual merge — the factory did not merge this PR. Merge it on GitHub:
               https://github.com/gabriele-baldoni_ddog/dark-factory-e2e/pull/15"
```

Screenshot: `/tmp/console-e2e/shots/0070-manual-banner.png`.

## Live e2e — auto regression (profile restored to auto)

Profile re-seeded to `merge_mode: "auto"` → **rev 5**, digest
`sha256:b9bb1a362ceee31f…` (seeder reported `Update (drift: merge_mode)`).
Task `en-01a0afb0-2fca-7670-845a-eb1ff8693fda` (prompt: add `pub fn whisper` +
unit test) completed in ~640s:

```
Status:             Completed
merge_disposition:  (empty)
merge_commit_sha:   9c463a58e717…
pull_request_url:   https://github.com/gabriele-baldoni_ddog/dark-factory-e2e/pull/16
pin:                rev 5 · sha256:b9bb1a362ceee31f…  (snapshot carries merge_mode=auto)
```

Auto behavior is unchanged: the factory merged PR #16 exactly as pre-0070.

## Summary

| Mode | Pin | PR | Disposition | merge_commit_sha | Banner |
| --- | --- | --- | --- | --- | --- |
| manual | rev 4 | #15 open (unmerged) | `manual` | empty | rendered + verified live |
| auto | rev 5 | #16 merged | empty | `9c463a58e717…` | none (correct) |
