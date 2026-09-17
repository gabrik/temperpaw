// Pure view-model mapping for the dark-factory console (ADR-0067 D2/D4).
// No browser or React dependencies — unit-tested with node:test.
//
// The console talks to Temper OData directly; entity rows come back with
// `entity_id`/`status` at top level plus `fields` (which also carries
// Id/Status), `counters`, and `events`. These helpers flatten that shape
// into what the UI renders and build the human-gate dispatch params
// (CAS digest/sha + operation-key fencing) exactly as the spec requires.

export const STAGES = [
  ["Requested", "Request"],
  ["Planning", "Planning"],
  ["AwaitingPlanApproval", "Plan approval"],
  ["Implementing", "Implementing"],
  ["Validating", "Validating"],
  ["PublishingPR", "Preparing review"],
  ["AwaitingMergeApproval", "Code approval"],
  ["Merging", "Deploy"],
  ["Observing", "Observing"],
  ["FinalizingMerge", "Finalize merge"],
  // GB feedback: the rail concludes with Finalize merge (10 steps).
  // Completed/Failed are terminal states rendered via the rail's done
  // rule and the status pill, not rail steps.
];

export function shortId(value = "") {
  return String(value).replace(/^en-/, "").slice(0, 8);
}

export function stageIndex(status) {
  return STAGES.findIndex(([state]) => state === status);
}

const TASK_FIELDS = [
  "task_prompt",
  "factory_id",
  "computer_id",
  "created_by",
  "base_sha",
  "branch_name",
  "plan_text",
  "plan_digest",
  "head_sha",
  "published_sha",
  "pull_request_url",
  "merge_sha",
  "merge_commit_sha",
  "validation_summary",
  "observation_summary",
  "repair_context",
  "failure_reason",
];

export function toTaskView(row = {}) {
  const fields = row.fields ?? {};
  const view = {
    Id: fields.Id ?? row.entity_id ?? "",
    Status: fields.Status ?? row.status ?? "",
    repair_round: Number(row.counters?.repair_round ?? 0),
    phase_ticks: Number(row.counters?.phase_ticks ?? 0),
  };
  for (const name of TASK_FIELDS) view[name] = fields[name] ?? "";
  return view;
}

export function toTaskList(payload = {}) {
  return [...(payload.value ?? [])]
    .sort((a, b) => String(a.entity_id ?? "").localeCompare(String(b.entity_id ?? "")))
    .reverse()
    .map(toTaskView);
}

export function toActivityEvents(payload = {}) {
  return [...(payload.value ?? [])]
    .sort((a, b) => Number(a.sequence_nr ?? 0) - Number(b.sequence_nr ?? 0))
    .map((row) => {
      const fields = row.fields ?? {};
      const status = fields.Status ?? row.status ?? "";
      const exitCode = fields.exit_code ?? "";
      const label = fields.task_description || (fields.command ?? "").split("\n")[0];
      const suffix = exitCode !== "" ? ` (exit ${exitCode})` : "";
      return {
        id: fields.Id ?? row.entity_id ?? "",
        seq: Number(row.sequence_nr ?? 0),
        level: status === "Failed" || (exitCode !== "" && exitCode !== "0") ? "warn" : "info",
        stage: status,
        message: `${formatActivityLabel(label)}${suffix}`,
        stdoutTail: renderExecStream(fields.stdout_tail ?? ""),
        stderrTail: fields.stderr_tail ?? "",
      };
    });
}

export function latestPatch(payload = {}) {
  const patches = (payload.value ?? [])
    .map((row) => row.fields ?? {})
    .filter((fields) => fields.kind === "patch" && typeof fields.content === "string");
  if (!patches.length) return null;
  patches.sort((a, b) => roundOf(a.name) - roundOf(b.name));
  const latest = patches[patches.length - 1];
  return { name: latest.name ?? "", content: latest.content, created_by: latest.created_by ?? "" };
}

// --- Provisioning-phase activity (ADR-0067 follow-up) ---
// The Activity panel was empty until the first governed Exec (~2 min of
// sandbox provisioning invisible). OData entity rows carry no event
// history, so the sandbox lifecycle is synthesized from current task +
// Computer state as done/active/pending steps.

