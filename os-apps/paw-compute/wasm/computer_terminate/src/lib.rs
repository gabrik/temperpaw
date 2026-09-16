//! computer_terminate — WASM module deleting a Computer's sandbox.
//!
//! Runs on the Computer entity's Destroy action (→ Destroyed, terminal).
//! Destroy is the last transition the row ever takes, so this module always
//! reports an empty callback: the sandbox deletion is best-effort cleanup
//! behind a state change that has already happened. A Computer that never
//! provisioned (no recorded machine_id) is a no-op, and a sandbox that is
//! already gone is a success — terminate is idempotent by design.
//!
//! A provider error is logged (the observability pipeline surfaces it to the
//! human channel) but never blocks Destroy.
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;
use wasm_helpers::entity_field_str;
use wasm_helpers::sandbox::{self, SandboxHandle, normalize_sandbox_provider};

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        match handle_from_row(&fields) {
            None => {
                ctx.log(
                    "info",
                    &format!(
                        "computer_terminate: computer {} never provisioned; nothing to delete",
                        ctx.entity_id
                    ),
                );
            }
            Some(handle) => {
                if let Err(e) = sandbox::sandbox_terminate(&ctx, &handle) {
                    // Best-effort: the row is already Destroyed. Log loudly;
                    // the error must surface through the observability
                    // pipeline, never silently.
                    ctx.log(
                        "error",
                        &format!(
                            "computer_terminate: failed to delete sandbox {} for computer {}: {e}",
                            handle.sandbox_id, ctx.entity_id
                        ),
                    );
                } else {
                    ctx.log(
                        "info",
                        &format!(
                            "computer_terminate: deleted sandbox {} for computer {}",
                            handle.sandbox_id, ctx.entity_id
                        ),
                    );
                }
            }
        }

        // Destroy is terminal: no callback action. The kernel accepts an
        // empty callback as "nothing further to dispatch".
        set_success_result("", &json!({}));
        Ok(())
    })();

    if let Err(e) = result {
        // Even here, never fail the invocation: Destroy already happened and
        // there is no on_failure wired for a terminal transition.
        // (set_error_result would only record noise on a completed row.)
        // Context may not even be available; nothing more we can do.
        let _ = e;
    }
    0
}

/// Build a handle from the row's recorded coordinates, if any.
///
/// Provider resolution mirrors computer_exec: the recorded provider wins,
/// defaulting to tensorlake; the sandbox_url is carried through for
/// completeness even though termination only needs the control-plane id.
fn handle_from_row(fields: &Value) -> Option<SandboxHandle> {
    let machine_id = entity_field_str(fields, &["machine_id", "MachineId"]).unwrap_or("");
    if machine_id.is_empty() {
        return None;
    }
    let provider = entity_field_str(fields, &["provider", "Provider"])
        .filter(|s| !s.is_empty())
        .map(normalize_sandbox_provider)
        .unwrap_or_else(|| "tensorlake".to_string());
    let sandbox_url = entity_field_str(fields, &["sandbox_url", "SandboxUrl"]).unwrap_or("");
    Some(SandboxHandle {
        sandbox_url: sandbox_url.to_string(),
        sandbox_id: machine_id.to_string(),
        provider,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_machine_id_means_nothing_to_delete() {
        assert!(handle_from_row(&json!({})).is_none());
        assert!(handle_from_row(&json!({"machine_id": ""})).is_none());
    }

    #[test]
    fn handle_uses_recorded_provider_and_coordinates() {
        let handle = handle_from_row(&json!({
            "machine_id": "sbx-abc123",
            "sandbox_url": "https://sbx-abc123.sandbox.tensorlake.ai",
            "provider": "tl",
        }))
        .expect("machine_id recorded");
        assert_eq!(handle.sandbox_id, "sbx-abc123");
        assert_eq!(handle.provider, "tensorlake");
        assert_eq!(handle.sandbox_url, "https://sbx-abc123.sandbox.tensorlake.ai");
    }

    #[test]
    fn handle_defaults_provider_to_tensorlake() {
        let handle = handle_from_row(&json!({"machine_id": "sbx-1"})).expect("machine_id");
        assert_eq!(handle.provider, "tensorlake");
    }
}
