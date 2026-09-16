//! factory_planner — Phase 1 planning agent driver for dark-factory
//! (ADR-0066).
//!
//! Pure-WASM planner:
//!
//! 1. **Computer ensure**: when the task has no `computer_id`, the planner
//!    creates a Computer entity (sized from the FactoryConfig), dispatches
//!    Configure + Provision, waits across ticks for Ready, then reports
//!    `AttachComputer` (CAS-fenced). One computer per task until F6 is
//!    decided. Discovery of the planner-created computer is by deterministic
//!    name marker `df-plan-<task16>` (the task row cannot be updated until
//!    AttachComputer lands).
//! 2. **Checkout (D5)**: resolves the FactoryConfig base branch head via the
//!    GitHub API (that sha becomes the task's frozen `base_sha`), writes it
//!    to `/work/base_sha` and the repo tree into `/work/repo` via the sandbox
//!    file API, then runs a checkout Exec (git init/commit) for provenance.
//! 3. **Plan run**: writes `/run/factory/env` (model API key — never in an
//!    audited Exec row) and `/work/factory-prompt.md` module-side, then runs
//!    pi READ-ONLY (`--tools read,grep,find,ls`) with stdout redirected to
//!    `/work/plan.md` via a governed Exec.
//! 4. **Submit**: reads `/work/plan.md` + `/work/base_sha`, computes
//!    `plan_digest = sha256(plan_text)`, and reports `SubmitPlan` with the
//!    CAS pair. Reporting transitions the task to AwaitingPlanApproval, so
//!    CheckPlanning stops firing; a RejectPlan re-enters Planning with a
//!    fresh operation_key, so marker discovery under the new key is empty
//!    and the chain reruns from scratch (fresh checkout, fresh plan).

use serde_json::{json, Value};
use temper_wasm_sdk::prelude::*;
use wasm_helpers::entity_field_str;
use wasm_helpers::sandbox::{sandbox_file_read, sandbox_file_write};

/// ~65 minutes of Planning at the default 30s tick.
const MAX_PLANNING_TICKS: u64 = 130;
const REPO_WORKDIR: &str = "/work/repo";
const PLAN_PATH: &str = "/work/plan.md";
const BASE_SHA_PATH: &str = "/work/base_sha";
const PROMPT_PATH: &str = "/work/factory-prompt.md";
const ENV_PATH: &str = "/run/factory/env";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Checkout,
    Plan,
}

impl Step {
    fn marker(self) -> &'static str {
        match self {
            Step::Checkout => "checkout",
            Step::Plan => "plan",
        }
    }
    fn description(self) -> &'static str {
        match self {
            Step::Checkout => "dark-factory checkout (git init and base commit)",
            Step::Plan => "dark-factory planning run (pi, read-only)",
        }
    }
}

#[derive(Debug, PartialEq)]
enum TickDecision {
    /// A step exec is in flight (or just reported); wait for the next tick.
    Wait,
    /// No computer attached and none created yet: create + provision one.
    CreateComputer,
    /// The planner-created computer exists but is not Ready yet.
    AwaitComputer,
    /// The planner-created computer is Ready; bind it to the task.
    AttachComputer,
    StartCheckout,
    StartPlan,
    /// Plan exec Succeeded: read the artifacts and report SubmitPlan.
    Submit,
    Fail(String),
}

