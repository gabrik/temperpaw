// Session flow for the MVP console: no login page — the SPA restores an
// existing cookie session and falls back to auto-login with dev
// credentials (VITE_CONSOLE_EMAIL / VITE_CONSOLE_PASSWORD, defaults are
// the local e2e user). Run: node --test.
import { test } from "node:test";
import assert from "node:assert/strict";

function stubFetch(routes) {
  const calls = [];
  globalThis.fetch = async (url, options = {}) => {
    calls.push({ url: String(url), method: options.method ?? "GET" });
    const route = routes.find(([method, path]) => String(url).includes(path) && (options.method ?? "GET") === method);
    if (!route) return new Response("not found", { status: 404 });
    const [, , status, body] = route;
    return new Response(JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json" },
    });
  };
  return calls;
}

test("ensureSession returns the current user when the cookie is valid", async () => {
  const { ensureSession } = await import("../src/api.js");
  const calls = stubFetch([["GET", "/auth/me", 200, { email: "e2e@darkfactory.local" }]]);
  const user = await ensureSession();
  assert.equal(user.email, "e2e@darkfactory.local");
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, "/auth/me");
});

test("ensureSession falls back to auto-login with dev credentials", async () => {
  const { ensureSession } = await import("../src/api.js");
  let authed = false;
  const calls = stubFetch([
    ["GET", "/auth/me", 401, { error: "unauthorized" }],
    ["POST", "/auth/login", 200, { email: "e2e@darkfactory.local" }],
  ]);
  // after login, /auth/me would succeed; ensureSession only needs login's 200
  const user = await ensureSession();
  assert.equal(user.email, "e2e@darkfactory.local");
  assert.deepEqual(
    calls.map((c) => `${c.method} ${c.url}`),
    ["GET /auth/me", "POST /auth/login"],
  );
});

test("ensureSession surfaces a real error when auto-login fails", async () => {
  const { ensureSession } = await import("../src/api.js");
  stubFetch([
    ["GET", "/auth/me", 401, { error: "unauthorized" }],
    ["POST", "/auth/login", 401, { error: "Invalid credentials" }],
  ]);
  await assert.rejects(() => ensureSession(), /Invalid credentials/);
});
