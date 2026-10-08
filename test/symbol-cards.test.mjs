// Карточки символов (RFC 14, шаг 2): извлечение вызовов, лексер конца объявления, отбор,
// хэш входа без номеров строк и проводка через пайплайн. Запуск: node --test test/symbol-cards.test.mjs
import './helpers/reference-profile.mjs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const {
  extractUsedIdentifiers, scanDeclaration, findDeclarationEnd, buildSymbolCards, selectChunkCards,
  symbolCardsHashInput, renderSymbolCard, addedLinesOfSection, declarationRegex, createLineReader, packageImportedNames,
  resolveCardNode, hasLocalBinding, symbolCardsOutcome, isAbsenceClaim,
  relativeImportBindings, resolveImportPath, extractNamespaceCalls, cardDeclaration,
} = await import('../lib/symbol-cards.mjs');
const { buildGraphIndex } = await import('../lib/graphify.mjs');
const { backfillMissing } = await import('../lib/config.mjs');
const { fileInputHashes, configFingerprint } = await import('../lib/incremental.mjs');
const { createReviewPipeline, loadReviewConfig } = await import('../index.mjs');

const lines = (s) => s.split('\n');

test('extractUsedIdentifiers: вызовы и new; без методов через точку, ключевых слов, глобалов и литералов', () => {
  const used = extractUsedIdentifiers([
    'const x = parseLimits(env.LIMITS, { strict: true });',
    'if (ok) return new ReviewEngine(cfg).run(items.map(fn));',
    'console.log("call fake(1) in string"); // comment(2)',
    'const y = parseLimits(other) + JSON.stringify(z); assert . ok(y);',
    '  // строка-комментарий: helper()',
    'return compute(a);',
  ]);
  assert.deepEqual([...used.entries()].sort(), [['ReviewEngine', 1], ['compute', 1], ['parseLimits', 2]].sort());
});

test('packageImportedNames: имена из пакетов, но не из файлов репозитория и алиасов', () => {
  const names = packageImportedNames([
    "import { ok, deepEqual as de } from 'node:assert';",
    "import express from 'express';",
    "import * as path from 'node:path';",
    "import { local } from './local.mjs';",
    "import { aliased } from '@/lib/x';",
    "import { scoped } from '@scope/pkg';",
    "const { readFile: rf, stat } = require('fs');",
  ]);
  assert.deepEqual([...names].sort(), ['de', 'express', 'ok', 'path', 'rf', 'scoped', 'stat']);
});

test('разбор текста автора PR не проваливается в сверхлинейный перебор (ReDoS)', () => {
  const n = 100_000;
  const cases = {
    'литерал из обратных слэшей': () => extractUsedIdentifiers([`"${'\\\\'.repeat(5000)}x(1)`]),
    'вызов и пробелы': () => extractUsedIdentifiers([`x${' '.repeat(1990)}`]),
    'точка и пробелы': () => extractUsedIdentifiers([`.${' '.repeat(1990)}a b c`]),
    'пробелы по краям в обход предела длины': () => extractUsedIdentifiers([`${' '.repeat(n)}${'/*'.repeat(900)}${' '.repeat(n)}`]),
    'import и пробелы': () => packageImportedNames([`import a${' '.repeat(n)}x`]),
    'многострочный import без from': () => packageImportedNames(['import {', ...Array(n).fill(' '.repeat(50))]),
    'объявление: пустые строки': () => declarationRegex('foo').test('\n'.repeat(n)),
    'объявление: длинные аргументы': () => declarationRegex('foo').test(`foo(${'a'.repeat(n)}`),
  };
  for (const [label, fn] of Object.entries(cases)) {
    const t0 = performance.now();
    fn();
    assert.ok(performance.now() - t0 < 500, label);
  }
});

test('createLineReader: symlink за пределы корня не читается, обычный файл — читается', async () => {
  const { mkdtemp, mkdir, symlink, writeFile: wf } = await import('node:fs/promises');
  const root = await mkdtemp(join(tmpdir(), 'cards-root-'));
  const outside = await mkdtemp(join(tmpdir(), 'cards-out-'));
  try {
    await mkdir(join(root, 'lib'));
    await wf(join(root, 'lib', 'ok.mjs'), 'export function ok() {}\n');
    await wf(join(outside, 'secret.txt'), 'TOKEN=abc\n');
    await symlink(join(outside, 'secret.txt'), join(root, 'lib', 'leak.mjs'));
    const read = createLineReader(root);
    assert.deepEqual(read('lib/ok.mjs'), ['export function ok() {}', '']);
    assert.equal(read('lib/leak.mjs'), null, 'symlink наружу');
    assert.equal(read('../' + outside.split('/').pop() + '/secret.txt'), null, 'выход через ..');
  } finally {
    await rm(root, { recursive: true, force: true });
    await rm(outside, { recursive: true, force: true });
  }
});

