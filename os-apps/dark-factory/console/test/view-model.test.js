// Thin client-side test layer for the dark-factory console view-model.
// These cover the OData entity-row -> view mapping, the human-gate param
// building (CAS + op-key fencing), activity mapping from governed Execs,
// and latest-patch selection from FactoryArtifacts. Run: node --test.
import { describe, test } from "node:test";
import assert from "node:assert/strict";
import {
  shouldRefreshForEvent,
  STAGES,
  toTaskView,
  toTaskList,
  toActivityEvents,
  latestPatch,
  plannerComputerName,
  toProvisioningSteps,
  gateDecision,
  shortId,
  stageIndex,
} from "../src/view-model.js";

const taskRow = {
  entity_id: "en-01a0ae2f-95aa-7903-ba5a-7709225c6aa1",
  status: "AwaitingMergeApproval",
  counters: { phase_ticks: 0, repair_round: 1 },
  fields: {
    Id: "en-01a0ae2f-95aa-7903-ba5a-7709225c6aa1",
    Status: "AwaitingMergeApproval",
    task_prompt: "Create finalize-e2e.txt",
    factory_id: "en-factory-1",
    computer_id: "en-computer-1",
    plan_text: "1. do the thing",
    plan_digest: "abc123digest",
    head_sha: "6280aadbed1fd0abcd2cadcad7027124fd4b2af3",
    merge_sha: "",
    merge_commit_sha: "",
    published_sha: "2616c644",
    pull_request_url: "https://github.com/example/repo/pull/4",
    branch_name: "darkfactory/en01a0ae2f95aa79-r0",
    validation_summary: "tests passed",
    observation_summary: "",
    failure_reason: "",
  },
};

test("toTaskView flattens entity row shape", () => {
  const task = toTaskView(taskRow);
  assert.equal(task.Id, taskRow.entity_id);
  assert.equal(task.Status, "AwaitingMergeApproval");
  assert.equal(task.repair_round, 1);
  assert.equal(task.head_sha, taskRow.fields.head_sha);
  assert.equal(task.pull_request_url, taskRow.fields.pull_request_url);
  assert.equal(task.branch_name, taskRow.fields.branch_name);
});

test("toTaskView tolerates fields-only shape (Id/Status inside fields)", () => {
  const row = { fields: { Id: "en-x", Status: "Planning", task_prompt: "t" }, counters: {} };
  const task = toTaskView(row);
  assert.equal(task.Id, "en-x");
  assert.equal(task.Status, "Planning");
  assert.equal(task.repair_round, 0);
});

test("toTaskList returns views newest-first", () => {
  const payload = {
    value: [
      { entity_id: "en-01a0ae1c-aaaa", fields: { Status: "Completed" }, counters: {} },
      { entity_id: "en-01a0ae2f-bbbb", fields: { Status: "Planning" }, counters: {} },
    ],
  };
  const list = toTaskList(payload);
  assert.equal(list.length, 2);
  assert.equal(list[0].Id, "en-01a0ae2f-bbbb");
  assert.equal(list[1].Id, "en-01a0ae1c-aaaa");
});

test("toActivityEvents maps governed execs to log lines ordered by sequence", () => {
  const payload = {
    value: [
      {
        entity_id: "en-exec-2",
        sequence_nr: 5,
        fields: { Status: "Succeeded", task_description: "run validation tests", exit_code: "0" },
      },
      {
        entity_id: "en-exec-1",
        sequence_nr: 3,
        fields: { Status: "Succeeded", task_description: "checkout", exit_code: "1" },
      },
    ],
  };
  const logs = toActivityEvents(payload);
  assert.equal(logs.length, 2);
  assert.equal(logs[0].id, "en-exec-1");
  assert.equal(logs[0].level, "warn"); // non-zero exit code
  assert.equal(logs[1].id, "en-exec-2");
  assert.equal(logs[1].level, "info");
  assert.match(logs[1].message, /run validation tests/);
});

test("latestPatch picks the highest repair round artifact", () => {
  const payload = {
    value: [
      { fields: { kind: "patch", name: "changes-r1.patch", content: "diff-r1" } },
      { fields: { kind: "patch", name: "changes-r0.patch", content: "diff-r0" } },
      { fields: { kind: "note", name: "ignore-me", content: "nope" } },
    ],
  };
  const patch = latestPatch(payload);
  assert.equal(patch.name, "changes-r1.patch");
  assert.equal(patch.content, "diff-r1");
});

test("latestPatch returns null when no patch artifacts exist", () => {
  assert.equal(latestPatch({ value: [] }), null);
  assert.equal(
    latestPatch({ value: [{ fields: { kind: "note", name: "x", content: "" } }] }),
    null,
  );
});

