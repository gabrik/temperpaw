import { useEffect, useMemo, useRef, useState } from "react";
import { api, ensureSession, openFactoryEventStream } from "./api.js";
import { STAGES, toTaskView, toTaskList, toActivityEvents, latestPatch, gateDecision, shortId, stageIndex, toProvisioningSteps, shouldRefreshForEvent } from "./view-model.js";

const STATUS_COPY = {
  Requested: "Your request is queued for the factory.",
  Planning: "The coding agent is inspecting the target repo and preparing a plan.",
  AwaitingPlanApproval: "The plan is ready for your decision.",
  Implementing: "The agent is editing the target repo in an isolated sandbox.",
  Validating: "The configured test and lint commands are running in the sandbox.",
  PublishingPR: "The validated change-set is being published for review.",
  AwaitingMergeApproval: "Review the exact published head before deploy and observation.",
  Merging: "The approved head is recorded as the deploy candidate; the pull request stays open.",
  Observing: "Observation commands are running against the approved head. The PR is merged only if they pass.",
  FinalizingMerge: "Observation passed — the pull request is being merged.",
  Completed: "The approved change passed observation and the pull request was merged. The task is complete.",
  Failed: "The factory stopped after an unrecoverable error.",
};

const POLL_MS = 3000;

function formatTime(value) {
  if (!value) return "";
  return new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit", second: "2-digit" }).format(
    new Date(value),
  );
}

function Sidebar({ tasks, selectedId, onSelect, onNewTask, online, userEmail }) {
  return (
    <aside className="sidebar">
      <div className="brand">
        <div className="brand-mark">df</div>
        <div>
          <strong>dark factory</strong>
          <span>Temper control plane</span>
        </div>
      </div>
      <button className="button new-task" onClick={onNewTask}>
        <span>＋</span> New request
      </button>
      <div className="sidebar-label">Tasks</div>
      <nav className="task-list">
        {tasks.map((task) => (
          <button
            key={task.Id}
            className={`task-item ${selectedId === task.Id ? "selected" : ""}`}
            onClick={() => onSelect(task.Id)}
          >
            <span className={`status-dot ${task.Status?.toLowerCase()}`} />
            <span className="task-item-copy">
              <strong>{shortId(task.Id)}</strong>
              <span>{task.Status}</span>
            </span>
          </button>
        ))}
        {!tasks.length && <p className="empty-small">No factory tasks yet.</p>}
      </nav>
      <div className="sidebar-footer" title={userEmail}>
        <span className={`connection-dot ${online ? "online" : ""}`} />
        {online ? "Temper connected" : "Connecting to Temper"}
      </div>
    </aside>
  );
}

function StageRail({ task }) {
  const activeIndex = stageIndex(task.Status);
  return (
    <section className="stage-card" aria-label="Factory progress">
      <div className="stage-header">
        <div>
          <p className="eyebrow">DURABLE WORKFLOW</p>
          <h2>{STAGES[activeIndex]?.[1] ?? task.Status}</h2>
        </div>
        <div className={`status-pill ${task.Status?.toLowerCase()}`}>
          <span className="pulse" /> {task.Status}
        </div>
      </div>
      <p className="status-description">{STATUS_COPY[task.Status] ?? "Temper is processing this task."}</p>
      <div className="stage-rail">
        {STAGES.map(([state, label], index) => {
          const active = state === task.Status;
          const done = task.Status === "Completed" || (activeIndex >= 0 && index < activeIndex);
          return (
            <div className={`stage ${active ? "active" : ""} ${done ? "done" : ""}`} key={state}>
              <div className="stage-node">{done ? "✓" : index + 1}</div>
              <span>{label}</span>
            </div>
          );
        })}
      </div>
      <div className="task-facts">
        <span>Task <strong>{shortId(task.Id)}</strong></span>
        <span>Repair round <strong>{task.repair_round ?? 0}</strong></span>
        {task.branch_name && <span>Branch <strong className="mono">{task.branch_name}</strong></span>}
        {task.head_sha && <span>Head <strong className="mono">{task.head_sha.slice(0, 10)}</strong></span>}
        {task.merge_commit_sha
          ? <span>Merged <strong className="mono">{task.merge_commit_sha.slice(0, 10)}</strong></span>
          : task.merge_sha && <span>Deployed <strong className="mono">{task.merge_sha.slice(0, 10)}</strong></span>}
      </div>
    </section>
  );
}