// Matches the factory_planner's deterministic computer name:
// "df-plan-" + first 16 alphanumeric chars of the task entity id.
export function plannerComputerName(taskId = "") {
  const short = [...String(taskId)].filter((c) => /[a-zA-Z0-9]/.test(c)).slice(0, 16).join("");
  return `df-plan-${short}`;
}

const READY_STATES = ["Ready", "Checkpointing", "Sleeping", "Destroying", "Destroyed"];
const CONFIGURED_STATES = ["Provisioning", ...READY_STATES];

export function toProvisioningSteps(taskRow, computerRow) {
  const taskFields = taskRow?.fields ?? {};
  const taskId = taskRow?.entity_id ?? taskFields.Id ?? "";
  const name = plannerComputerName(taskId);
  const cFields = computerRow?.fields ?? {};
  const cStatus = computerRow?.status ?? cFields.Status ?? "";
  const cId = computerRow?.entity_id ?? cFields.Id ?? "";
  const configured = CONFIGURED_STATES.includes(cStatus);
  const ready = READY_STATES.includes(cStatus);
  const failed = Boolean(cFields.error_message) && !ready;
  const attached = Boolean(taskFields.computer_id) && taskFields.computer_id === cId;

  const steps = [
    {
      key: "register",
      text: computerRow ? `sandbox ${cFields.name || name} registered` : `registering sandbox ${name}`,
      state: computerRow ? "done" : "active",
    },
    {
      key: "prepare",
      text: `preparing image ${cFields.base_image || "…"} (${cFields.cpu_cores || "?"} cpu · ${cFields.memory_gb || "?"} GB)`,
      state: !computerRow ? "pending" : configured ? "done" : "active",
    },
    {
      key: "provision",
      text: failed
        ? `provisioning failed: ${cFields.error_message}`
        : `provisioning on ${cFields.provider || "tensorlake"}${cFields.setup_script ? " · running setup script" : ""}`,
      state: !computerRow ? "pending" : failed ? "failed" : ready ? "done" : configured ? "active" : "pending",
    },
    {
      key: "ready",
      text: cFields.machine_id ? `sandbox ready · machine ${cFields.machine_id}` : "sandbox ready",
      state: ready ? "done" : "pending",
    },
    {
      key: "attach",
      text: attached ? "sandbox attached to task" : "attaching sandbox to task",
      state: attached ? "done" : ready ? "active" : "pending",
    },
  ];
  if (cStatus === "Destroying" || cStatus === "Destroyed") {
    steps.push({
      key: "destroy",
      text: "sandbox destroyed",
      state: cStatus === "Destroyed" ? "done" : "active",
    });
  }
  return steps;
}

function roundOf(name = "") {
  const match = /-r(\d+)\.patch$/.exec(name);
  return match ? Number(match[1]) : -1;
}

// Build a human-gate dispatch. `gate` is "plan" | "code"; `decision` is
// "approve" | "reject". CAS params mirror the spec: ApprovePlan/RejectPlan
// fence on plan_digest, ApproveMerge/RequestChanges on head_sha. Rejects
// require feedback, which travels as repair_context into the next round.
export function gateDecision(task, gate, decision, comment, owner, uuid = () => crypto.randomUUID()) {
  const trimmed = (comment ?? "").trim();
  if (decision === "reject" && !trimmed) {
    throw new Error("Please include feedback so the agent knows what to change.");
  }
  const base = {
    operation_key: uuid(),
    operation_owner: owner,
    phase_ticks: 0,
  };
  if (gate === "plan") {
    if (!task.plan_digest) throw new Error("Task has no plan_digest yet — cannot decide the plan gate.");
    return decision === "approve"
      ? { action: "ApprovePlan", params: { ...base, plan_digest: task.plan_digest } }
      : { action: "RejectPlan", params: { ...base, plan_digest: task.plan_digest, repair_context: trimmed } };
  }
  if (!task.head_sha) throw new Error("Task has no head_sha yet — cannot decide the code gate.");
  return decision === "approve"
    ? { action: "ApproveMerge", params: { ...base, head_sha: task.head_sha } }
    : { action: "RequestChanges", params: { ...base, head_sha: task.head_sha, repair_context: trimmed } };
}

