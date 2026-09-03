/**
 * Parity-прогон Фазы 2, шаг 4: Rust structure_protocol ↔ чистый JS fallback
 * lib/structure-protocol.mjs (ручная валидация structure-пейлоадов + фингерпринты).
 *
 * Запуск: node rust/parity/structure-protocol.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/structure-protocol.mjs: resolveProtocolBin()
 * читает AI_REVIEWER_STRUCTURE_PROTOCOL_FORCE_JS на каждый вызов, поэтому
 * js-сторона прогоняется с FORCE_JS=1, rust-сторона — без него. Сравниваются
 * вердикты (ok/throw) и тексты ошибок ПОСИМВОЛЬНО (доменные ошибки валидатора);
 * успешные возвраты — через assert.deepEqual после JSON round-trip.
 *
 * Корпус:
 *   - канонические fixtures/structure/*.json — все три валидатора + parse;
 *   - битые варианты: неверный schemaRef/version/kind, не-объект payload,
 *     отсутствующие массивы facts, превышение каждого лимита (entities,
 *     dependencies, contracts, boundaries, testCases, obligations), висячие
 *     ссылки referential integrity (dependency.from/to, contract.entityRef/
 *     targetRef), небезопасные пути, длинные пути границ, битые fingerprint'ы,
 *     temporalAlignment вне enum, отсутствующие массивы assessment;
 *   - лимиты: дефолтные, частичные (отсутствующий ключ — «нет проверки»),
 *     null → 0, строковый/нечисловой лимит, maxBytes через parseStructurePayload;
 *   - inspectValue: строка длиной в UTF-16 code units (astral-символы),
 *     вложенность > maxDepth, порядок own-property enumeration
 *     (целочисленные ключи первыми → та же первая ошибка);
 *   - factsFingerprint/assessmentFingerprint — побайтовое равенство hex
 *     (в т.ч. scope/obligationsRevision falsy → ''/'none');
 *   - temporalAlignment — все комбинации aligned/mismatch/unknown;
 *   - assertPayloadSize — UTF-8 байты (не code units), текст ошибки с raw-лимитом.
 *
 * Отклонения (сверяется только вердикт, тексты сознательно не совпадают):
 *   - синтаксически битый JSON в parseStructurePayload: V8 «Unexpected token
 *     'o', "nope" is not valid JSON» vs anyhow-контекст serde — обе стороны
 *     бросают, текст не сравнивается (как в json_repair).
 */

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_STRUCTURE_PROTOCOL_FORCE_JS = '1';
const P = await import('../../lib/structure-protocol.mjs');
const { sha256 } = await import('../../lib/util.mjs');

let compared = 0;
const failures = [];

function fail(label, js, rust) {
  failures.push({ label, js: String(js)?.slice(0, 400), rust: String(rust)?.slice(0, 400) });
}

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_STRUCTURE_PROTOCOL_FORCE_JS = '1';
  try {
    return { ok: true, value: fn(...args) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_STRUCTURE_PROTOCOL_FORCE_JS;
  }
};

