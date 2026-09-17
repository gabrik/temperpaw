//! factory-common — shared helpers for dark-factory WASM modules.
//!
//! Two concerns live here:
//!
//! 1. **Temper API access from WASM** — GET entity rows, dispatch actions on
//!    other entities, list rows with an OData filter. All calls go through
//!    the gated `http_call` host function with `runtime_headers` auth, so
//!    every request is Cedar-authorized as the calling module.
//!
//! 2. **Exec operation discovery** — a FactoryTask's asynchronous work runs
//!    in governed Exec entities attached to its Computer. The module that
//!    starts an operation embeds a `# factory-op: <task_id>:<operation_id>`
//!    marker in the Exec's task_description so a later tick can find the
//!    right Exec without extra state. Discovery is by marker, never by id
//!    stored on the task row: the marker survives crashes between "Exec
//!    created" and "callback reported".

use serde_json::{Value, json};
use temper_wasm_sdk::prelude::*;
use wasm_helpers::{resolve_temper_api_url, runtime_headers};

/// Marker prefix embedded in Exec task_descriptions. The trailing colon is
/// part of the prefix so a plain `contains` cannot confuse ids.
pub const OP_MARKER_PREFIX: &str = "# factory-op: ";

/// Build the operation marker for a (task, operation) pair.
pub fn op_key(task_id: &str, operation_id: &str) -> String {
    format!("{OP_MARKER_PREFIX}{task_id}:{operation_id}")
}

/// True when `text` carries exactly this operation marker.
pub fn op_key_matches(text: &str, task_id: &str, operation_id: &str) -> bool {
    text.contains(&op_key(task_id, operation_id))
}

/// Escape a string literal for an OData $filter expression.
pub fn escape_odata(value: &str) -> String {
    value.replace('\'', "''")
}

/// URL for a bound-action dispatch (POST).
pub fn dispatch_url(
    base: &str,
    entity_set: &str,
    entity_id: &str,
    namespace: &str,
    action: &str,
    tenant: &str,
) -> String {
    format!(
        "{}/tdata/{}('{}')/{}.{}?tenant={}",
        base.trim_end_matches('/'),
        entity_set,
        entity_id,
        namespace,
        action,
        tenant
    )
}

/// URL for a single entity row (GET).
pub fn entity_url(base: &str, entity_set: &str, entity_id: &str, tenant: &str) -> String {
    format!(
        "{}/tdata/{}('{}')?tenant={}",
        base.trim_end_matches('/'),
        entity_set,
        entity_id,
        tenant
    )
}

/// URL for an entity-set list query with an optional OData $filter (GET).
pub fn list_url(base: &str, entity_set: &str, tenant: &str, filter: Option<&str>) -> String {
    let mut url = format!(
        "{}/tdata/{}?tenant={}",
        base.trim_end_matches('/'),
        entity_set,
        tenant
    );
    if let Some(f) = filter {
        url.push_str(&format!("&$filter={}", url_encode(f)));
    }
    url
}

/// Minimal percent-encoding for query values (OData filters live in the
/// query string; spaces and quotes must not break the URL).
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '\'' | '(' | ')' | '$'
            | '=' | ':' => out.push(ch),
            ' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", ch as u32)),
        }
    }
    out
}

/// GET one entity row. Returns the parsed JSON body.
pub fn get_entity(
    ctx: &Context,
    entity_set: &str,
    entity_id: &str,
    fields: &Value,
) -> Result<Value, String> {
    let base = resolve_temper_api_url(ctx, fields);
    let url = entity_url(&base, entity_set, entity_id, &ctx.tenant);
    let headers = runtime_headers(ctx, &ctx.tenant, fields, None, Some("application/json"));
    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "GET {entity_set}('{entity_id}') returned status {}",
            resp.status
        ));
    }
    serde_json::from_str(&resp.body).map_err(|e| format!("failed to parse entity row: {e}"))
}

/// Dispatch a bound action on another entity. `namespace` is the full CSDL
/// namespace (e.g. `Temper.PawCompute`). Returns the updated row.
pub fn dispatch_action(
    ctx: &Context,
    entity_set: &str,
    entity_id: &str,
    namespace: &str,
    action: &str,
    params: &Value,
    fields: &Value,
) -> Result<Value, String> {
    let base = resolve_temper_api_url(ctx, fields);
    let url = dispatch_url(&base, entity_set, entity_id, namespace, action, &ctx.tenant);
    let headers = runtime_headers(
        ctx,
        &ctx.tenant,
        fields,
        Some("application/json"),
        Some("application/json"),
    );
    let resp = ctx.http_call("POST", &url, &headers, &params.to_string())?;
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "dispatch {namespace}.{action} on {entity_set}('{entity_id}') returned status {}: {}",
            resp.status,
            truncate(&resp.body, 300)
        ));
    }
    serde_json::from_str(&resp.body).map_err(|e| format!("failed to parse dispatch result: {e}"))
}

