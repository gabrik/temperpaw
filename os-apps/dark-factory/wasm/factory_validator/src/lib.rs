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
        Some(e) => {
            // F20: status Succeeded is not enough — computer_exec reports
            // RunSucceeded even for non-zero exits; exit_code is the real
            // outcome signal.
            if factory_common::exec_succeeded(e) {
                return Ok(TickDecision::Passed);
            }
            match exec_phase_status(e) {
                "Succeeded" | "Failed" => {
                    if repair_round >= max_repair_rounds {
                        Ok(TickDecision::Fail)
                    } else {
                        Ok(TickDecision::Repair)
                    }
                }
                _ => Ok(TickDecision::Wait),
            }
        }
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

// -- ADR-0069: CommandSpec phase commands ---------------------------------------
//
// Profiles carry typed CommandSpec arrays (validation_commands /
// observation_commands) rendered via factory_common::profile_commands; the
// legacy FactoryConfig path carries raw shell strings (test_commands /
// observation_commands).

use factory_common::factory_context_env as factory_env;

fn str_field(profile: &Value, key: &str) -> String {
    profile
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Resolve the shell command for a validation/observation phase:
/// CommandSpec arrays win; legacy raw-shell strings are the fallback; the
/// observation phase falls back to validation commands when it declares none.
fn phase_command(
    profile: &Value,
    observing: bool,
    extra_env: &[(String, String)],
) -> Result<String, String> {
    if observing {
        if let Some(shell) = factory_common::profile_commands(profile, "observation_commands", extra_env)? {
            return Ok(shell);
        }
        let legacy_obs = str_field(profile, "observation_commands");
        if !legacy_obs.is_empty() && !legacy_obs.starts_with('[') {
            return Ok(validation_command(&legacy_obs, REPO_WORKDIR));
        }
    }
    // Build runs before validation inside the same Exec: a compile failure
    // is ordinary ValidationFailed evidence for the repair loop (ADR-0069).
    // Build artifacts persist in /work/repo for later deploy/observation
    // commands on the same Computer.
    let build = factory_common::profile_commands(profile, "build_commands", extra_env)?;
    let validation = if let Some(shell) = factory_common::profile_commands(profile, "validation_commands", extra_env)? {
        Some(shell)
    } else {
        let legacy = str_field(profile, "test_commands");
        if legacy.is_empty() {
            None
        } else {
            Some(validation_command(&legacy, REPO_WORKDIR))
        }
    };
    match (build, validation) {
        (Some(b), Some(v)) => Ok(format!("{b} && {v}")),
        (Some(b), None) => Ok(b),
        (None, Some(v)) => Ok(v),
        (None, None) => Err("profile declares no validation commands (validation_commands/test_commands empty)".into()),
    }
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
    // F22: name the failing gate — silent commands (test -f, grep -q)
    // leave empty tails and the agent cannot guess what to satisfy.
    let command = exec
        .pointer("/fields/command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    factory_common::truncate(
        &format!(
            "validation tests failed (exit {exit_code})\ncommand: {}\nstderr:\n{}\nstdout:\n{}",
            command.trim(),
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
        let factory_repo_id = optional_field(&fields, "factory_repo_id");
        let factory_id = optional_field(&fields, "factory_id");
        if factory_repo_id.is_empty() && factory_id.is_empty() {
            return Err("task has neither factory_repo_id nor factory_id".to_string());
        }
        let phase_ticks = counter(&counters, "phase_ticks");
        let repair_round = counter(&counters, "repair_round");

        // Profile: pinned snapshot (ADR-0069) or legacy FactoryConfig row.
        let profile = factory_common::load_profile(&ctx, &fields, &fields)?;
        let status = ctx
            .entity_state
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("Validating")
            .to_string();
        let observing = status == "Observing";
        // Observing prefers the observation commands and falls back to the
        // validation suite (ADR-0066); CommandSpec arrays win over legacy
        // raw-shell strings (ADR-0069).
        let extra_env = factory_env(
            &task_id,
            &optional_field(&fields, "base_sha"),
            &optional_field(&fields, "head_sha"),
            &optional_field(&fields, "deployment_ref"),
        );
        let commands = phase_command(&profile, observing, &extra_env)?;
        let max_repair_rounds = profile
            .get("max_repair_rounds")
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
                    &commands,
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
        // Real shape: computer_exec sets exit_code on every completed run.
        let exec = json!({"status": "Succeeded", "fields": {"exit_code": "0"}});
        assert_eq!(
            decide_tick(Some(&exec), 3, 0, 6).unwrap(),
            TickDecision::Passed
        );
    }

    // F20: a command that exits non-zero still lands as Exec status
    // "Succeeded" (computer_exec RunSucceeded carries exit_code) — the
    // validator must gate on exit_code, not status alone (TID18 merged a
    // PR whose validation and observation both exited 1).
    #[test]
    fn succeeded_exec_with_nonzero_exit_repairs_within_budget() {
        let exec = json!({"status": "Succeeded", "fields": {"exit_code": "1"}});
        assert_eq!(
            decide_tick(Some(&exec), 3, 2, 6).unwrap(),
            TickDecision::Repair
        );
    }

    #[test]
    fn succeeded_exec_with_nonzero_exit_fails_at_budget() {
        let exec = json!({"status": "Succeeded", "fields": {"exit_code": "127"}});
        assert_eq!(
            decide_tick(Some(&exec), 3, 6, 6).unwrap(),
            TickDecision::Fail
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

    // -- ADR-0069: CommandSpec phase commands ----------------------------------

    #[test]
    fn phase_command_prefers_commandspec_arrays_over_legacy_strings() {
        let profile = json!({
            "validation_commands": "[{\"argv\":[\"cargo\",\"test\"],\"cwd\":\"/work/repo\"}]",
            "test_commands": "legacy-should-lose"
        });
        let cmd = phase_command(&profile, false, &[]).unwrap();
        assert!(cmd.contains("( cd /work/repo && cargo test )"), "got: {cmd}");
        assert!(!cmd.contains("legacy"), "got: {cmd}");
    }

    #[test]
    fn phase_command_falls_back_to_legacy_strings() {
        let profile = json!({"validation_commands": "[]", "test_commands": "cargo test"});
        assert_eq!(
            phase_command(&profile, false, &[]).unwrap(),
            "cd /work/repo && sh -ec 'cargo test'"
        );
        let legacy_only = json!({"test_commands": "cargo test"});
        assert!(phase_command(&legacy_only, false, &[]).unwrap().contains("cargo test"));
    }

    #[test]
    fn phase_command_observing_prefers_observation_then_validation() {
        let profile = json!({
            "observation_commands": "[{\"argv\":[\"make\",\"observe\"]}]",
            "validation_commands": "[{\"argv\":[\"cargo\",\"test\"]}]"
        });
        assert!(phase_command(&profile, true, &[]).unwrap().contains("make observe"));
        let no_obs = json!({"validation_commands": "[{\"argv\":[\"cargo\",\"test\"]}]", "observation_commands": "[]"});
        assert!(phase_command(&no_obs, true, &[]).unwrap().contains("cargo test"));
        // Legacy raw-shell observation string still works.
        let legacy_obs = json!({"observation_commands": "cargo test --release", "test_commands": "cargo test"});
        assert!(phase_command(&legacy_obs, true, &[]).unwrap().contains("release"));
    }

    #[test]
    fn phase_command_injects_factory_context_env() {
        let profile = json!({"validation_commands": "[{\"argv\":[\"cargo\",\"test\"]}]"});
        let env = factory_env("en-1", "base1", "head2", "deploy3");
        let cmd = phase_command(&profile, false, &env).unwrap();
        assert!(cmd.contains("FACTORY_TASK_ID=en-1"), "got: {cmd}");
        assert!(cmd.contains("FACTORY_BASE_SHA=base1"), "got: {cmd}");
        assert!(cmd.contains("FACTORY_HEAD_SHA=head2"), "got: {cmd}");
        assert!(cmd.contains("FACTORY_DEPLOYMENT_REF=deploy3"), "got: {cmd}");
    }

    #[test]
    fn phase_command_errors_when_nothing_declared() {
        let profile = json!({"validation_commands": "[]", "test_commands": ""});
        assert!(phase_command(&profile, false, &[]).is_err());
        let broken = json!({"validation_commands": "[{not json"});
        assert!(phase_command(&broken, false, &[]).is_err(), "malformed CommandSpec JSON fails loud");
    }

    #[test]
    fn phase_command_chains_build_before_validation() {
        let profile = json!({
            "build_commands": "[{\"argv\":[\"cargo\",\"build\"],\"cwd\":\"/work/repo\"}]",
            "validation_commands": "[{\"argv\":[\"cargo\",\"test\"],\"cwd\":\"/work/repo\"}]"
        });
        let cmd = phase_command(&profile, false, &[]).unwrap();
        let build_at = cmd.find("cargo build").unwrap();
        let test_at = cmd.find("cargo test").unwrap();
        assert!(build_at < test_at, "build must run first: {cmd}");
        assert!(cmd.contains("&&"), "{cmd}");
    }

    #[test]
    fn phase_command_allows_build_only_and_builds_before_legacy_strings() {
        let build_only = json!({"build_commands": "[{\"argv\":[\"make\"]}]", "validation_commands": "[]", "test_commands": ""});
        assert_eq!(
            phase_command(&build_only, false, &[]).unwrap(),
            "( make )"
        );
        let legacy = json!({"build_commands": "[{\"argv\":[\"cargo\",\"build\"]}]", "test_commands": "cargo test"});
        let cmd = phase_command(&legacy, false, &[]).unwrap();
        assert!(
            cmd.find("cargo build").unwrap() < cmd.find("sh -ec").unwrap(),
            "build precedes legacy test script: {cmd}"
        );
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

// F22: repair context must name the failing gate. Live e2e: a silent gate
// (`test -f gb-e2e.txt`, no output) produced "validation tests failed
// (exit 1)\nstderr:\n\nstdout:" — the agent burned three repair rounds
// guessing. Include the failing command in the evidence.
#[cfg(test)]
mod f22_tests {
    use serde_json::json;

    #[test]
    fn failure_evidence_includes_the_failing_command() {
        let exec = json!({
            "fields": {
                "exit_code": "1",
                "command": "cd /work/repo && test -f gb-e2e.txt",
                "stderr_tail": "",
                "stdout_tail": ""
            }
        });
        let evidence = super::failure_evidence(&exec);
        assert!(evidence.contains("exit 1"), "{evidence}");
        assert!(evidence.contains("test -f gb-e2e.txt"), "{evidence}");
    }
}
