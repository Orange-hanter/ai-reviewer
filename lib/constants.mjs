/** Generic MoA advisors (REVIEW_SPECIALISTS=0) — три независимых prose-ревьюера. */
export const DEFAULT_MOA_REFERENCES = 'kimi-k2.7-code,glm-5.2,deepseek-v4-flash';

/**
 * Specialist advisors (REVIEW_SPECIALISTS=1, default) — семантическая привязка ролей.
 * SWE-Bench / task-fit: kimi→correctness, glm→architecture, deepseek-flash→security patterns.
 */
export const DEFAULT_MOA_SPECIALIST_MODELS =
  'security:deepseek-v4-flash,correctness:kimi-k2.7-code,architecture:glm-5.2';

/** Jury aggregator (JSON + advisorClaims) — вне пула advisors (MiniMax: batch code review). */
export const DEFAULT_MOA_AGGREGATOR = 'minimax-m2.7';

/** Pass B verifier — глубокий structured judge, вне advisors (DeepSeek Pro). */
export const DEFAULT_REVIEW_VERIFIER_MODEL = 'deepseek-v4-pro';

export const DEFAULT_OLLAMA_MODELS_FALLBACK = 'deepseek-v4-flash';

export const REPO_RULES = `Project Orakul — a TypeScript monolith (Express + React/Vite). Layers under app/:
- shared/ — pure domain logic and types. MUST NOT import server/client, no node:/React.
- server/ (+ integrations/ops/scripts) — backend, imports shared, MUST NOT import client.
- client/src/ — frontend, imports shared (@shared/*); talks to the backend ONLY via client/src/api.ts
  (global fetch outside api.ts is forbidden).
Canonical primitives (do NOT duplicate locally — import from the source of truth):
- money/stock rounding: shared/round.ts round2/round1 (not Math.round(x*100)/100 / +toFixed).
- client IP: server/net.ts clientIp (never parse raw X-Forwarded-For — anti-brute bypass).
- audit+realtime: server/audit.ts auditAndPublish (not a pair of appendAudit+publishChange).
- data access: DataStore/StoreTx (server/dataStore.ts), not files/SQL directly.
- RBAC: shared/permissions.ts canWriteType/canReadType; enforcement in server/recordAccess.ts (server = source of truth).
- UI formatters: client/src/utils/format.ts (fmtMoney/fmtNumber2/fmtPct).
Tests: behavior is verified with Playwright e2e (app/e2e/*.spec.ts), not manual runs.
Style and import boundaries are enforced by eslint (no-restricted-imports) and tsc — do NOT repeat lint/style nitpicks.`;

export const SEV = { critical: '🔴', major: '🟠', minor: '🟡', nit: '🔵' };
export const VERDICT_RANK = { changes_requested: 2, comment: 1, approve: 0 };

export const MARKERS = {
  review: '<!-- orakul-ai-review -->',
  inline: '<!-- orakul-ai-review-inline -->',
  dataRe: /<!--\s*orakul-ai-review-data:([\s\S]*?)-->/,
};
