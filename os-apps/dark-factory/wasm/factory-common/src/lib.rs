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

#[cfg(test)]
mod tests {
    use super::*;

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
