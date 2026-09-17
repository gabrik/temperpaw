//! factory_implementer — runs Pi inside the task's Computer to implement an
//! approved plan, as a chain of governed Execs (ADR-0066 D3/D4/D5).
//!
//! Fires on each CheckImplementing tick (30s). The phase is a three-step
//! chain, each step a separate Exec discovered by marker
//! `# factory-op: <task>:<operation_key>:<step>`:
//!
//! 1. `checkout` — the MODULE (not the sandbox) fetches the repo tree at
//!    base_sha through the GitHub Git Data API and writes every file into
//!    `/work/repo` via the sandbox file API (D5: GH_TOKEN never reaches the
//!    Computer). The exec then `git init`s and commits the pristine base.
//! 2. `implement` — the module writes the model credential to
//!    `/run/factory/env` (file API, never an Exec field) and the prompt to
//!    `/work/factory-prompt.md`, then the exec runs
//!    `pi --print --approve` in the checkout.
//! 3. `extract` — the exec commits the agent's changes, writes the diff to
//!    `/work/changes.patch` (consumed later by factory_publisher from the
//!    sandbox) and prints the new head sha; the module then reports
//!    SubmitImplementation.
//!
//! Any step's exec failing fails the task (validation-driven repairs re-enter
//! Implementing through ValidationFailed with fresh operation keys, which
//! naturally re-runs the whole chain under new markers).
//!
//! Phase budget: MAX_IMPLEMENTATION_TICKS entity-counter ticks (incremented
//! by the CheckImplementing self-loop), then the module errors and the kernel
//! dispatches ExpireTask.

use serde_json::{Value, json};
use temper_wasm_sdk::prelude::*;
use wasm_helpers::entity_field_str;
use wasm_helpers::sandbox::sandbox_file_write;

/// ~65 min at the 30s tick cadence (ADR-0066 phase budget for Implementing).
const MAX_IMPLEMENTATION_TICKS: u64 = 130;

/// Repo workdir convention shared across factory modules.
const REPO_WORKDIR: &str = "/work/repo";
/// Where the publisher expects the implement/extract diff.
const PATCH_PATH: &str = "/work/changes.patch";
/// Pi prompt file (avoids shell-quoting a multi-line prompt in the command).
const PROMPT_PATH: &str = "/work/factory-prompt.md";
/// Model credential env file, written via the file API so the secret never
/// appears in an audited Exec row.
const ENV_PATH: &str = "/run/factory/env";

/// Steps of the implementation chain.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    Checkout,
    Implement,
    Extract,
}

impl Step {
    fn marker(self) -> &'static str {
        match self {
            Step::Checkout => "checkout",
            Step::Implement => "implement",
            Step::Extract => "extract",
        }
    }
}

/// What one tick decided to do.
#[derive(Debug, PartialEq)]
enum TickDecision {
    Wait,
    StartCheckout,
    StartImplement,
    StartExtract,
    Submit,
    Fail(String),
}

fn exec_status(exec: &Value) -> &str {
    exec.get("status").and_then(|v| v.as_str()).unwrap_or("")
}

fn exec_error(exec: &Value) -> String {
    exec.pointer("/fields/error")
        .and_then(|v| v.as_str())
        .unwrap_or("exec failed")
        .to_string()
}

/// Pure chain state machine: given the three step execs (any may be absent),
/// decide this tick's action.
fn decide_tick(
    checkout: Option<&Value>,
    implement: Option<&Value>,
    extract: Option<&Value>,
    phase_ticks: u64,
) -> Result<TickDecision, String> {
    if phase_ticks >= MAX_IMPLEMENTATION_TICKS {
        return Err(format!(
            "implementation phase exceeded {} ticks",
            MAX_IMPLEMENTATION_TICKS
        ));
    }
    for (exec, start) in [
        (checkout, TickDecision::StartCheckout),
        (implement, TickDecision::StartImplement),
        (extract, TickDecision::StartExtract),
    ] {
        match exec {
            None => return Ok(start),
            Some(e) => match exec_status(e) {
                "Succeeded" => continue,
                "Failed" => return Ok(TickDecision::Fail(exec_error(e))),
                _ => return Ok(TickDecision::Wait),
            },
        }
    }
    Ok(TickDecision::Submit)
}

