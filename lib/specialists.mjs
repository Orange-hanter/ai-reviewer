/**
 * Фаза 3, шаг 2: lib/specialists.mjs — обёртка над Rust-бинарём
 * rust/target/debug/specialists (порт R5-логики: парсинг назначений ролей,
 * normalizeAdvisorClaims, decideRefutation/applyRefutation jury-разбор,
 * mergeSpecialistReferences, formatRefutationLog).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Функции
 * чистые и вызываются синхронно (pipeline.mjs, lib/eval.mjs, lib/findings.mjs,
 * lib/config.mjs, тесты), поэтому вместо async execFile используется
 * execFileSync: бинарь вызывается на каждую операцию с JSON {"op", ...args}
 * на stdin, ответ — JSON с результатом.
 *
 * Резолвинг бинаря (по аналогии с lib/findings.mjs):
 *   AI_REVIEWER_SPECIALISTS_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/specialists;
 *   AI_REVIEWER_SPECIALISTS_FORCE_JS=1 — принудительно чистый JS (паритет/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm
 * test/eval идут раньше cargo build, см. .github/workflows/ci.yml), обёртка
 * деградирует на чистый JS fallback ниже — эталонную копию исходной
 * реализации. Если бинарь ЕСТЬ, но упал (non-zero exit) — ошибка
 * пробрасывается, fallback не используется: молчаливая подмена реализации
 * при битом Rust-коде скрывала бы регрессии.
 *
 * Нюансы паритета Rust-порта (проверены parity-прогоном
 * rust/parity/specialists.parity.mjs):
 *   - applyRefutation в JS мутирует находки на месте и ВОЗВРАЩАЕТ отфильтрованный
 *     массив той же ссылок; бинарь не мутирует вход — возвращает kept-индексы
 *     и мутации (index → severity/refutationStatus), обёртка накладывает их на
 *     исходные объекты и собирает результат по индексам (семантика разделяемых
 *     ссылок пайплайна сохранена);
 *   - логи refutation (strict/dropped/downgraded) генерирует бинарь списком
 *     строк в порядке следования находок — обёртка шлёт их в log;
 *   - normalizeAdvisorClaims — тот же порт, что в findings.rs (normalizeFinding
 *     внутри себя его дергает), теперь экспортирован отдельным op;
 *   - parseSpecialistAssignments получает из env только нужные ключи
 *     (обёртка выделяет подмножество — весь process.env гонять незачем).
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DEFAULT_MOA_SPECIALIST_MODELS } from './constants.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_SPECIALISTS_BIN = resolve(__dir, '../rust/target/debug/specialists');

function resolveSpecialistsBin() {
  if (process.env.AI_REVIEWER_SPECIALISTS_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_SPECIALISTS_BIN || '').trim() || DEFAULT_SPECIALISTS_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; undefined — бинарь недоступен (fallback на JS).
 * Легитимный JSON null в ответе (пустые advisorClaims) — валидное значение. */