/// Create an entity row. Returns the created row (its `entity_id` field is
/// the new id).
pub fn create_entity(
    ctx: &Context,
    entity_set: &str,
    body: &Value,
    fields: &Value,
) -> Result<Value, String> {
    let base = resolve_temper_api_url(ctx, fields);
    let url = format!(
        "{}/tdata/{}?tenant={}",
        base.trim_end_matches('/'),
        entity_set,
        ctx.tenant
    );
    let headers = runtime_headers(
        ctx,
        &ctx.tenant,
        fields,
        Some("application/json"),
        Some("application/json"),
    );
    let resp = ctx.http_call("POST", &url, &headers, &body.to_string())?;
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "create {entity_set} returned status {}: {}",
            resp.status,
            truncate(&resp.body, 300)
        ));
    }
    serde_json::from_str(&resp.body).map_err(|e| format!("failed to parse created row: {e}"))
}

/// List rows of an entity set, optionally OData-filtered. Returns the raw
/// `value` array (empty when the query matched nothing).
pub fn list_entities(
    ctx: &Context,
    entity_set: &str,
    filter: Option<&str>,
    fields: &Value,
) -> Result<Vec<Value>, String> {
    let base = resolve_temper_api_url(ctx, fields);
    let url = list_url(&base, entity_set, &ctx.tenant, filter);
    let headers = runtime_headers(ctx, &ctx.tenant, fields, None, Some("application/json"));
    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "list {entity_set} returned status {}: {}",
            resp.status,
            truncate(&resp.body, 300)
        ));
    }
    let parsed: Value =
        serde_json::from_str(&resp.body).map_err(|e| format!("failed to parse list: {e}"))?;
    Ok(parsed
        .get("value")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Create a governed Exec on a Computer, with the operation marker embedded
/// in the task description. Returns the new Exec id.
pub fn create_exec(
    ctx: &Context,
    computer_id: &str,
    task_id: &str,
    operation_id: &str,
    description: &str,
    command: &str,
    fields: &Value,
) -> Result<String, String> {
    let body = json!({
        "computer_id": computer_id,
        "task_description": format!("{}\n{}", description, op_key(task_id, operation_id)),
        "command": command,
    });
    let row = create_entity(ctx, "Execs", &body, fields)?;
    row.get("entity_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "created Exec row carried no entity_id".to_string())
}

/// Find the Exec carrying this operation marker on a Computer. Discovery is
/// by marker in task_description (see module docs); returns the first match.
pub fn find_exec_by_op_key(
    ctx: &Context,
    computer_id: &str,
    task_id: &str,
    operation_id: &str,
    fields: &Value,
) -> Result<Option<Value>, String> {
    let filter = format!("computer_id eq '{}'", escape_odata(computer_id));
    let execs = list_entities(ctx, "Execs", Some(&filter), fields)?;
    let marker = op_key(task_id, operation_id);
    Ok(execs.into_iter().find(|e| {
        e.pointer("/fields/task_description")
            .and_then(|v| v.as_str())
            .map(|d| d.contains(&marker))
            .unwrap_or(false)
    }))
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

/// True when the exec completed AND its command exited 0. computer_exec
/// reports RunSucceeded (Exec status "Succeeded") even when the command
/// itself exits non-zero — `fields.exit_code` is the real outcome signal
/// (ADR-0067 F20: deciding on status alone let failing validations pass).
pub fn exec_succeeded(exec: &Value) -> bool {
    exec.get("status").and_then(|v| v.as_str()) == Some("Succeeded")
        && exec.pointer("/fields/exit_code").and_then(|v| v.as_str()) == Some("0")
}

/// Short human-facing evidence for a failed exec: exit code + output tails.
pub fn exec_failure_summary(exec: &Value) -> String {
    let fields = exec.get("fields").cloned().unwrap_or_else(|| json!({}));
    let get = |k: &str| fields.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let mut parts = vec![format!("exit {}", {
        let c = get("exit_code");
        if c.is_empty() { "?" } else { c }
    })];
    if !get("error").is_empty() {
        parts.push(truncate(get("error"), 200));
    }
    if !get("stderr_tail").is_empty() {
        parts.push(format!("stderr: {}", truncate(get("stderr_tail"), 200)));
    }
    if !get("stdout_tail").is_empty() {
        parts.push(format!("stdout: {}", truncate(get("stdout_tail"), 200)));
    }
    parts.join(" | ")
}

// ---------------------------------------------------------------------------
// GitHub (D5): modules hold the token; Computers never see it.
// ---------------------------------------------------------------------------

/// Parse `https://github.com/owner/repo(.git)` into `owner/repo`.
pub fn github_repo_slug(repo_url: &str) -> Result<String, String> {
    let trimmed = repo_url.trim().trim_end_matches('/').trim_end_matches(".git");
    let without_scheme = trimmed.split("://").last().unwrap_or(trimmed);
    let segs: Vec<&str> = without_scheme.split('/').collect();
    if segs.len() < 3 || segs[1].is_empty() || segs[2].is_empty() {
        return Err(format!("cannot parse github repo slug from '{repo_url}'"));
    }
    Ok(format!("{}/{}", segs[1], segs[2]))
}

fn github_token(ctx: &Context) -> Result<String, String> {
    ctx.config
        .get("github_token")
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .ok_or_else(|| "trigger config is missing github_token".to_string())
}

/// Call the GitHub REST API. `path` starts with `/repos/...`. Returns the
/// parsed JSON body.
pub fn github_api(
    ctx: &Context,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let url = format!("https://api.github.com{path}");
    let headers = vec![
        ("Authorization".to_string(), format!("Bearer {}", github_token(ctx)?)),
        ("Accept".to_string(), "application/vnd.github+json".to_string()),
        ("User-Agent".to_string(), "temperpaw-dark-factory".to_string()),
        ("X-GitHub-Api-Version".to_string(), "2022-11-28".to_string()),
    ];
    let resp = match body {
        Some(b) => ctx.http_call(method, &url, &headers, &b.to_string())?,
        None => ctx.http_call(method, &url, &headers, "")?,
    };
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "github api {method} {path} failed (HTTP {}): {}",
            resp.status,
            truncate(&resp.body, 300)
        ));
    }
    serde_json::from_str(&resp.body).map_err(|e| format!("github api {method} {path} returned invalid json: {e}"))
}

