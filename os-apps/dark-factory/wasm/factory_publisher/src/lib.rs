//! factory_publisher — Phase 3 PR publisher for dark-factory (ADR-0066).
//!
//! Pure-WASM publisher, no Exec steps: everything happens module-side in a
//! single tick.
//!
//! - **github mode** (FactoryConfig.publish_mode = "github"): reads the
//!   changed-file list written by the implementer's extract step
//!   (`/work/changed-files.txt`, `git diff --name-status`), reads each changed
//!   file's FULL content out of the sandbox, and replays the change onto
//!   GitHub through the Git Data API: blob(s) -> tree (base_tree = base_sha)
//!   -> commit (parent = base_sha) -> ref (branch_name) -> pull request.
//!   The sandbox git remote is never touched and `gh`/GH_TOKEN never reach
//!   the Computer (D4). Retries are idempotent: an existing branch ref is
//!   force-updated (same task, same branch_name), an existing open PR for
//!   the branch is reused.
//! - **local mode**: no GitHub writes; reports a synthetic
//!   `local://dark-factory/<branch>` URL so the flow (and the human merge
//!   gate) still works against the sandbox-only change.
//!
//! Reports `PublishPullRequest` with the CAS pair. Text files only in v1 —
//! a non-UTF-8 changed file fails loudly with its path (same rule as the
//! checkout).

use serde_json::{json, Value};
use temper_wasm_sdk::prelude::*;
use wasm_helpers::entity_field_str;
use wasm_helpers::sandbox::sandbox_file_read;

/// ~15 minutes of PublishingPR at the default 30s tick (the publish itself
/// is one tick; the budget guards against retries).
const MAX_PUBLISH_TICKS: u64 = 30;
const REPO_WORKDIR: &str = "/work/repo";
const PATCH_PATH: &str = "/work/changes.patch";
const CHANGED_FILES_PATH: &str = "/work/changed-files.txt";

#[derive(Debug, PartialEq)]
enum TickDecision {
    /// Replay the change onto GitHub and open a PR.
    PublishGithub,
    /// Skip GitHub writes; report a synthetic local URL.
    PublishLocal,
    Fail(String),
}

fn decide_tick(publish_mode: &str, phase_ticks: u64) -> TickDecision {
    if phase_ticks >= MAX_PUBLISH_TICKS {
        return TickDecision::Fail(format!(
            "publishing exceeded {MAX_PUBLISH_TICKS} ticks"
        ));
    }
    match publish_mode {
        "github" => TickDecision::PublishGithub,
        _ => TickDecision::PublishLocal,
    }
}

/// One parsed `git diff --name-status` row.
#[derive(Debug, PartialEq)]
enum Change {
    AddedOrModified(String),
    Deleted(String),
}

/// Parse `git diff --name-status` output. Renames become delete+add.
fn parse_changed_files(name_status: &str) -> Result<Vec<Change>, String> {
    let mut out = Vec::new();
    for line in name_status.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        match parts.as_slice() {
            ["A", path] | ["M", path] | ["T", path] => {
                out.push(Change::AddedOrModified(path.to_string()))
            }
            ["D", path] => out.push(Change::Deleted(path.to_string())),
            [r, old, new] if r.starts_with('R') || r.starts_with('C') => {
                out.push(Change::Deleted(old.to_string()));
                out.push(Change::AddedOrModified(new.to_string()));
            }
            other => return Err(format!("unparseable name-status row: {other:?}")),
        }
    }
    Ok(out)
}

/// Synthetic URL for local mode (must be non-empty per the action contract).
fn local_pr_url(branch_name: &str) -> String {
    format!("local://dark-factory/{branch_name}")
}

