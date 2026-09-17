# Proof — ADR-0069 FactoryRepo profiles (implementation slices 1–8)

Date: 2026-09-17. Branch: `claude/paw-compute-access` (local; unpushed).

## What was built

- **factory-common**: `profile_commands` (CommandSpec-array render, legacy-string
  fallback, malformed JSON fails loud), `factory_context_env`, `build_profile_snapshot`
  (+ `build_commands` / `preparation_commands` passthrough), and the **single digest
  rule**: `repo_profile_digest_payload` (sort-keyed compact JSON of the 21 profile
  params + `profile_revision`, counter numbers normalized to strings) hashed as
  `sha256:<hex>`. Mirrored byte-for-byte in `scripts/bootstrap_factory_repo.py`.
- **Specs/CSDL/Cedar**: `factory_repo.ioa.toml` (Draft→Active→Archived, Revise/Update
  bump `profile_revision`, writer-computed `profile_digest`), `factory_task.ioa.toml`
  (Deploying state, `factory_repo_id`/pin fields, `PinRepoProfile`), `model.csdl.xml`,
  `factory.cedar` (FactoryRepo permits, `factory_deployer` in module lists).
- **Modules** (red-green TDD):
  - planner 13 tests — pin branch (Active-only, digest from row else canonical fallback),
    `pick_setting` precedence profile > config > default, `checkout_command(preparation)`
    runs `preparation_commands` before the baseline commit, fail-closed (`|| exit 1`).
  - validator 20 tests — `phase_command` chains `build_commands && validation_commands`
    (build-only allowed); compile failure becomes ordinary `ValidationFailed` evidence
    for the repair loop. No new pipeline state for build (ADR decision).
  - deployer 8 tests — new module owns Deploying; empty `deploy_commands` = explicit
    passthrough (legacy record-only behavior); failure is fail-closed (no auto-retry).
  - publisher 7 tests — Merging branch + `deploy_record_params` removed; Deploying
    guarded as deployer-owned.
  - factory-common 25 tests. Total factory wasm: 87 green.
- **Console** (45 tests green, vite build clean): STAGES `Merging`→`Deploying`,
  `api.listRepos` (Active only), create flow prefers FactoryRepo profiles
  (`factory_repo_id` sent on StartPlanning; legacy configs still selectable),
  task detail header shows `profile rev N · <digest12>` via `pinnedProfileSummary`.
- **Bootstrap**: `bootstrap_factory_repo.py` seeds FactoryRepo rows
  (create → Revise/Update with writer-computed digest → Activate), verifies by
  read-back polling, idempotent (digest compared at the row's current revision;
  a revision-N+1 digest would always drift).

## Live verification (localhost:3100, tenant default)

- Seeded `dark-factory-e2e` repo `en-01a0af51-9dd9-77b3-9925-a85a390f4953` (rev 2)
  and `den` repo `en-01a0af51-cffc-71f0-8982-48144aaed1d9` (rev 1), both Active.
- Digest recomputed from the live row == stored digest (Python canonical rule).
- Re-running bootstrap = no-op (rev stable) — idempotency proven.
- **Pin probe**: task `en-01a0af56-7545-71f1-b348-8c88e22580ef` StartPlanning with
  `factory_repo_id` → first planner tick pinned `repo_profile_revision: "2"`,
  `repo_profile_digest: sha256:eff9bad4…` — exactly the row's digest, proving the
  Rust fallback/row path and Python writer agree. Snapshot contains
  `build_commands` / `preparation_commands`. Probe task then FailTask'd (Status: Failed).
- Found+fixed live: counter fields project as JSON numbers — `repo_profile_revision`
  tolerates string-or-number (previously pinned "0").

## Full pipeline e2e (2026-09-17, localhost:3100, FactoryRepo path)

Task `en-01a0af62-5c2c-7192-ab89-539f14e88c13` on the `dark-factory-e2e`
profile (rev 3), driven end-to-end by `/tmp/e2e-0069-monitor.py` (auto-approves
both human gates with field-matching digests):

```
Planning(225s) → AwaitingPlanApproval → Implementing → Validating →
PublishingPR → AwaitingMergeApproval → Deploying → Observing →
FinalizingMerge → Completed   (585s total)
```

Verified evidence:
- Pin: `repo_profile_revision=3`, digest `sha256:d2d6c050…` on every transition.
- Preparation: checkout exec ran `FACTORY_PREP_RAN && rustc --version` on the
  Computer (exit 0) before the baseline commit.
- Build+validate chain: Validating execs ran `cargo build` then `cargo test`
  (600s, env-prefixed CommandSpec renders), exit 0.
- Publish: real PR — github.com/gabriele-baldoni_ddog/dark-factory-e2e/pull/12
  (published_sha 68afce46, branch darkfactory/en01a0af625c2c71-r0).
- Deploying: passthrough (empty deploy_commands) — `merge_sha=head_sha`,
  deployment_ref empty, as designed.
- Observing: observation_summary = validation output, exit 0.
- Merge: **PR #12 merged=true, merge_commit_sha=37440ee4… == task row == main
  HEAD**; `pub fn greet` + `greet_returns_hello_name` verified live on `main`.

### Bugs found by this e2e (unit tests had not caught them)

1. `factory_implementer` still required `factory_id` and loaded FactoryConfigs
   directly → "task row is missing required field 'factory_id'" on the profile
   path. Fixed: `load_profile` + `profile_repo_url` (13 tests).
2. `factory_publisher` FinalizingMerge github branch required FactoryConfigs
   for repo_url. Fixed: `load_profile` (7 tests).

## Not yet done (next session)

- Console e2e (Playwright) for the repo picker + pinned-profile header.
- Commit/push: 4 prior commits + all ADR-0069 work await user approval.

## Environment note

Disk was 100% full mid-session (root cause of SQLite `disk I/O error`s);
~37G freed from regenerable caches (Homebrew/go/pip/Yarn). `~/Library/Caches/bazel`
is 206G — recommend the owner review it.