/// Recursive file listing of a repo tree at `sha`.
/// Returns (path, blob_sha) for every blob entry.
pub fn github_tree_blobs(ctx: &Context, slug: &str, sha: &str) -> Result<Vec<(String, String)>, String> {
    let tree = github_api(ctx, "GET", &format!("/repos/{slug}/git/trees/{sha}?recursive=1"), None)?;
    if tree.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(format!("github tree {slug}@{sha} is truncated (repo too large for single-tree checkout)"));
    }
    let mut out = Vec::new();
    for entry in tree.get("tree").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let is_blob = entry.get("type").and_then(|v| v.as_str()) == Some("blob");
        let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let sha = entry.get("sha").and_then(|v| v.as_str()).unwrap_or("");
        if is_blob && !path.is_empty() && !sha.is_empty() {
            out.push((path.to_string(), sha.to_string()));
        }
    }
    Ok(out)
}

/// Fetch one blob's content decoded to bytes.
pub fn github_blob_bytes(ctx: &Context, slug: &str, blob_sha: &str) -> Result<Vec<u8>, String> {
    let blob = github_api(ctx, "GET", &format!("/repos/{slug}/git/blobs/{blob_sha}"), None)?;
    let encoding = blob.get("encoding").and_then(|v| v.as_str()).unwrap_or("");
    let content = blob.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if encoding != "base64" {
        return Err(format!("github blob {blob_sha} has unexpected encoding '{encoding}'"));
    }
    base64_decode(content)
}

/// Minimal base64 decoder (GitHub blob payloads; may contain newlines).
pub fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    fn val(b: u8) -> Result<u8, String> {
        match b {
            b'A'..=b'Z' => Ok(b - b'A'),
            b'a'..=b'z' => Ok(b - b'a' + 26),
            b'0'..=b'9' => Ok(b - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("invalid base64 byte 0x{b:02x}")),
        }
    }
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        let vals: Vec<u8> = chunk
            .iter()
            .filter(|&&b| b != b'=')
            .map(|&b| val(b))
            .collect::<Result<_, _>>()?;
        if vals.len() < 2 {
            break;
        }
        out.push((vals[0] << 2) | (vals[1] >> 4));
        if vals.len() >= 3 && pad < 2 {
            out.push((vals[1] << 4) | (vals[2] >> 2));
        }
        if vals.len() >= 4 && pad == 0 {
            out.push((vals[2] << 6) | vals[3]);
        }
    }
    Ok(out)
}

/// Build a wasm-helpers SandboxHandle from a Computer row's fields
/// (`provider` / `machine_id` / `sandbox_url` as written by
/// computer_provision). Provider defaults to `tensorlake` when the row was
/// configured without one (the only provider wired end-to-end today).
pub fn computer_sandbox_handle(fields: &Value) -> Result<wasm_helpers::sandbox::SandboxHandle, String> {
    let get = |name: &str| {
        fields
            .get(name)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("computer row is missing field '{name}'"))
    };
    let provider = fields
        .get("provider")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("tensorlake")
        .to_string();
    Ok(wasm_helpers::sandbox::SandboxHandle {
        sandbox_url: get("sandbox_url")?,
        sandbox_id: get("machine_id")?,
        provider,
    })
}

/// Mint a per-phase operation key. Fencing is CAS, not crypto: the key must
/// simply differ from every key minted before it on this task. Phase name +
/// repair round + tick makes each mint unique along the task's trajectory.
pub fn mint_operation_key(task_id: &str, phase: &str, repair_round: u64, phase_ticks: u64) -> String {
    format!("{task_id}:{phase}:r{repair_round}:t{phase_ticks}")
}

/// Create a governed Exec on a Computer AND dispatch Run on it (the two-step
/// the paw-compute lifecycle requires: Created -> Run -> Running). Returns
/// the new Exec id.
pub fn create_and_run_exec(
    ctx: &Context,
    computer_id: &str,
    task_id: &str,
    operation_id: &str,
    description: &str,
    command: &str,
    fields: &Value,
) -> Result<String, String> {
    let exec_id = create_exec(ctx, computer_id, task_id, operation_id, description, command, fields)?;
    dispatch_action(
        ctx,
        "Execs",
        &exec_id,
        "Temper.PawCompute",
        "Run",
        &json!({}),
        fields,
    )?;
    Ok(exec_id)
}

// ---------------------------------------------------------------------------
// ADR-0069: CommandSpec → governed shell.
//
// Teams declare commands as typed JSON (argv/cwd/env/timeout_seconds) — never
// as shell text. The module layer renders them to a deterministically quoted
// shell string for the governed Exec surface (which runs `sh -ec`). Team-declared
// env keys may not use the reserved FACTORY_* prefix: factory context
// (task id, SHAs, deployment ref) is injected by the modules via extra_env.
// ---------------------------------------------------------------------------