function DecisionCard({ kind, content, digest, onDecision, busy }) {
  const [comment, setComment] = useState("");
  const plan = kind === "plan";
  return (
    <section className="panel decision-panel">
      <div className="decision-banner">
        <span className="decision-icon">{plan ? "◇" : "∆"}</span>
        <div>
          <p className="eyebrow">HUMAN GATE {plan ? "01" : "02"}</p>
          <h3>{plan ? "Review implementation plan" : "Review final code"}</h3>
          <p>{plan
            ? "Approve the exact plan before the agent edits the target repo."
            : "Approve the exact head SHA before deploy-record and observation."}</p>
        </div>
      </div>
      <div className={plan ? "document" : "diff-view"}>
        {plan ? <pre>{content || "Waiting for plan content…"}</pre> : <Diff content={content} />}
      </div>
      {digest && <div className="digest"><span>{plan ? "Plan digest" : "Head SHA"}</span><code>{digest}</code></div>}
      <textarea
        className="review-comment"
        rows={3}
        value={comment}
        onChange={(event) => setComment(event.target.value)}
        placeholder={plan ? "Optional guidance, or explain why the plan should change…" : "Optional note, or describe the changes you want…"}
      />
      <div className="decision-actions">
        <button className="button secondary" disabled={busy} onClick={() => onDecision("reject", comment)}>
          {plan ? "Reject plan" : "Request changes"}
        </button>
        <button className="button primary" disabled={busy} onClick={() => onDecision("approve", comment)}>
          {busy ? "Submitting…" : plan ? "Approve & implement" : "Approve, deploy & observe"}
        </button>
      </div>
    </section>
  );
}

function Diff({ content = "" }) {
  if (!content.trim()) return <div className="empty-diff">No patch recorded yet — the publisher attaches one per publish round.</div>;
  return (
    <pre>
      {content.split("\n").map((line, index) => {
        const className = line.startsWith("+") && !line.startsWith("+++")
          ? "addition"
          : line.startsWith("-") && !line.startsWith("---")
            ? "deletion"
            : line.startsWith("@@")
              ? "hunk"
              : line.startsWith("diff ") || line.startsWith("index ")
                ? "diff-header"
                : "";
        return <span className={className} key={`${index}-${line.slice(0, 8)}`}>{line}{"\n"}</span>;
      })}
    </pre>
  );
}

function PatchPanel({ patch, prUrl }) {
  return (
    <section className="panel patch-panel">
      <div className="panel-title">
        <div>
          <p className="eyebrow">CHANGE-SET</p>
          <h3>{patch ? patch.name : "Patch"}</h3>
        </div>
        {prUrl?.startsWith("http") && <a className="button ghost small" href={prUrl} target="_blank" rel="noreferrer">PR ↗</a>}
      </div>
      <div className="diff-view"><Diff content={patch?.content ?? ""} /></div>
    </section>
  );
}

