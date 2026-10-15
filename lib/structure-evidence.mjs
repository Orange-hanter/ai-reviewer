/**
 * Фаза 2, шаг 3: lib/structure-evidence.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/structure_evidence (порт validateEvidenceReceipt /
 * createEvidenceReceipt / receiptRef).
 *
 * Публичное API не изменилось (те же экспорты, включая реэкспорт canonicalJson/
 * normalizeRepoPath/sha256/stableId из lib/util.mjs — они остаются чистым JS,
 * т.к. это generic-хелперы; Rust-бинарь дублирует их для parity-прогона).
 * Синхронные функции вызывают бинарь через execFileSync: JSON {"op", ...args}
 * на stdin, ответ — JSON в формате JS-возврата.
 *
 * Резолвинг бинаря (по аналогии с lib/structure-lcov.mjs):
 *   AI_REVIEWER_STRUCTURE_EVIDENCE_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/structure_evidence;
 *   AI_REVIEWER_STRUCTURE_EVIDENCE_FORCE_JS=1 — принудительно чистый JS.
 *
 * Если бинарь не существует (чистый клон до первого `cargo build`), обёртка
 * деградирует на чистый JS fallback ниже — эталонную копию исходной реализации.
 * Если бинарь ЕСТЬ, но упал (non-zero exit) — ошибка пробрасывается с текстом
 * доменной ошибки Rust ({"error"} со stderr), fallback не используется:
 * молчаливая подмена реализации при битом Rust-коде скрывала бы регрессии.
 *
 * Нюансы паритета (проверены parity-прогоном rust/parity/structure-evidence.parity.mjs):
 *   - canonicalJson/sha256 в Rust побайтово совпадают с lib/util.mjs: сортировка
 *     ключей по UTF-16 code units (astral-символы перед U+E000..U+FFFF),
 *     экранирование JSON.stringify (ES2019: U+2028/U+2029 БЕЗ экранирования,
 *     контролы < 0x20 → \u00xx), числа через ECMA Number::toString с тай-брейком
 *     «ближайшее, при равенстве чётное» (точная арифметика: std {:e} в тай-
 *     кейсах даёт иную последнюю цифру, чем V8); -0 → '0', 1e999 → 'null';
 *   - strings(): String(v).trim(), dedup, slice(0, max) до сортировки;
 *   - complete — строгое `!== false` (0/''/null → true), truncated — `=== true`;
 *   - exitStatus — только целочисленный number (5.0 ок, '5'/5.5/true → null);
 *   - массив как artifact/observation проходит typeof-ветку (ошибки полей, а
 *     не 'artifact is required');
 *   - Date.parse(observedAt) — строгое подмножество ISO-8601 с V8-леньностью
 *     (rollover дня/часа 24, 't'/'z', ±HHMM); RFC-2822 и прочие V8-форматы не
 *     принимаются (отклонение, producers шлют ISO).
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
// generic-хелперы живут в lib/util.mjs; здесь — реэкспорт для обратной совместимости
import {
  canonicalJson, normalizeRepoPath, sha256, stableId,
} from './util.mjs';
import { defaultRustBin } from './rust-bin.mjs';

export { canonicalJson, normalizeRepoPath, sha256, stableId };

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_EVIDENCE_BIN = defaultRustBin('structure_evidence');

function resolveEvidenceBin() {
  if (process.env.AI_REVIEWER_STRUCTURE_EVIDENCE_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_STRUCTURE_EVIDENCE_BIN || '').trim() || DEFAULT_EVIDENCE_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). Ошибка бинаря пробрасывается. */