/// Quote one shell word: bare when every byte is shell-safe, single-quoted
/// otherwise (embedded quotes become the '"'"' idiom).
pub fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' | b':' | b'=' | b'+' | b'@' | b'%' | b','));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Render one CommandSpec (`{"argv":[..], "cwd":?, "env":?, "timeout_seconds":?}`)
/// to a shell fragment: `cd <cwd> && timeout <s> env K=V <argv...>`. Missing
/// pieces are simply omitted. `extra_env` (factory context) is appended after
/// the spec's own env; spec env keys with the reserved FACTORY_ prefix are
/// rejected so teams cannot spoof factory context.
pub fn commandspec_to_shell(spec: &Value, extra_env: &[(String, String)]) -> Result<String, String> {
    let argv = spec
        .get("argv")
        .and_then(|v| v.as_array())
        .ok_or("CommandSpec requires an argv array")?;
    if argv.is_empty() {
        return Err("CommandSpec argv must not be empty".into());
    }
    let mut words: Vec<String> = Vec::new();
    for arg in argv {
        words.push(
            arg.as_str()
                .map(shell_quote)
                .ok_or("CommandSpec argv entries must be strings")?,
        );
    }
    let mut env_words: Vec<String> = Vec::new();
    if let Some(env) = spec.get("env").and_then(|v| v.as_object()) {
        for (key, value) in env {
            if key.starts_with("FACTORY_") {
                return Err(format!(
                    "CommandSpec env key '{key}' uses the reserved FACTORY_ prefix"
                ));
            }
            if !valid_env_key(key) {
                return Err(format!("CommandSpec env key '{key}' is not a shell identifier"));
            }
            let value = value
                .as_str()
                .ok_or_else(|| format!("CommandSpec env value for '{key}' must be a string"))?;
            env_words.push(format!("{key}={}", shell_quote(value)));
        }
    }
    for (key, value) in extra_env {
        env_words.push(format!("{key}={}", shell_quote(value)));
    }
    let mut cmd = String::new();
    if !env_words.is_empty() {
        cmd.push_str("env ");
        cmd.push_str(&env_words.join(" "));
        cmd.push(' ');
    }
    if let Some(timeout) = spec.get("timeout_seconds").and_then(|v| v.as_u64()) {
        if timeout == 0 {
            return Err("CommandSpec timeout_seconds must be positive".into());
        }
        cmd = format!("timeout {timeout} {cmd}");
    }
    cmd.push_str(&words.join(" "));
    if let Some(cwd) = spec.get("cwd").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        cmd = format!("cd {} && {cmd}", shell_quote(cwd));
    }
    Ok(cmd)
}

/// Render a JSON array of CommandSpecs to one fail-fast shell command:
/// `( spec1 ) && ( spec2 )`. An empty array renders to an empty command —
/// callers treat that as "nothing to run" (e.g. deploy passthrough).
pub fn commandspecs_to_shell(specs: &Value, extra_env: &[(String, String)]) -> Result<String, String> {
    let list = specs
        .as_array()
        .ok_or("command list must be a JSON array of CommandSpec objects")?;
    let mut parts: Vec<String> = Vec::new();
    for spec in list {
        parts.push(format!("( {} )", commandspec_to_shell(spec, extra_env)?));
    }
    Ok(parts.join(" && "))
}

// ---------------------------------------------------------------------------
// ADR-0069: repository profile snapshots.
//
// A task pins a non-secret snapshot of its FactoryRepo on the first Planning
// tick (PinRepoProfile); every phase afterwards reads the snapshot from the
// task row, so mid-flight profile edits never retarget in-flight tasks.
// Legacy tasks (no snapshot) fall back to the FactoryConfig row named by
// factory_id. Snapshot keys deliberately mirror the legacy FactoryConfig
// field names so module code reads one shape.
// ---------------------------------------------------------------------------

/// Parse the task's pinned snapshot. Ok(None) means "not pinned" (legacy
/// task or first tick); Err means the pinned payload is corrupt (fail loud).
pub fn pinned_profile(task_fields: &Value) -> Result<Option<Value>, String> {
    let raw = task_fields
        .get("repo_profile_snapshot")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if raw.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(raw)
        .map(Some)
        .map_err(|e| format!("task repo_profile_snapshot is not valid JSON: {e}"))
}

/// Load the effective profile for a task: the pinned snapshot when present,
/// otherwise the legacy FactoryConfig row. Returns a flat object of profile
/// keys (legacy-compatible names).
pub fn load_profile(ctx: &Context, task_fields: &Value, fields: &Value) -> Result<Value, String> {
    if let Some(snap) = pinned_profile(task_fields)? {
        return Ok(snap);
    }
    let factory_id = task_fields
        .get("factory_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if factory_id.is_empty() {
        return Err(
            "task has neither repo_profile_snapshot nor factory_id — select a FactoryRepo (ADR-0069)"
                .into(),
        );
    }
    let row = get_entity(ctx, "FactoryConfigs", factory_id, fields)?;
    Ok(row.get("fields").cloned().unwrap_or(json!({})))
}

