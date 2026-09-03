/**
 * Parity-прогон Фазы 2, шаг 3: Rust structure_mutation ↔ чистый JS fallback
 * lib/structure-mutation.mjs.
 *
 * Запуск: node rust/parity/structure-mutation.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/structure-mutation.mjs: resolveMutationBin()
 * читает AI_REVIEWER_STRUCTURE_MUTATION_FORCE_JS на каждый вызов, поэтому
 * js-сторона прогоняется с FORCE_JS=1, rust-сторона — без него. Результаты
 * сравниваются после JSON round-trip.
 *
 * Корпус (векторы из test/structure.test.mjs + edge-cases):
 *   - привязка к вмещающему контракту по location (guards line/endLine,
 *     blind-мутант: один кандидат файла vs несколько, tie-дистанция → первый
 *     контракт, entity.line строка 'abc' — guard пропускается, дистанция NaN);
 *   - Number(location.start?.line || mutant.line || 0) || null: строка '0' → null,
 *     ' 5 '/'1e2' → числа, mutant.line fallback, endLine из start.line,
 *     '1e999' → Infinity (JSON.stringify → null);
 *   - статусы MUTANT_STATUS + lowercase-fallback ('killed', число, missing);
 *   - killedBy: killedByTests → killedBy fallback, Set-дедуп по raw, строка
 *     итерируется посимвольно, объекты не дедуплицируются ('[object Object]'),
 *     побайтовая сортировка после map(String);
 *   - replacement/mutator: null → null, число/бул → String, type-fallback;
 *   - warnings по unsafe-путям (порядок следования), files как массив,
 *     files не-object → пусто, facts/report null/undefined;
 *   - scopeContractRefs: фильтр по разрешённым контрактам, пустой скоуп;
 *   - contract.id falsy → subjectRef file:path, confidence exact;
 *   - stableId: id 0 → '0', id null → '', совпадающие id сосуществуют;
 *   - duplicate observedId — последний выигрывает (JS Map);
 *   - criticalContractRefs: must+confirmed фильтр, highFanIn contractRef/entityRef,
 *     god-ноды, дедуп через источники, filter(Boolean), дефолтный Array.sort
 *     (по String: [10, 9] → [10, 9]), contractRefs не-массивом оборачивается,
 *     entityRef === observedId — строгое равенство (undefined === undefined).
 */

import assert from 'node:assert/strict';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_STRUCTURE_MUTATION_FORCE_JS = '1';
const M = await import('../../lib/structure-mutation.mjs');

let compared = 0;
const failures = [];

/** JSON round-trip как в проде: undefined отсутствуют, Infinity/NaN → null. */
const rt = (v) => JSON.parse(JSON.stringify(v));

function compare(label, jsResult, rustResult) {
  compared += 1;
  try {
    assert.deepEqual(rustResult, jsResult);
  } catch (e) {
    failures.push({
      label,
      js: JSON.stringify(jsResult)?.slice(0, 600),
      rust: JSON.stringify(rustResult)?.slice(0, 600),
    });
  }
}

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_STRUCTURE_MUTATION_FORCE_JS = '1';
  try {
    return { ok: true, value: rt(fn(...args)) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_STRUCTURE_MUTATION_FORCE_JS;
  }
};

const runRust = (fn, ...args) => {
  delete process.env.AI_REVIEWER_STRUCTURE_MUTATION_FORCE_JS;
  try {
    return { ok: true, value: rt(fn(...args)) };
  } catch (e) {
    const message = String(e?.message || e);
    const m = message.match(/"error":\s*"((?:[^"\\]|\\.)*)"/);
    return { ok: false, error: m ? JSON.parse(`"${m[1]}"`) : message };
  }
};

function check(label, fn, ...args) {
  const js = runJs(fn, ...args);
  const rust = runRust(fn, ...args);
  if (js.ok !== rust.ok) {
    compared += 1;
    failures.push({ label, js: JSON.stringify(js)?.slice(0, 400), rust: JSON.stringify(rust)?.slice(0, 400) });
    return;
  }
  compare(label, js.value ?? js.error, rust.value ?? rust.error);
}

// ---------------------------------------------------------------------------
// Корпус: parseMutationReport
// ---------------------------------------------------------------------------

const FACTS = {
  entities: [
    { observedId: 'e1', path: 'src/a.ts', line: 10, endLine: 20 },
    { observedId: 'e2', path: 'src/a.ts', line: 30, endLine: 40 },
    { observedId: 'e3', path: 'src/b.ts', line: 5, endLine: 8 },
    { observedId: 'e4', path: 'src/a.ts', line: 'abc' },
  ],
  contracts: [
    { id: 'c1', entityRef: 'e1' },
    { id: 'c2', entityRef: 'e2' },
    { id: 'c3', entityRef: 'e3' },
    { id: 'c4', entityRef: 'e4' },
  ],
};

const MUTANT = (over = {}) => ({
  id: 'm1',
  mutatorName: 'BooleanLiteral',
  status: 'Killed',
  location: { start: { line: 12 }, end: { line: 12 } },
  ...over,
});

