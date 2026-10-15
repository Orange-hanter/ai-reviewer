/**
 * Фаза 2, шаг 2: lib/structure-tests.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/structure_tests (порт экстрактора тест-кейсов, слияния
 * кейсов, драфтов property-тестов и парсера property-отчётов).
 *
 * Публичное API не изменилось (те же экспорты: extractTestCasesFromSource/
 * mergeTestCases/generatePropertyDrafts/parsePropertyReport синхронно).
 * Синхронные функции вызывают бинарь через execFileSync: JSON {"op", ...args}
 * на stdin, ответ — JSON в формате JS-возврата.
 *
 * Резолвинг бинаря (по аналогии с lib/structure-lcov.mjs):
 *   AI_REVIEWER_STRUCTURE_TESTS_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/structure_tests;
 *   AI_REVIEWER_STRUCTURE_TESTS_FORCE_JS=1 — принудительно чистый JS (паритет/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm test/eval
 * идут раньше cargo build, см. .github/workflows/ci.yml), обёртка деградирует на чистый
 * JS fallback ниже — эталонную копию исходной реализации. Если бинарь ЕСТЬ, но упал
 * (non-zero exit) — ошибка пробрасывается, fallback не используется: молчаливая
 * подмена реализации при битом Rust-коде скрывала бы регрессии.
 *
 * Контрактные стыки:
 *   - mergeTestCases вариативен (...groups): обёртка шлёт {groups: [...groups]},
 *     бинарь принимает ровно этот формат;
 *   - parsePropertyReport(report, drafts): report может быть undefined — обёртка
 *     опускает ключ, бинарь трактует отсутствие как null.
 *
 * Нюансы паритета (проверены parity-прогоном rust/parity/structure-tests.parity.mjs):
 *   - TEST_DECLARATION_RE /\b(?:it|test|specify)\s*\(\s*(['"`])(.+?)\1/g ручным
 *     сканером (regex-крейт без backreference): кавычка на offset 0 имени НЕ
 *     закрывающая — `it('')` матчится с именем "'" как в JS;
 *   - body теста — до начала следующей декларации либо min(len, start+6000);
 *   - \s — полный JS-класс (включает U+FEFF), LineTerminator обрывает имя;
 *   - контрактные матчеры: label || entityLabel || entity.label по entityRef,
 *     normalizedLabel, case-insensitive \b…\b;
 *   - merge: {...previous, ...testCase} + Set-union targetContractRefs/evidence
 *     (побайтовая сортировка) и дедуп assertions по kind;
 *   - parsePropertyReport: stale по semanticHash, seed/shrinkPath → String,
 *     numRuns/discardCount — Number(x || 0) (NaN → null).
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { stableId } from './structure-evidence.mjs';
import { defaultRustBin } from './rust-bin.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_TESTS_BIN = defaultRustBin('structure_tests');

function resolveTestsBin() {
  if (process.env.AI_REVIEWER_STRUCTURE_TESTS_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_STRUCTURE_TESTS_BIN || '').trim() || DEFAULT_TESTS_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). Ошибка бинаря пробрасывается. */
