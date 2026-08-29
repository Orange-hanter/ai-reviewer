Project Orakul — a TypeScript monolith (Express + React/Vite). Layers under app/:
- shared/ — pure domain logic and types. MUST NOT import server/client, no node:/React.
- server/ (+ integrations/ops/scripts) — backend, imports shared, MUST NOT import client.
- client/src/ — frontend, imports shared (@shared/*); talks to the backend ONLY via client/src/api.ts
  (global fetch outside api.ts is forbidden).
Canonical primitives (do NOT duplicate locally — import from the source of truth):
- money/stock rounding: shared/round.ts round1/round2/round4 (not Math.round(x*100)/100 / +toFixed).
- client IP: server/net.ts clientIp (never parse raw X-Forwarded-For — anti-brute bypass).
- audit+realtime: server/audit.ts auditAndPublish (not a pair of appendAudit+publishChange).
- data access: DataStore/StoreTx (server/dataStore.ts), not files/SQL directly.
- RBAC: shared/permissions.ts canWriteType/canReadType; enforcement in server/recordAccess.ts (server = source of truth).
- UI formatters: client/src/utils/format.ts (fmtPrice/fmtMoney/fmtNumber2/fmtPct).
Tests: behavior is verified with Playwright e2e (app/e2e/*.spec.ts), not manual runs.
Style and import boundaries are enforced by eslint (no-restricted-imports) and tsc — do NOT repeat lint/style nitpicks.