test("gateDecision builds CAS-fenced approve/reject params", () => {
  const task = toTaskView(taskRow);
  const uuid = () => "op-key-1";
  const approve = gateDecision(task, "code", "approve", "", "e2e@darkfactory.local", uuid);
  assert.equal(approve.action, "ApproveMerge");
  assert.equal(approve.params.head_sha, task.head_sha);
  assert.equal(approve.params.operation_key, "op-key-1");
  assert.equal(approve.params.operation_owner, "e2e@darkfactory.local");
  assert.equal(approve.params.phase_ticks, 0);

  const reject = gateDecision(task, "code", "reject", "change the thing", "e2e@darkfactory.local", uuid);
  assert.equal(reject.action, "RequestChanges");
  assert.equal(reject.params.repair_context, "change the thing");
  assert.equal(reject.params.head_sha, task.head_sha);
});

test("gateDecision plan gate uses plan_digest CAS", () => {
  const task = toTaskView({ ...taskRow, status: "AwaitingPlanApproval" });
  const approve = gateDecision(task, "plan", "approve", "", "me", () => "k");
  assert.equal(approve.action, "ApprovePlan");
  assert.equal(approve.params.plan_digest, "abc123digest");
  const reject = gateDecision(task, "plan", "reject", "redo", "me", () => "k");
  assert.equal(reject.action, "RejectPlan");
  assert.equal(reject.params.repair_context, "redo");
  assert.equal(reject.params.plan_digest, "abc123digest");
});

test("gateDecision mints an operation_key via the default uuid provider", () => {
  const task = toTaskView(taskRow);
  // no explicit uuid arg — exercises the default (regression: the default
  // must be the crypto.randomUUID function reference, not a call result)
  const approve = gateDecision(task, "code", "approve", "", "me");
  assert.equal(typeof approve.params.operation_key, "string");
  assert.ok(approve.params.operation_key.length > 0);
});

test("gateDecision requires CAS value and reject feedback", () => {
  const noHead = toTaskView({ ...taskRow, fields: { ...taskRow.fields, head_sha: "" } });
  assert.throws(() => gateDecision(noHead, "code", "approve", "", "me", () => "k"), /head_sha/);
  const task = toTaskView(taskRow);
  assert.throws(() => gateDecision(task, "code", "reject", "  ", "me", () => "k"), /feedback/);
});

test("STAGES is the 10-step rail concluding with Finalize merge", () => {
  // GB feedback: keep 10 steps, conclude with Finalize merge; Completed is
  // the terminal all-done state rendered by the rail's done rule, not a
  // step of its own. The merge gate (AwaitingMergeApproval) stays a human
  // gate — dropping the Complete pseudo-step changes nothing about it.
  const names = STAGES.map(([state]) => state);
  assert.equal(STAGES.length, 10, `expected 10 steps, got ${STAGES.length}`);
  assert.deepEqual(STAGES[STAGES.length - 1], ["FinalizingMerge", "Finalize merge"]);
  assert.ok(!names.includes("Completed"), "Completed must not be a rail step");
  for (const expected of [
    "Requested",
    "Planning",
    "AwaitingPlanApproval",
    "Implementing",
    "Validating",
    "PublishingPR",
    "AwaitingMergeApproval",
    "Merging",
    "Observing",
    "FinalizingMerge",
  ]) {
    assert.ok(names.includes(expected), `missing stage ${expected}`);
  }
});

test("Completed maps off-rail so StageRail renders every step done", () => {
  assert.equal(stageIndex("Completed"), -1);
  assert.equal(stageIndex("Failed"), -1);
  assert.equal(stageIndex("FinalizingMerge"), STAGES.length - 1);
});

test("shortId strips the en- prefix noise for display", () => {
  assert.equal(shortId("en-01a0ae2f-95aa-7903"), "01a0ae2f");
  assert.equal(shortId(""), "");
});

// --- Provisioning-phase activity steps (ADR-0067 follow-up: the Activity
// panel was empty until the first Exec; synthesize the sandbox lifecycle
// from task + Computer entity state — OData rows carry no event history).

test("plannerComputerName matches the factory_planner deterministic name", () => {
  assert.equal(
    plannerComputerName("en-01a0ae79-398a-7361-af84-5db377c26fae"),
    "df-plan-en01a0ae79398a73",
  );
  assert.equal(plannerComputerName(""), "df-plan-");
});

test("toProvisioningSteps before the computer exists: register active, rest pending", () => {
  const steps = toProvisioningSteps(
    { entity_id: "en-01a0ae79-398a-7361-af84-5db377c26fae", fields: {} },
    null,
  );
  assert.deepEqual(
    steps.map((s) => [s.key, s.state]),
    [["register", "active"], ["prepare", "pending"], ["provision", "pending"], ["ready", "pending"], ["attach", "pending"]],
  );
  assert.match(steps[0].text, /df-plan-en01a0ae79398a73/);
});

