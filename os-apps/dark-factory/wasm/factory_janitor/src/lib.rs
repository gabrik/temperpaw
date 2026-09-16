//! factory_janitor — terminal cleanup for a FactoryTask.
//!
//! Fires on the three terminal-entry actions (ObservationPassed → Completed,
//! FailTask → Failed, ExpireTask → Failed). Its one job: dispatch `Destroy`
//! on the task's `computer_id`, tearing the sandbox down through the
//! governed paw-compute lifecycle (computer_terminate deletes the provider
//! sandbox).
//!
//! Idempotency: janitor triggers are wired on transitions, not states, so a
//! well-behaved kernel runs this once per terminal entry; a Computer already
//! Destroyed (e.g. a retried dispatch) is treated as success. A task that
//! never attached a Computer is a no-op.
//!
//! The task row is already terminal when this module runs, so it always
//! reports an empty success callback.

use serde_json::{Value, json};
use temper_wasm_sdk::prelude::*;
use wasm_helpers::entity_field_str;

/// What the janitor decided to do with the task's attached Computer.
#[derive(Debug, PartialEq)]
enum CleanupPlan {
    /// No computer attached — nothing to do.
    Noop,
    /// Dispatch Destroy on this computer id.
    Destroy(String),
}

fn cleanup_plan(fields: &Value) -> CleanupPlan {
    let computer_id = entity_field_str(fields, &["computer_id", "ComputerId"])
        .unwrap_or("")
        .trim();
    if computer_id.is_empty() {
        CleanupPlan::Noop
    } else {
        CleanupPlan::Destroy(computer_id.to_string())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        match cleanup_plan(&fields) {
            CleanupPlan::Noop => {
                ctx.log(
                    "info",
                    &format!(
                        "factory_janitor: task {} has no attached computer; nothing to clean",
                        ctx.entity_id
                    ),
                );
            }
            CleanupPlan::Destroy(computer_id) => {
                let status = factory_common::get_entity(&ctx, "Computers", &computer_id, &fields)
                    .ok()
                    .and_then(|row| {
                        row.get("status")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    });
                match status.as_deref() {
                    Some("Destroyed") => {
                        ctx.log(
                            "info",
                            &format!(
                                "factory_janitor: computer {computer_id} already Destroyed (idempotent no-op)"
                            ),
                        );
                    }
                    _ => {
                        factory_common::dispatch_action(
                            &ctx,
                            "Computers",
                            &computer_id,
                            "Temper.PawCompute",
                            "Destroy",
                            &json!({}),
                            &fields,
                        )
                        .map_err(|e| {
                            format!(
                                "factory_janitor: failed to Destroy computer {computer_id}: {e}"
                            )
                        })?;
                        ctx.log(
                            "info",
                            &format!(
                                "factory_janitor: destroyed computer {computer_id} for task {}",
                                ctx.entity_id
                            ),
                        );
                    }
                }
            }
        }

        // TerminalCleanup is a self-loop on a terminal state: no callback.
        set_success_result("", &json!({}));
        Ok(())
    })();

    if let Err(e) = result {
        // The task is terminal; a cleanup failure must surface but cannot
        // change the row. Log loudly — observability carries it to the
        // human channel. No error callback: TerminalCleanup has no
        // on_failure wired, and an error result would only record noise
        // on a completed row (mirrors computer_terminate).
        if let Ok(ctx) = Context::from_host() {
            ctx.log("error", &format!("factory_janitor: {e}"));
        }
        set_success_result("", &json!({}));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_attached_computer_is_noop() {
        assert_eq!(cleanup_plan(&json!({})), CleanupPlan::Noop);
        assert_eq!(cleanup_plan(&json!({"computer_id": "  "})), CleanupPlan::Noop);
    }

    #[test]
    fn attached_computer_is_destroyed() {
        assert_eq!(
            cleanup_plan(&json!({"computer_id": "en-abc"})),
            CleanupPlan::Destroy("en-abc".to_string())
        );
    }
}