test('addedLinesOfSection: только строки `N: +`', () => {
  const section = '    8:  keep();\n     : -  gone();\n    9: +  added();\n   10: +  more();';
  assert.deepEqual(addedLinesOfSection(section), ['  added();', '  more();']);
});

test('declarationRegex: функция, класс, const, метод, свойство-функция; вызов — не объявление', () => {
  for (const decl of ['function foo(a) {', 'export async function foo() {', 'class foo {', 'const foo = (x) => x',
    '  async foo(a, b) {', '  foo: async (x) => x,', 'let foo = function () {']) {
    assert.ok(declarationRegex('foo').test(decl), decl);
  }
  assert.ok(!declarationRegex('foo').test('const r = foo(1);'));
  assert.ok(!declarationRegex('foo').test('foodle(1)'));
});

test('scanDeclaration: заголовок без тела и строка конца', () => {
  const src = lines([
    'export function parseLimits(raw, { strict = false } = {}) {',
    '  const s = "}";',
    '  const t = `${raw}}`;',
    '  // } в комментарии',
    '  return /[}]/.test(s) ? t : raw;',
    '}',
  ].join('\n'));
  const r = scanDeclaration(src, 0);
  assert.equal(r.header, 'export function parseLimits(raw, { strict = false } = {})');
  assert.equal(r.endIdx, 5);
});

test('scanDeclaration: класс, метод, async, стрелочные с телом и без, цепочка на следующей строке', () => {
  assert.deepEqual(scanDeclaration(lines('class A extends B {\n  m() {}\n}'), 0), { header: 'class A extends B', endIdx: 2 });
  assert.deepEqual(scanDeclaration(lines('  async run(a, b) {\n    return a;\n  }'), 0), { header: 'async run(a, b)', endIdx: 2 });
  assert.deepEqual(scanDeclaration(lines('const f = async (x) => {\n  return x;\n};'), 0), { header: 'const f = async (x) =>', endIdx: 2 });
  assert.deepEqual(scanDeclaration(lines('const g = (x) => x + 1;\nconst h = 2;'), 0), { header: 'const g = (x) =>', endIdx: 0 });
  assert.deepEqual(
    scanDeclaration(lines('const n = (m) => (Array.isArray(m) ? m : [])\n  .reduce((a, b) => a + b, 0);\nnext();'), 0),
    { header: 'const n = (m) =>', endIdx: 1 },
  );
  assert.deepEqual(scanDeclaration(lines('const LIMIT = 40;'), 0), { header: 'const LIMIT = 40', endIdx: 0 });
});

test('scanDeclaration: незакрытый литерал или нет пары за maxLines → endIdx null, заголовок остаётся', () => {
  assert.equal(scanDeclaration(lines('function f() {\n  const s = "oops\n}'), 0).endIdx, null);
  const r = scanDeclaration(lines('function g() {\n  a();\n  b();\n'), 0, { maxLines: 2 });
  assert.equal(r.endIdx, null);
  assert.equal(r.header, 'function g()');
  assert.equal(findDeclarationEnd(['x'], 5), null);
});

test('scanDeclaration: длинный многострочный заголовок обрезается до 3 строк и 160 символов', () => {
  const r = scanDeclaration(lines(`export function veryLong(\n  ${'a'.repeat(100)},\n  ${'b'.repeat(100)},\n  c,\n) {\n}`), 0);
  assert.ok(r.header.length <= 160);
  assert.ok(r.header.endsWith('…'));
  assert.equal(r.endIdx, 5);
});

const GRAPH = buildGraphIndex({
  nodes: [
    { id: 'p', label: 'parseLimits()', source_file: 'lib/config.mjs', source_location: 'L2' },
    { id: 'h1', label: 'helper()', source_file: 'lib/a.mjs', source_location: 'L1' },
    { id: 'h2', label: 'helper()', source_file: 'lib/b.mjs', source_location: 'L1' },
    { id: 's', label: 'staleFn()', source_file: 'lib/stale.mjs', source_location: 'L1' },
    { id: 'py', label: 'pyThing()', source_file: 'tools/x.py', source_location: 'L1' },
    { id: 'own', label: 'localFar()', source_file: 'src/app.ts', source_location: 'L90' },
    { id: 'short', label: 'tiny()', source_file: 'lib/tiny.mjs', source_location: 'L1' },
    // одноимённая с `ok` из node:assert функция репозитория — карточки быть не должно
    { id: 'okn', label: 'ok()', source_file: 'lib/ok.mjs', source_location: 'L1' },
  ],
  links: [],
});

