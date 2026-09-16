//! computer_provision — WASM module provisioning a sandbox for a Computer row.
//!
//! Runs on the Computer entity's Provision action (Created → Provisioning,
//! and Sleeping → Provisioning for Wake). Reports ProvisionComplete with the
//! sandbox coordinates, or ProvisionFailed with a human-readable message.
//!
//! Wake semantics: when the row already records a sandbox that still answers
//! a health check, the existing sandbox is reused and no new one is created.
//! A stale recorded sandbox is discarded and replaced.
//!
//! Failure reporting: a domain failure (provider error, unhealthy sandbox,
//! setup script failure) is reported as a *successful* module run whose
//! callback is ProvisionFailed(error_message), so the reason lands on the
//! Computer row. set_error_result (→ the spec's on_failure backstop) is
//! reserved for invocations that cannot report at all.
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;
use wasm_helpers::entity_field_str;
use wasm_helpers::sandbox::{
    self, SandboxConfig, SandboxHandle, normalize_sandbox_provider,
};

/// Sandboxes are created with a 6h provider-side TTL so a Computer survives
/// a full dark-factory task (plan + implement + validate + publish + merge +
/// observe, with repair rounds) without the floor disappearing mid-phase.
/// Overridable from the spec's `[action.triggers.config]` with a plain
/// (non-secret) `sandbox_timeout_seconds` value.
const DEFAULT_SANDBOX_TIMEOUT_SECONDS: u32 = 21_600;

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        // CheckProvision ticks (state_timeout self-loops) poll sandbox health
        // instead of creating; Provision/Wake ensure a sandbox exists. WASM
        // has no timers, so waiting lives in the state machine, not here.
        if ctx.trigger_action == "CheckProvision" {
            return poll_tick(&ctx, &fields);
        }

        match provision(&ctx, &fields) {
            Ok(Provisioned {
                machine_id,
                sandbox_url,
                reused,
            }) => {
                if reused {
                    // Healthy recorded sandbox (Wake fast-path): straight to Ready.
                    ctx.log(
                        "info",
                        &format!(
                            "computer_provision: computer {} reusing healthy sandbox {machine_id}",
                            ctx.entity_id
                        ),
                    );
                    set_success_result(
                        "ProvisionComplete",
                        &json!({
                            "machine_id": machine_id,
                            "sandbox_url": sandbox_url,
                            "ssh_host": "",
                        }),
                    );
                } else {
                    // Fresh sandbox: record the handle and let CheckProvision
                    // ticks poll health + run setup before completing.
                    ctx.log(
                        "info",
                        &format!(
                            "computer_provision: computer {} created sandbox {machine_id}; polling health",
                            ctx.entity_id
                        ),
                    );
                    set_success_result(
                        "ProvisionCreated",
                        &json!({
                            "machine_id": machine_id,
                            "sandbox_url": sandbox_url,
                            "provision_ticks": 0,
                        }),
                    );
                }
            }
            Err(message) => {
                ctx.log(
                    "error",
                    &format!(
                        "computer_provision: computer {} failed: {message}",
                        ctx.entity_id
                    ),
                );
                set_success_result("ProvisionFailed", &json!({ "error_message": message }));
            }
        }
        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

struct Provisioned {
    machine_id: String,
    sandbox_url: String,
    reused: bool,
}