function Activity({ logs = [], steps = [] }) {
  const viewport = useRef(null);
  useEffect(() => {
    if (viewport.current) viewport.current.scrollTop = viewport.current.scrollHeight;
  }, [logs.length, steps.filter((s) => s.state === "done").length]);
  return (
    <section className="panel activity-panel">
      <div className="panel-title">
        <div>
          <p className="eyebrow">GOVERNED EXECUTION</p>
          <h3>Activity</h3>
        </div>
        <span className="log-count">{logs.length} execs</span>
      </div>
      <div className="log-viewport" ref={viewport}>
        {steps.map((step) => (
          <div className={`log-line step ${step.state}`} key={`step-${step.key}`}>
            <span className="step-marker">
              {step.state === "done" ? "✓" : step.state === "active" ? "◌" : step.state === "failed" ? "✗" : "·"}
            </span>
            <span>{step.text}</span>
          </div>
        ))}
        {logs.map((log) => (
          <div className={`log-line ${log.level}`} key={log.id}>
            {log.stdoutTail || log.stderrTail ? (
              <details className="log-details">
                <summary>
                  <span className="log-stage">{log.stage}</span>
                  <span>{log.message}</span>
                </summary>
                {log.stdoutTail && <pre className="log-output">{log.stdoutTail}</pre>}
                {log.stderrTail && <pre className="log-output stderr">{log.stderrTail}</pre>}
              </details>
            ) : (
              <>
                <span className="log-stage">{log.stage}</span>
                <span>{log.message}</span>
              </>
            )}
          </div>
        ))}
        {!logs.length && !steps.length && <div className="empty-log">Sandbox execs will appear here as soon as the task begins.</div>}
      </div>
    </section>
  );
}

function NewTask({ factories, onCreate, busy, onCancel, hasTasks }) {
  const [request, setRequest] = useState("");
  const [factoryId, setFactoryId] = useState(factories[0]?.entity_id ?? "");
  async function submit(event) {
    event.preventDefault();
    if (request.trim() && factoryId) await onCreate({ factoryId, prompt: request.trim() });
  }
  return (
    <section className="new-request-shell">
      <div className="new-request-copy">
        <p className="eyebrow">NEW FACTORY RUN</p>
        <h1>What should the factory build?</h1>
        <p>Describe a focused engineering task. The factory will plan it, wait for your approval, implement it, validate it, publish it, and merge only after observation passes.</p>
      </div>
      <form className="request-card" onSubmit={submit}>
        <div className="request-avatar">You</div>
        <select className="factory-picker" value={factoryId} onChange={(event) => setFactoryId(event.target.value)}>
          {factories.map((factory) => {
            const fields = factory.fields ?? {};
            return (
              <option key={factory.entity_id} value={factory.entity_id}>
                {fields.repo_url} · {fields.publish_mode}
              </option>
            );
          })}
          {!factories.length && <option value="">No active FactoryConfigs — create one first</option>}
        </select>
        <textarea
          rows={7}
          value={request}
          onChange={(event) => setRequest(event.target.value)}
          placeholder="Example: Add a /livez endpoint that always returns 200, preserve /readyz behavior, and add focused tests."
          autoFocus
        />
        <div className="request-footer">
          <span>Two human approvals · real Temper state · governed sandbox execution</span>
          <div>
            {hasTasks && <button type="button" className="button ghost" onClick={onCancel}>Cancel</button>}
            <button className="button primary" disabled={busy || !request.trim() || !factoryId}>{busy ? "Creating…" : "Start factory"}</button>
          </div>
        </div>
      </form>
    </section>
  );
}