function runRust(op, args) {
  const bin = resolveSpecialistsBin();
  if (!bin) return undefined;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

export const SPECIALIST_ROLES = ['security', 'correctness', 'architecture'];

/** Семантические дефолты (не зависят от порядка MOA_REFERENCE_MODELS). */
export const ROLE_DEFAULT_MODEL = {
  security: 'deepseek-v4-flash',
  correctness: 'kimi-k2.7-code',
  architecture: 'glm-5.2',
};

function parseSpecialistBulkJs(bulk) {
  const fromBulk = {};
  for (const part of bulk.split(',').map((s) => s.trim()).filter(Boolean)) {
    const i = part.indexOf(':');
    if (i <= 0) continue;
    const role = part.slice(0, i).trim();
    const model = part.slice(i + 1).trim();
    if (SPECIALIST_ROLES.includes(role) && model) fromBulk[role] = model;
  }
  return fromBulk;
}

/**
 * Парсит MOA_SPECIALIST_MODELS / MOA_SPECIALIST_<ROLE>_MODEL / ROLE_DEFAULT_MODEL.
 * moaReferences — legacy fallback только если роль не задана явно (deprecated).
 */
export function parseSpecialistAssignments(env, moaReferences = []) {
  const r = runRust('parseAssignments', {
    env: {
      MOA_SPECIALIST_MODELS: env.MOA_SPECIALIST_MODELS ?? null,
      MOA_SPECIALIST_SECURITY_MODEL: env.MOA_SPECIALIST_SECURITY_MODEL ?? null,
      MOA_SPECIALIST_CORRECTNESS_MODEL: env.MOA_SPECIALIST_CORRECTNESS_MODEL ?? null,
      MOA_SPECIALIST_ARCHITECTURE_MODEL: env.MOA_SPECIALIST_ARCHITECTURE_MODEL ?? null,
    },
    moaReferences,
  });
  if (r !== undefined) return r;
  return parseSpecialistAssignmentsJs(env, moaReferences);
}

export function normalizeAdvisorClaims(raw) {
  const r = runRust('normalizeAdvisorClaims', { raw: raw ?? null });
  if (r !== undefined) return r ?? undefined; // мост отдаёт JSON null там, где JS — undefined
  return normalizeAdvisorClaimsJs(raw);
}

export function decideRefutation(claims) {
  const r = runRust('decideRefutation', { claims: claims || [] });
  if (r !== undefined) return r;
  return decideRefutationJs(claims);
}

/**
 * Применяет refutation ко всем findings.
 * strict: critical/major без advisorClaims → downgrade minor (не блок без jury).
 */
export function applyRefutation(allFindings, log = () => {}, { strict = false, blockOn = ['critical', 'major'] } = {}) {
  const list = allFindings || [];
  const r = runRust('applyRefutation', { findings: list, strict: !!strict, blockOn: blockOn || [] });
  if (r !== undefined) {
    for (const m of r.mutations) {
      const f = list[m.index];
      if (m.severity !== undefined) f.severity = m.severity;
      if (m.refutationStatus !== undefined) f.refutationStatus = m.refutationStatus;
    }
    for (const line of r.logs) log(line);
    return { findings: r.kept.map((i) => list[i]), stats: r.stats };
  }
  return applyRefutationJs(allFindings, log, { strict, blockOn });
}

/** Сливает specialist references со всех чанков (для sticky report). */
export function mergeSpecialistReferences(chunkResults) {
  const r = runRust('mergeSpecialistReferences', { chunkResults: chunkResults || [] });
  if (r !== undefined) return r;
  return mergeSpecialistReferencesJs(chunkResults);
}

export function formatRefutationLog(stats) {
  const r = runRust('formatRefutationLog', { stats: stats || {} });
  if (r !== undefined) return r;
  return formatRefutationLogJs(stats);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/specialists.mjs
// до Фазы 3). Используется только когда Rust-бинарь отсутствует; при битом
// бинаре НЕ подменяет его.
// ---------------------------------------------------------------------------

function parseSpecialistAssignmentsJs(env, moaReferences = []) {
  const fromEnv = parseSpecialistBulkJs(env.MOA_SPECIALIST_MODELS || DEFAULT_MOA_SPECIALIST_MODELS);
  for (const role of SPECIALIST_ROLES) {
    const key = `MOA_SPECIALIST_${role.toUpperCase()}_MODEL`;
    if (env[key]?.trim()) fromEnv[role] = env[key].trim();
  }
  return SPECIALIST_ROLES.map((role, i) => ({
    role,
    model: fromEnv[role] || ROLE_DEFAULT_MODEL[role] || moaReferences[i],
  }));
}

function normalizeAdvisorClaimsJs(raw) {
  if (!Array.isArray(raw)) return undefined;
  const claims = raw
    .filter((c) => c && SPECIALIST_ROLES.includes(c.role) &&
      ['verified', 'rejected', 'downgraded'].includes(c.status))
    .map((c) => {
      const claim = {
        role: c.role,
        status: c.status,
        reason: typeof c.reason === 'string' ? c.reason : '',
      };
      if (c.status === 'downgraded' && ['minor', 'nit'].includes(c.severity)) {
        claim.severity = c.severity;
      }
      return claim;
    });
  return claims.length ? claims : undefined;
}

/**
 * Refutation по advisorClaims на finding (aggregator jury).
 * - все rejected → drop
 * - verified → keep
 * - downgraded (no verified) → severity из claims (nit если любой nit, иначе minor)
 */
function decideRefutationJs(claims) {
  if (!claims?.length) return { action: 'keep' };
  const statuses = claims.map((c) => c.status);
  if (statuses.every((s) => s === 'rejected')) return { action: 'drop' };
  if (statuses.includes('verified')) return { action: 'keep' };
  if (statuses.includes('downgraded')) {
    const downs = claims.filter((c) => c.status === 'downgraded');
    const severity = downs.some((c) => c.severity === 'nit') ? 'nit' : 'minor';
    return { action: 'downgrade', severity };
  }
  return { action: 'keep' };
}

function applyRefutationJs(allFindings, log = () => {}, { strict = false, blockOn = ['critical', 'major'] } = {}) {
  const tiers = new Set(blockOn || []);
  const stats = { kept: 0, dropped: 0, downgraded: 0, noClaims: 0, downgradedNoClaims: 0 };
  const out = [];

  for (const f of allFindings) {
    if (f.prescan) {
      out.push(f);
      continue;
    }
    const claims = f.advisorClaims;
    if (!claims?.length) {
      if (strict && tiers.has(f.severity)) {
        f.severity = 'minor';
        f.refutationStatus = 'downgraded_no_claims';
        stats.downgradedNoClaims++;
        stats.downgraded++;
        log(`refutation strict: [block-tier без advisorClaims] ${f.file}:${f.line} «${f.title}» → minor`);
        out.push(f);
        continue;
      }
      stats.noClaims++;
      out.push(f);
      continue;
    }
    const decision = decideRefutationJs(claims);
    if (decision.action === 'drop') {
      stats.dropped++;
      f.refutationStatus = 'dropped';
      log(`refutation dropped: [${f.severity}] ${f.file}:${f.line} «${f.title}» — все advisors rejected`);
      continue;
    }
    if (decision.action === 'downgrade') {
      f.severity = decision.severity || 'minor';
      f.refutationStatus = 'downgraded';
      stats.downgraded++;
      log(`refutation downgraded: «${f.title}» → ${f.severity}`);
    } else {
      f.refutationStatus = claims.some((c) => c.status === 'verified') ? 'verified' : 'kept';
      stats.kept++;
    }
    out.push(f);
  }

  return { findings: out, stats };
}

/** Сливает specialist references со всех чанков (для sticky report). */
function mergeSpecialistReferencesJs(chunkResults) {
  const map = new Map();
  for (const res of chunkResults || []) {
    for (const r of res?.references || []) {
      const key = `${r.role || 'advisor'}::${r.model}`;
      const prev = map.get(key);
      if (!prev) {
        map.set(key, { ...r, chunkCount: 1 });
        continue;
      }
      prev.chunkCount++;
      if (r.text && !r.error) {
        const tag = prev.chunkCount > 1 ? `\n\n[chunk ${prev.chunkCount}]\n` : '\n\n';
        prev.text = prev.text ? `${prev.text}${tag}${r.text}` : r.text;
      }
      if (r.error && !prev.text) prev.error = r.error;
    }
  }
  return [...map.values()];
}

function formatRefutationLogJs(stats) {
  const extra = stats.downgradedNoClaims
    ? ` downgradedNoClaims=${stats.downgradedNoClaims}` : '';
  return `kept=${stats.kept} dropped=${stats.dropped} downgraded=${stats.downgraded} noClaims=${stats.noClaims}${extra}`;
}