/// One CheckProvision tick: health-check the recorded sandbox, run the
/// setup script on first health, then report Complete / Poll / Failed.
fn poll_tick(ctx: &Context, fields: &Value) -> Result<(), String> {
    let provider = entity_field_str(fields, &["provider", "Provider"])
        .filter(|s| !s.is_empty())
        .map(normalize_sandbox_provider)
        .unwrap_or_else(|| "tensorlake".to_string());
    let Some(handle) = recorded_handle(fields, &provider) else {
        set_success_result(
            "ProvisionFailed",
            &json!({ "error_message": "provisioning lost the sandbox handle (no machine_id/sandbox_url on the row)" }),
        );
        return Ok(());
    };

    let healthy = sandbox::sandbox_health_check(ctx, &handle).unwrap_or(false);
    let ticks = counter_from_state(&ctx.entity_state, "provision_ticks");
    match poll_decision(healthy, ticks, PROVISION_MAX_TICKS) {
        TickDecision::Retry { next_ticks } => {
            ctx.log(
                "info",
                &format!(
                    "computer_provision: sandbox {} not healthy yet (tick {next_ticks})",
                    handle.sandbox_id
                ),
            );
            set_success_result(
                "ProvisionPoll",
                &json!({ "provision_ticks": next_ticks }),
            );
        }
        TickDecision::Failed { reason } => {
            let _ = sandbox::sandbox_terminate(ctx, &handle);
            set_success_result("ProvisionFailed", &json!({ "error_message": reason }));
        }
        TickDecision::Complete => {
            if let Err(message) = run_setup_script(ctx, fields, &handle) {
                let _ = sandbox::sandbox_terminate(ctx, &handle);
                set_success_result("ProvisionFailed", &json!({ "error_message": message }));
                return Ok(());
            }
            ctx.log(
                "info",
                &format!(
                    "computer_provision: computer {} ready (machine_id={})",
                    ctx.entity_id, handle.sandbox_id
                ),
            );
            set_success_result(
                "ProvisionComplete",
                &json!({
                    "machine_id": handle.sandbox_id,
                    "sandbox_url": handle.sandbox_url,
                    "ssh_host": "",
                }),
            );
        }
    }
    Ok(())
}

/// Optional operator-supplied setup (toolchain, CLIs). Runs once per
/// sandbox on the first healthy poll tick; Wake reuse never reaches it.
/// Scripts must be idempotent: a host crash between setup and the
/// ProvisionComplete callback re-runs the script on the next tick.
fn run_setup_script(ctx: &Context, fields: &Value, handle: &SandboxHandle) -> Result<(), String> {
    let setup_script = entity_field_str(fields, &["setup_script", "SetupScript"]).unwrap_or("");
    if setup_script.trim().is_empty() {
        return Ok(());
    }
    let result = sandbox::sandbox_exec(ctx, handle, setup_script, "/")?;
    if result.exit_code != 0 {
        return Err(format!(
            "setup_script exited {}: {}",
            result.exit_code,
            tail(&result.stderr, 500)
        ));
    }
    Ok(())
}

fn provision(ctx: &Context, fields: &Value) -> Result<Provisioned, String> {
    let provider = entity_field_str(fields, &["provider", "Provider"])
        .filter(|s| !s.is_empty())
        .map(normalize_sandbox_provider)
        .unwrap_or_else(|| "tensorlake".to_string());

    // Wake path: a recorded sandbox that is still healthy is reused as-is.
    let recorded = recorded_handle(fields, &provider);
    if let Some(handle) = &recorded {
        match sandbox::sandbox_health_check(ctx, handle) {
            Ok(true) => {
                return Ok(Provisioned {
                    machine_id: handle.sandbox_id.clone(),
                    sandbox_url: handle.sandbox_url.clone(),
                    reused: true,
                });
            }
            Ok(false) | Err(_) => {
                ctx.log(
                    "info",
                    &format!(
                        "computer_provision: recorded sandbox {} is gone; creating a fresh one",
                        handle.sandbox_id
                    ),
                );
            }
        }
    }

    let timeout_override = ctx.config.get("sandbox_timeout_seconds").cloned();
    // Tensorlake accepts image-less creates that never boot (phantom
    // sandboxes that 404 on the data plane); refuse before creating.
    require_image(fields, &provider)?;
    let config = sandbox_config_from_computer(fields, timeout_override.as_deref());
    // Create returns as soon as the provider acknowledges; health polling,
    // setup, and completion all happen on CheckProvision ticks.
    let handle = sandbox::sandbox_create(ctx, &provider, &config)?;

    Ok(Provisioned {
        machine_id: handle.sandbox_id,
        sandbox_url: handle.sandbox_url,
        reused: false,
    })
}

/// Build a handle from the row's recorded coordinates, if both are present.
fn recorded_handle(fields: &Value, provider: &str) -> Option<SandboxHandle> {
    let machine_id = entity_field_str(fields, &["machine_id", "MachineId"]).unwrap_or("");
    let sandbox_url = entity_field_str(fields, &["sandbox_url", "SandboxUrl"]).unwrap_or("");
    if machine_id.is_empty() || sandbox_url.is_empty() {
        return None;
    }
    Some(SandboxHandle {
        sandbox_url: sandbox_url.to_string(),
        sandbox_id: machine_id.to_string(),
        provider: provider.to_string(),
    })
}