export default function App() {
  const [user, setUser] = useState(null);
  const [authChecked, setAuthChecked] = useState(false);
  const [factories, setFactories] = useState([]);
  const [tasks, setTasks] = useState([]);
  const [selectedId, setSelectedId] = useState("");
  const [bundle, setBundle] = useState(null);
  const [creating, setCreating] = useState(false);
  const [showNewTask, setShowNewTask] = useState(false);
  const [busy, setBusy] = useState(false);
  const [online, setOnline] = useState(false);
  const [error, setError] = useState("");
  // Stable handle to the selected-task refresh so the SSE effect (below)
  // can trigger it without re-opening the stream on every selection change.
  const refreshRef = useRef(null);

  async function loadTasks(preferredId = "") {
    const result = await api.listTasks();
    const sorted = toTaskList(result);
    setTasks(sorted);
    const next = preferredId || selectedId || sorted[0]?.Id || "";
    if (next) setSelectedId(next);
    setShowNewTask(sorted.length === 0);
  }

  useEffect(() => {
    ensureSession()
      .then(async (me) => {
        setUser(me);
        setAuthChecked(true);
        const factoryRows = await api.listFactories().catch(() => []);
        setFactories(factoryRows);
        return loadTasks();
      })
      .catch((sessionError) => {
        setAuthChecked(true);
        setError(`Cannot reach the factory: ${sessionError.message}`);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Polling fallback (ADR-0068: the SSE feed below is the primary
  // invalidation path; this keeps the view honest across reconnects).
  useEffect(() => {
    refreshRef.current = null;
    if (!user || !selectedId || showNewTask) return undefined;
    let disposed = false;
    async function refresh() {
      try {
        const row = await api.getTask(selectedId);
        if (disposed) return;
        const nextBundle = await api.getTaskBundle(row);
        if (disposed) return;
        setBundle(nextBundle);
        setOnline(true);
        const view = toTaskView(nextBundle.task);
        setTasks((current) => current.map((t) => (t.Id === view.Id ? view : t)));
      } catch (refreshError) {
        if (!disposed) {
          setOnline(false);
          setError(refreshError.message);
          // The cookie session may have expired; re-auth silently so the
          // next poll recovers without any user action.
          ensureSession().catch(() => {});
        }
      }
    }
    refresh();
    refreshRef.current = refresh;
    const timer = setInterval(refresh, POLL_MS);
    return () => {
      disposed = true;
      refreshRef.current = null;
      clearInterval(timer);
    };
  }, [user, selectedId, showNewTask]);

  // Live activity feed (ADR-0068): the tenant-scoped event stream pushes
  // every entity dispatch; shouldRefreshForEvent keeps only dark-factory
  // surfaces and a ~300 ms debounce coalesces bursts (a running exec
  // reports ReportOutput every 5 s plus CheckOutput ticks, which the
  // predicate drops). Falls back to reloading the task list when no task
  // is open; the interval poll above remains as the reconnect safety net.
  useEffect(() => {
    if (!user) return undefined;
    let debounceTimer = null;
    const source = openFactoryEventStream({
      onOpen: () => {
        // E2E-observable liveness for the stream (EventSource does not
        // reliably appear in performance resource entries).
        window.__factoryStreamOpen = true;
      },
      onChange: (change) => {
        if (!shouldRefreshForEvent(change)) return;
        clearTimeout(debounceTimer);
        debounceTimer = setTimeout(() => {
          if (refreshRef.current) {
            refreshRef.current();
          } else {
            loadTasks().catch(() => {});
          }
        }, 300);
      },
    });
    return () => {
      clearTimeout(debounceTimer);
      source.close();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [user]);

  const task = bundle ? toTaskView(bundle.task) : null;
  const logs = useMemo(() => (bundle ? toActivityEvents(bundle.execs) : []), [bundle]);
  const provisionSteps = useMemo(() => (bundle ? toProvisioningSteps(bundle.task, bundle.computer) : []), [bundle]);
  const patch = useMemo(() => (bundle ? latestPatch(bundle.artifacts) : null), [bundle]);
  const pageTitle = useMemo(() => {
    if (!task?.task_prompt) return "Factory task";
    return task.task_prompt.length > 76 ? `${task.task_prompt.slice(0, 76)}…` : task.task_prompt;
  }, [task?.task_prompt]);

  async function createTask({ factoryId, prompt }) {
    setCreating(true);
    setError("");
    try {
      const result = await api.createTask({
        factoryId,
        prompt,
        owner: user?.email ?? "console",
        operationKey: crypto.randomUUID(),
      });
      await loadTasks(result.taskId);
      setShowNewTask(false);
    } catch (createError) {
      setError(createError.message);
    } finally {
      setCreating(false);
    }
  }

  async function decide(kind, decision, comment) {
    setBusy(true);
    setError("");
    try {
      const { action, params } = gateDecision(task, kind, decision, comment, user?.email ?? "console");
      await api.decide(selectedId, action, params);
      const row = await api.getTask(selectedId);
      setBundle(await api.getTaskBundle(row));
    } catch (decisionError) {
      setError(decisionError.message);
    } finally {
      setBusy(false);
    }
  }

  if (!authChecked) {
    return <div className="boot-screen"><div className="loader" /><span>Connecting to the factory…</span></div>;
  }
  if (!user) {
    return (
      <div className="boot-screen">
        <div className="error-banner"><span>!</span>{error || "Cannot reach the factory."}</div>
      </div>
    );
  }

  return (
    <div className="app-shell">
      <Sidebar
        tasks={tasks}
        selectedId={selectedId}
        onSelect={(id) => { setSelectedId(id); setShowNewTask(false); setBundle(null); }}
        onNewTask={() => setShowNewTask(true)}
        online={online}
        userEmail={user?.email}
      />
      <main className="main-area">
        <header className="topbar">
          <div>
            <p className="eyebrow">DARK FACTORY</p>
            <h1>{showNewTask ? "Create a task" : pageTitle}</h1>
          </div>
          <div className="topbar-actions">
            {task?.pull_request_url?.startsWith("http") && (
              <a className="button ghost" href={task.pull_request_url} target="_blank" rel="noreferrer">View PR ↗</a>
            )}
            <div className="avatar" title={user?.email} aria-label={user?.email}>
              {(user?.email ?? "??").slice(0, 2).toUpperCase()}
            </div>
          </div>
        </header>
        {error && <div className="error-banner"><span>!</span>{error}<button onClick={() => setError("")}>×</button></div>}
        {showNewTask ? (
          <NewTask
            factories={factories}
            onCreate={createTask}
            busy={creating}
            onCancel={() => setShowNewTask(false)}
            hasTasks={tasks.length > 0}
          />
        ) : !task ? (
          <div className="content-loader"><div className="loader" /><span>Loading durable task state…</span></div>
        ) : (
          <div className="dashboard">
            <StageRail task={task} />
            <div className="workspace-grid">
              <div className="primary-column">
                {task.Status === "AwaitingPlanApproval" && (
                  <DecisionCard
                    kind="plan"
                    content={task.plan_text}
                    digest={task.plan_digest}
                    busy={busy}
                    onDecision={(decision, comment) => decide("plan", decision, comment)}
                  />
                )}
                {task.Status === "AwaitingMergeApproval" && (
                  <DecisionCard
                    kind="code"
                    content={patch?.content ?? ""}
                    digest={task.head_sha}
                    busy={busy}
                    onDecision={(decision, comment) => decide("code", decision, comment)}
                  />
                )}
                {!new Set(["AwaitingPlanApproval", "AwaitingMergeApproval"]).has(task.Status) && (
                  <section className="panel current-work">
                    <div className="orb"><span /></div>
                    <p className="eyebrow">CURRENT ACTIVITY</p>
                    <h3>{STAGES[stageIndex(task.Status)]?.[1] ?? task.Status}</h3>
                    <p>{STATUS_COPY[task.Status]}</p>
                    {task.validation_summary && task.Status !== "Validating" && (
                      <details><summary>Latest validation evidence</summary><pre>{task.validation_summary}</pre></details>
                    )}
                    {task.observation_summary && (
                      <details><summary>Latest observation evidence</summary><pre>{task.observation_summary}</pre></details>
                    )}
                    {task.failure_reason && <div className="failure-box">{task.failure_reason}</div>}
                  </section>
                )}
                <Activity logs={logs} steps={provisionSteps} />
              </div>
              <PatchPanel patch={patch} prUrl={task.pull_request_url} />
            </div>
          </div>
        )}
      </main>
    </div>
  );
}
