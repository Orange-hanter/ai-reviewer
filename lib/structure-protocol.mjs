/**
 * Фаза 2, шаг 4: lib/structure-protocol.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/structure_protocol (порт ручной валидации structure-пейлоадов
 * против контракта schemas/*.json + фингерпринты и temporalAlignment).
 *
 * Публичное API не изменилось (те же экспорты и семантика). Синхронные функции
 * вызывают бинарь через execFileSync: JSON {"op", ...args} на stdin, ответ —
 * JSON в формате JS-возврата.
 *
 * Резолвинг бинаря (по аналогии с lib/structure-evidence.mjs):
 *   AI_REVIEWER_STRUCTURE_PROTOCOL_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/structure_protocol;
 *   AI_REVIEWER_STRUCTURE_PROTOCOL_FORCE_JS=1 — принудительно чистый JS.
 *
 * Если бинарь не существует (чистый клон до первого `cargo build`), обёртка
 * деградирует на чистый JS fallback ниже — эталонную копию исходной реализации.
 * Если бинарь ЕСТЬ, но упал (non-zero exit) — ошибка пробрасывается с текстом
 * доменной ошибки Rust ({"error"} со stderr), fallback не используется:
 * молчаливая подмена реализации при битом Rust-коде скрывала бы регрессии.
 *
 * Нюансы паритета (проверены parity-прогоном rust/parity/structure-protocol.parity.mjs):
 *   - схемы с диска НЕ читаются ни на одной стороне: проверки зашиты в код
 *     как зеркало контракта schemas/*.json (это и есть «ручная валидация»);
 *   - семантика лимитов как в JS: отсутствующий/нечисловой ключ — «нет
 *     проверки» (len > undefined/NaN → false), null — 0 (len > null),
 *     bool — 0/1, числовая строка — Number(строка); в текстах ошибок лимит
 *     подставляется raw-значением (null → "null"), как в JS-шаблоне;
 *   - длина строк — UTF-16 code units (value.length), не байты/codepoints;
 *   - порядок Object.values — JS own-property enumeration (целочисленные ключи
 *     первыми): при нескольких одновременных нарушениях inspectValue бросает
 *     ту же ошибку, что и JS;
 *   - Set-сравнение observedId/references — SameValueZero (числа по f64,
 *     -0 === 0; объекты по идентичности);
 *   - отклонения: тексты SyntaxError JSON.parse не воспроизводятся (V8:
 *     "Unexpected token 'o', ..." — здесь anyhow-контекст serde; вердикт
 *     броска совпадает); возврат валидаторов идёт через round-trip JSON
 *     (undefined-поля не выживают — на входе всегда JSON.parse'нутые пейлоады).
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { normalizeRepoPath, sha256 } from './structure-evidence.mjs';
import { defaultRustBin } from './rust-bin.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_PROTOCOL_BIN = defaultRustBin('structure_protocol');

function resolveProtocolBin() {
  if (process.env.AI_REVIEWER_STRUCTURE_PROTOCOL_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_STRUCTURE_PROTOCOL_BIN || '').trim() || DEFAULT_PROTOCOL_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). Ошибка бинаря пробрасывается. */