/// Map Computer row fields to a SandboxConfig. `cpu_cores`/`memory_gb` are
/// the operator-facing fields; garbage values fall back to the defaults
/// rather than failing provisioning.
fn sandbox_config_from_computer(fields: &Value, timeout_override: Option<&str>) -> SandboxConfig {
    let image = entity_field_str(fields, &["base_image", "BaseImage"])
        .unwrap_or("")
        .trim()
        .to_string();
    let cpus = parse_u32_field(fields, &["cpu_cores", "CpuCores"], 2).max(1);
    let memory_gb = parse_u32_field(fields, &["memory_gb", "MemoryGb"], 4).max(1);
    let timeout_seconds = timeout_override
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SANDBOX_TIMEOUT_SECONDS);
    SandboxConfig {
        image,
        cpus,
        memory_mb: memory_gb * 1024,
        timeout_seconds,
        ..SandboxConfig::default()
    }
}

/// Tensorlake requires an image; other providers may default server-side.
fn require_image(fields: &Value, provider: &str) -> Result<String, String> {
    let image = entity_field_str(fields, &["base_image", "BaseImage"])
        .unwrap_or("")
        .trim()
        .to_string();
    if provider == "tensorlake" && image.is_empty() {
        return Err(
            "base_image is required for tensorlake sandboxes (image-less creates never boot)"
                .to_string(),
        );
    }
    Ok(image)
}

