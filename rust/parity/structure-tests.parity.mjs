/**
 * Parity-прогон Фазы 2, шаг 2: Rust structure_tests ↔ чистый JS fallback
 * lib/structure-tests.mjs.
 *
 * Запуск: node rust/parity/structure-tests.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/structure-tests.mjs: resolveTestsBin() читает
 * AI_REVIEWER_STRUCTURE_TESTS_FORCE_JS на каждый вызов, поэтому js-сторона
 * прогоняется с FORCE_JS=1, rust-сторона — без него. Результаты сравниваются
 * после JSON round-trip.
 *
 * Корпус (векторы из test/structure.test.mjs + edge-cases):
 *   - it/test/specify с кавычками ' " `, пустое имя it('') → имя "'", многострочные
 *     имена (LineTerminator обрывает матч), keyword внутри слова (contest/await),
 *     \s* между keyword/(/кавычкой включая новую строку и U+FEFF;
 *   - framework detection: vitest → jest → node:test по source, .spec. по пути;
 *   - пять assertion-паттернов (throws/rejects/http_status/equality/property),
 *     http_status через ( = :;
 *   - привязка контрактов по label || entityLabel || entity.label по entityRef
 *     (SCIP-контракты без label), normalizedLabel (хвостовые "()", leading '.',
 *     lowercase), case-insensitive \b…\b, дедуп и сортировка refs;
 *   - body теста — до следующей декларации (label в соседнем теле не цепляется)
 *     и 6000-кап последнего тела;
 *   - mergeTestCases: {...previous, ...testCase}, Set-union refs/evidence,
 *     дедуп assertions по kind, итоговая сортировка path/line/name;
 *   - generatePropertyDrafts: только confirmed boundary_value/invariant/error_path,
 *     generators/expectedFailures по kind, сорт по id;
 *   - parsePropertyReport: stale по semanticHash/отсутствию драфта, fallback
 *     obligationRef, seed/shrinkPath → String, numRuns NaN → null.
 */

import assert from 'node:assert/strict';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_STRUCTURE_TESTS_FORCE_JS = '1';
const S = await import('../../lib/structure-tests.mjs');

let compared = 0;
const failures = [];

/** JSON round-trip как в проде: undefined отсутствуют, ключи — insertion order. */
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
  process.env.AI_REVIEWER_STRUCTURE_TESTS_FORCE_JS = '1';
  try {
    return { ok: true, value: rt(fn(...args)) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_STRUCTURE_TESTS_FORCE_JS;
  }
};

