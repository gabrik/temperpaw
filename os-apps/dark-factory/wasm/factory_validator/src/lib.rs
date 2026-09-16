//! factory_validator — runs a FactoryTask's test suite as a governed Exec.
//!
//! Fires on each CheckValidating tick (30s state_timeout, self-loop). The
//! module is a pure observer/starter of the Exec that carries the test run:
//!
//! - No Exec for this phase yet → create + Run one (command: the
//!   FactoryConfig's test_commands in the repo workdir) and wait.
//! - Exec still running → wait.
//! - Exec Succeeded → ValidationPassed (mints the publish-phase key).
//! - Exec Failed → ValidationFailed with the stderr/stdout evidence, unless
//!   the repair budget (FactoryConfig.max_repair_rounds) is exhausted, in
//!   which case FailTask.
//!
//! Phase budget: the CheckValidating self-loop increments the entity's
//! phase_ticks counter once per tick; exceeding MAX_VALIDATION_TICKS fails
//! the module invocation, and the kernel dispatches ExpireTask via
//! on_failure.
//!
//! The validator never runs a test itself — all execution goes through the
//! governed Exec surface (ADR-0066 D3).

use serde_json::{Value, json};
use temper_wasm_sdk::prelude::*;
use wasm_helpers::entity_field_str;

/// ~30 min at the 30s tick cadence (ADR-0066 phase budget for Validating).
const MAX_VALIDATION_TICKS: u64 = 60;

/// Where the implementer's checkout step places the repository inside the
/// Computer's sandbox. Convention shared across factory modules.
const REPO_WORKDIR: &str = "/work/repo";

/// What one tick decided to report.
#[derive(Debug, PartialEq)]
enum TickDecision {
    /// Nothing to report; wait for the next tick.
    Wait,
    /// Create the validation Exec (none exists for this phase yet).
    StartExec,
    /// Tests passed: ValidationPassed.
    Passed,
    /// Tests failed with budget left: ValidationFailed (repair loop).
    Repair,
    /// Tests failed, budget exhausted: FailTask.
    Fail,
}

fn exec_phase_status(exec: &Value) -> &str {
    exec.get("status").and_then(|v| v.as_str()).unwrap_or("")
}

fn decide_tick(
    exec: Option<&Value>,
    phase_ticks: u64,
    repair_round: u64,
    max_repair_rounds: u64,
) -> Result<TickDecision, String> {
    if phase_ticks >= MAX_VALIDATION_TICKS {
        return Err(format!(
            "validation phase exceeded {} ticks",
            MAX_VALIDATION_TICKS
        ));
    }
    match exec {
        None => Ok(TickDecision::StartExec),
        Some(e) => match exec_phase_status(e) {
            "Succeeded" => Ok(TickDecision::Passed),
            "Failed" => {
                if repair_round >= max_repair_rounds {
                    Ok(TickDecision::Fail)
                } else {
                    Ok(TickDecision::Repair)
                }
            }
            _ => Ok(TickDecision::Wait),
        },
    }
}