function runRust(op, args) {
  const bin = resolveTestsBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

/** Сканирует исходник на it/test/specify-декларации, привязывает контракты и assertion-паттерны. */
export function extractTestCasesFromSource(path, source, contracts = [], entities = []) {
  const r = runRust('extractTestCasesFromSource', {
    path: String(path || ''),
    source: String(source || ''),
    contracts: contracts || [],
    entities: entities || [],
  });
  if (r !== null) return r;
  return extractTestCasesFromSourceJs(path, source, contracts, entities);
}

/** Сливает кейсы из разных источников по id (вариативен: mergeTestCases(a, b, ...)). */
export function mergeTestCases(...groups) {
  const r = runRust('mergeTestCases', { groups });
  if (r !== null) return r;
  return mergeTestCasesJs(...groups);
}

/** Строит драфты property-тестов из подтверждённых boundary_value/invariant/error_path-обязательств. */
export function generatePropertyDrafts(obligations) {
  const r = runRust('generatePropertyDrafts', { obligations: obligations || [] });
  if (r !== null) return r;
  return generatePropertyDraftsJs(obligations);
}

/** Сверяет результаты property-прогона с драфтами (stale по semanticHash). */
export function parsePropertyReport(report, drafts = []) {
  const r = runRust('parsePropertyReport', { report: report ?? null, drafts: drafts || [] });
  if (r !== null) return r;
  return parsePropertyReportJs(report, drafts);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/structure-tests.mjs
// до Фазы 2). Используется только когда Rust-бинарь отсутствует; при битом бинаре
// НЕ подменяет его.
// ---------------------------------------------------------------------------

const TEST_DECLARATION_RE = /\b(?:it|test|specify)\s*\(\s*(['"`])(.+?)\1/g;
const ASSERTION_PATTERNS = [
  ['throws', /\b(?:toThrow|throws|assert\.throws)\b/],
  ['rejects', /\b(?:rejects|assert\.rejects)\b/],
  ['http_status', /\b(?:status|statusCode|toHaveStatus)\s*(?:\(|[=:])\s*(?:4\d\d|5\d\d)/],
  ['equality', /\b(?:toEqual|toBe|deepEqual|strictEqual|assert\.equal)\b/],
  ['property', /\b(?:fc\.assert|property|forall|forAll)\b/],
];

function frameworkForPathJs(path, source) {
  if (/\bvitest\b/.test(source)) return 'vitest';
  if (/\bjest\b/.test(source)) return 'jest';
  if (/\bnode:test\b/.test(source)) return 'node:test';
  if (/\.spec\./.test(path)) return 'unknown-spec';
  return 'unknown-test';
}

function lineAtJs(source, offset) {
  return source.slice(0, offset).split('\n').length;
}

function assertionsInJs(source) {
  return ASSERTION_PATTERNS
    .filter(([, pattern]) => pattern.test(source))
    .map(([kind]) => ({ kind, confidence: 'heuristic' }));
}

function normalizedLabelJs(value) {
  return String(value || '').replace(/\(\)+$/, '').replace(/^\./, '').toLowerCase();
}

function extractTestCasesFromSourceJs(path, source, contracts = [], entities = []) {
  // label контракта может отсутствовать (SCIP-контракты: entityRef/kind/signature) —
  // выводим из сущности по entityRef
  const labelByEntityRef = new Map((entities || []).map((entity) => [entity.observedId, entity.label]));
  // Матчеры компилируются один раз на контракт, а не на каждую пару тест x контракт: на
  // большом репозитории это были миллионы компиляций одного и того же выражения. Флага 'g'
  // нет, поэтому у RegExp не остаётся состояния и экземпляр безопасно переиспользовать.
  const contractMatchers = [];
  for (const contract of contracts || []) {
    const label = normalizedLabelJs(
      contract.label || contract.entityLabel || labelByEntityRef.get(contract.entityRef) || '',
    );
    if (!label) continue;
    contractMatchers.push({
      id: contract.id,
      pattern: new RegExp(`\\b${label.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\b`, 'i'),
    });
  }
  const out = [];
  TEST_DECLARATION_RE.lastIndex = 0;
  let match;
  while ((match = TEST_DECLARATION_RE.exec(source))) {
    const start = match.index;
    const next = source.slice(TEST_DECLARATION_RE.lastIndex).search(TEST_DECLARATION_RE);
    const end = next >= 0 ? TEST_DECLARATION_RE.lastIndex + next : Math.min(source.length, start + 6000);
    const body = source.slice(start, end);
    const targetContractRefs = [];
    for (const matcher of contractMatchers) {
      if (matcher.pattern.test(body)) targetContractRefs.push(matcher.id);
    }
    const name = match[2].trim();
    const line = lineAtJs(source, start);
    out.push({
      id: stableId('test', path, line, name),
      name,
      path,
      line,
      framework: frameworkForPathJs(path, source),
      targetContractRefs: [...new Set(targetContractRefs)].sort(),
      assertions: assertionsInJs(body),
      confidence: 'heuristic',
      evidence: [`source:test-declaration:${path}:${line}`],
    });
  }
  return out;
}

function mergeTestCasesJs(...groups) {
  const byId = new Map();
  for (const testCase of groups.flat()) {
    const previous = byId.get(testCase.id);
    if (!previous) {
      byId.set(testCase.id, testCase);
      continue;
    }
    byId.set(testCase.id, {
      ...previous,
      ...testCase,
      targetContractRefs: [...new Set([
        ...(previous.targetContractRefs || []),
        ...(testCase.targetContractRefs || []),
      ])].sort(),
      assertions: [...(previous.assertions || []), ...(testCase.assertions || [])]
        .filter((item, index, all) => all.findIndex((candidate) => candidate.kind === item.kind) === index),
      evidence: [...new Set([...(previous.evidence || []), ...(testCase.evidence || [])])].sort(),
    });
  }
  return [...byId.values()].sort((a, b) => a.path.localeCompare(b.path) || a.line - b.line || a.name.localeCompare(b.name));
}

function generatePropertyDraftsJs(obligations) {
  return (obligations || [])
    .filter((obligation) => obligation.status === 'confirmed'
      && ['boundary_value', 'invariant', 'error_path'].includes(obligation.kind))
    .map((obligation) => {
      const generators = obligation.kind === 'boundary_value'
        ? ['minimum', 'maximum', 'below_minimum', 'above_maximum', 'empty']
        : obligation.kind === 'error_path'
          ? ['invalid', 'empty', 'malformed']
          : ['valid_domain'];
      return {
        id: stableId('property', obligation.id, obligation.contentHash),
        obligationRef: obligation.id,
        status: 'draft',
        kind: obligation.kind,
        statement: obligation.statement,
        preconditions: [],
        generators,
        invariant: obligation.statement,
        expectedFailures: obligation.kind === 'error_path' ? ['invalid_input_rejected'] : [],
        semanticHash: obligation.contentHash,
        requiresConfirmation: true,
        evidence: [`obligation:${obligation.id}:${obligation.contentHash}`],
      };
    })
    .sort((a, b) => a.id.localeCompare(b.id));
}

function parsePropertyReportJs(report, drafts = []) {
  const byId = new Map(drafts.map((draft) => [draft.id, draft]));
  return (report?.properties || []).map((property) => {
    const draft = byId.get(property.propertyId);
    const stale = !draft || draft.semanticHash !== property.semanticHash;
    return {
      propertyId: property.propertyId,
      obligationRef: draft?.obligationRef || property.obligationRef || null,
      status: stale ? 'stale' : (property.failed ? 'failed' : 'passed'),
      numRuns: Number(property.numRuns || 0),
      discardCount: Number(property.discardCount || 0),
      seed: property.seed == null ? null : String(property.seed),
      shrinkPath: property.shrinkPath == null ? null : String(property.shrinkPath),
      counterexample: property.counterexample ?? null,
      semanticHash: property.semanticHash || '',
      confidence: stale ? 'unresolved' : 'exact',
      evidence: [`property-report:${property.propertyId}`],
    };
  }).sort((a, b) => a.propertyId.localeCompare(b.propertyId));
}