/// git identity used for the in-sandbox base/implement commits. Local only —
/// the publisher re-creates commits through the Git Data API at publish time.
const GIT_IDENTITY: &str = "-c user.email=factory@darkfactory.local -c user.name=dark-factory";

/// Step 1 exec: initialise the pristine base the module just wrote.
fn checkout_command() -> String {
    format!(
        "cd {REPO_WORKDIR} && git init -q && git add -A && git {GIT_IDENTITY} commit -qm base"
    )
}

/// Step 2 exec: run pi against the prompt file with the env-file credential.
fn implement_command(pi_provider: &str, pi_model: &str, session_id: &str) -> String {
    // The setup_script installs pi asynchronously during provisioning; wait
    // for it rather than racing (F12).
    format!(
        "for i in $(seq 1 90); do command -v pi >/dev/null 2>&1 && break; sleep 5; done; \
         command -v pi >/dev/null 2>&1 || {{ echo 'pi still not installed after wait'; exit 127; }}; \
         cd {REPO_WORKDIR} && set -a && . {ENV_PATH} && set +a && \
         pi --provider {pi_provider} --model {pi_model} --session-id {session_id} \
         --approve --print \"$(cat {PROMPT_PATH})\""
    )
}

/// Step 3 exec: commit the agent's work, materialise the diff, print head.
/// Fails (non-zero) when the agent produced no changes — an empty patch can
/// never become a PR (mirrors DEN's "Pi completed without changing DEN").
/// The diff is ROOT..HEAD (cumulative vs the original checkout), NOT
/// HEAD~1..HEAD: repair rounds stack another base+implement commit pair on
/// the persisted sandbox git, and a last-commit diff silently drops earlier
/// rounds' files from the published tree (found live in e2e: a fix-forward
/// PR contained only the round-1 delta). The publisher replays this
/// cumulative diff onto base_tree=base_sha, so root..head is correct for
/// every round shape (fresh sandbox or persisted).
fn extract_command() -> String {
    format!(
        "cd {REPO_WORKDIR} && git add -A && git {GIT_IDENTITY} commit -qm implement --allow-empty && \
         BASE=$(git rev-list --max-parents=0 HEAD) && \
         git diff --binary $BASE HEAD > {PATCH_PATH} && test -s {PATCH_PATH} && \
         git diff --name-status $BASE HEAD > /work/changed-files.txt && git rev-parse HEAD"
    )
}