const FILES = {
  'lib/config.mjs': lines('// header\nexport function parseLimits(raw, { strict = false } = {}) {\n  return raw;\n}'),
  'lib/stale.mjs': lines('export function renamed() {}'),
  'lib/ok.mjs': lines('export function ok(x) {}'),
  'lib/tiny.mjs': lines('export function tiny(a) {\n  return a + 1;\n}'),
  'src/app.ts': ["import { ok } from 'node:assert';", ...Array(88).fill('// filler'), 'function localFar(x: number): number {', '  return x;', '}'],
};
const readLines = (p) => FILES[p] ?? null;

function section(added) {
  return added.map((l, i) => `${String(i + 10).padStart(5)}: +${l}`).join('\n');
}

test('buildSymbolCards: каждый фильтр отсекает свой случай, остальное — карточка с диапазоном', () => {
  const sections = new Map([['src/app.ts', section([
    'const a = parseLimits(raw);',
    'const b = helper();',
    'const c = staleFn();',
    'const d = pyThing();',
    'const e = visibleFn();',
    'const f = localFar(1);',
    'const g = unknownFn();',
    'ok(g);',
  ])], ['README.md', section(['parseLimits()'])]]);
  const windows = new Map([['src/app.ts', 'function visibleFn() {\n  return 1;\n}']]);
  const { byPath, metrics } = buildSymbolCards(['src/app.ts', 'README.md'], {
    graphIndex: GRAPH, sections, windows, readLines,
  });
  assert.deepEqual([...byPath.keys()], ['src/app.ts'], 'не JS/TS-файл не разбирается');
  const cards = byPath.get('src/app.ts');
  assert.deepEqual(cards.map((c) => [c.name, c.path, c.line, c.endLine]), [
    ['localFar', 'src/app.ts', 90, 92],
    ['parseLimits', 'lib/config.mjs', 2, 4],
  ]);
  assert.equal(cards.find((c) => c.name === 'parseLimits').declaration, 'export function parseLimits(raw, { strict = false } = {})');
  assert.deepEqual(metrics.skip_reasons, {
    ambiguous: 1, // helper — два узла
    unresolved: 1, // unknownFn — нет узла
    stale_graph: 1,
    non_js_target: 1,
    visible: 1,
    package_import: 1,
  });
  assert.equal(metrics.candidates, 8);
  assert.equal(metrics.cards, 2);
});

test('buildSymbolCards: без графа — карточек нет, причина в метриках', () => {
  const r = buildSymbolCards(['src/app.ts'], { graphIndex: null, sections: new Map(), windows: new Map(), readLines });
  assert.equal(r.byPath.size, 0);
  assert.deepEqual(r.metrics.skip_reasons, { graph_unavailable: 1 });
});

test('buildSymbolCards: короткое тело едет целиком при bodyMaxLines', () => {
  const sections = new Map([['src/app.ts', section(['tiny(1);', 'parseLimits(x);'])]]);
  const off = buildSymbolCards(['src/app.ts'], { graphIndex: GRAPH, sections, windows: new Map(), readLines });
  assert.ok(off.byPath.get('src/app.ts').every((c) => !c.body));
  const on = buildSymbolCards(['src/app.ts'], {
    graphIndex: GRAPH, sections, windows: new Map(), readLines, limits: { bodyMaxLines: 3 },
  });
  const tiny = on.byPath.get('src/app.ts').find((c) => c.name === 'tiny');
  assert.equal(tiny.body, 'export function tiny(a) {\n  return a + 1;\n}');
  assert.match(renderSymbolCard(tiny), /^\/\/ lib\/tiny\.mjs:L1–L3 \(full body\)\n/);
});