/// Shell command the validation Exec runs: the config's test_commands in the
/// repo workdir. test_commands may hold several lines; run them as one
/// `sh -e` script so a failing line fails the run.
fn validation_command(test_commands: &str, workdir: &str) -> String {
    format!("cd {workdir} && sh -ec {}", shell_quote(test_commands))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Human-facing summary carried by ValidationPassed.
fn passed_summary(exec: &Value) -> String {
    let out = exec
        .pointer("/fields/stdout_tail")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    format!(
        "validation tests passed (exit 0)\n{}",
        factory_common::truncate(out.trim(), 500)
    )
}

/// Repair evidence carried by ValidationFailed / FailTask. Infra failures
/// (sandbox gone, exec never ran) land in the Exec's error field rather than
/// exit_code/stdout_tail, so surface those first.
fn failure_evidence(exec: &Value) -> String {
    let error = exec
        .pointer("/fields/error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !error.is_empty() {
        return factory_common::truncate(&format!("validation exec failed: {error}"), 1500);
    }
    let exit_code = exec
        .pointer("/fields/exit_code")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let stderr = exec
        .pointer("/fields/stderr_tail")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stdout = exec
        .pointer("/fields/stdout_tail")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    factory_common::truncate(
        &format!(
            "validation tests failed (exit {exit_code})\nstderr:\n{}\nstdout:\n{}",
            stderr.trim(),
            stdout.trim()
        ),
        1500,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
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
        let factory_id = required_field(&fields, "factory_id")?;
        let phase_ticks = counter(&counters, "phase_ticks");
        let repair_round = counter(&counters, "repair_round");

        // Config: test commands + repair budget.
        let config = factory_common::get_entity(&ctx, "FactoryConfigs", &factory_id, &fields)?;
        let test_commands = config
            .pointer("/fields/test_commands")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let status = ctx
            .entity_state
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("Validating")
            .to_string();
        let observing = status == "Observing";
        let observation_commands = config
            .pointer("/fields/observation_commands")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // Observing prefers observation_commands and falls back to
        // test_commands (the post-merge suite, ADR-0066).
        let commands = if observing && !observation_commands.is_empty() {
            observation_commands
        } else {
            test_commands
        };
        if commands.is_empty() {
            return Err(format!(
                "FactoryConfig {factory_id} has empty test_commands"
            ));
        }
        let max_repair_rounds = config
            .pointer("/fields/max_repair_rounds")
            .and_then(|v| v.as_str())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(6);

        // Find this phase's Exec (marker = task:current operation_key).
        let exec = factory_common::find_exec_by_op_key(
            &ctx,
            &computer_id,
            &task_id,
            &operation_key,
            &fields,
        )?;

        match decide_tick(exec.as_ref(), phase_ticks, repair_round, max_repair_rounds)? {
            TickDecision::Wait => {
                ctx.log("info", &format!(
                    "factory_validator: task {task_id} validation still running (tick {phase_ticks})"
                ));
                set_success_result("", &json!({}));
            }
            TickDecision::StartExec => {
                let description = if observing {
                    "run observation commands (post-merge)"
                } else {
                    "run validation tests"
                };
                let exec_id = factory_common::create_and_run_exec(
                    &ctx,
                    &computer_id,
                    &task_id,
                    &operation_key,
                    description,
                    &validation_command(&commands, REPO_WORKDIR),
                    &fields,
                )?;
                ctx.log("info", &format!(
                    "factory_validator: task {task_id} started validation exec {exec_id}"
                ));
                set_success_result("", &json!({}));
            }
            TickDecision::Passed => {
                let exec = exec.as_ref().expect("passed implies exec");
                if observing {
                    set_success_result(
                        "ObservationPassed",
                        &json!({
                            "observation_summary": passed_summary(exec),
                            "operation_result": "passed",
                            "expected_operation_key": operation_key,
                            "expected_operation_owner": operation_owner,
                        }),
                    );
                } else {
                    set_success_result(
                        "ValidationPassed",
                        &json!({
                            "validation_summary": passed_summary(exec),
                            "operation_result": "passed",
                            "expected_operation_key": operation_key,
                            "expected_operation_owner": operation_owner,
                            "operation_key": factory_common::mint_operation_key(&task_id, "publish", repair_round, phase_ticks),
                            "operation_owner": "factory_validator",
                            "phase_ticks": 0,
                        }),
                    );
                }
            }
            TickDecision::Repair => {
                let exec = exec.as_ref().expect("repair implies exec");
                if observing {
                    set_success_result(
                        "ObservationFailed",
                        &json!({
                            "observation_summary": "post-merge observation failed; returning evidence for repair",
                            "repair_context": failure_evidence(exec),
                            "operation_result": "failed",
                            "expected_operation_key": operation_key,
                            "expected_operation_owner": operation_owner,
                            "operation_key": factory_common::mint_operation_key(&task_id, "implement", repair_round + 1, phase_ticks),
                            "operation_owner": "factory_validator",
                        }),
                    );
                    return Ok(());
                }
                set_success_result(
                    "ValidationFailed",
                    &json!({
                        "validation_summary": "validation failed; returning evidence for repair",
                        "repair_context": failure_evidence(exec),
                        "operation_result": "failed",
                        "expected_operation_key": operation_key,
                        "expected_operation_owner": operation_owner,
                        "operation_key": factory_common::mint_operation_key(&task_id, "implement", repair_round + 1, phase_ticks),
                        "operation_owner": "factory_validator",
                        "phase_ticks": 0,
                    }),
                );
            }
            TickDecision::Fail => {
                let exec = exec.as_ref().expect("fail implies exec");
                set_success_result(
                    "FailTask",
                    &json!({
                        "failure_reason": format!(
                            "repair budget exhausted ({max_repair_rounds} rounds): {}",
                            factory_common::truncate(&failure_evidence(exec), 400)
                        ),
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
        // Hard failure: the CheckValidating trigger has on_failure =
        // ExpireTask, so the kernel drives the task to Failed.
        set_error_result(&format!("factory_validator: {e}"));
    }
    0
}

fn required_field(fields: &Value, name: &str) -> Result<String, String> {
    entity_field_str(fields, &[name])
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("task row is missing required field '{name}'"))
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

    fn exec_with(status: &str) -> Value {
        json!({"status": status, "fields": {}})
    }

    #[test]
    fn no_exec_starts_one() {
        assert_eq!(decide_tick(None, 0, 0, 6).unwrap(), TickDecision::StartExec);
    }

    #[test]
    fn running_exec_waits() {
        for status in ["Created", "Running"] {
            assert_eq!(
                decide_tick(Some(&exec_with(status)), 3, 0, 6).unwrap(),
                TickDecision::Wait,
                "status {status}"
            );
        }
    }

    #[test]
    fn succeeded_exec_passes() {
        assert_eq!(
            decide_tick(Some(&exec_with("Succeeded")), 3, 0, 6).unwrap(),
            TickDecision::Passed
        );
    }

    #[test]
    fn failed_exec_repairs_within_budget() {
        assert_eq!(
            decide_tick(Some(&exec_with("Failed")), 3, 2, 6).unwrap(),
            TickDecision::Repair
        );
    }

    #[test]
    fn failed_exec_fails_task_at_budget() {
        assert_eq!(
            decide_tick(Some(&exec_with("Failed")), 3, 6, 6).unwrap(),
            TickDecision::Fail
        );
    }

    #[test]
    fn tick_budget_is_enforced() {
        let err = decide_tick(None, MAX_VALIDATION_TICKS, 0, 6).unwrap_err();
        assert!(err.contains("exceeded"), "unexpected: {err}");
    }

    #[test]
    fn validation_command_runs_script_in_workdir() {
        let cmd = validation_command("cargo test\nnpm run lint", "/work/repo");
        assert_eq!(
            cmd,
            "cd /work/repo && sh -ec 'cargo test\nnpm run lint'"
        );
    }

    #[test]
    fn validation_command_quotes_safely() {
        let cmd = validation_command("echo 'hi'", "/w");
        assert_eq!(cmd, "cd /w && sh -ec 'echo '\"'\"'hi'\"'\"''");
    }

    #[test]
    fn failure_evidence_prefers_exec_error_field() {
        let exec = json!({"status":"Failed","fields":{
            "error": "computer_exec: computer c1: computer is Destroyed, not Ready"
        }});
        let ev = failure_evidence(&exec);
        assert!(ev.contains("computer is Destroyed"), "got: {ev}");
    }

    #[test]
    fn failure_evidence_includes_exit_and_tails() {
        let exec = json!({"status":"Failed","fields":{
            "exit_code": "1",
            "stderr_tail": "boom",
            "stdout_tail": "running 3 tests"
        }});
        let ev = failure_evidence(&exec);
        assert!(ev.contains("exit 1"));
        assert!(ev.contains("boom"));
        assert!(ev.contains("running 3 tests"));
    }
}