/// Build the pinned snapshot from a FactoryRepo row's fields plus the global
/// policy fields (FactoryConfig retains budgets/Pi defaults during the
/// compatibility window). Credential references are never copied — the
/// snapshot is non-secret by construction (ADR-0069 D7).
pub fn build_profile_snapshot(repo: &Value, policy: &Value, revision: &str, digest: &str) -> Value {
    let r = |key: &str| repo.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let p = |key: &str, default: &str| {
        let v = policy.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if v.is_empty() { default.to_string() } else { v }
    };
    let or = |value: String, default: &str| if value.is_empty() { default.to_string() } else { value };
    json!({
        "repo_id": r("repo_id"),
        "display_name": r("display_name"),
        "git_url": r("git_url"),
        // Legacy-compatible keys (read by every phase module):
        "repo_url": r("git_url"),
        "base_branch": or(r("base_branch"), "main"),
        "checkout_mode": or(r("checkout_mode"), "api"),
        "publish_mode": or(r("publish_mode"), "local"),
        "merge_mode": or(r("merge_mode"), "auto"),
        "validation_commands": or(r("validation_commands"), "[]"),
        "build_commands": or(r("build_commands"), "[]"),
        "deploy_commands": or(r("deploy_commands"), "[]"),
        "observation_commands": or(r("observation_commands"), "[]"),
        "preparation_commands": or(r("preparation_commands"), "[]"),
        "test_commands": "",
        "lint_commands": "",
        "computer_image": r("computer_image"),
        "setup_script": r("setup_script"),
        "computer_cpu_cores": or(r("cpu_cores"), "4"),
        "computer_memory_gb": or(r("memory_gb"), "8"),
        "computer_storage_gb": or(r("storage_gb"), "20"),
        "computer_provider": "tensorlake",
        // Global policy (FactoryConfig compatibility window):
        "pi_provider": p("pi_provider", "anthropic"),
        "pi_model": p("pi_model", "claude-sonnet-4-6"),
        "model_env_var": p("model_env_var", "ANTHROPIC_API_KEY"),
        "max_repair_rounds": p("max_repair_rounds", "6"),
        "max_files_per_task": p("max_files_per_task", "8"),
        "max_lines_per_task": p("max_lines_per_task", "400"),
        // Provenance:
        "profile_revision": revision,
        "profile_digest": digest,
    })
}

// ---------------------------------------------------------------------------
// ADR-0069: profile command fields + reserved factory context env.
// ---------------------------------------------------------------------------

/// Render a profile command field (`validation_commands`, `deploy_commands`,
/// `observation_commands`) to a fail-fast shell string. Ok(None) when the
/// field is absent, empty, an empty array, or a legacy raw-shell string
/// (first non-space byte is not '[') — callers then apply their legacy
/// fallback. Malformed CommandSpec JSON is an error (fail loud).
pub fn profile_commands(
    profile: &Value,
    key: &str,
    extra_env: &[(String, String)],
) -> Result<Option<String>, String> {
    let raw = profile
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if !raw.starts_with('[') {
        return Ok(None);
    }
    let specs: Value = serde_json::from_str(raw)
        .map_err(|e| format!("profile field '{key}' is not valid CommandSpec JSON: {e}"))?;
    if specs.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(None);
    }
    commandspecs_to_shell(&specs, extra_env).map(Some)
}