test('selectChunkCards: объединение без повторов, приоритет по числу вызовов, лимиты', () => {
  const card = (name, path, line, count) => ({ name, path, line, endLine: line + 2, declaration: `function ${name}()`, count });
  const byPath = new Map([
    ['a.ts', [card('x', 'lib/x.mjs', 1, 1), card('y', 'lib/y.mjs', 5, 3)]],
    ['b.ts', [card('x', 'lib/x.mjs', 1, 4)]],
  ]);
  const all = selectChunkCards(byPath, ['a.ts', 'b.ts']);
  assert.deepEqual(all.cards, ['// lib/x.mjs:L1–L3\nfunction x()', '// lib/y.mjs:L5–L7\nfunction y()']);
  assert.equal(all.dropped, 0);
  const capped = selectChunkCards(byPath, ['a.ts', 'b.ts'], { max: 1 });
  assert.deepEqual(capped.cards, ['// lib/x.mjs:L1–L3\nfunction x()']);
  assert.equal(capped.dropped, 1);
  const byChars = selectChunkCards(byPath, ['a.ts', 'b.ts'], { maxChars: 35 });
  assert.equal(byChars.cards.length, 1);
  assert.equal(byChars.chars, byChars.cards[0].length);
});

test('symbolCardsHashInput: сдвиг строк не меняет вход хэша, смена сигнатуры — меняет', () => {
  const base = [{ path: 'lib/x.mjs', line: 10, endLine: 20, declaration: 'function x(a)' }];
  const shifted = [{ path: 'lib/x.mjs', line: 42, endLine: 60, declaration: 'function x(a)' }];
  const changed = [{ path: 'lib/x.mjs', line: 10, endLine: 20, declaration: 'function x(a, b)' }];
  assert.equal(symbolCardsHashInput(base), symbolCardsHashInput(shifted));
  assert.notEqual(symbolCardsHashInput(base), symbolCardsHashInput(changed));
});

test('fileInputHashes: без карточек хэш прежний, с карточками — другой; сдвиг объявления не инвалидирует', () => {
  const sections = new Map([['a.ts', 'diff']]);
  const ctx = new Map([['a.ts', 'ctx']]);
  const before = fileInputHashes(['a.ts'], sections, ctx);
  assert.deepEqual(fileInputHashes(['a.ts'], sections, ctx, new Map()), before, 'нет карточек у файла — ключа нет');
  const card = (line, declaration) => new Map([['a.ts', symbolCardsHashInput([{ path: 'lib/x.mjs', line, declaration }])]]);
  const withCards = fileInputHashes(['a.ts'], sections, ctx, card(10, 'function x(a)'));
  assert.notDeepEqual(withCards, before);
  assert.deepEqual(fileInputHashes(['a.ts'], sections, ctx, card(99, 'function x(a)')), withCards);
  assert.notDeepEqual(fileInputHashes(['a.ts'], sections, ctx, card(10, 'function x(a, b)')), withCards);
});

test('configFingerprint: выключенные карточки отпечаток не меняют, лимиты включённых — меняют', () => {
  const cfg = { limits: { chunkBytes: 1 } };
  assert.equal(configFingerprint({ limits: { chunkBytes: 1, symbolCards: false, symbolCardsMax: 5 } }), configFingerprint(cfg));
  const on = (max) => configFingerprint({ limits: { chunkBytes: 1, symbolCards: true, symbolCardsMax: max } });
  assert.notEqual(on(20), configFingerprint(cfg));
  assert.notEqual(on(20), on(10));
});

const DIFF = `diff --git a/foo.ts b/foo.ts
--- a/foo.ts
+++ b/foo.ts
@@ -1,2 +1,3 @@
 const a = 1;
-const b = 2;
+const b = parseLimits(a, { strict: true });
`;

async function runWithCards(env, { graph = GRAPH } = {}) {
  const tmp = join(tmpdir(), `symbol-cards-${process.pid}-${Math.random().toString(36).slice(2)}.patch`);
  await writeFile(tmp, DIFF);
  const seen = { chunks: null };
  try {
    const cfg = loadReviewConfig({
      OLLAMA_API_KEY: 'k', GITHUB_TOKEN: 't', REVIEW_MODE: 'consensus', OLLAMA_MODELS: 'm1,m2',
      REVIEW_OBSERVABILITY: '0', REVIEW_EXPORT: '0', REVIEW_AUTO_TRIAGE: '0', REVIEW_VERIFIER: '0', ...env,
    }, { dryRun: false, diffFile: tmp, repo: 'o/r', prNumber: 1 });
    const deps = {
      log: () => {},
      die: (m) => { throw new Error(`die: ${m}`); },
      repoFiles: async () => null,
      readLines,
      graphify: { buildBlastRadius: async () => ({ blocks: [], metrics: null, graphIndex: graph }) },
      prescan: {
        run: async () => ({
          findings: [], scanned: 0, skipped: 0, truncated: false, lintableTotal: 0, byTool: null, error: null, promptSection: '',
        }),
      },
      engine: {
        reviewAllModels: async (chunks) => { seen.chunks = chunks; seen.calls = (seen.calls || 0) + 1; return [{ model: 'm1', summary: 'ok', findings: [] }]; },
        verifyFindings: async () => [],
      },
      github: {
        readPriorStickyData: async () => ({ findings: [], runs: [], sha: '' }),
        readPriorFindings: async () => [],
        deleteStaleInline: async () => 0,
        postReview: async () => 1,
        upsertSticky: async () => 'created',
      },
    };
    await createReviewPipeline(cfg, deps).run();
    return seen;
  } finally {
    await rm(tmp, { force: true });
  }
}

