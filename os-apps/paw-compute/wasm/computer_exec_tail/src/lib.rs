//! computer_exec_tail — WASM module streaming IN-FLIGHT exec output (ADR-0005).
//!
//! Runs on the Exec entity's CheckOutput self-loop (every 5 s while Running).
//! Reads the tee'd per-exec log file on the sandbox (`/tmp/.exec-out/<id>.log`,
//! written by computer_exec's wrap_command) and reports ReportOutput with a
//! bounded tail, so the Exec row — and any `/tdata/$events` subscriber — sees
//! output while the command is still running. The final RunSucceeded report
//! from computer_exec still carries the authoritative separated streams.
//!
//! This module NEVER errors: any failure (computer not Ready, missing log
//! file, sandbox hiccup) degrades to the empty stay callback so a tailing
//! problem can never kill the run (CheckOutput has no on_failure).
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;
use wasm_helpers::sandbox::{self, SandboxHandle, normalize_sandbox_provider};
use wasm_helpers::{bounded_reads, entity_field_str, odata_headers, resolve_temper_api_url};

/// Same bound as computer_exec: the row tail never exceeds this many bytes.
const OUTPUT_TAIL_BYTES: usize = 262_144;

/// Absolute log directory shared with computer_exec's wrap_command. Absolute
/// (not `~`) so the sandbox file API can read it without shell expansion.
const EXEC_LOG_DIR: &str = "/tmp/.exec-out";

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    // Never error: every failure path degrades to the empty stay callback
    // (set_success_result("", {})), so a tailing problem can never kill the
    // run — CheckOutput carries no on_failure by design (ADR-0005).
    let ctx = match Context::from_host() {
        Ok(ctx) => ctx,
        Err(_) => {
            set_success_result("", &json!({}));
            return 0;
        }
    };
    let outcome = (|| -> Result<Option<Value>, String> {
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));
        let exec_id = ctx.entity_id.clone();
        let computer_id = fields
            .get("computer_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("missing computer_id")?
            .to_string();

        let temper_api_url = resolve_temper_api_url(&ctx, &fields);
        let computer = fetch_computer(&ctx, &temper_api_url, &fields, &computer_id)?;
        let handle = sandbox_handle_from_computer(&computer)?;

        let path = exec_log_path(&exec_id);
        match sandbox::sandbox_file_read(&ctx, &handle, &path) {
            Ok(content) => {
                let report = decide_report(&content);
                if report.is_some() {
                    ctx.log(
                        "info",
                        &format!(
                            "computer_exec_tail: exec {exec_id} in-flight output ({} bytes)",
                            content.len()
                        ),
                    );
                }
                Ok(report)
            }
            Err(err) => {
                ctx.log(
                    "info",
                    &format!("computer_exec_tail: exec {exec_id} log not readable yet ({err})"),
                );
                Ok(None)
            }
        }
    })();

    match outcome {
        Ok(Some(params)) => set_success_result("ReportOutput", &params),
        Ok(None) => set_success_result("", &json!({})),
        Err(err) => {
            ctx.log("warn", &format!("computer_exec_tail: degraded to stay: {err}"));
            set_success_result("", &json!({}))
        }
    }
    0
}

/// The ReportOutput params for a log body, or None when there is nothing to
/// report yet (missing/empty output). Pure for unit tests.
fn decide_report(content: &str) -> Option<Value> {
    if content.is_empty() {
        return None;
    }
    Some(json!({
        "stdout_tail": output_tail(content, OUTPUT_TAIL_BYTES),
        "stdout_bytes": content.len().to_string(),
    }))
}

/// Keep the LAST `max_bytes` of `text` (byte-boundary safe on UTF-8 chars).
fn output_tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// Absolute log path for an exec id — MUST stay in lockstep with
/// computer_exec::wrap_command (same dir, same sanitization).
fn exec_log_path(exec_id: &str) -> String {
    format!("{EXEC_LOG_DIR}/{}.log", sanitize_exec_id(exec_id))
}

/// Reduce an exec id to a filename-safe token. Copy of
/// computer_exec::sanitize_exec_id — keep in sync (covered by parity tests
/// in both crates against the same vectors).
fn sanitize_exec_id(exec_id: &str) -> String {
    let cleaned: String = exec_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "exec".to_string()
    } else {
        cleaned
    }
}