/// PR body with provenance (patch is truncated to keep bodies readable).
fn pr_body(task_prompt: &str, base_sha: &str, head_sha: &str, patch: &str) -> String {
    format!(
        "Opened by dark-factory (ADR-0066).\n\n\
         Task: {task_prompt}\n\n\
         Base: {base_sha}\nSandbox head: {head_sha}\n\n\
         <details><summary>changes.patch (truncated)</summary>\n\n\
         ```diff\n{}\n```\n</details>",
        factory_common::truncate(patch, 6000)
    )
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
                ctx.log("error", &format!("factory_publisher: {e}"));
                set_error_result(&format!("factory_publisher: {e}"));
            }
        }
        Err(e) => {
            set_error_result(&format!("factory_publisher: context init failed: {e}"));
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
    let computer_id = required(fields, "computer_id")?;
    let operation_key = required(fields, "operation_key")?;
    let operation_owner = required(fields, "operation_owner")?;
    let factory_id = required(fields, "factory_id")?;
    let task_prompt = required(fields, "task_prompt")?;
    let branch_name = required(fields, "branch_name")?;
    let base_sha = required(fields, "base_sha")?;
    let head_sha = required(fields, "head_sha")?;
    let pull_request_url = field_or(fields, "pull_request_url", "");
    let phase_ticks = counter(counters, "phase_ticks");
    let repair_round = counter(counters, "repair_round");
    let status = ctx
        .entity_state
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("PublishingPR")
        .to_string();

    let config = factory_common::get_entity(ctx, "FactoryConfigs", &factory_id, fields)?;
    let cfg = config.get("fields").cloned().unwrap_or(json!({}));
    let publish_mode = field_or(&cfg, "publish_mode", "local");

    ctx.log(
        "info",
        &format!("factory_publisher: tick task={task_id} mode={publish_mode} branch={branch_name} ticks={phase_ticks}"),
    );

    if status == "Merging" {
        return run_merge_tick(
            ctx,
            &task_id,
            &pull_request_url,
            &head_sha,
            &publish_mode,
            phase_ticks,
            repair_round,
            &operation_key,
            &operation_owner,
        );
    }

    match decide_tick(&publish_mode, phase_ticks) {
        TickDecision::PublishLocal => {
            report_publish(
                &local_pr_url(&branch_name),
                &head_sha,
                "",
                &operation_key,
                &operation_owner,
            );
        }
        TickDecision::PublishGithub => {
            let repo_url = field_or(&cfg, "repo_url", "");
            if repo_url.is_empty() {
                return Err("FactoryConfig is missing repo_url".into());
            }
            let base_branch = field_or(&cfg, "base_branch", "main");
            let slug = factory_common::github_repo_slug(&repo_url)?;
            let computer = factory_common::get_entity(ctx, "Computers", &computer_id, fields)?;
            let handle = factory_common::computer_sandbox_handle(
                computer.get("fields").unwrap_or(&json!({})),
            )?;

            // Change set from the extract artifacts.
            let name_status = sandbox_file_read(ctx, &handle, CHANGED_FILES_PATH)?;
            let changes = parse_changed_files(&name_status)?;
            if changes.is_empty() {
                return Err("changed-files artifact is empty; nothing to publish".into());
            }
            let patch = sandbox_file_read(ctx, &handle, PATCH_PATH).unwrap_or_default();

            // Blobs for added/modified files.
            let mut tree_entries: Vec<Value> = Vec::new();
            for change in &changes {
                match change {
                    Change::AddedOrModified(path) => {
                        let content = sandbox_file_read(
                            ctx,
                            &handle,
                            &format!("{REPO_WORKDIR}/{path}"),
                        )?;
                        let blob = factory_common::github_api(
                            ctx,
                            "POST",
                            &format!("/repos/{slug}/git/blobs"),
                            Some(&json!({"content": content, "encoding": "utf-8"})),
                        )?;
                        let blob_sha = blob
                            .get("sha")
                            .and_then(|v| v.as_str())
                            .ok_or("github blob create returned no sha")?;
                        tree_entries.push(json!({
                            "path": path,
                            "mode": "100644",
                            "type": "blob",
                            "sha": blob_sha,
                        }));
                    }
                    Change::Deleted(path) => {
                        tree_entries.push(json!({
                            "path": path,
                            "mode": "100644",
                            "type": "blob",
                            "sha": Value::Null,
                        }));
                    }
                }
            }

            // Tree on top of the frozen base.
            let tree = factory_common::github_api(
                ctx,
                "POST",
                &format!("/repos/{slug}/git/trees"),
                Some(&json!({"base_tree": base_sha, "tree": tree_entries})),
            )?;
            let tree_sha = tree
                .get("sha")
                .and_then(|v| v.as_str())
                .ok_or("github tree create returned no sha")?;

            // Commit (parent = base; the sandbox commit never leaves the box).
            let title = task_prompt.lines().next().unwrap_or("dark-factory change");
            let commit = factory_common::github_api(
                ctx,
                "POST",
                &format!("/repos/{slug}/git/commits"),
                Some(&json!({
                    "message": format!("dark-factory: {}", factory_common::truncate(title, 60)),
                    "tree": tree_sha,
                    "parents": [base_sha],
                })),
            )?;
            let commit_sha = commit
                .get("sha")
                .and_then(|v| v.as_str())
                .ok_or("github commit create returned no sha")?
                .to_string();

            // Branch ref (idempotent: force-update our own branch on retry).
            let ref_name = format!("refs/heads/{branch_name}");
            let created = factory_common::github_api(
                ctx,
                "POST",
                &format!("/repos/{slug}/git/refs"),
                Some(&json!({"ref": ref_name, "sha": commit_sha})),
            );
            if created.is_err() {
                factory_common::github_api(
                    ctx,
                    "PATCH",
                    &format!("/repos/{slug}/git/refs/heads/{branch_name}"),
                    Some(&json!({"sha": commit_sha, "force": true})),
                )?;
            }

            // Pull request (idempotent: reuse an existing open PR).
            let body = pr_body(&task_prompt, &base_sha, &head_sha, &patch);
            let pr = factory_common::github_api(
                ctx,
                "POST",
                &format!("/repos/{slug}/pulls"),
                Some(&json!({
                    "title": format!("dark-factory: {}", factory_common::truncate(title, 60)),
                    "head": branch_name,
                    "base": base_branch,
                    "body": body,
                })),
            );
            let pr_url = match pr {
                Ok(v) => v
                    .get("html_url")
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string())
                    .ok_or("github PR create returned no html_url")?,
                Err(create_err) => {
                    let existing = factory_common::github_api(
                        ctx,
                        "GET",
                        &format!("/repos/{slug}/pulls?head={}:{}&state=open", slug.split('/').next().unwrap_or(""), branch_name),
                        None,
                    )?;
                    existing
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|p| p.get("html_url"))
                        .and_then(|u| u.as_str())
                        .map(|s| s.to_string())
                        .ok_or_else(|| {
                            format!("PR create failed ({create_err}) and no open PR found for {branch_name}")
                        })?
                }
            };

            ctx.log(
                "info",
                &format!("factory_publisher: task {task_id} published {branch_name} ({commit_sha}) -> {pr_url}"),
            );
            report_publish(&pr_url, &head_sha, &commit_sha, &operation_key, &operation_owner);
        }
        TickDecision::Fail(reason) => {
            set_success_result(
                "FailTask",
                &json!({
                    "failure_reason": factory_common::truncate(&format!("publish failed: {reason}"), 500),
                    "operation_result": "failed",
                    "expected_operation_key": operation_key,
                    "expected_operation_owner": operation_owner,
                }),
            );
        }
    }
    Ok(())
}