const runRust = (fn, ...args) => {
  delete process.env.AI_REVIEWER_STRUCTURE_PROTOCOL_FORCE_JS;
  try {
    return { ok: true, value: fn(...args) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  }
};

/** Полный паритет: вердикт, текст ошибки посимвольно, успех — deepEqual после round-trip. */
function check(label, fn, ...args) {
  const js = runJs(fn, ...args);
  const rust = runRust(fn, ...args);
  compared += 1;
  if (js.ok !== rust.ok) {
    failures.push({ label, js: JSON.stringify(js)?.slice(0, 400), rust: JSON.stringify(rust)?.slice(0, 400) });
    return;
  }
  if (!js.ok) {
    if (js.error !== rust.error) {
      failures.push({ label, js: js.error, rust: rust.error });
    }
    return;
  }
  try {
    assert.deepEqual(JSON.parse(JSON.stringify(rust.value)), JSON.parse(JSON.stringify(js.value)));
  } catch {
    failures.push({
      label,
      js: JSON.stringify(js.value)?.slice(0, 500),
      rust: JSON.stringify(rust.value)?.slice(0, 500),
    });
  }
}

/** Только вердикт (отклонённые случаи — тексты ошибок намеренно разные). */
function checkVerdictOnly(label, fn, ...args) {
  const js = runJs(fn, ...args);
  const rust = runRust(fn, ...args);
  compared += 1;
  if (js.ok !== rust.ok) {
    failures.push({ label, js: JSON.stringify(js)?.slice(0, 400), rust: JSON.stringify(rust)?.slice(0, 400) });
  }
}

// ---------------------------------------------------------------------------
// Канонические fixtures + битые варианты
// ---------------------------------------------------------------------------

const FACTS = JSON.parse(readFileSync(resolve(REPO, 'fixtures/structure/structure-facts.v1.json'), 'utf8'));
const OBLIGATIONS = JSON.parse(readFileSync(resolve(REPO, 'fixtures/structure/obligations-export.v1.json'), 'utf8'));
const ASSESSMENT = JSON.parse(readFileSync(resolve(REPO, 'fixtures/structure/structure-assessment.v1.json'), 'utf8'));

check('facts: каноническая fixture', P.validateStructureFacts, FACTS);
check('obligations: каноническая fixture', P.validateObligationsExport, OBLIGATIONS);
check('assessment: каноническая fixture', P.validateStructureAssessment, ASSESSMENT);

const clone = (v) => JSON.parse(JSON.stringify(v));

// --- assertKind ветки ---
{
  const bad = clone(FACTS);
  bad.schemaRef = 'nope';
  check('facts: неверный schemaRef', P.validateStructureFacts, bad);
}
{
  const bad = clone(FACTS);
  bad.version = 2;
  check('facts: version=2', P.validateStructureFacts, bad);
}
{
  const bad = clone(FACTS);
  delete bad.version;
  check('facts: version отсутствует', P.validateStructureFacts, bad);
}
{
  const bad = clone(FACTS);
  bad.version = '1'; // строка !== число
  check('facts: version="1"', P.validateStructureFacts, bad);
}
{
  const bad = clone(FACTS);
  bad.kind = 'mystery';
  check('facts: неверный kind', P.validateStructureFacts, bad);
}
check('facts: payload null', P.validateStructureFacts, null);
check('facts: payload массив', P.validateStructureFacts, [1, 2]);
check('facts: payload строка', P.validateStructureFacts, 'nope');
check('obligations: payload число', P.validateObligationsExport, 42);
{
  const bad = clone(OBLIGATIONS);
  bad.kind = 'obligations-export';
  check('obligations: неверный kind', P.validateObligationsExport, bad);
}
{
  const bad = clone(ASSESSMENT);
  bad.schemaRef = 'code-structure/structure-facts.v1';
  check('assessment: чужой schemaRef', P.validateStructureAssessment, bad);
}

// --- facts: массивы и лимиты ---
{
  const bad = clone(FACTS);
  bad.facts = { entities: [], contracts: [], dependencies: [], testCases: [] };
  check('facts: нет boundaries', P.validateStructureFacts, bad);
}
{
  const bad = clone(FACTS);
  bad.facts.entities = {};
  check('facts: entities не массив', P.validateStructureFacts, bad);
}
{
  const entity = {
    observedId: 'entity:one', lineageId: 'entity:one', label: 'a', kind: 'function',
    path: 'src/a.ts', confidence: 'extracted', evidence: [], externalIds: {},
  };
  const base = clone(FACTS);
  base.facts = {
    ...base.facts,
    boundaries: [],
    entities: [entity],
    contracts: [],
    dependencies: [],
    testCases: [],
  };
  check('facts: entities=1 под дефолтом ок', P.validateStructureFacts, base);
  check('facts: entities=1 лимит 0', P.validateStructureFacts, base, { maxEntities: 0 });
  check('facts: entities=1 лимит "0" (строка)', P.validateStructureFacts, base, { maxEntities: '0' });
  check('facts: entities=1 лимит null → 0', P.validateStructureFacts, base, { maxEntities: null });
  check('facts: entities=1 лимит "nope" → NaN, нет проверки', P.validateStructureFacts, base, { maxEntities: 'nope' });
  check('facts: entities=1 частичные лимиты без maxEntities', P.validateStructureFacts, base, { maxEdges: 1 });
  // лимиты по каждому массиву
  const over = (field, items) => {
    const bad = clone(base);
    bad.facts[field] = items;
    return bad;
  };
  check('facts: entity limit exceeded', P.validateStructureFacts,
    over('entities', Array.from({ length: 5001 }, (_, i) => ({ ...entity, observedId: `e${i}`, lineageId: `e${i}` }))));
  check('facts: dependency limit exceeded', P.validateStructureFacts,
    over('dependencies', Array.from({ length: 15001 }, (_, i) => ({
      id: `d${i}`, from: 'entity:one', to: 'entity:one', relation: 'calls',
      confidence: 'extracted', evidence: [],
    }))));
  check('facts: contract limit exceeded', P.validateStructureFacts,
    over('contracts', Array.from({ length: 5001 }, (_, i) => ({
      id: `c${i}`, entityRef: 'entity:one', kind: 'entrypoint', confidence: 'extracted', evidence: [],
    }))));
  check('facts: boundary limit exceeded', P.validateStructureFacts,
    over('boundaries', Array.from({ length: 5001 }, (_, i) => ({
      id: `b${i}`, kind: 'module', name: `m${i}`, paths: [`m${i}`],
    }))));
  check('facts: test case limit exceeded', P.validateStructureFacts,
    over('testCases', Array.from({ length: 10001 }, (_, i) => ({
      id: `t${i}`, name: `t${i}`, path: 'src/a.test.ts', line: 1, confidence: 'extracted', evidence: [],
    }))));
  // dependency limit раньше entities? нет: entities проверяется первым
  const both = clone(base);
  both.facts.entities = Array.from({ length: 5001 }, (_, i) => ({ ...entity, observedId: `e${i}`, lineageId: `e${i}` }));
  both.facts.dependencies = Array.from({ length: 15001 }, (_, i) => ({
    id: `d${i}`, from: 'e0', to: 'e0', relation: 'calls', confidence: 'extracted', evidence: [],
  }));
  check('facts: при двух нарушениях — entity first', P.validateStructureFacts, both);
}

// --- facts: пути, ссылочная целостность, fingerprint ---
{
  const entity = {
    observedId: 'known', lineageId: 'known', label: 'a', kind: 'function',
    path: 'src/a.ts', confidence: 'extracted', evidence: [], externalIds: {},
  };
  const base = clone(FACTS);
  base.facts = { boundaries: [], entities: [entity], contracts: [], dependencies: [], testCases: [] };

  const badPath = clone(base);
  badPath.facts.entities[0].path = '../evil';
  check('facts: небезопасный путь entity', P.validateStructureFacts, badPath);

  const badTestPath = clone(base);
  badTestPath.facts.testCases = [{
    id: 't1', name: 't', path: '/abs', line: 1, confidence: 'extracted', evidence: [],
  }];
  check('facts: абсолютный путь testCase', P.validateStructureFacts, badTestPath);

  const badFp = clone(base);
  badFp.fingerprint = 'ff';
  check('facts: fingerprint короткий', P.validateStructureFacts, badFp);
  const badFp2 = clone(base);
  badFp2.fingerprint = sha256('x').toUpperCase();
  check('facts: fingerprint uppercase', P.validateStructureFacts, badFp2);

  // referential integrity
  const edgeToGone = clone(base);
  edgeToGone.facts.dependencies = [{
    id: 'e1', from: 'known', to: 'gone', relation: 'calls', confidence: 'extracted', evidence: [],
  }];
  check('facts: dependency.to → gone', P.validateStructureFacts, edgeToGone);
  const edgeFromGone = clone(base);
  edgeFromGone.facts.dependencies = [{
    id: 'e1', from: 'gone', to: 'known', relation: 'calls', confidence: 'extracted', evidence: [],
  }];
  check('facts: dependency.from → gone', P.validateStructureFacts, edgeFromGone);
  const contractRefGone = clone(base);
  contractRefGone.facts.contracts = [{
    id: 'c1', entityRef: 'gone', kind: 'export', confidence: 'extracted', evidence: [],
  }];
  check('facts: contract.entityRef → gone', P.validateStructureFacts, contractRefGone);
  const contractTargetGone = clone(base);
  contractTargetGone.facts.contracts = [{
    id: 'c1', entityRef: 'known', kind: 'implements', targetRef: 'gone',
    confidence: 'extracted', evidence: [],
  }];
  check('facts: contract.targetRef → gone', P.validateStructureFacts, contractTargetGone);
  const okRefs = clone(contractTargetGone);
  okRefs.facts.contracts[0].targetRef = 'known';
  okRefs.facts.dependencies = [{
    id: 'e1', from: 'known', to: 'known', relation: 'calls', confidence: 'extracted', evidence: [],
  }];
  check('facts: ссылки сходятся — ок', P.validateStructureFacts, okRefs);
  // нестроковый observedId / реф (SameValueZero по числам)
  const numeric = clone(base);
  numeric.facts.entities[0].observedId = 7;
  numeric.facts.dependencies = [{
    id: 'e1', from: 7, to: 7, relation: 'calls', confidence: 'extracted', evidence: [],
  }];
  check('facts: числовые observedId (SameValueZero)', P.validateStructureFacts, numeric);
  const numericGone = clone(numeric);
  numericGone.facts.dependencies[0].to = 8;
  check('facts: числовой реф мимо', P.validateStructureFacts, numericGone);
}

// --- facts: длина путей границ ---
{
  const base = clone(FACTS);
  base.facts = {
    ...base.facts, boundaries: [], entities: [], contracts: [], dependencies: [], testCases: [],
  };
  const long = clone(base);
  long.facts.boundaries = [{ id: 'b1', kind: 'module', name: 'm', paths: ['x'.repeat(1001)] }];
  check('facts: путь границы 1001', P.validateStructureFacts, long);
  const exact = clone(base);
  exact.facts.boundaries = [{ id: 'b1', kind: 'module', name: 'm', paths: ['x'.repeat(1000)] }];
  check('facts: путь границы ровно 1000 — ок', P.validateStructureFacts, exact);
  // не-ASCII: UTF-16 code units, а не байты/символы
  const wide = clone(base);
  wide.facts.boundaries = [{ id: 'b1', kind: 'module', name: 'm', paths: ['😀'.repeat(501)] }];
  check('facts: astral-путь 1002 code units', P.validateStructureFacts, wide);
  const nullBoundary = clone(base);
  nullBoundary.facts.boundaries = [null];
  check('facts: boundary=null (V8 TypeError)', P.validateStructureFacts, nullBoundary);
}

// --- inspectValue: строки, вложенность, порядок ключей ---
{
  const base = clone(FACTS);
  base.facts = {
    ...base.facts, boundaries: [], entities: [{
      observedId: 'e1', lineageId: 'e1', label: 'a', kind: 'function',
      path: 'src/a.ts', confidence: 'extracted', evidence: [], externalIds: {},
    }], contracts: [], dependencies: [], testCases: [],
  };
  const longLabel = clone(base);
  longLabel.facts.entities[0].label = 'a'.repeat(10001);
  check('facts: строка 10001', P.validateStructureFacts, longLabel);
  const astralLabel = clone(base);
  astralLabel.facts.entities[0].label = '😀'.repeat(5001); // 10002 UTF-16 units
  check('facts: astral-строка 10002 code units', P.validateStructureFacts, astralLabel);
  const okLabel = clone(base);
  okLabel.facts.entities[0].label = '😀'.repeat(5000); // ровно 10000
  check('facts: astral-строка ровно 10000 — ок', P.validateStructureFacts, okLabel);

  const deep = clone(base);
  let cursor = deep.facts.entities[0].externalIds;
  for (let i = 0; i < 27; i += 1) {
    cursor.x = {};
    cursor = cursor.x;
  }
  check('facts: вложенность 31 > maxDepth', P.validateStructureFacts, deep);

  // own-property enumeration: целочисленный ключ "2" бросится раньше "b"
  const keys = clone(base);
  keys.facts.entities[0].externalIds = {
    b: 'x'.repeat(20000),
    2: 'y'.repeat(20000),
    a: 'z'.repeat(20000),
  };
  check('facts: integer-like ключи первыми', P.validateStructureFacts, keys);
}

// --- obligations ---
{
  const bad = clone(OBLIGATIONS);
  delete bad.obligations;
  check('obligations: массив отсутствует', P.validateObligationsExport, bad);
}
{
  const ok = clone(OBLIGATIONS);
  check('obligations: пустой массив ок', P.validateObligationsExport, ok, { maxObligations: 0 });
}
{
  const many = clone(OBLIGATIONS);
  many.obligations = Array.from({ length: 10001 }, (_, i) => ({
    id: `o${i}`, contentHash: sha256(`o${i}`), statement: 's',
  }));
  check('obligations: 10001 — limit exceeded', P.validateObligationsExport, many);
  const exact = clone(many);
  exact.obligations = exact.obligations.slice(0, 10000);
  check('obligations: ровно 10000 — ок', P.validateObligationsExport, exact);
}
for (const broken of [
  { contentHash: sha256('x'), statement: 's' },
  { id: '', contentHash: sha256('x'), statement: 's' },
  { id: 'o1', statement: 's' },
  { id: 'o1', contentHash: sha256('x') },
  { id: 'o1', contentHash: 0, statement: 's' },
  { id: 'o1', contentHash: sha256('x'), statement: null },
]) {
  const bad = clone(OBLIGATIONS);
  bad.obligations = [broken];
  check(`obligations: битое ${JSON.stringify(broken).slice(0, 60)}`, P.validateObligationsExport, bad);
}

// --- assessment ---
for (const field of ['fingerprint', 'factsFingerprint', 'coverageHash', 'thresholdsHash']) {
  const bad = clone(ASSESSMENT);
  bad[field] = 'ff';
  check(`assessment: битый ${field}`, P.validateStructureAssessment, bad);
}
{
  const bad = clone(ASSESSMENT);
  bad.temporalAlignment = 'bogus';
  check('assessment: temporalAlignment вне enum', P.validateStructureAssessment, bad);
}
for (const [k, v] of [
  ['coverageObservations', {}],
  ['assessments', null],
  ['assessments', { contractScenarios: 'no' }],
  ['hints', 'no'],
  ['receipts', 42],
]) {
  const bad = clone(ASSESSMENT);
  bad[k] = v;
  check(`assessment: битый ${k}`, P.validateStructureAssessment, bad);
}

// ---------------------------------------------------------------------------
// parseStructurePayload
// ---------------------------------------------------------------------------

const FACTS_TEXT = readFileSync(resolve(REPO, 'fixtures/structure/structure-facts.v1.json'), 'utf8');
const OBLIGATIONS_TEXT = readFileSync(resolve(REPO, 'fixtures/structure/obligations-export.v1.json'), 'utf8');
const ASSESSMENT_TEXT = readFileSync(resolve(REPO, 'fixtures/structure/structure-assessment.v1.json'), 'utf8');

check('parse: facts fixture', P.parseStructurePayload, FACTS_TEXT);
check('parse: obligations fixture', P.parseStructurePayload, OBLIGATIONS_TEXT);
check('parse: assessment fixture', P.parseStructurePayload, ASSESSMENT_TEXT);
check('parse: unsupported kind', P.parseStructurePayload, '{"kind":"mystery"}');
check('parse: JSON null', P.parseStructurePayload, 'null');
check('parse: JSON число', P.parseStructurePayload, '42');
checkVerdictOnly('parse: пустая строка — только вердикт (V8 "Unexpected end of JSON input")', P.parseStructurePayload, '');
check('parse: maxBytes мал', P.parseStructurePayload, FACTS_TEXT, { maxBytes: 100 });
check('parse: maxBytes с запасом', P.parseStructurePayload, FACTS_TEXT, { maxBytes: 1024 * 1024 });
check('parse: maxBytes null → 0', P.parseStructurePayload, '{"kind":"x"}', { maxBytes: null });
checkVerdictOnly('parse: битый JSON — только вердикт (V8/serde тексты расходятся)', P.parseStructurePayload, 'nope');
checkVerdictOnly('parse: оборванный JSON — только вердикт', P.parseStructurePayload, '{"kind": ');

// ---------------------------------------------------------------------------
// assertPayloadSize
// ---------------------------------------------------------------------------

check('size: пустая строка → 0', P.assertPayloadSize, '');
check('size: UTF-8 байты', P.assertPayloadSize, 'é😀'); // 6 байт, 3 code units
check('size: лимит ниже', P.assertPayloadSize, '123456', { maxBytes: 5 });
check('size: лимит null → 0', P.assertPayloadSize, 'x', { maxBytes: null });
check('size: лимит строка "5"', P.assertPayloadSize, '123456', { maxBytes: '5' });
check('size: лимит нечисловой — нет проверки', P.assertPayloadSize, '123456', { maxBytes: 'nope' });
check('size: text=null → 0 байт', P.assertPayloadSize, null);

// ---------------------------------------------------------------------------
// Фингерпринты и temporalAlignment
// ---------------------------------------------------------------------------

const FP_INPUT = {
  headSha: 'abcdef1234567890',
  providerVersion: '1.0.0',
  configHash: sha256('config'),
  scope: 'src',
};
const pinnedFactsFp = `facts:${sha256(['structure_facts', FP_INPUT.headSha, FP_INPUT.providerVersion, FP_INPUT.configHash, 'src'].join('\0'))}`;
console.log(`pinned factsFingerprint: ${pinnedFactsFp}`);

check('factsFingerprint: полный ввод', P.factsFingerprint, FP_INPUT);
check('factsFingerprint: scope=0 → ""', P.factsFingerprint, { ...FP_INPUT, scope: 0 });
check('factsFingerprint: scope="" → ""', P.factsFingerprint, { ...FP_INPUT, scope: '' });
check('factsFingerprint: scope=null → ""', P.factsFingerprint, {
  headSha: FP_INPUT.headSha, providerVersion: FP_INPUT.providerVersion, configHash: FP_INPUT.configHash,
});
check('factsFingerprint: поля null → ""', P.factsFingerprint, {
  headSha: null, providerVersion: null, configHash: null, scope: null,
});

const AP_INPUT = {
  facts: 'factsfp', obligationsRevision: 'rev1', coverageHash: 'covhash', thresholdsHash: 'thhash',
};
const pinnedAssessmentFp = `assessment:${sha256(['structure_assessment', 'factsfp', 'rev1', 'covhash', 'thhash'].join('\0'))}`;
console.log(`pinned assessmentFingerprint: ${pinnedAssessmentFp}`);

check('assessmentFingerprint: полный ввод', P.assessmentFingerprint, AP_INPUT);
check('assessmentFingerprint: obligationsRevision отсутствует → none', P.assessmentFingerprint, {
  facts: 'factsfp', coverageHash: 'covhash', thresholdsHash: 'thhash',
});
check('assessmentFingerprint: obligationsRevision="" → none', P.assessmentFingerprint, { ...AP_INPUT, obligationsRevision: '' });
check('assessmentFingerprint: obligationsRevision=0 → none', P.assessmentFingerprint, { ...AP_INPUT, obligationsRevision: 0 });
check('assessmentFingerprint: facts=null → ""', P.assessmentFingerprint, {
  obligationsRevision: 'rev1', coverageHash: 'covhash', thresholdsHash: 'thhash',
});

for (const [i, input] of [
  { factsHeadSha: 's1', testHeadSha: 's1', obligationsHeadSha: '' },
  { factsHeadSha: 's1', testHeadSha: 's1', obligationsHeadSha: 's1' },
  { factsHeadSha: 's1', testHeadSha: 's2', obligationsHeadSha: '' },
  { factsHeadSha: 's1', testHeadSha: 's1', obligationsHeadSha: 's2' },
  { factsHeadSha: '', testHeadSha: 's1', obligationsHeadSha: '' },
  { factsHeadSha: 's1', testHeadSha: '', obligationsHeadSha: '' },
  { factsHeadSha: null, testHeadSha: 's1', obligationsHeadSha: '' },
  { factsHeadSha: 0, testHeadSha: 's1', obligationsHeadSha: '' },
  { factsHeadSha: 's1', testHeadSha: 's1', obligationsHeadSha: null },
  {},
].entries()) {
  check(`temporalAlignment[${i}] ${JSON.stringify(input)}`, P.temporalAlignment, input);
}

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

console.log(`parity cases compared: ${compared}`);
if (failures.length) {
  console.log(`FAILURES: ${failures.length}`);
  for (const f of failures.slice(0, 20)) {
    console.log('---', f.label);
    console.log('  js  :', f.js);
    console.log('  rust:', f.rust);
  }
  process.exit(1);
}
console.log('PARITY OK');
