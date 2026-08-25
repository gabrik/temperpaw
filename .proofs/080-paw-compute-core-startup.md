# Proof Report: 080 — paw-compute core startup installation

## Date
2026-08-25

## Branch / Commit
`codex/paw-compute-startup-local` (local-only worktree; commit pending)

## What Was Done
- Added `startup_install = "core"` to `os-apps/paw-compute/app.toml` so the platform catalog includes it in the local default startup surface.
- Added `paw-compute` to the startup-surface regression test.
- Added `os-apps/paw-compute/wasm/build.sh` to the CI OS-app WASM build loop.

## Verification Flow
1. Extended the startup-surface test to require `paw-compute`.
2. Ran the test before the manifest change.
3. Marked the app as a core startup app and added its CI WASM build step.
4. Re-ran the startup-surface test.
5. Built `computer_exec.wasm` through the app build script.
6. Asserted CI contains the compute WASM build step.

## Verification Results
| Step | Expected | Actual | Status |
|------|----------|--------|--------|
| Red test | `paw-compute` missing from core startup apps | Failed; catalog omitted `paw-compute` | Pass |
| Green test | Core startup app list includes `paw-compute` | `startup_os_apps_includes_core_apps` passed | Pass |
| WASM build | Required `computer_exec.wasm` exists | Built successfully; 259,182 bytes | Pass |
| CI wiring | CI invokes compute build script | `os-apps/paw-compute/wasm/build.sh` present | Pass |

## What Worked
- Core startup selection is manifest-driven: `startup_install = "core"` puts `paw-compute` into `list_startup_os_apps()`.
- The existing app build script produces the required `computer_exec.wasm` artifact.

## What Didn't Work
- The initial exact-name Cargo filter selected no tests; the standard substring filter was used for the red/green run.

## Limitations
- No live server was started from this isolated worktree because the user already has a local server running on port 3467 and its dashboard route has a separate 401 regression. The startup-surface unit test and WASM artifact build cover the directly changed installation path.

## What Still Doesn't Work
- The existing `/dashboard/login` 401 issue is outside this change.

## Artifacts
- `os-apps/paw-compute/app.toml`
- `os-apps/paw-compute/wasm/computer_exec/computer_exec.wasm` (generated, ignored)
- `.github/workflows/ci.yml`
- `crates/temperpaw/src/startup.rs`

## Architecture Diagram
```text
app.toml: startup_install = core
            |
            v
list_startup_os_apps()
            |
            v
startup reconciliation installs paw-compute
            |
            +--> Computer entity set
            +--> Exec entity set
            +--> computer_exec.wasm
```