fn decide_tick(
    computer_attached: bool,
    created_status: Option<&str>,
    checkout: Option<&Value>,
    plan: Option<&Value>,
    phase_ticks: u64,
) -> TickDecision {
    if phase_ticks >= MAX_PLANNING_TICKS {
        return TickDecision::Fail(format!(
            "planning exceeded {MAX_PLANNING_TICKS} ticks"
        ));
    }
    if !computer_attached {
        return match created_status {
            None => TickDecision::CreateComputer,
            Some("Ready") => TickDecision::AttachComputer,
            Some(other) if other.is_empty() => TickDecision::AwaitComputer,
            Some(_) => TickDecision::AwaitComputer,
        };
    }
    for (exec, start) in [
        (checkout, TickDecision::StartCheckout),
        (plan, TickDecision::StartPlan),
    ] {
        match exec {
            None => return start,
            Some(e) => match exec_status(e) {
                "Succeeded" => continue,
                "Failed" => return TickDecision::Fail(exec_error(e)),
                _ => return TickDecision::Wait,
            },
        }
    }
    TickDecision::Submit
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

fn checkout_command() -> String {
    format!(
        "set -eu; mkdir -p {REPO_WORKDIR}; cd {REPO_WORKDIR}; git init -q; git add -A; git -c user.email=factory@temper.local -c user.name=dark-factory commit -q -m 'dark-factory checkout'"
    )
}

fn plan_command(provider: &str, model: &str, session_id: &str) -> String {
    // The setup_script installs pi asynchronously during provisioning; wait
    // for it rather than racing (F12).
    format!(
        "for i in $(seq 1 90); do command -v pi >/dev/null 2>&1 && break; sleep 5; done; \
         command -v pi >/dev/null 2>&1 || {{ echo 'pi still not installed after wait'; exit 127; }}; \
         cd {REPO_WORKDIR} && set -a && . {ENV_PATH} && set +a && \
         pi --provider {provider} --model {model} --session-id {session_id} \
         --print --tools read,grep,find,ls \"$(cat {PROMPT_PATH})\" > {PLAN_PATH}"
    )
}

/// Planning prompt (DEN factory-controller shape, generalized).
fn plan_prompt(task_prompt: &str, repair_context: &str) -> String {
    let mut prompt = format!(
        "You are the planning agent for a software engineering task.\n\
         You have READ-ONLY tools. Do not modify any file.\n\n\
         Task:\n{task_prompt}\n\n\
         Read the checked out repository and produce a concrete implementation plan.\n\
         Ground every step in files and symbols that actually exist in the checkout.\n\
         Keep the plan minimal: the smallest change that satisfies the task.\n\n\
         Respond with the plan only: numbered steps, each naming the files it\n\
         touches and the exact change to make."
    );
    if !repair_context.is_empty() {
        prompt.push_str(&format!(
            "\n\nThe human reviewer rejected the previous plan with this feedback. Address it:\n{repair_context}"
        ));
    }
    prompt
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

#[no_mangle]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    match Context::from_host() {
        Ok(ctx) => {
            if let Err(e) = run_inner(&ctx) {
                // F9: set_error_result is log-silent on the host — surface the
                // error text through the guest log first.
                ctx.log("error", &format!("factory_planner: {e}"));
                set_error_result(&format!("factory_planner: {e}"));
            }
        }
        Err(e) => {
            set_error_result(&format!("factory_planner: context init failed: {e}"));
        }
    }
    0
}

