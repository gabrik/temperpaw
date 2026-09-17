// Direct Temper OData client for the dark-factory console (ADR-0067 D2 —
// no BFF). The browser authenticates as an Admin human via the temperpaw
// cookie session (`paw_session`) and talks to /tdata + /auth directly; in
// dev/preview the vite proxy forwards both to the temper server so
// everything stays same-origin.
//
// Human-gate dispatches follow the console create/decide flows:
//   POST /tdata/FactoryTasks('<id>')/Temper.DarkFactory.<Action>?tenant=default
// with CAS params built by view-model.gateDecision.

const TENANT = "default";

import { plannerComputerName } from "./view-model.js";

async function request(path, options = {}) {
  const headers = { ...(options.headers ?? {}) };
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  const response = await fetch(path, {
    credentials: "same-origin",
    ...options,
    headers,
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  });
  const contentType = response.headers.get("content-type") ?? "";
  const payload = contentType.includes("application/json") ? await response.json() : await response.text();
  if (!response.ok) {
    const message = payload?.error ?? payload?.message ?? (typeof payload === "string" ? payload : null);
    throw new Error(message ?? `Request failed (${response.status})`);
  }
  return payload;
}

function collection(name) {
  return `/tdata/${name}?tenant=${TENANT}`;
}

function entity(name, id) {
  return `/tdata/${name}('${encodeURIComponent(id)}')?tenant=${TENANT}`;
}

function dispatchUrl(id, action) {
  return `/tdata/FactoryTasks('${encodeURIComponent(id)}')/Temper.DarkFactory.${action}?tenant=${TENANT}`;
}

export async function restoreSession() {
  return request("/auth/me");
}

export async function login(email, password) {
  return request("/auth/login", { method: "POST", body: { email, password } });
}

// MVP console: no login page. Boot restores an existing cookie session and
// falls back to auto-login with local dev credentials. Override with
// VITE_CONSOLE_EMAIL / VITE_CONSOLE_PASSWORD for a different user; the
// defaults are the throwaway e2e account on the local 3100 server.
const DEV_EMAIL = import.meta.env?.VITE_CONSOLE_EMAIL ?? "e2e@darkfactory.local";
const DEV_PASSWORD = import.meta.env?.VITE_CONSOLE_PASSWORD ?? "e2e-factory-pass";

export async function ensureSession() {
  try {
    return await restoreSession();
  } catch {
    return login(DEV_EMAIL, DEV_PASSWORD);
  }
}

export const api = {
  listFactories: async () => {
    const payload = await request(collection("FactoryConfigs"));
    return (payload.value ?? []).filter((row) => (row.fields?.Status ?? row.status) === "Active");
  },

  listTasks: () => request(collection("FactoryTasks")),

  getTask: (taskId) => request(entity("FactoryTasks", taskId)),

  getExecs: (computerId) =>
    request(`${collection("Execs")}&$filter=${encodeURIComponent(`computer_id eq '${computerId}'`)}`),

  getArtifacts: (taskId) =>
    request(`${collection("FactoryArtifacts")}&$filter=${encodeURIComponent(`task_id eq '${taskId}'`)}`),

  getComputerByName: (name) =>
    request(`${collection("Computers")}&$filter=${encodeURIComponent(`name eq '${name}'`)}`),

  async getTaskBundle(taskRow) {
    const taskId = taskRow.entity_id ?? taskRow.fields?.Id;
    const [task, artifacts, computers] = await Promise.all([
      request(entity("FactoryTasks", taskId)),
      request(`${collection("FactoryArtifacts")}&$filter=${encodeURIComponent(`task_id eq '${taskId}'`)}`),
      // The planner's computer is discoverable by deterministic name even
      // before it is attached to the task (computer_id lags a tick).
      api.getComputerByName(plannerComputerName(taskId)).catch(() => ({ value: [] })),
    ]);
    const computer = (computers.value ?? [])[0] ?? null;
    const computerId = task.fields?.computer_id || computer?.entity_id || "";
    const execs = computerId
      ? await request(`${collection("Execs")}&$filter=${encodeURIComponent(`computer_id eq '${computerId}'`)}`)
      : { value: [] };
    return { task, artifacts, execs, computer };
  },

  // Create flow (ADR-0067): create the row, then StartPlanning with the
  // prompt + chosen factory. The planner self-creates the Computer.
  async createTask({ factoryId, prompt, owner, operationKey }) {
    const created = await request(collection("FactoryTasks"), {
      method: "POST",
      body: { created_by: owner },
    });
    const taskId = created.entity_id;
    await request(dispatchUrl(taskId, "StartPlanning"), {
      method: "POST",
      body: {
        task_prompt: prompt,
        factory_id: factoryId,
        computer_id: "",
        operation_key: operationKey,
        operation_owner: owner,
        phase_ticks: 0,
      },
    });
    return { taskId };
  },

  decide: (taskId, action, params) => request(dispatchUrl(taskId, action), { method: "POST", body: params }),
};