test('pipeline: по умолчанию карточка доезжает до чанка; REVIEW_SYMBOL_CARDS=0 — поля нет', async () => {
  const on = await runWithCards({});
  assert.deepEqual(on.chunks[0].symbolCards, [
    '// lib/config.mjs:L2–L4\nexport function parseLimits(raw, { strict = false } = {})',
  ]);
  const off = await runWithCards({ REVIEW_SYMBOL_CARDS: '0' });
  assert.equal(off.chunks[0].symbolCards, undefined);
});

// ── Аудит RFC 14 (docs/dev/audit/2026-10-09-rfc-14-symbol-cards.md) ──

test('аудит 0.1: backfillMissing дописывает только отсутствующие ключи, вложенно', () => {
  const rust = { limits: { chunkBytes: 1, contextMargin: 0 }, other: 'x' };
  const js = { limits: { chunkBytes: 9, contextMargin: 40, symbolCards: true }, other: 'y', added: { a: 1 } };
  assert.deepEqual(backfillMissing(rust, js).sort(), ['added', 'limits.symbolCards']);
  assert.deepEqual(rust, { limits: { chunkBytes: 1, contextMargin: 0, symbolCards: true }, other: 'x', added: { a: 1 } });
});

const AUDIT_GRAPH = buildGraphIndex({
  nodes: [
    { id: 'call', label: 'run()', source_file: 'lib/uses-run.mjs', source_location: 'L1' },
    { id: 'prop', label: 'getId()', source_file: 'lib/widget.mjs', source_location: 'L1' },
    { id: 'mock', label: 'listTypes()', source_file: 'src/svc.spec.ts', source_location: 'L2' },
    { id: 'log', label: 'log()', source_file: 'lib/logger.mjs', source_location: 'L1' },
    { id: 'var', label: 'dup', source_file: 'lib/a.mjs', source_location: 'L1' },
    { id: 'fn', label: 'dup()', source_file: 'lib/b.mjs', source_location: 'L1' },
    { id: 'P', label: 'Parser', source_file: 'lib/parser.mjs', source_location: 'L1' },
    { id: 'p', label: 'parser()', source_file: 'lib/parse.mjs', source_location: 'L1' },
    { id: 'm', label: '.method()', source_file: 'lib/m.mjs', source_location: 'L1' },
    { id: 'app', label: 'inApp()', source_file: 'server/x.ts', source_location: 'L1' },
  ],
  links: [],
});
const AUDIT_FILES = {
  'lib/uses-run.mjs': lines('const r = run(a, b);'),
  'lib/widget.mjs': lines('const widget = { getId: () => 1 };'),
  'src/svc.spec.ts': lines('// spec\nconst listTypes = jest.fn();'),
  'lib/logger.mjs': lines('export function log(msg) {}'),
  'src/caller.ts': lines('function work(log) {\n  log("x");\n}'),
  'app/server/x.ts': lines('export function inApp(a: number) {\n  return a;\n}'),
};
const auditRead = (p) => AUDIT_FILES[p] ?? null;
const auditCards = (added, { path = 'src/caller.ts', appDir = '' } = {}) => buildSymbolCards([path], {
  graphIndex: AUDIT_GRAPH, sections: new Map([[path, section(added)]]), windows: new Map(), readLines: auditRead, appDir,
});

test('аудит 0.2: узел на строке вызова или свойства объекта — не объявление, карточки нет', () => {
  const r = auditCards(['go(run(1));', 'show(getId());']);
  assert.equal(r.byPath.size, 0);
  assert.equal(r.metrics.skip_reasons.stale_graph, 1, 'run: строка — вызов');
  assert.equal(r.metrics.skip_reasons.not_a_declaration, 1, 'getId: заголовок `const widget =` без имени');
});

