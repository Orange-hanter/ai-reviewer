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
} = await import('../lib/symbol-cards.mjs');
const { buildGraphIndex } = await import('../lib/graphify.mjs');
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
    unresolved_or_ambiguous: 2, // helper (два узла) и unknownFn (нет узла)
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

async function runWithCards(env) {
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
      graphify: { buildBlastRadius: async () => ({ blocks: [], metrics: null, graphIndex: GRAPH }) },
      prescan: {
        run: async () => ({
          findings: [], scanned: 0, skipped: 0, truncated: false, lintableTotal: 0, byTool: null, error: null, promptSection: '',
        }),
      },
      engine: {
        reviewAllModels: async (chunks) => { seen.chunks = chunks; return [{ model: 'm1', summary: 'ok', findings: [] }]; },
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

test('pipeline: REVIEW_SYMBOL_CARDS=1 — карточка доезжает до чанка; по умолчанию поля нет', async () => {
  const on = await runWithCards({ REVIEW_SYMBOL_CARDS: '1' });
  assert.deepEqual(on.chunks[0].symbolCards, [
    '// lib/config.mjs:L2–L4\nexport function parseLimits(raw, { strict = false } = {})',
  ]);
  const off = await runWithCards({});
  assert.equal(off.chunks[0].symbolCards, undefined);
});