/// The implement prompt, mirroring DEN's wording.
fn implement_prompt(task_prompt: &str, plan_text: &str, repair_context: &str) -> String {
    [
        "Implement the approved task in this checkout.".to_string(),
        "Do not push, merge, or change Git credentials/remotes/hooks.".to_string(),
        "Keep the change focused and add useful tests.".to_string(),
        format!("Task: {task_prompt}"),
        format!("Approved plan:\n{plan_text}"),
        if repair_context.trim().is_empty() {
            String::new()
        } else {
            format!("Feedback or failing evidence to address:\n{repair_context}")
        },
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n")
}

/// Branch name for the task's PR (DEN: darkfactory/<id16>-r<round>).
/// DEN parity (factory-controller `implement()`): pre-deploy repairs
/// (RequestChanges / ValidationFailed — merge_sha still empty) reuse the
/// existing branch so the publisher force-updates the same ref and reuses
/// the same open PR; post-deploy repairs (ObservationFailed — merge_sha is
/// set) mint a fresh -r<round> branch.
fn branch_name(task_id: &str, repair_round: u64, existing: &str, merge_sha: &str) -> String {
    if !existing.is_empty() && merge_sha.is_empty() {
        return existing.to_string();
    }
    let short: String = task_id.chars().filter(|c| c.is_alphanumeric()).take(16).collect();
    format!("darkfactory/{short}-r{repair_round}")
}

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
        let counters = ctx.entity_state.get("counters").cloned().unwrap_or(json!({}));

        let task_id = ctx.entity_id.clone();
        let computer_id = required(&fields, "computer_id")?;
        let operation_key = required(&fields, "operation_key")?;
        let operation_owner = required(&fields, "operation_owner")?;
        let factory_id = required(&fields, "factory_id")?;
        let task_prompt = required(&fields, "task_prompt")?;
        let plan_text = field_or(&fields, "plan_text", "");
        let repair_context = field_or(&fields, "repair_context", "");
        let base_sha = required(&fields, "base_sha")?;
        // Post-merge repair loops re-base on the merged SHA so the repair
        // branch starts from the exact merged content (ADR-0066 phase 6).
        let merge_sha = field_or(&fields, "merge_sha", "");
        let checkout_sha = if merge_sha.is_empty() { base_sha.clone() } else { merge_sha.clone() };
        let phase_ticks = counter(&counters, "phase_ticks");
        let repair_round = counter(&counters, "repair_round");

        ctx.log("info", &format!(
            "factory_implementer: tick task={} computer={} key={} ticks={}",
            task_id, computer_id, operation_key, phase_ticks
        ));

        // Resolve the sandbox via the Computer row.
        let computer = factory_common::get_entity(&ctx, "Computers", &computer_id, &fields)?;
        ctx.log("info", "factory_implementer: computer row fetched");
        let handle = factory_common::computer_sandbox_handle(
            computer.get("fields").unwrap_or(&json!({})),
        )?;
        ctx.log("info", "factory_implementer: sandbox handle resolved");

        // Find the three step execs by marker.
        let find = |step: Step| {
            factory_common::find_exec_by_op_key(
                &ctx,
                &computer_id,
                &task_id,
                &format!("{operation_key}:{}", step.marker()),
                &fields,
            )
        };
        let checkout = find(Step::Checkout)?;
        let implement = find(Step::Implement)?;
        let extract = find(Step::Extract)?;
        ctx.log("info", &format!(
            "factory_implementer: execs checkout={} implement={} extract={}",
            checkout.as_ref().map(|e| exec_status(e)).unwrap_or("-"),
            implement.as_ref().map(|e| exec_status(e)).unwrap_or("-"),
            extract.as_ref().map(|e| exec_status(e)).unwrap_or("-"),
        ));

        match decide_tick(checkout.as_ref(), implement.as_ref(), extract.as_ref(), phase_ticks)? {
            TickDecision::Wait => {
                set_success_result("", &json!({}));
            }
            TickDecision::StartCheckout => {
                let config = factory_common::get_entity(&ctx, "FactoryConfigs", &factory_id, &fields)?;
                let repo_url = config
                    .pointer("/fields/repo_url")
                    .and_then(|v| v.as_str())
                    .ok_or("FactoryConfig is missing repo_url")?;
                let slug = factory_common::github_repo_slug(repo_url)?;
                ctx.log("info", &format!(
                    "factory_implementer: task {task_id} checkout of {slug}@{checkout_sha} into {computer_id}"
                ));
                write_repo_tree(&ctx, &handle, &slug, &checkout_sha)?;
                factory_common::create_and_run_exec(
                    &ctx,
                    &computer_id,
                    &task_id,
                    &format!("{operation_key}:{}", Step::Checkout.marker()),
                    "initialise pristine base checkout",
                    &checkout_command(),
                    &fields,
                )?;
                set_success_result("", &json!({}));
            }
            TickDecision::StartImplement => {
                let model_key = ctx
                    .config
                    .get("model_api_key")
                    .filter(|s| !s.trim().is_empty())
                    .cloned()
                    .ok_or("trigger config is missing model_api_key")?;
                let pi_provider = ctx.config.get("pi_provider").cloned().unwrap_or_else(|| "anthropic".into());
                let pi_model = ctx.config.get("pi_model").cloned().unwrap_or_else(|| "claude-sonnet-4-6".into());
                // Credential via the file API — never in an audited Exec row.
                sandbox_file_write(&ctx, &handle, ENV_PATH, &format!("ANTHROPIC_API_KEY={model_key}\n"))?;
                sandbox_file_write(
                    &ctx,
                    &handle,
                    PROMPT_PATH,
                    &implement_prompt(&task_prompt, &plan_text, &repair_context),
                )?;
                factory_common::create_and_run_exec(
                    &ctx,
                    &computer_id,
                    &task_id,
                    &format!("{operation_key}:{}", Step::Implement.marker()),
                    "run pi implementation agent",
                    &implement_command(&pi_provider, &pi_model, &format!("{task_id}-r{repair_round}")),
                    &fields,
                )?;
                set_success_result("", &json!({}));
            }
            TickDecision::StartExtract => {
                factory_common::create_and_run_exec(
                    &ctx,
                    &computer_id,
                    &task_id,
                    &format!("{operation_key}:{}", Step::Extract.marker()),
                    "commit agent change and extract patch",
                    &extract_command(),
                    &fields,
                )?;
                set_success_result("", &json!({}));
            }
            TickDecision::Submit => {
                let extract = extract.as_ref().expect("submit implies extract");
                let head_sha = extract
                    .pointer("/fields/stdout_tail")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if head_sha.len() < 7 {
                    return Err(format!(
                        "extract exec succeeded but head sha is unusable: '{head_sha}'"
                    ));
                }
                set_success_result(
                    "SubmitImplementation",
                    &json!({
                        "base_sha": base_sha,
                        "branch_name": branch_name(&task_id, repair_round, &field_or(&fields, "branch_name", ""), &merge_sha),
                        "head_sha": head_sha,
                        "operation_result": "implementation complete",
                        "expected_operation_key": operation_key,
                        "expected_operation_owner": operation_owner,
                        "operation_key": factory_common::mint_operation_key(&task_id, "validate", repair_round, phase_ticks),
                        "operation_owner": "factory_implementer",
                        "phase_ticks": 0,
                    }),
                );
            }
            TickDecision::Fail(reason) => {
                set_success_result(
                    "FailTask",
                    &json!({
                        "failure_reason": factory_common::truncate(&format!("implementation chain failed: {reason}"), 500),
                        "operation_result": "failed",
                        "expected_operation_key": operation_key,
                        "expected_operation_owner": operation_owner,
                    }),
                );
            }
        }
        Ok(())
    })();

    if let Err(e) = result {
        // Hard failure: CheckImplementing has on_failure = ExpireTask.
        set_error_result(&format!("factory_implementer: {e}"));
    }
    0
}