test('аудит 1.1: объявление метода в добавленной строке — не вызов; mock из spec — не источник', () => {
  assert.deepEqual([...extractUsedIdentifiers([
    '  listTypes(filter: Filter): Promise<Item[]>;',
    '  async run(a, b) {',
    '  private build<T>(x: T): void {',
  ]).keys()], []);
  assert.deepEqual([...extractUsedIdentifiers(['run(a);', 'build(x).then(done);']).keys()].sort(), ['build', 'run'], 'вызов-оператор остаётся вызовом');
  const r = auditCards(['const t = listTypes(f);']);
  assert.equal(r.metrics.skip_reasons.test_source, 1);
  const fromTest = auditCards(['const t = listTypes(f);'], { path: 'src/other.spec.ts' });
  assert.equal(fromTest.metrics.skip_reasons.test_source, undefined, 'тест может опираться на тестовый хелпер');
});

test('аудит 1.2: имя-параметр или деструктуризация в вызывающем файле — чужая карточка не выдаётся', () => {
  const r = auditCards(['  log("y");']);
  assert.equal(r.byPath.size, 0);
  assert.equal(r.metrics.skip_reasons.local_shadow, 1);
  assert.ok(hasLocalBinding(['const { log, warn: w } = deps;'], 'log'));
  assert.ok(hasLocalBinding(['items.forEach((item, log) => {'], 'log'));
  assert.ok(hasLocalBinding(['} catch (log) {'], 'log'));
  assert.ok(hasLocalBinding(['const f = log => log + 1;'], 'log'));
  assert.ok(!hasLocalBinding(['logger.info(dialog);', 'import { log } from "./logger.mjs";'], 'log'), 'импорт — не тень');
});

test('аудит 1.3: resolveCardNode — неоднозначность name/name() и регистр', () => {
  assert.deepEqual(resolveCardNode(AUDIT_GRAPH, 'dup'), { node: null, reason: 'ambiguous' });
  assert.equal(resolveCardNode(AUDIT_GRAPH, 'Parser').node?.id, 'P');
  assert.equal(resolveCardNode(AUDIT_GRAPH, 'parser').node?.id, 'p');
  assert.deepEqual(resolveCardNode(AUDIT_GRAPH, 'method'), { node: null, reason: 'unresolved' }, 'метка метода `.method()` по голому имени не совпадает');
});

test('аудит 1.7: interface/type/enum/namespace считаются объявлением', () => {
  for (const d of ['interface Foo {', 'export type Foo = {', 'enum Foo {', 'namespace Foo {']) {
    assert.ok(declarationRegex('Foo').test(d), d);
  }
});

test('аудит 1.11: граф от layout.appDir — путь узла дополняется префиксом', () => {
  const r = auditCards(['const v = inApp(1);'], { path: 'app/client/y.ts', appDir: 'app' });
  assert.deepEqual(r.byPath.get('app/client/y.ts').map((c) => [c.path, c.line, c.endLine]), [['app/server/x.ts', 1, 3]]);
  const noDir = auditCards(['const v = inApp(1);'], { path: 'app/client/y.ts' });
  assert.equal(noDir.metrics.skip_reasons.unreadable, 1);
});

test('аудит 1.8: карточка, чьё объявление видно в окне другого файла чанка, не дублируется', () => {
  const byPath = new Map([['a.ts', [{ name: 'shared', path: 'lib/s.mjs', line: 1, endLine: 2, declaration: 'function shared()', count: 1 }]]]);
  const sel = selectChunkCards(byPath, ['a.ts', 'b.ts'], { visibleText: '### b.ts\nexport function shared() {\n}' });
  assert.deepEqual(sel.cards, []);
  assert.equal(sel.visible, 1);
});

test('аудит 1.5: isAbsenceClaim — утверждение об отсутствии рядом с идентификатором, не рантайм', () => {
  assert.ok(isAbsenceClaim({ title: 'Функция `parseLimits` не определена' }));
  assert.ok(isAbsenceClaim({ body: 'В модуле `x.ts` экспорт `foo` is not exported' }));
  assert.ok(!isAbsenceClaim({ title: '`isCurrentUser` не защищён от null' }));
  assert.ok(!isAbsenceClaim({ body: 'Отсутствует обработка случая отсутствия OrganizationMapping' }));
});