fn fetch_computer(
    ctx: &Context,
    temper_api_url: &str,
    fields: &Value,
    computer_id: &str,
) -> Result<Value, String> {
    let headers = odata_headers(ctx, &ctx.tenant, fields);
    let path = format!(
        "/tdata/Computers('{}')",
        bounded_reads::odata_escape(computer_id)
    );
    bounded_reads::get_json(ctx, temper_api_url, &path, &headers, "computer_exec_tail")
}

/// Build a SandboxHandle from a Computer row — same readiness contract as
/// computer_exec: refuse to touch an unprovisioned computer.
fn sandbox_handle_from_computer(computer: &Value) -> Result<SandboxHandle, String> {
    let status = entity_field_str(computer, &["Status", "status"]).unwrap_or("");
    if !status.is_empty() && status != "Ready" {
        return Err(format!("computer is {status}, not Ready"));
    }

    let sandbox_url = entity_field_str(computer, &["SandboxUrl", "sandbox_url"])
        .map(str::trim)
        .unwrap_or("");
    if sandbox_url.is_empty() {
        return Err("no sandbox_url recorded — provision the computer first".to_string());
    }

    let sandbox_id = entity_field_str(computer, &["MachineId", "machine_id"])
        .filter(|s| !s.is_empty())
        .or_else(|| entity_field_str(computer, &["Name", "name"]).filter(|s| !s.is_empty()))
        .unwrap_or("computer-sandbox");

    let provider = entity_field_str(computer, &["Provider", "provider"])
        .filter(|s| !s.is_empty())
        .map(normalize_sandbox_provider)
        .unwrap_or_else(|| "tensorlake".to_string());

    Ok(SandboxHandle {
        sandbox_url: sandbox_url.to_string(),
        sandbox_id: sandbox_id.to_string(),
        provider,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- decide_report --------------------------------------------------------

    #[test]
    fn empty_output_stays_silent() {
        assert_eq!(decide_report(""), None, "no output yet → stay callback");
    }

    #[test]
    fn short_output_reports_in_full() {
        let params = decide_report("line one\nline two\n").unwrap();
        assert_eq!(params["stdout_tail"], "line one\nline two\n");
        assert_eq!(params["stdout_bytes"], "18");
    }

    #[test]
    fn long_output_is_truncated_to_the_tail_bound() {
        let big = "x".repeat(OUTPUT_TAIL_BYTES + 10_000);
        let params = decide_report(&big).unwrap();
        let tail = params["stdout_tail"].as_str().unwrap();
        assert_eq!(tail.len(), OUTPUT_TAIL_BYTES);
        assert_eq!(params["stdout_bytes"], (OUTPUT_TAIL_BYTES + 10_000).to_string());
    }

    // -- exec_log_path / sanitize parity with computer_exec -------------------

    #[test]
    fn log_path_uses_the_absolute_dir() {
        // Absolute so the sandbox file API needs no shell ~ expansion.
        assert_eq!(exec_log_path("exec-1"), "/tmp/.exec-out/exec-1.log");
    }

    #[test]
    fn sanitize_matches_computer_exec_vectors() {
        // Same vectors as computer_exec's tests — drift here breaks tailing.
        assert_eq!(sanitize_exec_id("exec-1"), "exec-1");
        assert_eq!(sanitize_exec_id("abc/../../../etc/9"), "abc_.._.._.._etc_9");
        assert_eq!(sanitize_exec_id(""), "exec");
        assert_eq!(sanitize_exec_id("en-01a0ae6c-f7aa"), "en-01a0ae6c-f7aa");
    }

    // -- output_tail ----------------------------------------------------------

    #[test]
    fn tail_keeps_short_output_intact() {
        assert_eq!(output_tail("hello", 8192), "hello");
    }

    #[test]
    fn tail_respects_utf8_boundaries() {
        let s = "é".repeat(100); // 200 bytes
        let tail = output_tail(&s, 3);
        assert!(tail.len() <= 3 && !tail.is_empty());
        assert!(std::str::from_utf8(tail.as_bytes()).is_ok());
    }
}