const runRust = (fn, ...args) => {
  delete process.env.AI_REVIEWER_STRUCTURE_TESTS_FORCE_JS;
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
// Корпус: extractTestCasesFromSource
// ---------------------------------------------------------------------------

const SRC_BASIC = `import { describe, it, test } from 'vitest';
it('does auth', async () => {
  expect(() => login()).toThrow();
  await expect(refresh()).rejects.toThrow();
});
test("checks status", () => {
  expect(res.status).toBe(404);
  assert.deepEqual(body, { ok: false });
});
specify(\`runs property\`, () => {
  fc.assert(fc.property(arb, (x) => x >= 0));
});
`;

check('extract: базовый vitest-файл, три декларации, assertions', S.extractTestCasesFromSource, 'test/a.test.ts', SRC_BASIC, [], []);
check('extract: пустой исходник', S.extractTestCasesFromSource, 'test/a.test.ts', '', [], []);
check('extract: null-контракты/сущности по умолчанию', S.extractTestCasesFromSource, 'test/a.test.ts', "it('x', () => {});");

check('extract: пустое имя it("") — имя из кавычки', S.extractTestCasesFromSource, 'a.test.ts', "it('')", [], []);
check('extract: it(") — имя из двойной кавычки', S.extractTestCasesFromSource, 'a.test.ts', 'it(")', [], []);
check('extract: многострочное имя обрывается — декларации нет', S.extractTestCasesFromSource, 'a.test.ts', "it('multi\nline', () => {});", [], []);
check('extract: keyword внутри слова не матчится', S.extractTestCasesFromSource, 'a.test.ts', 'contest("nope", () => {});\nawait("nope");\nit("yes", () => {});', [], []);
check('extract: \\s* включает новую строку и несколько пробелов', S.extractTestCasesFromSource, 'a.test.ts', "it\n  (\n\t 'spaced' , () => {});", [], []);
check('extract: backtick-имя', S.extractTestCasesFromSource, 'a.test.ts', 'it(`tick`, () => {});', [], []);
check('extract: смешанные кавычки не закрывают имя', S.extractTestCasesFromSource, 'a.test.ts', 'it("has \'single\' inside", () => {});', [], []);
check('extract: specify с \\r в имени обрывается', S.extractTestCasesFromSource, 'a.test.ts', "specify('a\rb', () => {});", [], []);

const SRC_FRAMEWORKS_JEST = `/** jest-environment jsdom */
test('x', () => { expect(1).toBe(1); });
`;
const SRC_FRAMEWORKS_NODE = `import { test } from 'node:test';
test('x', () => {});
`;
check('extract: framework jest по слову в комментарии', S.extractTestCasesFromSource, 'test/a.test.ts', SRC_FRAMEWORKS_JEST, [], []);
check('extract: framework node:test', S.extractTestCasesFromSource, 'test/a.test.ts', SRC_FRAMEWORKS_NODE, [], []);
check('extract: framework unknown-spec по пути', S.extractTestCasesFromSource, 'src/a.spec.ts', "it('x', () => {});", [], []);
check('extract: framework unknown-test', S.extractTestCasesFromSource, 'src/a.ts', "it('x', () => {});", [], []);
check('extract: vitest побеждает jest', S.extractTestCasesFromSource, 'a.test.ts', '// vitest and jest\nit(\'x\', () => {});', [], []);

check('extract: http_status через ( = и :', S.extractTestCasesFromSource, 'a.test.ts', [
  "it('a', () => { expect(status(500)); });",
  "it('b', () => { expect(statusCode = 401); });",
  "it('c', () => { assert(res.status: 403); });",
  "it('d', () => { expect(toHaveStatus(200)); });",
].join('\n'), [], []);
check('extract: throws/rejects/equality/property', S.extractTestCasesFromSource, 'a.test.ts', [
  "it('t', () => assert.throws(fn));",
  "it('r', () => assert.rejects(p));",
  "it('e', () => strictEqual(a, b));",
  "it('p', () => forall(arb, f));",
].join('\n'), [], []);
check('extract: граница \\b — toThrowXYZ не throws', S.extractTestCasesFromSource, 'a.test.ts', "it('x', () => expect(fn).toThrowXYZ());", [], []);

const CONTRACTS = [
  { id: 'c-auth', label: 'AuthService.login()' },
  { id: 'c-bare', label: '.Normalize' },
  { id: 'c-entity', entityLabel: 'parseConfig' },
  { id: 'c-nolabel', kind: 'signature', entityRef: 'scip:1' },
];
const ENTITIES = [
  { observedId: 'scip:1', label: 'fromScipLabel' },
  { observedId: 'scip:2', label: 'unused' },
];

const SRC_CONTRACTS = [
  "it('calls login wrapper', () => {",
  '  const svc = new AuthService();',
  '  svc.login();',
  '});',
  "it('normalize lowercases', () => {",
  '  expect(normalize(\" A \")).toBe(\"a\");',
  '});',
  "it('parses config', () => {",
  '  const cfg = parseConfig(raw);',
  '  expect(cfg.fromScipLabel).toBeTruthy();',
  '});',
  "it('mentions authorizer but not login', () => {",
  '  expect(authorizer).toBeDefined();',
  '});',
].join('\n');

check('extract: привязка контрактов label/entityLabel/entityRef', S.extractTestCasesFromSource,
  'test/a.test.ts', SRC_CONTRACTS, CONTRACTS, ENTITIES);
check('extract: привязка без entities — c-nolabel без label пропускается', S.extractTestCasesFromSource,
  'test/a.test.ts', SRC_CONTRACTS, CONTRACTS);
check('extract: пустой список контрактов', S.extractTestCasesFromSource,
  'test/a.test.ts', SRC_CONTRACTS, [], ENTITIES);

const LONG_TAIL = `${'// filler\n'.repeat(700)}mentionAuthServiceLogin`;
const SRC_6000 = `it('first', () => {
  expect(a).toBe(1);
});
it('last', () => {
${'// pad\n'.repeat(400)}AuthService.login();
});`;
check('extract: body последнего теста ограничен 6000', S.extractTestCasesFromSource,
  'a.test.ts', `it('last', () => {\n${LONG_TAIL}\n});`, CONTRACTS, []);

const SRC_BODY_BOUNDARY = `it('one', () => {
  helper();
});
it('two', () => {
  parseConfig({});
});
it('three', () => {
  nothing();
});`;
check('extract: label в соседнем теле не цепляется', S.extractTestCasesFromSource,
  'a.test.ts', SRC_BODY_BOUNDARY, CONTRACTS, []);

check('extract: дубли refs схлопываются и сортируются', S.extractTestCasesFromSource,
  'a.test.ts', "it('x', () => { login(); login(); });", [
    { id: 'z-last', label: 'login' },
    { id: 'a-first', label: 'login()' },
  ], []);

// ---------------------------------------------------------------------------
// Корпус: mergeTestCases
// ---------------------------------------------------------------------------

const CASE_A = {
  id: 't:1', name: 'same', path: 'a.test.ts', line: 5, framework: 'vitest',
  targetContractRefs: ['c2', 'c1'], assertions: [{ kind: 'throws', confidence: 'heuristic' }],
  confidence: 'heuristic', evidence: ['e:b'],
};
const CASE_B = {
  id: 't:1', name: 'same', path: 'a.test.ts', line: 5, framework: 'jest',
  targetContractRefs: ['c1', 'c3'], assertions: [
    { kind: 'throws', confidence: 'exact' },
    { kind: 'equality', confidence: 'heuristic' },
  ],
  confidence: 'extracted', evidence: ['e:a'], extraField: 'wins',
};
const CASE_C = {
  id: 't:2', name: 'other', path: 'a.test.ts', line: 2, framework: 'jest',
  targetContractRefs: [], assertions: [], confidence: 'heuristic', evidence: ['e:c'],
};

check('merge: перекрытие полей, union refs/evidence, дедуп assertions', S.mergeTestCases,
  [CASE_A, CASE_C], [CASE_B]);
check('merge: одиночная группа без дублей', S.mergeTestCases, [CASE_A]);
check('merge: пустые группы', S.mergeTestCases, [], []);
check('merge: три группы, порядок по path/line/name', S.mergeTestCases,
  [{ id: 'x', name: 'b', path: 'b.test.ts', line: 1 }, { id: 'y', name: 'a', path: 'a.test.ts', line: 9 }],
  [{ id: 'z', name: 'a', path: 'a.test.ts', line: 2 }],
  [{ id: 'x', name: 'b-over', path: 'b.test.ts', line: 1 }]);
check('merge: кейс без assertions/refs/evidence', S.mergeTestCases,
  [{ id: 'x', name: 'a', path: 'p', line: 1 }], [{ id: 'x', name: 'a', path: 'p', line: 1 }]);

// ---------------------------------------------------------------------------
// Корпус: generatePropertyDrafts
// ---------------------------------------------------------------------------

const OBLIGATIONS = [
  { id: 'ob:1', status: 'confirmed', kind: 'boundary_value', statement: 'n in [0, 10]', contentHash: 'h1' },
  { id: 'ob:2', status: 'confirmed', kind: 'invariant', statement: 'sum preserved', contentHash: 'h2' },
  { id: 'ob:3', status: 'confirmed', kind: 'error_path', statement: 'rejects null', contentHash: 'h3' },
  { id: 'ob:4', status: 'draft', kind: 'boundary_value', statement: 'not confirmed', contentHash: 'h4' },
  { id: 'ob:5', status: 'confirmed', kind: 'entrypoint', statement: 'not a property kind', contentHash: 'h5' },
];

check('drafts: фильтр confirmed + kind, generators по kind', S.generatePropertyDrafts, OBLIGATIONS);
check('drafts: undefined obligations', S.generatePropertyDrafts, undefined);
check('drafts: пустой массив', S.generatePropertyDrafts, []);
check('drafts: сортировка по id', S.generatePropertyDrafts, [
  { id: 'ob:b', status: 'confirmed', kind: 'invariant', statement: 's', contentHash: 'hb' },
  { id: 'ob:a', status: 'confirmed', kind: 'invariant', statement: 's', contentHash: 'ha' },
]);

// ---------------------------------------------------------------------------
// Корпус: parsePropertyReport
// ---------------------------------------------------------------------------

const DRAFTS = [
  {
    id: 'prop:1', obligationRef: 'ob:1', semanticHash: 'h1', status: 'draft', kind: 'boundary_value',
  },
  {
    id: 'prop:2', obligationRef: 'ob:2', semanticHash: 'h2-old', status: 'draft', kind: 'invariant',
  },
];
const REPORT = {
  properties: [
    { propertyId: 'prop:2', semanticHash: 'h2', numRuns: 100, failed: false, seed: 42, shrinkPath: null },
    { propertyId: 'prop:1', semanticHash: 'h1', numRuns: 50, failed: true, seed: 'abc', shrinkPath: [1, 2], counterexample: { x: -1 } },
    { propertyId: 'prop:9', semanticHash: 'h9', numRuns: 10, obligationRef: 'ob:orphan' },
    { propertyId: 'prop:0', semanticHash: 'h1', numRuns: 'oops', discardCount: '3' },
  ],
};

check('report: stale/fresh/orphan/NaN-numRuns', S.parsePropertyReport, REPORT, DRAFTS);
check('report: без драфтов — всё stale', S.parsePropertyReport, REPORT, []);
check('report: undefined report', S.parsePropertyReport, undefined, DRAFTS);
check('report: null properties', S.parsePropertyReport, { properties: null }, DRAFTS);
check('report: drafts по умолчанию', S.parsePropertyReport, REPORT);

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
