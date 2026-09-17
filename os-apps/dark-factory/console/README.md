# dark-factory console

Browser console for the dark-factory Temper app (ADR-0067). Port of the DEN
demo SPA with the sidecar/BFF removed: the browser talks to the temper
server's OData endpoint directly, authenticated as an Admin human via the
temperpaw cookie session (`paw_session`).

## Run

```bash
npm install
npm run dev        # vite dev on http://127.0.0.1:8081
# or production build:
npm run build && npm run preview   # static dist + proxy on 8081
```

The console expects a temper server on `http://127.0.0.1:3100` (override
with `TEMPER_URL`). Both `/tdata` and `/auth` are proxied so the session
cookie stays same-origin.

**No login page (MVP):** the SPA restores any existing cookie session and
otherwise auto-logs-in with local dev credentials. Override with
`VITE_CONSOLE_EMAIL` / `VITE_CONSOLE_PASSWORD` (defaults: the throwaway
`e2e@darkfactory.local` / `e2e-factory-pass` account on the local server).
Human gates still carry the session user's email as `operation_owner`, so
self-reporting attribution is kept, and if the session expires mid-use the
polling loop re-authenticates silently. Real multi-user auth is a
follow-up (see ADR-0067 open questions).

## Test

```bash
npm test           # node --test: view-model mapping + gate params + session flow
npm run check      # tests + production build
```

## What the panels map to (ADR-0067 D4)

| DEN sidecar feature | Console source |
| --- | --- |
| Task list / detail | `GET /tdata/FactoryTasks` |
| Activity log | `GET /tdata/Execs?$filter=computer_id eq …` |
| Diff | `GET /tdata/FactoryArtifacts?$filter=task_id eq …` (patch pane) + PR link |
| Chat | folded into the decision-card comment (`repair_context`) |
| SSE events | 3 s polling (entity change feed is a follow-up) |

Human gates dispatch with CAS params: `ApprovePlan`/`RejectPlan` fence on
`plan_digest`, `ApproveMerge`/`RequestChanges` on `head_sha`, each with a
fresh `operation_key` and `phase_ticks: 0` (see `src/view-model.js`).
