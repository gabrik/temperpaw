# ADR-0068: Live activity feed — SSE `/tdata/$events` + in-flight exec output

## Status

Accepted — implemented and live-verified (2026-09-17, GB: "can we now
tackle the SSE").

## Context

ADR-0067 deferred live streaming: the console polls OData every 3 s, and
exec output appeared only after completion (ADR-0067 "SSE or an entity
change feed" follow-up). Research for this ADR found the transport
already exists in the Temper platform at temperpaw's pinned rev
(a747f7d4): `GET /tdata/$events` streams one SSE `state_change` per
dispatch (`EntityStateChange{seq, entity_type, entity_id, action,
status, tenant, …}`), tenant-scoped, gated by Cedar `read_events` on
`Entity`. No Temper repo change is required.

Transport alone is insufficient: during an exec run there are no
dispatches, so nothing streams. paw-compute ADR-0005 adds the
entity-side counterpart (`CheckOutput` self-loop + `ReportOutput`) so
the Exec row changes mid-run and every change emits an event.

## Decision

1. **Console consumes `/tdata/$events` via `EventSource`** through the
   same vite proxy as OData (cookie auth, same-origin). On each
   `state_change` for an entity the open view cares about (FactoryTask,
   Exec, Computer, FactoryArtifact for the open task), the console
   schedules a debounced (~300 ms) refetch of that view's queries.
2. **Polling stays as the correctness net.** The 3 s poller is not
   removed; SSE is a latency optimization (gate changes, step
   transitions, exec tails appear ≤ ~1 s instead of ≤ 3 s). If
   EventSource errors or is unsupported, the console behaves exactly as
   before. Reconnect uses EventSource's built-in retry.
3. **Cedar**: `factory.cedar` gains a tenant-open `read_events` permit
   (unqualified `resource`, matching the file's other permits — no `is
   Entity` refinement since no in-repo precedent exists and the action
   only ever applies to the Entity resource anyway), same posture as the
   existing open read/list permits — API access is already
   credential-gated and the stream is tenant-scoped server-side.
4. **View-model purity (D2)**: event handling is a pure function
   `shouldRefreshForEvent(change)` in `view-model.js` (entity_type
   allowlist; `Exec.CheckOutput` ticks are dropped — they carry no new
   data and would double the refetch churn every 5 s), unit-tested;
   `api.js` owns the EventSource lifecycle (`openFactoryEventStream`);
   `App.jsx` wires callback → 300 ms debounce → the same `refresh` the
   poller uses (via a ref, so the stream doesn't re-open on selection
   changes).

## Alternatives considered

- **Platform SSE change in the Temper repo** — unnecessary; the endpoint
  exists at the pinned rev.
- **Exec-output-only WebSocket in temperpaw** — duplicates the platform
  feed, violates "extend Temper, don't work around it".
- **Drop polling for SSE-only** — rejected: SSE has no replay; a
  disconnected tab would miss transitions. Poll + SSE is belt and
  suspenders at trivial cost.

## Consequences

- Console feels live: provisioning steps, gate arrivals, exec tails
  (via ADR-0005) and status transitions render within ~1 s.
- Every console open on a task page holds one SSE connection; trivial at
  single-user scale, noted for multi-user future.
- The read_events permit is tenant-wide for all entity types — matches
  the console's existing read posture.

## Verification

Red-green view-model tests (event allowlist/id matching). Live e2e:
`curl -N /tdata/$events` shows `state_change` for CheckOutput /
ReportOutput / task transitions; Playwright probe watches exec output
appear while the exec is still `Running`; full factory task run
regression (Implementing → gates → Completed) with the console open.
Proof: os-apps/dark-factory/.proofs/0068-live-activity.md.
