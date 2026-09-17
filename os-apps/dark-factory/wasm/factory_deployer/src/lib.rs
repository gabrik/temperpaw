//! factory_deployer — ADR-0069 Deploy phase integration.
//!
//! Runs the pinned profile's `deploy_commands` (typed CommandSpec array) on
//! the task's Computer via the governed Exec surface (ADR-0066 D3/D7). An
//! empty command list is an explicit passthrough: the deployer immediately
//! records `RecordDeployed` with merge_sha = head_sha, preserving the
//! pre-ADR-0069 record-only Merging behavior.
//!
//! Deploy is fail-closed: a failing deploy command, a failing deploy Exec,
//! or an exhausted tick budget drives FailTask — never an automatic merge,
//! retry, or rollback (deploys may be non-idempotent; the human or agent
//! intervenes). The module never touches git credentials; publish is the
//! factory_publisher's job, deploy is this module's job.

use serde_json::{json, Value};
use temper_wasm_sdk::prelude::*;
use wasm_helpers::entity_field_str;

/// ~30 min at the 30s tick cadence (ADR-0069 phase budget for Deploying).
const MAX_DEPLOY_TICKS: u64 = 60;

/// What one tick decided to report.
#[derive(Debug, PartialEq)]
enum TickDecision {
    /// Nothing to report; wait for the next tick.
    Wait,
    /// Create the deploy Exec (none exists for this phase yet).
    StartExec,
    /// No deploy commands declared: record the deploy immediately.
    Passthrough,
    /// Deploy commands completed: RecordDeployed.
    Deployed,
    /// Fail-closed: FailTask with evidence.
    Fail(String),
}

fn exec_phase_status(exec: &Value) -> &str {
    exec.get("status").and_then(|v| v.as_str()).unwrap_or("")
}

fn decide_tick(
    has_deploy_commands: bool,
    exec: Option<&Value>,
    phase_ticks: u64,
) -> Result<TickDecision, String> {
    if phase_ticks >= MAX_DEPLOY_TICKS {
        return Ok(TickDecision::Fail(format!(
            "deploy exceeded {MAX_DEPLOY_TICKS} ticks — fail-closed (no automatic retry)"
        )));
    }
    let Some(exec) = exec else {
        return Ok(if has_deploy_commands {
            TickDecision::StartExec
        } else {
            TickDecision::Passthrough
        });
    };
    if factory_common::exec_succeeded(exec) {
        return Ok(TickDecision::Deployed);
    }
    match exec_phase_status(exec) {
        "Succeeded" | "Failed" => Ok(TickDecision::Fail(factory_common::truncate(
            &format!("deploy exec failed: {}", factory_common::exec_failure_summary(exec)),
            1500,
        ))),
        _ => Ok(TickDecision::Wait),
    }
}

/// The deployment reference recorded on the task: the last non-empty line of
/// the deploy Exec's stdout tail (a URL, a deployment id, …). Empty when the
/// commands print nothing (or on passthrough).
fn deployment_ref_from(exec: &Value) -> String {
    exec.pointer("/fields/stdout_tail")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| factory_common::truncate(l, 500))
        .unwrap_or_default()
}

/// RecordDeployed params: merge_sha = head_sha (the approved head was
/// deployed; publish still happens in PublishingPR afterwards), plus a fresh
/// operation key minted for the Observing phase.
fn deployed_params(
    task_id: &str,
    head_sha: &str,
    deployment_ref: &str,
    operation_result: &str,
    repair_round: u64,
    phase_ticks: u64,
    expected_operation_key: &str,
    expected_operation_owner: &str,
) -> Value {
    json!({
        "merge_sha": head_sha,
        "deployment_ref": deployment_ref,
        "operation_result": operation_result,
        "expected_operation_key": expected_operation_key,
        "expected_operation_owner": expected_operation_owner,
        "operation_key": factory_common::mint_operation_key(task_id, "observe", repair_round, phase_ticks),
        "operation_owner": "factory_deployer",
        "phase_ticks": 0,
    })
}