check('parse: killed-мутант привязан к вмещающему контракту', M.parseMutationReport, {
  files: { 'src/a.ts': { mutants: [MUTANT({ killedByTests: ['t/b', 't/a', 't/a'] })] } },
}, FACTS);

check('parse: survived по строке 35 → c2, replacement строкой', M.parseMutationReport, {
  files: { 'src/a.ts': { mutants: [MUTANT({
    status: 'Survived', replacement: 'false', location: { start: { line: 35 } },
  })] } },
}, FACTS);

check('parse: NoCoverage, endLine падает на start.line', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({ status: 'NoCoverage', location: { start: { line: 7 } } })] } },
}, FACTS);

check('parse: все статусы MUTANT_STATUS + lowercase/число/missing', M.parseMutationReport, {
  files: { 'src/a.ts': { mutants: [
    MUTANT({ id: 's1', status: 'Timeout' }),
    MUTANT({ id: 's2', status: 'CompileError' }),
    MUTANT({ id: 's3', status: 'RuntimeError' }),
    MUTANT({ id: 's4', status: 'Ignored' }),
    MUTANT({ id: 's5', status: 'killed' }),
    MUTANT({ id: 's6', status: 5 }),
    MUTANT({ id: 's7' }),
  ] } },
}, FACTS);

check('parse: guards — строка 25 между контрактами, привязки нет', M.parseMutationReport, {
  files: { 'src/a.ts': { mutants: [MUTANT({ location: { start: { line: 25 } } })] } },
}, FACTS);

check('parse: guard по entity.line — мутант до контракта', M.parseMutationReport, {
  files: { 'src/a.ts': { mutants: [MUTANT({ id: 'g1', location: { start: { line: 5 } } })] } },
}, FACTS);

check('parse: entity.line строка abc — guard пропускается, дистанция NaN', M.parseMutationReport, {
  files: { 'src/a.ts': { mutants: [MUTANT({ id: 'n1', location: { start: { line: 33 } } })] } },
}, FACTS);

check('parse: blind-мутант, два кандидата — привязки нет', M.parseMutationReport, {
  files: { 'src/a.ts': { mutants: [MUTANT({ id: 'b1', location: null })] } },
}, FACTS);

check('parse: blind-мутант, единственный контракт файла — привязка есть', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({ id: 'b2', location: null })] } },
}, FACTS);

check('parse: line-семантика Number: "0" → null, " 5 " → 5, "1e2" → 100, 1e999 → Infinity→null',
  M.parseMutationReport, {
    files: { 'src/b.ts': { mutants: [
      MUTANT({ id: 'z1', location: { start: { line: '0' } } }),
      MUTANT({ id: 'z2', location: null, line: ' 5 ' }),
      MUTANT({ id: 'z3', location: { start: { line: '1e2' } } }),
      MUTANT({ id: 'z4', location: { start: { line: '1e999' } } }),
    ] } },
  }, FACTS);

check('parse: mutant.line fallback без location', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({ id: 'f1', location: null, line: 6 })] } },
}, FACTS);

check('parse: killedBy — дедуп, байтовая сортировка, микс типов', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({
    id: 'k1', killedByTests: ['b', 1, 'a', true, 'a', 10],
  })] } },
}, FACTS);

check('parse: killedByTests-строка итерируется посимвольно', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({ id: 'k2', killedByTests: 'ab' })] } },
}, FACTS);

check('parse: fallback на killedBy, объекты не дедуплицируются', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({ id: 'k3', killedBy: [{}, {}, 'x'] })] } },
}, FACTS);

check('parse: replacement null/missing/число/булево', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [
    MUTANT({ id: 'r1', replacement: null }),
    MUTANT({ id: 'r2' }),
    MUTANT({ id: 'r3', replacement: 0 }),
    MUTANT({ id: 'r4', replacement: false }),
  ] } },
}, FACTS);

check('parse: mutatorName falsy → type → unknown', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [
    MUTANT({ id: 'u1', mutatorName: 0, type: 'StringLiteral' }),
    MUTANT({ id: 'u2', mutatorName: '', type: '' }),
  ] } },
}, FACTS);

check('parse: warnings по unsafe-путям + валидный файл рядом', M.parseMutationReport, {
  files: {
    '../evil.ts': { mutants: [MUTANT({ id: 'w1' })] },
    '/abs.ts': { mutants: [MUTANT({ id: 'w2' })] },
    '': { mutants: [MUTANT({ id: 'w3' })] },
    'src/b.ts': { mutants: [MUTANT({ id: 'w4', location: { start: { line: 6 } } })] },
  },
}, FACTS);

check('parse: files как массив — ключи-индексы', M.parseMutationReport, {
  files: [{ mutants: [MUTANT({ id: 'a1' })] }, null, { mutants: [] }],
}, FACTS);