/// Write the whole repo tree at `sha` into the sandbox (D5 checkout).
/// Text files only in v1 — a binary blob fails loudly with its path.
fn write_repo_tree(
    ctx: &Context,
    handle: &wasm_helpers::sandbox::SandboxHandle,
    slug: &str,
    sha: &str,
) -> Result<usize, String> {
    let blobs = factory_common::github_tree_blobs(ctx, slug, sha)?;
    if blobs.is_empty() {
        return Err(format!("github tree {slug}@{sha} has no blobs"));
    }
    let mut written = 0usize;
    for (path, blob_sha) in &blobs {
        let bytes = factory_common::github_blob_bytes(ctx, slug, blob_sha)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("binary file '{path}' is not supported by dark-factory checkout v1"))?;
        sandbox_file_write(ctx, handle, &format!("{REPO_WORKDIR}/{path}"), &text)?;
        written += 1;
    }
    ctx.log("info", &format!(
        "factory_implementer: wrote {written} files of {slug}@{sha} into sandbox"
    ));
    Ok(written)
}

fn required(fields: &Value, name: &str) -> Result<String, String> {
    entity_field_str(fields, &[name])
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("task row is missing required field '{name}'"))
}

fn field_or(fields: &Value, name: &str, default: &str) -> String {
    entity_field_str(fields, &[name])
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

fn counter(counters: &Value, name: &str) -> u64 {
    counters
        .get(name)
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(status: &str) -> Value {
        json!({"status": status, "fields": {}})
    }

    #[test]
    fn no_excs_starts_checkout() {
        assert_eq!(
            decide_tick(None, None, None, 0).unwrap(),
            TickDecision::StartCheckout
        );
    }

    #[test]
    fn running_step_waits() {
        assert_eq!(
            decide_tick(Some(&exec("Running")), None, None, 1).unwrap(),
            TickDecision::Wait
        );
        assert_eq!(
            decide_tick(Some(&exec("Succeeded")), Some(&exec("Created")), None, 1).unwrap(),
            TickDecision::Wait
        );
    }

    #[test]
    fn chain_advances_step_by_step() {
        assert_eq!(
            decide_tick(Some(&exec("Succeeded")), None, None, 1).unwrap(),
            TickDecision::StartImplement
        );
        assert_eq!(
            decide_tick(Some(&exec("Succeeded")), Some(&exec("Succeeded")), None, 1).unwrap(),
            TickDecision::StartExtract
        );
        assert_eq!(
            decide_tick(Some(&exec("Succeeded")), Some(&exec("Succeeded")), Some(&exec("Succeeded")), 1).unwrap(),
            TickDecision::Submit
        );
    }

    #[test]
    fn failed_step_fails_task_with_evidence() {
        let failed = json!({"status":"Failed","fields":{"error":"boom"}});
        assert_eq!(
            decide_tick(Some(&failed), None, None, 1).unwrap(),
            TickDecision::Fail("boom".into())
        );
    }

    #[test]
    fn tick_budget_is_enforced() {
        assert!(decide_tick(None, None, None, MAX_IMPLEMENTATION_TICKS).is_err());
    }

    #[test]
    fn implement_command_sources_env_and_reads_prompt_file() {
        let cmd = implement_command("anthropic", "claude-sonnet-4-6", "s1");
        assert!(cmd.contains(". /run/factory/env"));
        assert!(cmd.contains("command -v pi"), "{cmd}");
        assert!(cmd.contains("--provider anthropic"));
        assert!(cmd.contains("--model claude-sonnet-4-6"));
        assert!(cmd.contains("--session-id s1"));
        assert!(cmd.contains("$(cat /work/factory-prompt.md)"));
        assert!(!cmd.contains("ANTHROPIC_API_KEY="), "secret must not be inline");
    }

    #[test]
    fn extract_command_fails_on_empty_patch() {
        let cmd = extract_command();
        assert!(cmd.contains("test -s /work/changes.patch"));
        assert!(cmd.contains("git rev-parse HEAD"));
        assert!(cmd.contains("/work/changed-files.txt"), "{cmd}");
        // Cumulative diff vs the checkout root, not HEAD~1: repair rounds
        // stack commits, and HEAD~1 would drop earlier rounds' files.
        assert!(cmd.contains("git rev-list --max-parents=0 HEAD"), "{cmd}");
        assert!(!cmd.contains("HEAD~1"), "{cmd}");
    }

    #[test]
    fn prompt_mirrors_den_wording_and_includes_repair_context() {
        let p = implement_prompt("do x", "step 1", "tests failed");
        assert!(p.contains("Implement the approved task in this checkout."));
        assert!(p.contains("Task: do x"));
        assert!(p.contains("Approved plan:\nstep 1"));
        assert!(p.contains("Feedback or failing evidence to address:\ntests failed"));
        let clean = implement_prompt("do x", "step 1", "");
        assert!(!clean.contains("Feedback or failing evidence"));
    }

    #[test]
    fn branch_names_follow_den_pattern() {
        // First implementation: no existing branch -> mint -r0.
        assert_eq!(
            branch_name("en-01a0aa9c-fba4-7b63", 0, "", ""),
            "darkfactory/en01a0aa9cfba47b-r0"
        );
        // Pre-deploy repairs (RequestChanges, ValidationFailed: merge_sha is
        // empty) KEEP the existing branch so the publisher force-updates it
        // and reuses the same open PR (DEN factory-controller implement()).
        assert_eq!(
            branch_name("en-01a0aa9c-fba4-7b63", 2, "darkfactory/en01a0aa9cfba47b-r0", ""),
            "darkfactory/en01a0aa9cfba47b-r0"
        );
        // Post-deploy repairs (ObservationFailed: merge_sha set) get a fresh
        // -r<round> branch, like DEN's post-merge repair loop.
        assert_eq!(
            branch_name("en-01a0aa9c-fba4-7b63", 2, "darkfactory/en01a0aa9cfba47b-r0", "abc123"),
            "darkfactory/en01a0aa9cfba47b-r2"
        );
        // Post-deploy with no existing branch still mints -r<round>.
        assert_eq!(
            branch_name("en-01a0aa9c-fba4-7b63", 2, "", "abc123"),
            "darkfactory/en01a0aa9cfba47b-r2"
        );
    }
}