// -- SSE event → refresh decisions (ADR-0068) ---------------------------------
//
// The console subscribes to the tenant-scoped Temper event stream
// (`GET /tdata/$events`, event `state_change`). Every dispatch on any entity
// arrives; refreshing the current OData view on all of them would be noisy
// and expensive (the platform emits Directory/Session chatter constantly).
// This predicate keeps exactly the dark-factory surfaces:
//
// - FactoryTask  — list rows, detail header, stage rail, gates
// - Exec         — activity feed; Run / ReportOutput (in-flight tail,
//                  ADR-0005) / RunSucceeded / RunFailed. NOT CheckOutput:
//                  the bare re-check tick carries no new data and would
//                  double the refetch churn every 5 s.
// - Computer     — provisioning steps
// - FactoryArtifact — latest patch / PR card
export function shouldRefreshForEvent(change) {
  if (!change || typeof change !== "object") return false;
  const type = change.entity_type;
  if (type === "FactoryTask" || type === "Computer" || type === "FactoryArtifact") return true;
  if (type === "Exec") {
    return ["Run", "ReportOutput", "RunSucceeded", "RunFailed"].includes(change.action);
  }
  return false;
}

// -- Live pi stream rendering (--mode json, ADR-0068 follow-up) ----------------
//
// The implementer runs pi with `--mode json`, so the exec's combined log (and
// therefore the in-flight tail, ADR-0005) is newline-delimited JSON events
// instead of plain text. Raw JSONL in the activity feed is unreadable, so
// each event is rendered as one compact line:
//
//   tool_execution_start  → "⚙ bash — cargo test"
//   tool_execution_end    → "✓ read" / "✗ bash (error)"
//   message_end (assistant) → the message text
//   everything else (session/turn/message_start/token deltas/tool updates)
//   → dropped (too noisy for a tail-based feed)
//
// Non-JSON lines (cargo, git, sh output) pass through untouched, and
// malformed JSON falls back to the raw line so a truncated tail never
// breaks rendering.
export function formatPiEventLine(line) {
  const trimmed = String(line ?? "").trim();
  if (!trimmed.startsWith("{")) return line;
  let ev;
  try {
    ev = JSON.parse(trimmed);
  } catch {
    return line;
  }
  switch (ev.type) {
    case "tool_execution_start": {
      const args = ev.args ?? {};
      const hint = String(args.command ?? args.path ?? args.file_path ?? "").slice(0, 120);
      return `⚙ ${ev.toolName ?? "tool"}${hint ? ` — ${hint}` : ""}`;
    }
    case "tool_execution_end":
      return `${ev.isError ? "✗" : "✓"} ${ev.toolName ?? "tool"}${ev.isError ? " (error)" : ""}`;
    case "message_end": {
      const msg = ev.message ?? {};
      if (msg.role !== "assistant") return null;
      const text = (msg.content ?? [])
        .filter((part) => part?.type === "text")
        .map((part) => part.text ?? "")
        .join("\n")
        .trim();
      return text || null;
    }
    default:
      return null;
  }
}

export function renderExecStream(text) {
  if (!text) return text;
  return String(text)
    .split("\n")
    .map(formatPiEventLine)
    .filter((line) => line !== null && line !== "")
    .join("\n");
}

// Exec task_descriptions carry the factory op marker for idempotency
// ("run pi implementation agent # factory-op: <task-id>:<session>:<channel>").
// The marker matters to the backend, not the reader — render it compactly.
export function formatActivityLabel(label) {
  const match = /^(.*?)\s*#\s*factory-op:\s*\S+:(\S+):(\w+)\s*$/.exec(String(label ?? ""));
  if (!match) return label;
  const [, head, session, channel] = match;
  return `${head.trim()} #${channel} · ${session.slice(0, 8)}`;
}