check('parse: files не-object → пусто; report/facts null/undefined', M.parseMutationReport, {
  files: 'broken',
}, null);
check('parse: report undefined', M.parseMutationReport, undefined, FACTS);
check('parse: report без files', M.parseMutationReport, { schemaVersion: '1.0' }, FACTS);
check('parse: file null, mutants пустые/отсутствующие', M.parseMutationReport, {
  files: { 'src/b.ts': null, 'src/a.ts': {} },
}, FACTS);

check('parse: scopeContractRefs фильтрует чужие контракты', M.parseMutationReport, {
  files: {
    'src/a.ts': { mutants: [MUTANT({ id: 'q1', location: { start: { line: 12 } } })] },
    'src/b.ts': { mutants: [MUTANT({ id: 'q2', location: null })] },
  },
}, FACTS, { scopeContractRefs: ['c1'] });
check('parse: пустой scope — фильтра нет', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({ id: 'q3', location: null })] } },
}, FACTS, { scopeContractRefs: [] });

check('parse: contract.id falsy → subjectRef file:path, confidence exact', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({ id: 'i1', location: { start: { line: 6 } } })] } },
}, {
  entities: [{ observedId: 'e9', path: 'src/b.ts', line: 5, endLine: 8 }],
  contracts: [{ id: '', entityRef: 'e9' }],
});

check('parse: entity.path не-строка — привязки нет', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({ id: 'i2', location: { start: { line: 6 } } })] } },
}, {
  entities: [{ observedId: 'e9', path: 5, line: 5, endLine: 8 }],
  contracts: [{ id: 'c9', entityRef: 'e9' }],
});

check('parse: stableId — id 0 → "0", id null → "", дубли id сосуществуют', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [
    MUTANT({ id: 0, location: { start: { line: 6 } } }),
    MUTANT({ id: null, location: { start: { line: 6 } } }),
  ] } },
}, FACTS);

check('parse: duplicate observedId — последняя entity выигрывает', M.parseMutationReport, {
  files: { 'src/b.ts': { mutants: [MUTANT({ id: 'd1', location: { start: { line: 6 } } })] } },
}, {
  entities: [
    { observedId: 'e9', path: 'src/elsewhere.ts', line: 1, endLine: 2 },
    { observedId: 'e9', path: 'src/b.ts', line: 5, endLine: 8 },
  ],
  contracts: [{ id: 'c9', entityRef: 'e9' }],
});

check('parse: сортировка наблюдений по id через несколько файлов', M.parseMutationReport, {
  files: {
    'src/b.ts': { mutants: [MUTANT({ id: 'x1' }), MUTANT({ id: 'x2' })] },
    'src/a.ts': { mutants: [MUTANT({ id: 'x3', location: { start: { line: 12 } } })] },
  },
}, FACTS);

// ---------------------------------------------------------------------------
// Корпус: criticalContractRefs
// ---------------------------------------------------------------------------

const FACTS_GOD = {
  entities: [
    { observedId: 'g1', isGodNode: true },
    { observedId: 'g2', isGodNode: false },
  ],
  contracts: [
    { id: 'god-c', entityRef: 'g1' },
    { id: 'plain-c', entityRef: 'g2' },
  ],
};
const OBLIGATIONS = [
  { status: 'confirmed', priority: 'must', contractRefs: ['must-a', 'must-b'] },
  { status: 'confirmed', priority: 'should', contractRefs: ['skip-1'] },
  { status: 'draft', priority: 'must', contractRefs: ['skip-2'] },
  { status: 'confirmed', priority: 'must' },
  { status: 'confirmed', priority: 'must', contractRefs: 'wrap-me' },
];
const RISK = { highFanIn: [{ contractRef: 'hot-1' }, { entityRef: 'hot-2' }, {}] };

check('refs: must + highFanIn + god-ноды, дедуп и сортировка', M.criticalContractRefs,
  FACTS_GOD, RISK, OBLIGATIONS);
check('refs: только обязательства', M.criticalContractRefs, null, null, OBLIGATIONS);
check('refs: дефолтный Array.sort по String — [10, 9] → [10, 9]', M.criticalContractRefs,
  null, null, [{ status: 'confirmed', priority: 'must', contractRefs: [10, 9] }]);
check('refs: микс типов сортируется по String', M.criticalContractRefs,
  null, null, [{ status: 'confirmed', priority: 'must', contractRefs: ['b', 1, 'a'] }]);
check('refs: пустой hotspot без refs — filter(Boolean) отбрасывает', M.criticalContractRefs,
  null, { highFanIn: [null, 0, ''] }, []);
check('refs: без аргументов — пусто', M.criticalContractRefs, undefined, undefined, undefined);
check('refs: entityRef === observedId, оба отсутствуют — строгое равенство',
  M.criticalContractRefs,
  { entities: [{ isGodNode: true }], contracts: [{ id: 'ghost-c' }, { id: 'x', entityRef: null }] },
  null, []);
check('refs: isGodNode truthy-строка', M.criticalContractRefs,
  { entities: [{ observedId: 'g1', isGodNode: 'yes' }], contracts: [{ id: 'god-c', entityRef: 'g1' }] },
  null, []);

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