/// Factory context injected into every profile command (validation, deploy,
/// observation) under the reserved FACTORY_* prefix (ADR-0069: context is
/// non-secret; team-declared env may not use this prefix).
pub fn factory_context_env(
    task_id: &str,
    base_sha: &str,
    head_sha: &str,
    deployment_ref: &str,
) -> Vec<(String, String)> {
    [
        ("FACTORY_TASK_ID", task_id),
        ("FACTORY_BASE_SHA", base_sha),
        ("FACTORY_HEAD_SHA", head_sha),
        ("FACTORY_DEPLOYMENT_REF", deployment_ref),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

// ---------------------------------------------------------------------------
// ADR-0069: profile digest canonical payload.
//
// profile_digest = sha256 over THIS exact string, computed by writers
// (bootstrap) over the post-write row and by the planner's pin fallback over
// the row it read. One rule, two implementations (Rust here, Python in
// scripts/bootstrap_factory_repo.py): sort-keyed compact JSON of the profile
// param fields + profile_revision. Credential-reference fields are included
// (they are opaque names, not secrets); computed/audit fields (profile_digest
// itself, created_by, updated_by) are excluded.
// ---------------------------------------------------------------------------

pub const REPO_PROFILE_PARAM_KEYS: [&str; 22] = [
    "display_name",
    "description",
    "team_id",
    "git_provider",
    "git_url",
    "base_branch",
    "checkout_mode",
    "publish_mode",
    "publish_credential_ref",
    "merge_mode",
    "validation_commands",
    "build_commands",
    "deploy_commands",
    "observation_commands",
    "preparation_commands",
    "computer_image",
    "setup_script",
    "cpu_cores",
    "memory_gb",
    "storage_gb",
    "source_credential_ref",
    "command_secret_bindings",
];

/// profile_revision as a string, tolerating counter fields projecting as
/// JSON numbers in OData rows.
pub fn repo_profile_revision(repo_fields: &Value) -> String {
    match repo_fields.get("profile_revision") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "0".to_string(),
    }
}

/// The canonical string hashed into profile_digest. BTreeMap iteration is
/// sort-keyed, matching Python's json.dumps(sort_keys=True, separators).
pub fn repo_profile_digest_payload(repo_fields: &Value) -> String {
    // Counter fields (profile_revision) project as JSON numbers; everything
    // else is stringly-typed. Normalize both to strings.
    let get = |key: &str| match repo_fields.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let mut map = std::collections::BTreeMap::new();
    for key in REPO_PROFILE_PARAM_KEYS {
        map.insert(key.to_string(), Value::String(get(key)));
    }
    let revision = repo_profile_revision(repo_fields);
    map.insert("profile_revision".to_string(), Value::String(revision));
    Value::Object(map.into_iter().collect()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // F20: exec outcome = status Succeeded AND exit_code "0" — status
    // alone is not enough (computer_exec reports RunSucceeded for exit 1).
    #[test]
    fn exec_succeeded_requires_zero_exit_code() {
        let ok = json!({"status": "Succeeded", "fields": {"exit_code": "0"}});
        let nonzero = json!({"status": "Succeeded", "fields": {"exit_code": "1"}});
        let missing = json!({"status": "Succeeded", "fields": {}});
        let running = json!({"status": "Running", "fields": {}});
        let failed = json!({"status": "Failed", "fields": {}});
        assert!(exec_succeeded(&ok));
        assert!(!exec_succeeded(&nonzero), "exit 1 must not count as success");
        assert!(!exec_succeeded(&missing), "missing exit_code fails closed");
        assert!(!exec_succeeded(&running));
        assert!(!exec_succeeded(&failed));
    }

    #[test]
    fn exec_failure_summary_carries_exit_and_tails() {
        let exec = json!({"status": "Succeeded", "fields": {
            "exit_code": "1", "stderr_tail": "boom", "stdout_tail": "running 3 tests"
        }});
        let summary = exec_failure_summary(&exec);
        assert!(summary.contains("exit 1"), "got: {summary}");
        assert!(summary.contains("boom"), "got: {summary}");
        assert!(summary.contains("running 3 tests"), "got: {summary}");
    }

    #[test]
    fn github_repo_slug_parses_common_forms() {
        assert_eq!(github_repo_slug("https://github.com/o/r").unwrap(), "o/r");
        assert_eq!(github_repo_slug("https://github.com/o/r.git").unwrap(), "o/r");
        assert_eq!(github_repo_slug("https://github.com/o/r/").unwrap(), "o/r");
        assert!(github_repo_slug("https://github.com/o").is_err());
    }

    #[test]
    fn base64_decode_roundtrips() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode("aGVs").unwrap(), b"hel");
        // GitHub wraps payloads in newlines
        assert_eq!(base64_decode("aGVs\nbG8=\n").unwrap(), b"hello");
        assert!(base64_decode("***").is_err());
    }

    #[test]
    fn minted_keys_differ_across_phases_and_rounds() {
        let a = mint_operation_key("t1", "publish", 0, 3);
        let b = mint_operation_key("t1", "merge", 0, 3);
        let c = mint_operation_key("t1", "publish", 1, 3);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("t1:publish:"));
    }

    #[test]
    fn op_key_format_is_stable() {
        assert_eq!(
            op_key("task-1", "op-2"),
            "# factory-op: task-1:op-2"
        );
    }

    #[test]
    fn op_key_matches_only_exact_pair() {
        let text = "run tests\n# factory-op: task-1:op-2";
        assert!(op_key_matches(text, "task-1", "op-2"));
        assert!(!op_key_matches(text, "task-1", "op-3"));
        assert!(!op_key_matches("no marker here", "task-1", "op-2"));
    }

    #[test]
    fn dispatch_url_builds_bound_action_path() {
        assert_eq!(
            dispatch_url(
                "http://localhost:3100/",
                "Computers",
                "en-1",
                "Temper.PawCompute",
                "Destroy",
                "default"
            ),
            "http://localhost:3100/tdata/Computers('en-1')/Temper.PawCompute.Destroy?tenant=default"
        );
    }

    #[test]
    fn entity_url_builds_row_path() {
        assert_eq!(
            entity_url("http://h:1", "FactoryTasks", "en-9", "t2"),
            "http://h:1/tdata/FactoryTasks('en-9')?tenant=t2"
        );
    }

    #[test]
    fn list_url_encodes_filter() {
        assert_eq!(
            list_url("http://h", "Execs", "default", Some("computer_id eq 'en-1'")),
            "http://h/tdata/Execs?tenant=default&$filter=computer_id%20eq%20'en-1'"
        );
        assert_eq!(
            list_url("http://h", "Execs", "default", None),
            "http://h/tdata/Execs?tenant=default"
        );
    }

    #[test]
    fn escape_odata_doubles_single_quotes() {
        assert_eq!(escape_odata("it's"), "it''s");
    }

    // -- ADR-0069: CommandSpec → governed shell --------------------------------

    #[test]
    fn shell_quote_leaves_safe_args_bare() {
        assert_eq!(shell_quote("cargo"), "cargo");
        assert_eq!(shell_quote("/work/repo"), "/work/repo");
        assert_eq!(shell_quote("--manifest-path=crates/den/Cargo.toml"), "--manifest-path=crates/den/Cargo.toml");
    }

    #[test]
    fn shell_quote_quotes_unsafe_args() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a;b"), "'a;b'");
    }

    #[test]
    fn commandspec_renders_argv_cwd_env_timeout() {
        let spec = json!({
            "argv": ["cargo", "test", "--workspace"],
            "cwd": "/work/repo",
            "env": {"CARGO_TERM_COLOR": "never", "RUSTFLAGS": "-D warnings"},
            "timeout_seconds": 120
        });
        let shell = commandspec_to_shell(&spec, &[]).unwrap();
        assert!(shell.starts_with("cd /work/repo && "), "got: {shell}");
        assert!(shell.contains("timeout 120 "), "got: {shell}");
        assert!(shell.contains("env "), "got: {shell}");
        assert!(shell.contains("CARGO_TERM_COLOR=never"), "got: {shell}");
        assert!(shell.contains("RUSTFLAGS='-D warnings'"), "got: {shell}");
        assert!(shell.ends_with("cargo test --workspace"), "got: {shell}");
    }

    #[test]
    fn commandspec_applies_factory_context_env() {
        let spec = json!({"argv": ["make", "deploy"]});
        let shell = commandspec_to_shell(
            &spec,
            &[("FACTORY_TASK_ID".to_string(), "en-1".to_string()), ("FACTORY_HEAD_SHA".to_string(), "abc 123".to_string())],
        )
        .unwrap();
        assert!(shell.contains("FACTORY_TASK_ID=en-1"), "got: {shell}");
        assert!(shell.contains("FACTORY_HEAD_SHA='abc 123'"), "got: {shell}");
    }

    #[test]
    fn commandspec_rejects_reserved_factory_prefix_in_team_env() {
        let spec = json!({"argv": ["make"], "env": {"FACTORY_HEAD_SHA": "spoof"}});
        let err = commandspec_to_shell(&spec, &[]).unwrap_err();
        assert!(err.contains("FACTORY_"), "got: {err}");
    }

    #[test]
    fn commandspec_rejects_malformed_specs() {
        assert!(commandspec_to_shell(&json!({}), &[]).is_err(), "argv required");
        assert!(commandspec_to_shell(&json!({"argv": []}), &[]).is_err(), "argv non-empty");
        assert!(commandspec_to_shell(&json!({"argv": ["a", 7]}), &[]).is_err(), "string argv only");
        assert!(
            commandspec_to_shell(&json!({"argv": ["a"], "env": {"BAD-KEY": "x"}}), &[]).is_err(),
            "env keys must be shell identifiers"
        );
    }

    #[test]
    fn commandspecs_join_fail_fast() {
        let specs = json!([
            {"argv": ["cargo", "fmt", "--check"], "cwd": "/work/repo"},
            {"argv": ["cargo", "clippy"], "cwd": "/work/repo", "timeout_seconds": 300}
        ]);
        let shell = commandspecs_to_shell(&specs, &[]).unwrap();
        assert!(shell.contains(" ) && ( "), "got: {shell}");
        assert!(shell.contains("cargo fmt --check"), "got: {shell}");
        assert!(shell.contains("timeout 300"), "got: {shell}");
        let empty = commandspecs_to_shell(&json!([]), &[]).unwrap();
        assert!(empty.is_empty());
    }

    // -- ADR-0069: profile snapshot --------------------------------------------

    #[test]
    fn build_snapshot_maps_repo_fields_to_legacy_profile_keys() {
        let repo = json!({
            "repo_id": "den",
            "display_name": "ddoghq/den",
            "git_url": "https://github.com/ddoghq/den",
            "base_branch": "main",
            "checkout_mode": "github",
            "publish_mode": "github",
            "merge_mode": "manual",
            "validation_commands": "[{\"argv\":[\"cargo\",\"test\"],\"cwd\":\"/work/repo\"}]",
            "deploy_commands": "[]",
            "observation_commands": "[{\"argv\":[\"cargo\",\"test\",\"--release\"],\"cwd\":\"/work/repo\"}]",
            "computer_image": "img-1",
            "setup_script": "npm i -g pi",
            "cpu_cores": "8",
            "memory_gb": "16",
            "storage_gb": "60",
            "profile_digest": "sha256:x",
        });
        let policy = json!({
            "max_repair_rounds": "4",
            "max_files_per_task": "6",
            "max_lines_per_task": "300",
        });
        let snap = build_profile_snapshot(&repo, &policy, "3", "sha256:x");
        assert_eq!(snap["repo_url"], "https://github.com/ddoghq/den");
        assert_eq!(snap["computer_cpu_cores"], "8");
        assert_eq!(snap["computer_memory_gb"], "16");
        assert_eq!(snap["computer_storage_gb"], "60");
        assert_eq!(snap["computer_image"], "img-1");
        assert_eq!(snap["setup_script"], "npm i -g pi");
        assert_eq!(snap["max_repair_rounds"], "4");
        assert_eq!(snap["profile_revision"], "3");
        assert_eq!(snap["profile_digest"], "sha256:x");
        assert_eq!(snap["merge_mode"], "manual");
        assert!(snap["validation_commands"].as_str().unwrap().contains("cargo"));
        assert!(snap["deploy_commands"].as_str().unwrap().starts_with('['));
        assert_eq!(snap["build_commands"], "[]");
        assert_eq!(snap["preparation_commands"], "[]");
        // Never carries credential references.
        assert!(snap.get("source_credential_ref").is_none());
        assert!(snap.get("command_secret_bindings").is_none());
        assert!(snap.get("publish_credential_ref").is_none());
    }

    #[test]
    fn build_snapshot_defaults_policy_when_missing() {
        let repo = json!({"git_url": "https://github.com/o/r"});
        let snap = build_profile_snapshot(&repo, &json!({}), "1", "d");
        assert_eq!(snap["base_branch"], "main");
        assert_eq!(snap["checkout_mode"], "api");
        assert_eq!(snap["publish_mode"], "local");
        assert_eq!(snap["merge_mode"], "auto");
        assert_eq!(snap["max_repair_rounds"], "6");
        assert_eq!(snap["computer_provider"], "tensorlake");
    }

    #[test]
    fn task_profile_snapshot_prefers_pinned_over_legacy() {
        let fields = json!({
            "repo_profile_snapshot": "{\"repo_url\":\"https://github.com/pinned/r\"}",
            "factory_id": "legacy-cfg"
        });
        let snap = pinned_profile(&fields).unwrap().unwrap();
        assert_eq!(snap["repo_url"], "https://github.com/pinned/r");
        let legacy_only = json!({"factory_id": "legacy-cfg"});
        assert!(pinned_profile(&legacy_only).unwrap().is_none());
        let broken = json!({"repo_profile_snapshot": "{not json"});
        assert!(pinned_profile(&broken).is_err());
    }

    #[test]
    fn profile_commands_renders_only_nonempty_commandspec_arrays() {
        let env = factory_context_env("en-1", "b", "h", "d");
        let p = json!({"deploy_commands": "[{\"argv\":[\"make\",\"deploy\"],\"cwd\":\"/work/repo\"}]"});
        let shell = profile_commands(&p, "deploy_commands", &env).unwrap().unwrap();
        assert!(shell.contains("make deploy"), "got: {shell}");
        assert!(shell.contains("FACTORY_TASK_ID=en-1"), "got: {shell}");
        for raw in ["", "[]", "  ", "cargo test --release"] {
            let p = json!({"deploy_commands": raw});
            assert!(profile_commands(&p, "deploy_commands", &[]).unwrap().is_none(), "raw={raw:?}");
        }
        let missing = json!({});
        assert!(profile_commands(&missing, "deploy_commands", &[]).unwrap().is_none());
        let broken = json!({"deploy_commands": "[{oops"});
        assert!(profile_commands(&broken, "deploy_commands", &[]).is_err());
    }

    #[test]
    fn factory_context_env_carries_reserved_keys_in_order() {
        let env = factory_context_env("en-1", "base", "head", "dep");
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["FACTORY_TASK_ID", "FACTORY_BASE_SHA", "FACTORY_HEAD_SHA", "FACTORY_DEPLOYMENT_REF"]
        );
        assert!(env.iter().all(|(k, _)| k.starts_with("FACTORY_")));
    }

    #[test]
    fn repo_profile_digest_payload_is_canonical_and_stable() {
        let fields = json!({
            "git_url": "https://github.com/o/r",
            "display_name": "den",
            "profile_revision": "3",
            "profile_digest": "sha256:ignored",
            "created_by": "ignored@example",
            "extra_field": "ignored"
        });
        let payload = repo_profile_digest_payload(&fields);
        // Sort-keyed compact JSON; computed/unknown fields excluded.
        assert!(payload.starts_with("{\"base_branch\":\"\""), "got: {payload}");
        assert!(payload.contains("\"git_url\":\"https://github.com/o/r\""), "got: {payload}");
        assert!(payload.contains("\"profile_revision\":\"3\""), "got: {payload}");
        assert!(!payload.contains("ignored"), "got: {payload}");
        // Counter fields project as JSON numbers — same canonical output.
        let numeric = json!({"profile_revision": 3});
        assert!(
            repo_profile_digest_payload(&numeric).contains("\"profile_revision\":\"3\""),
            "counter numbers must canonicalize to strings"
        );
        // No spaces (compact separators) — matches Python json.dumps(separators=(",",":")).
        assert!(!payload.contains("\": "), "got: {payload}");
        // Deterministic across calls.
        assert_eq!(payload, repo_profile_digest_payload(&fields));
    }

    #[test]
    fn repo_profile_digest_payload_includes_merge_mode() {
        // ADR-0070: merge authority is part of the profile contract, so it
        // is pinned and digest-covered like every other param key.
        assert!(REPO_PROFILE_PARAM_KEYS.contains(&"merge_mode"));
        assert_eq!(REPO_PROFILE_PARAM_KEYS.len(), 22);
        let fields = json!({
            "git_url": "https://github.com/o/r",
            "merge_mode": "manual",
            "profile_revision": "1"
        });
        let payload = repo_profile_digest_payload(&fields);
        assert!(payload.contains("\"merge_mode\":\"manual\""), "got: {payload}");
    }

    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex("hello from the dark-factory e2e"),
            "be2b17ced77607270b4026ed2f2b6488eb32f01726e8907d564e6e68240eab2b"
        );
    }
}

// ---------------------------------------------------------------------------
// SHA-256 (plan digests). Self-contained: no extra crate for one hash.
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 hex digest of `data` (lowercase, 64 chars).
pub fn sha256_hex(data: &str) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.as_bytes().to_vec();
    let bit_len = (msg.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA256_K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g; g = f; f = e;
            e = d.wrapping_add(t1);
            d = c; c = b; b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a); h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c); h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e); h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g); h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}