fn fail_params(reason: &str, operation_key: &str, operation_owner: &str) -> Value {
    json!({
        "failure_reason": reason,
        "operation_result": reason,
        "expected_operation_key": operation_key,
        "expected_operation_owner": operation_owner,
    })
}

fn run_inner(ctx: &Context) -> Result<(), String> {
    if ctx.entity_type != "FactoryTask" {
        set_success_result("", &json!({}));
        return Ok(());
    }
    let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
    let counters = ctx
        .entity_state
        .get("counters")
        .cloned()
        .unwrap_or(json!({}));

    let task_id = ctx.entity_id.clone();
    let computer_id = required_field(&fields, "computer_id")?;
    let operation_key = required_field(&fields, "operation_key")?;
    let operation_owner = required_field(&fields, "operation_owner")?;
    let head_sha = required_field(&fields, "head_sha")?;
    let factory_repo_id = optional_field(&fields, "factory_repo_id");
    let factory_id = optional_field(&fields, "factory_id");
    if factory_repo_id.is_empty() && factory_id.is_empty() {
        return Err("task has neither factory_repo_id nor factory_id".to_string());
    }
    let phase_ticks = counter(&counters, "phase_ticks");
    let repair_round = counter(&counters, "repair_round");

    // Profile: pinned snapshot (ADR-0069) or legacy FactoryConfig row. The
    // legacy row declares no deploy_commands, so legacy tasks pass through —
    // exactly the record-only Merging behavior they had before.
    let profile = factory_common::load_profile(ctx, &fields, &fields)?;
    let extra_env = factory_common::factory_context_env(
        &task_id,
        &optional_field(&fields, "base_sha"),
        &head_sha,
        &optional_field(&fields, "deployment_ref"),
    );
    let deploy_shell = factory_common::profile_commands(&profile, "deploy_commands", &extra_env)?;

    // Find this phase's Exec (marker = task:current operation_key).
    let exec = factory_common::find_exec_by_op_key(ctx, &computer_id, &task_id, &operation_key, &fields)?;

    match decide_tick(deploy_shell.is_some(), exec.as_ref(), phase_ticks)? {
        TickDecision::Wait => {
            ctx.log(
                "info",
                &format!("factory_deployer: task {task_id} deploy still running (tick {phase_ticks})"),
            );
            set_success_result("", &json!({}));
        }
        TickDecision::StartExec => {
            let shell = deploy_shell.expect("StartExec implies commands");
            let exec_id = factory_common::create_and_run_exec(
                ctx,
                &computer_id,
                &task_id,
                &operation_key,
                "run deploy commands",
                &shell,
                &fields,
            )?;
            ctx.log(
                "info",
                &format!("factory_deployer: task {task_id} started deploy exec {exec_id}"),
            );
            set_success_result("", &json!({}));
        }
        TickDecision::Passthrough => {
            ctx.log(
                "info",
                &format!("factory_deployer: task {task_id} has no deploy_commands — passthrough"),
            );
            set_success_result(
                "RecordDeployed",
                &deployed_params(
                    &task_id,
                    &head_sha,
                    "",
                    "deploy passthrough (profile declares no deploy_commands)",
                    repair_round,
                    phase_ticks,
                    &operation_key,
                    &operation_owner,
                ),
            );
        }
        TickDecision::Deployed => {
            let exec = exec.as_ref().expect("Deployed implies exec");
            let deployment_ref = deployment_ref_from(exec);
            set_success_result(
                "RecordDeployed",
                &deployed_params(
                    &task_id,
                    &head_sha,
                    &deployment_ref,
                    "deploy commands completed",
                    repair_round,
                    phase_ticks,
                    &operation_key,
                    &operation_owner,
                ),
            );
        }
        TickDecision::Fail(reason) => {
            ctx.log(
                "warn",
                &format!("factory_deployer: task {task_id} deploy failed: {reason}"),
            );
            set_success_result("FailTask", &fail_params(&reason, &operation_key, &operation_owner));
        }
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        run_inner(&ctx)
    })();
    if let Err(e) = result {
        // Module errors surface via the generic failure path: set_error_result
        // + on_failure = ExpireTask, so the kernel drives the task to Failed.
        set_error_result(&format!("factory_deployer: {e}"));
    }
    0
}