function runRust(op, args) {
  const bin = resolveEvidenceBin();
  if (!bin) return null;
  try {
    const raw = execFileSync(bin, [], {
      input: JSON.stringify({ op, ...args }),
      encoding: 'utf8',
      maxBuffer: 128 * 1024 * 1024,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    return JSON.parse(raw);
  } catch (error) {
    // доменные ошибки Rust: exit 2 + {"error": "..."} на stderr
    const m = String(error?.stderr || '').match(/"error":\s*"((?:[^"\\]|\\.)*)"/);
    if (m) throw new Error(JSON.parse(`"${m[1]}"`));
    throw error;
  }
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

export const EVIDENCE_RECEIPT_VERSION = 1;
export const TRUST_TIERS = Object.freeze(['signed_ci', 'trusted_local', 'untrusted']);
export const EVIDENCE_CEILINGS = Object.freeze([
  'static_inventory',
  'aggregate_lcov',
  'per_test_execution',
  'assertion_bound',
  'mutation_bound',
  'property_bound',
  'contract_verified',
]);

/** Валидирует receipt, возвращает список ошибок (пустой — валиден). */
export function validateEvidenceReceipt(receipt) {
  const r = runRust('validateEvidenceReceipt', { receipt });
  if (r !== null) return r;
  return validateEvidenceReceiptJs(receipt);
}

/** Создаёт нормализованный receipt; при невалидности бросает Error. */
export function createEvidenceReceipt(input) {
  // часы живут в JS: Rust берёт observedAt из input или из now
  const now = new Date().toISOString();
  const r = runRust('createEvidenceReceipt', { input, now });
  if (r !== null) return r;
  return createEvidenceReceiptJs(input);
}

/** receiptRef: `receipt:` + sha256(receipt) (canonical JSON для объектов). */
export function receiptRef(receipt) {
  const r = runRust('receiptRef', { receipt });
  if (r !== null) return r;
  return receiptRefJs(receipt);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/structure-evidence.mjs
// до Фазы 2). Используется только когда Rust-бинарь отсутствует; при битом бинаре
// НЕ подменяет его.
// ---------------------------------------------------------------------------

const SHA256_RE = /^[0-9a-f]{64}$/;
const HEAD_SHA_RE = /^[0-9a-f]{7,64}$/i;

function stringsJs(values, max = 100) {
  return [...new Set((values || []).map((value) => String(value).trim()).filter(Boolean))]
    .slice(0, max)
    .sort();
}

function validateEvidenceReceiptJs(receipt) {
  const errors = [];
  if (!receipt || typeof receipt !== 'object' || Array.isArray(receipt)) return ['receipt must be an object'];
  if (receipt.version !== EVIDENCE_RECEIPT_VERSION) errors.push('version must be 1');
  for (const field of ['provider', 'toolVersion', 'capability', 'headSha', 'configHash', 'scopeHash', 'observedAt']) {
    if (!String(receipt[field] || '').trim()) errors.push(`${field} is required`);
  }
  if (!HEAD_SHA_RE.test(String(receipt.headSha || ''))) errors.push('headSha must be a git-like hex sha');
  if (!SHA256_RE.test(String(receipt.configHash || ''))) errors.push('configHash must be sha256');
  if (!SHA256_RE.test(String(receipt.scopeHash || ''))) errors.push('scopeHash must be sha256');
  if (!receipt.artifact || typeof receipt.artifact !== 'object') errors.push('artifact is required');
  else {
    if (!String(receipt.artifact.uri || '').trim()) errors.push('artifact.uri is required');
    if (!SHA256_RE.test(String(receipt.artifact.sha256 || ''))) errors.push('artifact.sha256 must be sha256');
  }
  if (!receipt.observation || typeof receipt.observation !== 'object') errors.push('observation is required');
  else if (!String(receipt.observation.type || '').trim()) errors.push('observation.type is required');
  if (!TRUST_TIERS.includes(receipt.trustTier)) errors.push('invalid trustTier');
  if (!EVIDENCE_CEILINGS.includes(receipt.evidenceCeiling)) errors.push('invalid evidenceCeiling');
  if (!Array.isArray(receipt.limitations)) errors.push('limitations must be an array');
  if (Number.isNaN(Date.parse(String(receipt.observedAt || '')))) errors.push('observedAt must be ISO-8601');
  for (const ref of receipt.subjectRefs || []) {
    if (!String(ref || '').trim()) errors.push('subjectRefs cannot contain empty values');
  }
  if (receipt.observation?.path != null) {
    try { normalizeRepoPath(receipt.observation.path); } catch (error) { errors.push(error.message); }
  }
  return errors;
}

function createEvidenceReceiptJs(input) {
  const observation = { ...(input.observation || {}) };
  delete observation.covered;
  if (observation.path != null) observation.path = normalizeRepoPath(observation.path);
  const receipt = {
    version: EVIDENCE_RECEIPT_VERSION,
    provider: String(input.provider || '').trim(),
    toolVersion: String(input.toolVersion || '').trim(),
    capability: String(input.capability || '').trim(),
    headSha: String(input.headSha || '').trim(),
    testRunId: input.testRunId == null ? null : String(input.testRunId),
    configHash: String(input.configHash || ''),
    scopeHash: String(input.scopeHash || ''),
    subjectRefs: stringsJs(input.subjectRefs),
    artifact: {
      uri: String(input.artifact?.uri || '').trim(),
      sha256: String(input.artifact?.sha256 || ''),
    },
    observation,
    trustTier: input.trustTier || 'trusted_local',
    evidenceCeiling: input.evidenceCeiling || 'static_inventory',
    limitations: stringsJs(input.limitations, 50),
    complete: input.complete !== false,
    truncated: input.truncated === true,
    exitStatus: Number.isInteger(input.exitStatus) ? input.exitStatus : null,
    observedAt: input.observedAt || new Date().toISOString(),
  };
  const errors = validateEvidenceReceiptJs(receipt);
  if (errors.length) throw new Error(`invalid evidence receipt: ${errors.join('; ')}`);
  return receipt;
}

function receiptRefJs(receipt) {
  return `receipt:${sha256(receipt)}`;
}