test("toProvisioningSteps while provisioning: first two done, provision active", () => {
  const steps = toProvisioningSteps(
    { entity_id: "en-x", fields: {} },
    { entity_id: "en-c1", status: "Provisioning", fields: { name: "df-plan-enx", base_image: "den-dev-bookworm-dind-v4", cpu_cores: "2", memory_gb: "4", provider: "tensorlake" } },
  );
  assert.deepEqual(
    steps.map((s) => [s.key, s.state]),
    [["register", "done"], ["prepare", "done"], ["provision", "active"], ["ready", "pending"], ["attach", "pending"]],
  );
});

test("toProvisioningSteps ready + attached: every step done", () => {
  const steps = toProvisioningSteps(
    { entity_id: "en-x", fields: { computer_id: "en-c1" } },
    { entity_id: "en-c1", status: "Ready", fields: { name: "df-plan-enx", machine_id: "def8u11ryyuk4mlflkaop", provider: "tensorlake" } },
  );
  assert.ok(steps.every((s) => s.state === "done"));
  assert.match(steps.find((s) => s.key === "ready").text, /def8u11ryyuk4mlflkaop/);
});

test("toProvisioningSteps ready computer not yet attached: attach is active", () => {
  const steps = toProvisioningSteps(
    { entity_id: "en-x", fields: {} },
    { entity_id: "en-c1", status: "Ready", fields: { name: "df-plan-enx" } },
  );
  assert.equal(steps.find((s) => s.key === "attach").state, "active");
});

test("toProvisioningSteps destroyed computer appends a destroy step", () => {
  const steps = toProvisioningSteps(
    { entity_id: "en-x", fields: { computer_id: "en-c1" } },
    { entity_id: "en-c1", status: "Destroyed", fields: { name: "df-plan-enx" } },
  );
  assert.equal(steps.at(-1).key, "destroy");
  assert.equal(steps.at(-1).state, "done");
});

test("toProvisioningSteps surfaces a provisioning failure from error_message", () => {
  const steps = toProvisioningSteps(
    { entity_id: "en-x", fields: {} },
    { entity_id: "en-c1", status: "Created", fields: { name: "df-plan-enx", error_message: "tensorlake quota exceeded" } },
  );
  const provision = steps.find((s) => s.key === "provision");
  assert.equal(provision.state, "failed");
  assert.match(provision.text, /quota exceeded/);
});

// --- Exec output in the activity feed (GB feedback: DEN showed command
// output; we render the stored tails — live streaming stays the deferred
// SSE follow-up).

test("toActivityEvents carries stdout/stderr tails for expandable output", () => {
  const events = toActivityEvents({
    value: [
      {
        entity_id: "en-e1",
        sequence_nr: 1,
        fields: {
          Status: "Succeeded",
          task_description: "run validation tests",
          exit_code: "1",
          stdout_tail: "test gb-e2e: not found",
          stderr_tail: "",
        },
      },
    ],
  });
  assert.equal(events[0].stdoutTail, "test gb-e2e: not found");
  assert.equal(events[0].stderrTail, "");
  assert.equal(events[0].level, "warn", "non-zero exit stays warn-level");
});

test("toActivityEvents defaults missing tails to empty strings", () => {
  const events = toActivityEvents({
    value: [{ entity_id: "en-e2", sequence_nr: 1, fields: { Status: "Succeeded", task_description: "checkout", exit_code: "0" } }],
  });
  assert.equal(events[0].stdoutTail, "");
  assert.equal(events[0].stderrTail, "");
});

// -- SSE event → refresh decisions (ADR-0068) ---------------------------------

describe("shouldRefreshForEvent (live activity feed)", () => {
  it("refreshes on FactoryTask transitions", () => {
    assert.equal(
      shouldRefreshForEvent({ entity_type: "FactoryTask", action: "ApproveMerge", status: "Merging" }),
      true,
    );
  });

  it("refreshes on Exec lifecycle and output events but not bare re-checks", () => {
    for (const action of ["Run", "ReportOutput", "RunSucceeded", "RunFailed"]) {
      assert.equal(shouldRefreshForEvent({ entity_type: "Exec", action }), true, action);
    }
    // CheckOutput carries no new data — the row only changes on ReportOutput;
    // refreshing on the bare tick would double the refetch churn.
    assert.equal(shouldRefreshForEvent({ entity_type: "Exec", action: "CheckOutput" }), false);
  });

  it("refreshes on Computer and FactoryArtifact events", () => {
    assert.equal(shouldRefreshForEvent({ entity_type: "Computer", action: "ProvisionComplete" }), true);
    assert.equal(shouldRefreshForEvent({ entity_type: "FactoryArtifact", action: "PublishPR" }), true);
  });

  it("ignores unrelated platform entity chatter", () => {
    for (const entity_type of ["Directory", "Session", "EvolutionProposal", "Agent"]) {
      assert.equal(shouldRefreshForEvent({ entity_type, action: "Create" }), false, entity_type);
    }
  });

  it("tolerates malformed events", () => {
    assert.equal(shouldRefreshForEvent(null), false);
    assert.equal(shouldRefreshForEvent({}), false);
    assert.equal(shouldRefreshForEvent("state_change"), false);
  });
});