/// Report PublishPullRequest via the result envelope (host dispatches it).
fn report_publish(
    pr_url: &str,
    head_sha: &str,
    published_sha: &str,
    operation_key: &str,
    operation_owner: &str,
) {
    set_success_result(
        "PublishPullRequest",
        &json!({
            "pull_request_url": pr_url,
            "head_sha": head_sha,
            "published_sha": published_sha,
            "operation_result": "pull request published",
            "expected_operation_key": operation_key,
            "expected_operation_owner": operation_owner,
        }),
    );
}

/// Extract the PR number from a pull_request_url (".../pull/123").
fn pr_number_from_url(url: &str) -> Result<u64, String> {
    url.rsplit("/pull/")
        .next()
        .and_then(|tail| tail.split(&['/', '?', '#'][..]).next())
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(|| format!("cannot parse PR number from '{url}'"))
}

/// Merging-phase tick: the human approved the exact head; merge the PR.
fn run_merge_tick(
    ctx: &Context,
    task_id: &str,
    pull_request_url: &str,
    head_sha: &str,
    publish_mode: &str,
    phase_ticks: u64,
    repair_round: u64,
    operation_key: &str,
    operation_owner: &str,
) -> Result<(), String> {
    if phase_ticks >= MAX_PUBLISH_TICKS {
        set_success_result(
            "FailTask",
            &json!({
                "failure_reason": format!("merge exceeded {MAX_PUBLISH_TICKS} ticks"),
                "operation_result": "failed",
                "expected_operation_key": operation_key,
                "expected_operation_owner": operation_owner,
            }),
        );
        return Ok(());
    }

    // Local mode: nothing to merge server-side; the sandbox head is the merge.
    if publish_mode != "github" || pull_request_url.starts_with("local://") {
        report_merged(task_id, head_sha, repair_round, phase_ticks, operation_key, operation_owner);
        return Ok(());
    }

    let factory_id = required(ctx.entity_state.get("fields").unwrap(), "factory_id")?;
    let config = factory_common::get_entity(
        ctx,
        "FactoryConfigs",
        &factory_id,
        ctx.entity_state.get("fields").unwrap(),
    )?;
    let cfg = config.get("fields").cloned().unwrap_or(json!({}));
    let repo_url = field_or(&cfg, "repo_url", "");
    let slug = factory_common::github_repo_slug(&repo_url)?;
    let pr_number = pr_number_from_url(pull_request_url)?;

    // Idempotency: if the PR is already merged (retry after a crash between
    // the merge call and RecordMerged), reuse its merge_commit_sha.
    let published_sha = ctx
        .entity_state
        .get("fields")
        .and_then(|f| f.get("published_sha"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let pr = factory_common::github_api(ctx, "GET", &format!("/repos/{slug}/pulls/{pr_number}"), None)?;
    if pr.get("merged").and_then(|v| v.as_bool()) == Some(true) {
        let merge_sha = pr
            .get("merge_commit_sha")
            .and_then(|v| v.as_str())
            .ok_or("merged PR carried no merge_commit_sha")?;
        ctx.log("info", &format!("factory_publisher: task {task_id} PR #{pr_number} already merged as {merge_sha}"));
        report_merged(task_id, merge_sha, repair_round, phase_ticks, operation_key, operation_owner);
        return Ok(());
    }

    // Merge the PR's live head. The sandbox head_sha (approved by the human)
    // never exists on GitHub: the publisher created its own commit there, and
    // published_sha pins it so post-review tampering is detected.
    let pr_head = pr
        .pointer("/head/sha")
        .and_then(|v| v.as_str())
        .ok_or("github pr carried no head sha")?;
    if !published_sha.is_empty() && pr_head != published_sha {
        set_success_result(
            "FailTask",
            &json!({
                "failure_reason": format!(
                    "PR head {pr_head} diverged from published commit {published_sha}; refusing to merge an unreviewed head"
                ),
                "operation_result": "failed",
                "expected_operation_key": operation_key,
                "expected_operation_owner": operation_owner,
            }),
        );
        return Ok(());
    }
    let merge_sha_param = if published_sha.is_empty() { pr_head } else { published_sha.as_str() };
    let merge = factory_common::github_api(
        ctx,
        "PUT",
        &format!("/repos/{slug}/pulls/{pr_number}/merge"),
        Some(&json!({
            "commit_title": format!("dark-factory: merge {head_sha}"),
            "sha": merge_sha_param,
            "merge_method": "squash",
        })),
    );
    match merge {
        Ok(v) if v.get("merged").and_then(|m| m.as_bool()) == Some(true) => {
            let merge_sha = v
                .get("sha")
                .and_then(|m| m.as_str())
                .ok_or("github merge response carried no sha")?;
            ctx.log("info", &format!("factory_publisher: task {task_id} merged PR #{pr_number} as {merge_sha}"));
            report_merged(task_id, merge_sha, repair_round, phase_ticks, operation_key, operation_owner);
        }
        Ok(v) => {
            ctx.log("info", &format!("factory_publisher: task {task_id} merge not complete yet: {}", factory_common::truncate(&v.to_string(), 200)));
            set_success_result("", &json!({}));
        }
        Err(e) => {
            // Not mergeable yet (checks pending) or a transient error: wait,
            // bounded by MAX_PUBLISH_TICKS.
            ctx.log("info", &format!("factory_publisher: task {task_id} merge attempt failed, waiting: {e}"));
            set_success_result("", &json!({}));
        }
    }
    Ok(())
}

/// Report RecordMerged via the result envelope (mints the observation key).
fn report_merged(
    task_id: &str,
    merge_sha: &str,
    repair_round: u64,
    phase_ticks: u64,
    operation_key: &str,
    operation_owner: &str,
) {
    set_success_result(
        "RecordMerged",
        &json!({
            "merge_sha": merge_sha,
            "operation_result": "merged",
            "expected_operation_key": operation_key,
            "expected_operation_owner": operation_owner,
            "operation_key": factory_common::mint_operation_key(task_id, "observe", repair_round, phase_ticks),
            "operation_owner": "factory_publisher",
            "phase_ticks": 0,
        }),
    );
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_mode_reports_synthetic_url_without_github() {
        assert_eq!(decide_tick("local", 0), TickDecision::PublishLocal);
        assert_eq!(decide_tick("", 0), TickDecision::PublishLocal);
        assert_eq!(
            local_pr_url("darkfactory/abc-r0"),
            "local://dark-factory/darkfactory/abc-r0"
        );
    }

    #[test]
    fn github_mode_publishes_and_budget_fails() {
        assert_eq!(decide_tick("github", 0), TickDecision::PublishGithub);
        assert!(matches!(
            decide_tick("github", MAX_PUBLISH_TICKS),
            TickDecision::Fail(_)
        ));
    }

    #[test]
    fn parse_changed_files_handles_all_kinds() {
        let parsed = parse_changed_files("M\tsrc/a.rs\nA\tnew.txt\nD\told.txt\n").unwrap();
        assert_eq!(
            parsed,
            vec![
                Change::AddedOrModified("src/a.rs".into()),
                Change::AddedOrModified("new.txt".into()),
                Change::Deleted("old.txt".into()),
            ]
        );
        let renamed = parse_changed_files("R100\told.md\tnew.md\n").unwrap();
        assert_eq!(
            renamed,
            vec![
                Change::Deleted("old.md".into()),
                Change::AddedOrModified("new.md".into()),
            ]
        );
        assert!(parse_changed_files("X\t???").is_err());
        assert!(parse_changed_files("").unwrap().is_empty());
    }

    #[test]
    fn pr_number_parses_from_url() {
        assert_eq!(
            pr_number_from_url("https://github.com/o/r/pull/123").unwrap(),
            123
        );
        assert_eq!(
            pr_number_from_url("https://github.com/o/r/pull/7/files").unwrap(),
            7
        );
        assert!(pr_number_from_url("local://dark-factory/x").is_err());
        assert!(pr_number_from_url("https://github.com/o/r").is_err());
    }

    #[test]
    fn pr_body_carries_provenance_and_truncated_patch() {
        let body = pr_body("do the thing", "base123", "head456", "diff --git ...");
        assert!(body.contains("do the thing"), "{body}");
        assert!(body.contains("base123"), "{body}");
        assert!(body.contains("head456"), "{body}");
        assert!(body.contains("diff --git"), "{body}");
        let huge = "x".repeat(10_000);
        assert!(pr_body("t", "b", "h", &huge).len() < 7000);
    }
}