fn run_inner(ctx: &Context) -> Result<(), String> {
    if ctx.entity_type != "FactoryTask" {
        set_success_result("", &json!({}));
        return Ok(());
    }
    let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
    let counters = ctx.entity_state.get("counters").cloned().unwrap_or(json!({}));
    let fields = &fields;
    let counters = &counters;
    let task_id = ctx.entity_id.clone();
    let computer_id = field_or(fields, "computer_id", "");
    let operation_key = required(fields, "operation_key")?;
    let operation_owner = required(fields, "operation_owner")?;
    let factory_id = required(fields, "factory_id")?;
    let task_prompt = required(fields, "task_prompt")?;
    let repair_context = field_or(fields, "repair_context", "");
    let phase_ticks = counter(counters, "phase_ticks");
    let repair_round = counter(counters, "repair_round");

    ctx.log(
        "info",
        &format!(
            "factory_planner: tick task={task_id} computer={} key={operation_key} ticks={phase_ticks}",
            if computer_id.is_empty() { "-" } else { &computer_id }
        ),
    );

    // Planner-created computer discovery (deterministic name; one per task).
    let computer_name = format!(
        "df-plan-{}",
        task_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect::<String>()
    );
    let created = if computer_id.is_empty() {
        find_computer_by_name(ctx, &computer_name, fields)?
    } else {
        None
    };
    let created_status = created
        .as_ref()
        .map(|c| exec_status_like(c))
        .unwrap_or_default();
    let created_id = created
        .as_ref()
        .and_then(|c| entity_field_str(c, &["entity_id"]).map(|s| s.to_string()));

    // Step execs exist only once a computer is attached.
    let find = |step: Step| {
        factory_common::find_exec_by_op_key(
            ctx,
            &computer_id,
            &task_id,
            &format!("{operation_key}:{}", step.marker()),
            fields,
        )
    };
    let (checkout, plan) = if computer_id.is_empty() {
        (None, None)
    } else {
        (find(Step::Checkout)?, find(Step::Plan)?)
    };

    match decide_tick(
        !computer_id.is_empty(),
        if created.is_some() { Some(created_status.as_str()) } else { None },
        checkout.as_ref(),
        plan.as_ref(),
        phase_ticks,
    ) {
        TickDecision::Wait | TickDecision::AwaitComputer => {
            set_success_result("", &json!({}));
        }
        TickDecision::CreateComputer => {
            let config = factory_common::get_entity(ctx, "FactoryConfigs", &factory_id, fields)?;
            let cfg = config.get("fields").cloned().unwrap_or(json!({}));
            let image = field_or(&cfg, "computer_image", "");
            if image.is_empty() {
                return Err(
                    "FactoryConfig is missing computer_image (required for planner-created computers)"
                        .into(),
                );
            }
            let new_row = factory_common::create_entity(
                ctx,
                "Computers",
                &json!({ "name": computer_name }),
                fields,
            )?;
            let new_id = entity_field_str(&new_row, &["entity_id"])
                .map(|s| s.to_string())
                .ok_or("created Computer row carried no entity_id")?;
            factory_common::dispatch_action(
                ctx,
                "Computers",
                &new_id,
                "Temper.PawCompute",
                "Configure",
                &json!({
                    "provider": field_or(&cfg, "computer_provider", "tensorlake"),
                    "cpu_cores": field_or(&cfg, "computer_cpu_cores", "2"),
                    "memory_gb": field_or(&cfg, "computer_memory_gb", "4"),
                    "storage_gb": field_or(&cfg, "computer_storage_gb", "8"),
                    "base_image": image,
                    "setup_script": field_or(&cfg, "setup_script", ""),
                }),
                fields,
            )?;
            factory_common::dispatch_action(
                ctx,
                "Computers",
                &new_id,
                "Temper.PawCompute",
                "Provision",
                &json!({}),
                fields,
            )?;
            ctx.log(
                "info",
                &format!("factory_planner: task {task_id} created computer {new_id} ({computer_name}); provisioning"),
            );
            set_success_result("", &json!({}));
        }
        TickDecision::AttachComputer => {
            let cid = created_id.ok_or("planner-created computer row vanished")?;
            set_success_result(
                "AttachComputer",
                &json!({
                    "computer_id": cid,
                    "operation_result": "computer attached by factory_planner",
                    "expected_operation_key": operation_key,
                    "expected_operation_owner": operation_owner,
                }),
            );
        }
        TickDecision::StartCheckout => {
            let config = factory_common::get_entity(ctx, "FactoryConfigs", &factory_id, fields)?;
            let cfg = config.get("fields").cloned().unwrap_or(json!({}));
            let repo_url = field_or(&cfg, "repo_url", "");
            if repo_url.is_empty() {
                return Err("FactoryConfig is missing repo_url".into());
            }
            let base_branch = field_or(&cfg, "base_branch", "main");
            let slug = factory_common::github_repo_slug(&repo_url)?;
            let base_sha = github_ref_sha(ctx, &slug, &base_branch)?;
            let computer = factory_common::get_entity(ctx, "Computers", &computer_id, fields)?;
            let handle = factory_common::computer_sandbox_handle(
                computer.get("fields").unwrap_or(&json!({})),
            )?;
            ctx.log(
                "info",
                &format!("factory_planner: task {task_id} checkout of {slug}@{base_sha} into {computer_id}"),
            );
            sandbox_file_write(ctx, &handle, BASE_SHA_PATH, &format!("{base_sha}\n"))?;
            write_repo_tree(ctx, &handle, &slug, &base_sha)?;
            let marker = format!("{operation_key}:{}", Step::Checkout.marker());
            factory_common::create_and_run_exec(
                ctx,
                &computer_id,
                &task_id,
                &marker,
                Step::Checkout.description(),
                &checkout_command(),
                fields,
            )?;
            set_success_result("", &json!({}));
        }
        TickDecision::StartPlan => {
            let computer = factory_common::get_entity(ctx, "Computers", &computer_id, fields)?;
            let handle = factory_common::computer_sandbox_handle(
                computer.get("fields").unwrap_or(&json!({})),
            )?;
            let env_var = ctx
                .config
                .get("model_env_var")
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| "ANTHROPIC_API_KEY".to_string());
            let api_key = ctx
                .config
                .get("model_api_key")
                .filter(|s| !s.is_empty())
                .cloned()
                .ok_or("trigger config is missing model_api_key")?;
            let provider = ctx
                .config
                .get("pi_provider")
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| "anthropic".to_string());
            let model = ctx
                .config
                .get("pi_model")
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
            sandbox_file_write(ctx, &handle, ENV_PATH, &format!("{env_var}={api_key}\n"))?;
            sandbox_file_write(ctx, &handle, PROMPT_PATH, &plan_prompt(&task_prompt, &repair_context))?;
            let marker = format!("{operation_key}:{}", Step::Plan.marker());
            let session_id = marker.replace(':', "-");
            factory_common::create_and_run_exec(
                ctx,
                &computer_id,
                &task_id,
                &marker,
                Step::Plan.description(),
                &plan_command(&provider, &model, &session_id),
                fields,
            )?;
            ctx.log(
                "info",
                &format!("factory_planner: task {task_id} started planning run (provider={provider} model={model})"),
            );
            set_success_result("", &json!({}));
        }
        TickDecision::Submit => {
            let computer = factory_common::get_entity(ctx, "Computers", &computer_id, fields)?;
            let handle = factory_common::computer_sandbox_handle(
                computer.get("fields").unwrap_or(&json!({})),
            )?;
            let plan_text = sandbox_file_read(ctx, &handle, PLAN_PATH)?.trim().to_string();
            if plan_text.is_empty() {
                set_success_result(
                    "FailTask",
                    &json!({
                        "failure_reason": "planning run produced an empty plan",
                        "operation_result": "failed",
                        "expected_operation_key": operation_key,
                        "expected_operation_owner": operation_owner,
                    }),
                );
                return Ok(());
            }
            let base_sha = sandbox_file_read(ctx, &handle, BASE_SHA_PATH)?.trim().to_string();
            if base_sha.len() < 7 {
                return Err(format!("base sha artifact missing or corrupt: '{base_sha}'"));
            }
            let plan_digest = factory_common::sha256_hex(&plan_text);
            set_success_result(
                "SubmitPlan",
                &json!({
                    "plan_text": plan_text,
                    "plan_digest": plan_digest,
                    "base_sha": base_sha,
                    "operation_result": "plan submitted",
                    "expected_operation_key": operation_key,
                    "expected_operation_owner": operation_owner,
                    "operation_key": factory_common::mint_operation_key(&task_id, "review", repair_round, phase_ticks),
                    "operation_owner": "factory_planner",
                    "phase_ticks": 0,
                }),
            );
            ctx.log(
                "info",
                &format!("factory_planner: task {task_id} submitted plan (digest {plan_digest}, base {base_sha})"),
            );
        }
        TickDecision::Fail(reason) => {
            set_success_result(
                "FailTask",
                &json!({
                    "failure_reason": factory_common::truncate(&format!("planning chain failed: {reason}"), 500),
                    "operation_result": "failed",
                    "expected_operation_key": operation_key,
                    "expected_operation_owner": operation_owner,
                }),
            );
        }
    }
    Ok(())
}