function runRust(op, args) {
  const bin = resolveProtocolBin();
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

export const STRUCTURE_PROTOCOL_VERSION = 1;
export const STRUCTURE_FACTS_VERSION = STRUCTURE_PROTOCOL_VERSION;
export const STRUCTURE_FACTS_SCHEMA = 'code-structure/structure-facts.v1';
export const STRUCTURE_ASSESSMENT_SCHEMA = 'code-structure/structure-assessment.v1';
export const OBLIGATIONS_SCHEMA = 'code-structure/obligations-export.v1';
export const OBLIGATIONS_EXPORT_SCHEMA = OBLIGATIONS_SCHEMA;

export const DEFAULT_STRUCTURE_LIMITS = Object.freeze({
  maxBytes: 20 * 1024 * 1024,
  maxEntities: 5000,
  maxEdges: 15000,
  maxContracts: 5000,
  maxBoundaries: 5000,
  maxTests: 10000,
  maxObligations: 10000,
  maxPathLength: 1000,
  maxStringLength: 10000,
  maxDepth: 30,
});

export function assertPayloadSize(text, limits = DEFAULT_STRUCTURE_LIMITS) {
  const r = runRust('assertPayloadSize', { text, limits });
  if (r !== null) return r;
  return assertPayloadSizeJs(text, limits);
}

export function validateStructureFacts(payload, limits = DEFAULT_STRUCTURE_LIMITS) {
  const r = runRust('validateStructureFacts', { payload, limits });
  if (r !== null) return r;
  return validateStructureFactsJs(payload, limits);
}

export function validateObligationsExport(payload, limits = DEFAULT_STRUCTURE_LIMITS) {
  const r = runRust('validateObligationsExport', { payload, limits });
  if (r !== null) return r;
  return validateObligationsExportJs(payload, limits);
}

export function validateStructureAssessment(payload, limits = DEFAULT_STRUCTURE_LIMITS) {
  const r = runRust('validateStructureAssessment', { payload, limits });
  if (r !== null) return r;
  return validateStructureAssessmentJs(payload, limits);
}

export function parseStructurePayload(text, limits = DEFAULT_STRUCTURE_LIMITS) {
  const r = runRust('parseStructurePayload', { text, limits });
  if (r !== null) return r;
  return parseStructurePayloadJs(text, limits);
}

export function factsFingerprint(input) {
  const r = runRust('factsFingerprint', { ...input });
  if (r !== null) return r;
  return factsFingerprintJs(input);
}

export function assessmentFingerprint(input) {
  const r = runRust('assessmentFingerprint', { ...input });
  if (r !== null) return r;
  return assessmentFingerprintJs(input);
}

export function temporalAlignment(input) {
  const r = runRust('temporalAlignment', { ...input });
  if (r !== null) return r;
  return temporalAlignmentJs(input);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/structure-protocol.mjs
// до Фазы 2). Используется только когда Rust-бинарь отсутствует; при битом бинаре
// НЕ подменяет его.
// ---------------------------------------------------------------------------

function assertPayloadSizeJs(text, limits = DEFAULT_STRUCTURE_LIMITS) {
  const bytes = Buffer.byteLength(String(text || ''));
  if (bytes > limits.maxBytes) throw new Error(`structure payload exceeds ${limits.maxBytes} bytes`);
  return bytes;
}

function inspectValueJs(value, limits, depth = 0) {
  if (depth > limits.maxDepth) throw new Error(`structure payload nesting exceeds ${limits.maxDepth}`);
  if (typeof value === 'string' && value.length > limits.maxStringLength) {
    throw new Error(`structure payload string exceeds ${limits.maxStringLength} characters`);
  }
  if (Array.isArray(value)) {
    for (const item of value) inspectValueJs(item, limits, depth + 1);
  } else if (value && typeof value === 'object') {
    for (const item of Object.values(value)) inspectValueJs(item, limits, depth + 1);
  }
}

function assertKindJs(payload, kind, schemaRef) {
  if (!payload || typeof payload !== 'object' || Array.isArray(payload)) throw new Error(`${kind} payload must be an object`);
  if (payload.version !== STRUCTURE_PROTOCOL_VERSION) throw new Error(`unsupported ${kind} version: ${payload.version}`);
  if (payload.kind !== kind) throw new Error(`expected kind=${kind}`);
  if (payload.schemaRef !== schemaRef) throw new Error(`expected schemaRef=${schemaRef}`);
}

function assertPathsJs(records, pathField = 'path') {
  for (const record of records || []) {
    if (record?.[pathField] != null) normalizeRepoPath(record[pathField]);
  }
}

/**
 * Ссылочная целостность фактов: ребро или контракт не может указывать на сущность, которой
 * в наборе нет. Оба продюсера строят рёбра только из уцелевших сущностей, поэтому проверка
 * ничего не стоит на здоровом входе — но слияние SCIP с graphify однажды уже оставляло
 * висячие `from` после того, как сущность выбывала из набора, и артефакт уезжал потребителю
 * молча: до этой проверки валидатор смотрел только на массивы, лимиты, пути и fingerprint.
 */
function assertReferentialIntegrityJs(facts) {
  const known = new Set((facts.entities || []).map((entity) => entity?.observedId));
  const check = (ref, what) => {
    if (ref != null && ref !== '' && !known.has(ref)) {
      throw new Error(`${what} references unknown entity: ${JSON.stringify(String(ref))}`);
    }
  };
  for (const dependency of facts.dependencies || []) {
    check(dependency?.from, 'dependency.from');
    check(dependency?.to, 'dependency.to');
  }
  for (const contract of facts.contracts || []) {
    check(contract?.entityRef, 'contract.entityRef');
    check(contract?.targetRef, 'contract.targetRef');
  }
}

function validateStructureFactsJs(payload, limits = DEFAULT_STRUCTURE_LIMITS) {
  assertKindJs(payload, 'structure_facts', STRUCTURE_FACTS_SCHEMA);
  inspectValueJs(payload, limits);
  const facts = payload.facts || {};
  if (!Array.isArray(facts.boundaries) || !Array.isArray(facts.entities)
    || !Array.isArray(facts.contracts) || !Array.isArray(facts.dependencies)
    || !Array.isArray(facts.testCases)) {
    throw new Error('structure facts arrays are required');
  }
  if (facts.entities.length > limits.maxEntities) throw new Error('entity limit exceeded');
  if (facts.dependencies.length > limits.maxEdges) throw new Error('dependency limit exceeded');
  // потолки contracts/boundaries есть в schemas/structure-facts.v1.schema.json,
  // но раньше их не проверял никто — продюсер мог выдать факты, невалидные по своей же схеме
  if (facts.contracts.length > limits.maxContracts) throw new Error('contract limit exceeded');
  if (facts.boundaries.length > limits.maxBoundaries) throw new Error('boundary limit exceeded');
  if (facts.testCases.length > limits.maxTests) throw new Error('test case limit exceeded');
  assertPathsJs(facts.entities);
  assertPathsJs(facts.testCases);
  assertReferentialIntegrityJs(facts);
  for (const boundary of facts.boundaries) {
    for (const path of boundary.paths || []) {
      if (normalizeRepoPath(path).length > limits.maxPathLength) throw new Error('path length limit exceeded');
    }
  }
  if (!/^[0-9a-f]{64}$/.test(String(payload.fingerprint || ''))) throw new Error('facts fingerprint must be sha256');
  return payload;
}

function validateObligationsExportJs(payload, limits = DEFAULT_STRUCTURE_LIMITS) {
  assertKindJs(payload, 'obligations_export', OBLIGATIONS_SCHEMA);
  inspectValueJs(payload, limits);
  if (!Array.isArray(payload.obligations)) throw new Error('obligations array is required');
  if (payload.obligations.length > limits.maxObligations) throw new Error('obligation limit exceeded');
  for (const obligation of payload.obligations) {
    if (!obligation?.id || !obligation?.contentHash || !obligation?.statement) {
      throw new Error('obligation id, contentHash and statement are required');
    }
  }
  return payload;
}

function validateStructureAssessmentJs(payload, limits = DEFAULT_STRUCTURE_LIMITS) {
  assertKindJs(payload, 'structure_assessment', STRUCTURE_ASSESSMENT_SCHEMA);
  inspectValueJs(payload, limits);
  for (const field of ['fingerprint', 'factsFingerprint', 'coverageHash', 'thresholdsHash']) {
    if (!/^[0-9a-f]{64}$/.test(String(payload[field] || ''))) throw new Error(`${field} must be sha256`);
  }
  if (!['aligned', 'mismatch', 'unknown'].includes(payload.temporalAlignment)) {
    throw new Error('invalid temporalAlignment');
  }
  if (!Array.isArray(payload.coverageObservations)
    || !Array.isArray(payload.assessments?.contractScenarios)
    || !Array.isArray(payload.hints)
    || !Array.isArray(payload.receipts)) {
    throw new Error('assessment arrays are required');
  }
  return payload;
}

function parseStructurePayloadJs(text, limits = DEFAULT_STRUCTURE_LIMITS) {
  assertPayloadSizeJs(text, limits);
  const payload = JSON.parse(text);
  if (payload?.kind === 'structure_facts') return validateStructureFactsJs(payload, limits);
  if (payload?.kind === 'structure_assessment') return validateStructureAssessmentJs(payload, limits);
  if (payload?.kind === 'obligations_export') return validateObligationsExportJs(payload, limits);
  throw new Error(`unsupported structure payload kind: ${payload?.kind}`);
}

function factsFingerprintJs({ headSha, providerVersion, configHash, scope }) {
  return sha256(['structure_facts', headSha, providerVersion, configHash, scope || ''].join('\0'));
}

function assessmentFingerprintJs({ facts, obligationsRevision, coverageHash, thresholdsHash }) {
  return sha256([
    'structure_assessment',
    facts,
    obligationsRevision || 'none',
    coverageHash,
    thresholdsHash,
  ].join('\0'));
}

function temporalAlignmentJs({ factsHeadSha, testHeadSha, obligationsHeadSha }) {
  const facts = String(factsHeadSha || '');
  const test = String(testHeadSha || '');
  const docs = String(obligationsHeadSha || '');
  if (!facts || !test) return 'unknown';
  if (facts !== test) return 'mismatch';
  if (docs && docs !== facts) return 'mismatch';
  return 'aligned';
}