fn required_field(fields: &Value, name: &str) -> Result<String, String> {
    entity_field_str(fields, &[name])
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("task row is missing required field '{name}'"))
}

fn optional_field(fields: &Value, name: &str) -> String {
    entity_field_str(fields, &[name])
        .map(|s| s.to_string())
        .unwrap_or_default()
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

    fn running_exec() -> Value {
        json!({"status": "Running", "fields": {"exit_code": "", "error": ""}})
    }

    fn succeeded_exec() -> Value {
        json!({"status": "Succeeded", "fields": {"exit_code": "0", "stdout_tail": "line1\nhttps://deploy.example/abc\n"}})
    }

    fn failed_exec() -> Value {
        json!({"status": "Failed", "fields": {"exit_code": "1", "stderr_tail": "boom", "stdout_tail": ""}})
    }

    #[test]
    fn no_commands_is_passthrough_before_any_exec() {
        assert_eq!(
            decide_tick(false, None, 0).unwrap(),
            TickDecision::Passthrough,
            "empty deploy_commands preserves the record-only Merging behavior"
        );
    }

    #[test]
    fn commands_start_an_exec_then_wait_while_running() {
        assert_eq!(decide_tick(true, None, 0).unwrap(), TickDecision::StartExec);
        assert_eq!(
            decide_tick(true, Some(&running_exec()), 5).unwrap(),
            TickDecision::Wait
        );
    }

    #[test]
    fn succeeded_exec_records_deployed() {
        assert_eq!(
            decide_tick(true, Some(&succeeded_exec()), 3).unwrap(),
            TickDecision::Deployed
        );
    }

    #[test]
    fn deploy_failure_is_fail_closed() {
        let d = decide_tick(true, Some(&failed_exec()), 3).unwrap();
        match d {
            TickDecision::Fail(reason) => assert!(reason.contains("boom"), "got: {reason}"),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn tick_budget_exceeded_fails_closed() {
        match decide_tick(true, None, MAX_DEPLOY_TICKS).unwrap() {
            TickDecision::Fail(reason) => assert!(reason.contains("exceeded"), "got: {reason}"),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn deployment_ref_is_last_stdout_line() {
        assert_eq!(
            deployment_ref_from(&succeeded_exec()),
            "https://deploy.example/abc"
        );
        let quiet = json!({"fields": {"stdout_tail": "  \n\n"}});
        assert_eq!(deployment_ref_from(&quiet), "");
    }

    #[test]
    fn deployed_params_mints_observe_key_owned_by_deployer() {
        let p = deployed_params("en-1", "head1", "dep1", "ok", 2, 7, "task:en-1:k", "factory_deployer");
        assert_eq!(p["merge_sha"], "head1");
        assert_eq!(p["deployment_ref"], "dep1");
        assert_eq!(
            p["operation_key"],
            factory_common::mint_operation_key("en-1", "observe", 2, 7)
        );
        assert_eq!(p["operation_owner"], "factory_deployer");
        assert_eq!(p["phase_ticks"], 0);
        assert_eq!(p["expected_operation_key"], "task:en-1:k");
    }

    #[test]
    fn fail_params_carry_reason_and_fences() {
        let p = fail_params("nope", "task:en-1:k", "factory_deployer");
        assert_eq!(p["failure_reason"], "nope");
        assert_eq!(p["expected_operation_key"], "task:en-1:k");
        assert_eq!(p["expected_operation_owner"], "factory_deployer");
    }
}