/// Status field of an entity row (top-level in the OData projection).
fn exec_status_like(row: &Value) -> String {
    row.get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Resolve the head sha of a branch via the Git Data API.
fn github_ref_sha(ctx: &Context, slug: &str, branch: &str) -> Result<String, String> {
    let resp = factory_common::github_api(
        ctx,
        "GET",
        &format!("/repos/{slug}/git/ref/heads/{branch}"),
        None,
    )?;
    resp.pointer("/object/sha")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("github ref lookup for {slug}@{branch} returned no sha"))
}

/// Find the planner-created computer for this task by its name marker.
fn find_computer_by_name(
    ctx: &Context,
    name: &str,
    fields: &Value,
) -> Result<Option<Value>, String> {
    let rows = factory_common::list_entities(
        ctx,
        "Computers",
        Some(&format!("name eq '{}'", factory_common::escape_odata(name))),
        fields,
    )?;
    Ok(rows.into_iter().next())
}

/// Write the whole repo tree at `sha` into the sandbox (D5 checkout).
/// Text files only in v1 — a binary blob fails loudly with its path.
fn write_repo_tree(
    ctx: &Context,
    handle: &wasm_helpers::sandbox::SandboxHandle,
    slug: &str,
    sha: &str,
) -> Result<(), String> {
    let blobs = factory_common::github_tree_blobs(ctx, slug, sha)?;
    let mut wrote = 0usize;
    for (path, blob_sha) in &blobs {
        if path.contains("..") || path.starts_with('/') {
            return Err(format!("refusing unsafe repo path: {path}"));
        }
        let bytes = factory_common::github_blob_bytes(ctx, slug, blob_sha)?;
        let content = String::from_utf8(bytes).map_err(|_| {
            format!("blob {path} is not utf-8; binary checkout not supported yet (v1)")
        })?;
        sandbox_file_write(ctx, handle, &format!("{REPO_WORKDIR}/{path}"), &content)?;
        wrote += 1;
    }
    ctx.log(
        "info",
        &format!("factory_planner: wrote {wrote} files of {slug}@{sha} into sandbox"),
    );
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(id: &str, status: &str) -> Value {
        json!({"entity_id": id, "status": status})
    }

    #[test]
    fn creates_computer_when_unattached() {
        assert_eq!(
            decide_tick(false, None, None, None, 0),
            TickDecision::CreateComputer
        );
        assert_eq!(
            decide_tick(false, Some("Provisioning"), None, None, 0),
            TickDecision::AwaitComputer
        );
        assert_eq!(
            decide_tick(false, Some("Ready"), None, None, 0),
            TickDecision::AttachComputer
        );
    }

    #[test]
    fn checkout_starts_once_attached() {
        assert_eq!(
            decide_tick(true, None, None, None, 0),
            TickDecision::StartCheckout
        );
        assert_eq!(
            decide_tick(true, None, Some(&exec("e1", "Running")), None, 0),
            TickDecision::Wait
        );
        assert_eq!(
            decide_tick(true, None, Some(&exec("e1", "Succeeded")), None, 0),
            TickDecision::StartPlan
        );
        assert_eq!(
            decide_tick(
                true,
                None,
                Some(&exec("e1", "Succeeded")),
                Some(&exec("e2", "Succeeded")),
                0
            ),
            TickDecision::Submit
        );
    }

    #[test]
    fn failed_step_fails_task_with_evidence() {
        let failed = json!({"entity_id": "e2", "status": "Failed", "fields": {"error": "boom"}});
        match decide_tick(true, None, Some(&exec("e1", "Succeeded")), Some(&failed), 5) {
            TickDecision::Fail(reason) => {
                assert!(reason.contains("boom"), "{reason}");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn budget_exceeded_fails() {
        assert!(matches!(
            decide_tick(true, None, None, None, MAX_PLANNING_TICKS),
            TickDecision::Fail(_)
        ));
    }

    #[test]
    fn plan_command_is_read_only_and_redirects() {
        let cmd = plan_command("anthropic", "claude-sonnet-4-6", "sid-1");
        assert!(cmd.contains("command -v pi"), "{cmd}");
        assert!(cmd.contains("--tools read,grep,find,ls"), "{cmd}");
        assert!(cmd.contains("> /work/plan.md"), "{cmd}");
        assert!(cmd.contains(". /run/factory/env"), "{cmd}");
        assert!(!cmd.contains("sk-"), "{cmd}");
    }

    #[test]
    fn prompt_is_read_only_with_repair_context() {
        let p = plan_prompt("do the thing", "too broad, narrow step 2");
        assert!(p.contains("READ-ONLY"), "{p}");
        assert!(p.contains("do the thing"), "{p}");
        assert!(p.contains("too broad, narrow step 2"), "{p}");
        let p2 = plan_prompt("do the thing", "");
        assert!(!p2.contains("rejected the previous plan"), "{p2}");
    }
}