fn parse_u32_field(fields: &Value, keys: &[&str], default: u32) -> u32 {
    entity_field_str(fields, keys)
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

fn tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// What a CheckProvision poll tick decided. Pure so the state-machine
/// contract stays testable without a provider.
#[derive(Debug, PartialEq, Eq)]
enum TickDecision {
    /// Sandbox healthy: report ProvisionComplete (after setup, if any).
    Complete,
    /// Not healthy yet: report ProvisionPoll with the next tick count.
    Retry { next_ticks: u64 },
    /// Tick budget exhausted: report ProvisionFailed.
    Failed { reason: String },
}

/// Max CheckProvision ticks (30s apart) before provisioning is declared
/// failed. 20 ticks ≈ 10 minutes — generous for a provider cold start.
const PROVISION_MAX_TICKS: u64 = 20;

fn poll_decision(healthy: bool, current_ticks: u64, max_ticks: u64) -> TickDecision {
    if healthy {
        TickDecision::Complete
    } else if current_ticks + 1 >= max_ticks {
        TickDecision::Failed {
            reason: format!(
                "sandbox never became healthy after {max_ticks} provision ticks"
            ),
        }
    } else {
        TickDecision::Retry {
            next_ticks: current_ticks + 1,
        }
    }
}

/// Read a counter from the entity state snapshot (counters live next to
/// fields, not inside them).
fn counter_from_state(entity_state: &Value, name: &str) -> u64 {
    entity_state
        .get("counters")
        .and_then(|c| c.get(name))
        .and_then(|r| {
            r.as_u64()
                .or_else(|| r.as_str().and_then(|s| s.trim().parse().ok()))
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_decision_healthy_completes_regardless_of_ticks() {
        assert_eq!(poll_decision(true, 0, 20), TickDecision::Complete);
        assert_eq!(poll_decision(true, 19, 20), TickDecision::Complete);
        assert_eq!(poll_decision(true, 99, 20), TickDecision::Complete);
    }

    #[test]
    fn poll_decision_retries_and_counts_below_cap() {
        assert_eq!(
            poll_decision(false, 0, 20),
            TickDecision::Retry { next_ticks: 1 }
        );
        assert_eq!(
            poll_decision(false, 18, 20),
            TickDecision::Retry { next_ticks: 19 }
        );
    }

    #[test]
    fn poll_decision_fails_when_tick_budget_exhausted() {
        match poll_decision(false, 19, 20) {
            TickDecision::Failed { reason } => assert!(reason.contains("healthy")),
            other => panic!("expected Failed, got {other:?}"),
        }
        // Cap of 1: the first unhealthy tick already exhausts the budget.
        assert!(matches!(
            poll_decision(false, 0, 1),
            TickDecision::Failed { .. }
        ));
    }

    #[test]
    fn counter_from_state_reads_counters_object_and_defaults_zero() {
        let state = json!({"fields": {}, "counters": {"provision_ticks": 7}});
        assert_eq!(counter_from_state(&state, "provision_ticks"), 7);
        let missing = json!({"fields": {}});
        assert_eq!(counter_from_state(&missing, "provision_ticks"), 0);
        let stringy = json!({"counters": {"provision_ticks": "3"}});
        assert_eq!(counter_from_state(&stringy, "provision_ticks"), 3);
    }

    #[test]
    fn config_maps_base_image_into_image() {
        let fields = json!({"base_image": "den-dev-bookworm-dind-v4"});
        let config = sandbox_config_from_computer(&fields, None);
        assert_eq!(config.image, "den-dev-bookworm-dind-v4");
    }

    #[test]
    fn require_image_rejects_empty_for_tensorlake() {
        let fields = json!({"provider": "tensorlake", "base_image": "  "});
        let err = require_image(&fields, "tensorlake").unwrap_err();
        assert!(err.contains("base_image"), "unexpected error: {err}");
    }

    #[test]
    fn require_image_accepts_present_image() {
        let fields = json!({"base_image": "tensorlake/ubuntu-minimal"});
        assert_eq!(
            require_image(&fields, "tensorlake").unwrap(),
            "tensorlake/ubuntu-minimal"
        );
    }

    #[test]
    fn config_maps_computer_fields() {
        let fields = json!({"cpu_cores": "8", "memory_gb": "16"});
        let config = sandbox_config_from_computer(&fields, None);
        assert_eq!(config.cpus, 8);
        assert_eq!(config.memory_mb, 16 * 1024);
        assert_eq!(config.timeout_seconds, DEFAULT_SANDBOX_TIMEOUT_SECONDS);
        assert!(config.internet_access);
    }

    #[test]
    fn config_defaults_on_missing_or_garbage_fields() {
        let config = sandbox_config_from_computer(&json!({}), None);
        assert_eq!(config.cpus, 2);
        assert_eq!(config.memory_mb, 4 * 1024);

        let garbage = sandbox_config_from_computer(
            &json!({"cpu_cores": "many", "memory_gb": "lots"}),
            None,
        );
        assert_eq!(garbage.cpus, 2);
        assert_eq!(garbage.memory_mb, 4 * 1024);
    }

    #[test]
    fn config_clamps_zero_to_one() {
        let config =
            sandbox_config_from_computer(&json!({"cpu_cores": "0", "memory_gb": "0"}), None);
        assert_eq!(config.cpus, 1);
        assert_eq!(config.memory_mb, 1024);
    }

    #[test]
    fn timeout_override_parsed_and_validated() {
        let fields = json!({});
        assert_eq!(
            sandbox_config_from_computer(&fields, Some("7200")).timeout_seconds,
            7200
        );
        // Garbage or zero falls back to the default rather than creating a
        // sandbox that dies instantly.
        assert_eq!(
            sandbox_config_from_computer(&fields, Some("soon")).timeout_seconds,
            DEFAULT_SANDBOX_TIMEOUT_SECONDS
        );
        assert_eq!(
            sandbox_config_from_computer(&fields, Some("0")).timeout_seconds,
            DEFAULT_SANDBOX_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn recorded_handle_requires_both_coordinates() {
        let provider = "tensorlake";
        assert!(
            recorded_handle(&json!({}), provider).is_none(),
            "empty row has nothing to reuse"
        );
        assert!(
            recorded_handle(&json!({"machine_id": "sbx-1"}), provider).is_none(),
            "machine_id without sandbox_url cannot be reached"
        );
        let handle = recorded_handle(
            &json!({"machine_id": "sbx-1", "sandbox_url": "https://sbx-1.sandbox.tensorlake.ai"}),
            provider,
        )
        .expect("both coordinates recorded");
        assert_eq!(handle.sandbox_id, "sbx-1");
        assert_eq!(handle.provider, "tensorlake");
    }

    #[test]
    fn tail_keeps_short_text_and_cuts_long_text() {
        assert_eq!(tail("boom", 500), "boom");
        let long = "x".repeat(1000);
        assert_eq!(tail(&long, 500).len(), 500);
    }
}