test('аудит 1.5: symbolCardsOutcome связывает находки с карточками чанка их файла', () => {
  const chunks = [{ paths: ['a.ts'], symbolCardNames: ['parseLimits'] }, { paths: ['b.ts'] }];
  const onCard = { file: 'a.ts', severity: 'major', title: 'вызов `parseLimits` без strict', verifierStatus: 'verified' };
  const otherFile = { file: 'b.ts', severity: 'minor', title: 'parseLimits где-то ещё' };
  const substring = { file: 'a.ts', severity: 'minor', title: 'parseLimitsX — другое имя' };
  const absent = { file: 'a.ts', severity: 'minor', title: '`helper` не определён' };
  const out = symbolCardsOutcome({
    chunks, llmFindings: [onCard, otherFile, substring, absent], finalFindings: [onCard, otherFile], inline: [onCard],
  });
  assert.deepEqual(out.names, ['parseLimits']);
  assert.deepEqual(out.llm, { findings: 4, mentioning: 1, absenceClaims: 1 });
  assert.deepEqual(out.final, { findings: 2, mentioning: 1, inline: 1, bySeverity: { major: 1 }, byVerifier: { verified: 1 } });
});

// ── Аудит RFC 14, открытые пункты ──

test('аудит 1.12: relativeImportBindings — именованные, псевдонимы, default, namespace, require; пакеты мимо', () => {
  const b = relativeImportBindings([
    "import { parse, build as make } from './a.mjs';",
    "import def, { other } from '../b';",
    "import * as util from './util/index.js';",
    "import { ok } from 'node:assert';",
    "const helpers = require('./helpers');",
    'import {',
    '  multi,',
    "} from './c';",
  ]);
  assert.deepEqual(Object.fromEntries(b), {
    parse: { spec: './a.mjs', imported: 'parse' },
    make: { spec: './a.mjs', imported: 'build' },
    def: { spec: '../b', imported: 'default' },
    other: { spec: '../b', imported: 'other' },
    util: { spec: './util/index.js', imported: '*' },
    helpers: { spec: './helpers', imported: '*' },
    multi: { spec: './c', imported: 'multi' },
  });
});

test('аудит 1.12: resolveImportPath — точный путь, .js → .ts, расширения, index', () => {
  const files = new Set(['src/a.ts', 'src/lib/b.mjs', 'src/util/index.ts', 'shared/c.tsx']);
  const has = (f) => files.has(f);
  assert.equal(resolveImportPath('src/x.ts', './a.js', has), 'src/a.ts');
  assert.equal(resolveImportPath('src/x.ts', './lib/b.mjs', has), 'src/lib/b.mjs');
  assert.equal(resolveImportPath('src/x.ts', './util', has), 'src/util/index.ts');
  assert.equal(resolveImportPath('src/x.ts', '../shared/c', has), 'shared/c.tsx');
  assert.equal(resolveImportPath('src/x.ts', './missing', has), null);
  assert.equal(resolveImportPath('x.ts', '../../escape', has), null);
});

test('аудит 1.12: extractNamespaceCalls — только для namespace-импортов', () => {
  const calls = extractNamespaceCalls(['const r = util.parse(x) + other.parse(y);', 'util.parse(z); util.build();'], new Set(['util']));
  assert.deepEqual([...calls.values()], [{ ns: 'util', fn: 'parse', count: 2 }, { ns: 'util', fn: 'build', count: 1 }]);
});

const IMPORT_GRAPH = buildGraphIndex({
  nodes: [
    { id: 'p1', label: 'parse()', source_file: 'src/a.ts', source_location: 'L1' },
    { id: 'p2', label: 'parse()', source_file: 'src/other.ts', source_location: 'L1' },
    { id: 'u1', label: 'build()', source_file: 'src/util/index.ts', source_location: 'L1' },
  ],
  links: [],
});
const IMPORT_FILES = {
  'src/a.ts': lines('export function parse(raw: string, strict = false) {\n  return raw;\n}'),
  'src/other.ts': lines('export function parse(n: number) {\n  return n;\n}'),
  'src/util/index.ts': lines('export function build(cfg: Cfg) {\n  return cfg;\n}'),
  'src/caller.ts': lines("import { parse as px } from './a.js';\nimport * as util from './util';\n\nfunction go() {}"),
};
const importCards = (added) => buildSymbolCards(['src/caller.ts'], {
  graphIndex: IMPORT_GRAPH, sections: new Map([['src/caller.ts', section(added)]]), windows: new Map(),
  readLines: (f) => IMPORT_FILES[f] ?? null,
});

test('аудит 1.12: импорт снимает неоднозначность и открывает ns.fn(', () => {
  const r = importCards(['const v = px(x);', 'const c = util.build(cfg);']);
  assert.deepEqual(r.byPath.get('src/caller.ts').map((c) => [c.name, c.path, c.declaration]).sort(), [
    ['build', 'src/util/index.ts', 'export function build(cfg: Cfg)'],
    ['parse', 'src/a.ts', 'export function parse(raw: string, strict = false)'],
  ]);
  assert.equal(r.metrics.via_import, 2);
  // без импорта то же имя неоднозначно по графу — карточки нет
  const bare = buildSymbolCards(['src/z.ts'], {
    graphIndex: IMPORT_GRAPH, sections: new Map([['src/z.ts', section(['parse(x);'])]]), windows: new Map(), readLines: () => [],
  });
  assert.equal(bare.metrics.skip_reasons.ambiguous, 1);
});

test('аудит 1.6: TS — стрелка и литерал типа в аннотации не обрывают заголовок', () => {
  assert.deepEqual(
    scanDeclaration(lines('export function make(n: number): (x: number) => number {\n  return (x) => x + n;\n}'), 0),
    { header: 'export function make(n: number): (x: number) => number', endIdx: 2 },
  );
  assert.deepEqual(
    scanDeclaration(lines('async function load(id: string): Promise<{ id: string; ok: boolean }> {\n  return { id, ok: true };\n}'), 0),
    { header: 'async function load(id: string): Promise<{ id: string; ok: boolean }>', endIdx: 2 },
  );
  assert.deepEqual(
    scanDeclaration(lines('function shape(): { a: number } {\n  return { a: 1 };\n}'), 0),
    { header: 'function shape(): { a: number }', endIdx: 2 },
  );
  assert.deepEqual(
    scanDeclaration(lines('const twice: (x: number) => number = (x) => x * 2;\nnext();'), 0),
    { header: 'const twice: (x: number) => number = (x) =>', endIdx: 0 },
  );
  assert.deepEqual(
    scanDeclaration(lines('class Box<T extends { id: string }> extends Base<T> {\n  get() {}\n}'), 0),
    { header: 'class Box<T extends { id: string }> extends Base<T>', endIdx: 2 },
  );
});

test('аудит 1.6: перегрузки TS собираются вместе с реализацией', () => {
  const src = lines([
    'export function pick(a: string): string;',
    'export function pick(a: number): number;',
    'export function pick(a: string | number) {',
    '  return a;',
    '}',
    'const after = 1;',
  ].join('\n'));
  assert.deepEqual(cardDeclaration(src, 0, 'pick'), {
    header: 'export function pick(a: string): string\nexport function pick(a: number): number\nexport function pick(a: string | number)',
    endIdx: 4,
  });
});

test('аудит 1.6: класс — с сигнатурой конструктора для new X(…)', () => {
  const src = lines([
    'export class Client extends Base {',
    '  private readonly url: string;',
    '  constructor(url: string, opts: Opts = {}) {',
    '    super();',
    '  }',
    '}',
  ].join('\n'));
  assert.deepEqual(cardDeclaration(src, 0, 'Client'), {
    header: 'export class Client extends Base { constructor(url: string, opts: Opts = {}) }',
    endIdx: 5,
  });
});

test('аудит 1.9: граф недоступен во втором прогоне — хэши с прошлым входом карточек, файл не пере-ревьюится', async () => {
  const { mkdtemp, readFile: rf } = await import('node:fs/promises');
  const dir = await mkdtemp(join(tmpdir(), 'cards-inc-'));
  const stateFile = join(dir, 'state.json');
  try {
    const env = { REVIEW_INCREMENTAL: '1', REVIEW_INCREMENTAL_STATE: stateFile };
    await runWithCards(env);
    const first = JSON.parse(await rf(stateFile, 'utf8'));
    assert.ok(first.cardsInput?.['foo.ts'], 'вход карточек сохранён в состоянии');
    const second = await runWithCards(env, { graph: null });
    const after = JSON.parse(await rf(stateFile, 'utf8'));
    assert.deepEqual(after.hashes, first.hashes, 'сбой графа не меняет хэши');
    assert.equal(second.calls ?? 0, 0, 'неизменившийся файл не отправлен в LLM');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
