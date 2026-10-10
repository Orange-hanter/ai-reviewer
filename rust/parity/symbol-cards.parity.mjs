/**
 * Parity RFC 16 T38: rust/src/symbol_cards.rs ↔ эталон lib/symbol-cards.mjs.
 *
 * Запуск: node rust/parity/symbol-cards.parity.mjs   (требует cargo build)
 *
 * JS-обёртки у модуля нет — его зовёт pipeline (порт — T26), поэтому сверка идёт
 * напрямую с бинарём rust/target/debug/symbol_cards ({op: batch, calls}). Нет бинаря —
 * прогон пропускается с сообщением (как остальные parity без cargo build).
 *
 * Корпус — весь JS/TS репозитория (lib/, bin/, test/, rust/parity/, cursor-plugin/,
 * pipeline.mjs, index.mjs): лексеры и линейные замены регулярок сверяются на реальном
 * коде, а не только на кейсах тестов. Плюс:
 *   - кейсы test/symbol-cards.test.mjs и враждебные строки (ReDoS-кейсы, литералы,
 *     шаблоны, CRLF, не-ASCII);
 *   - buildSymbolCards на синтетическом графе по экспортам lib/*.mjs, где каждая строка
 *     файла — «добавленная»: прогоняет весь отбор (импорты, тени, stale_graph, тела);
 *   - selectChunkCards / symbolCardsHashInput / renderSymbolCard на его результате.
 */

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');
const BIN = process.env.AI_REVIEWER_SYMBOL_CARDS_BIN || resolve(REPO, 'rust/target/debug/symbol_cards');

if (!existsSync(BIN)) {
  console.log(`symbol_cards: бинаря нет (${BIN}) — parity пропущен; собери cargo build`);
  process.exit(0);
}

const SC = await import('../../lib/symbol-cards.mjs');
const { buildGraphIndex } = await import('../../lib/graphify.mjs');

// ── корпус ──
const SKIP_DIRS = new Set(['node_modules', '.git', 'target', '.claude', '.ai-review', '.graphify']);
function walk(dir, out) {
  for (const name of readdirSync(dir)) {
    if (SKIP_DIRS.has(name)) continue;
    const p = join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) walk(p, out);
    else if (/\.(?:mjs|js|ts|tsx|cjs)$/.test(name) && st.size < 400_000) out.push(p);
  }
  return out;
}
const roots = ['lib', 'bin', 'test', 'rust/parity', 'cursor-plugin', 'examples'].map((d) => resolve(REPO, d)).filter(existsSync);
const files = [...roots.flatMap((r) => walk(r, [])), resolve(REPO, 'pipeline.mjs'), resolve(REPO, 'index.mjs')]
  .filter(existsSync)
  .map((abs) => ({ rel: relative(REPO, abs), lines: readFileSync(abs, 'utf8').split('\n') }));

const ADVERSARIAL = [
  `"${'\\\\'.repeat(500)}x(1)`, `x${' '.repeat(1990)}`, `.${' '.repeat(1990)}a b c`,
  `${' '.repeat(3000)}${'/*'.repeat(900)}`, `import a${' '.repeat(3000)}x`, `a${'/a'.repeat(3000)}`,
  "const s = 'unterminated(call)", 'const t = `tpl ${inner(x)} more` + after(y);', 'f(a) /* c(1) */ g(b) // h(c)',
  'return /[}(]/.test(s) ? ok(s) : no(s);', 'x = a / b / c(d);', 'const re = /\\/(x)/g; run(re);',
  "import { a as b, type C, d: e } from './m.js'", "import type { T } from 'pkg'", 'import*as ns from "./ns"',
  "const { readFile: rf, stat } = require('fs');", "let x require('./a')", 'new  Foo(1); new\tBar; newer(2)',
  '$fn(1) _x(2) a$b(3) 9z(4)', 'obj . method(1); obj?.call(2); arr[0](3)', 'get(x) { return 1 }', 'async run(a, b) {',
  'private build<T>(x: T): void {', 'static async (x) {}', 'public get value(): number {', 'foo<T,>(x)',
  'ü(1) ünïcode(2) 名前(3) emoji😀(4)', 'line\rwith\rcr(x)', 'a\u2028b(c)', '  12: +added(x)', '\u00a0call(x)',
  'function* gen(a) {', 'const h = async x => x + 1;', 'export default function () {}', 'x: async function () {}',
  'class A { constructor(a) {} }', 'enum Color { Red }', 'interface I { m(): void }', 'type T = { a: (x) => void }',
];

const lineSets = [];
for (const f of files) lineSets.push(f.lines);
lineSets.push(ADVERSARIAL);

// ── вызовы: [label, jsFn, rustCall] ──
const cases = [];
const add = (label, js, call) => cases.push({ label, js, call });
const ent = (m) => [...m.entries()];

for (const [i, lines] of lineSets.entries()) {
  const tag = files[i]?.rel ?? 'adversarial';
  const ref = i; // linesRef/textRef → lineSets[i]
  add(`maskNonCode ${tag}`, () => SC.maskNonCode(lines), { op: 'maskNonCode', lines });
  add(`extractUsedIdentifiers ${tag}`, () => ent(SC.extractUsedIdentifiers(lines)), { op: 'extractUsedIdentifiers', lines });
  const masked = SC.maskNonCode(lines);
  add(`extractUsedIdentifiers(masked) ${tag}`, () => ent(SC.extractUsedIdentifiers(masked)), { op: 'extractUsedIdentifiers', lines: masked });
  add(`packageImportedNames ${tag}`, () => [...SC.packageImportedNames(lines)], { op: 'packageImportedNames', lines });
  const bindings = SC.relativeImportBindings(lines);
  add(`relativeImportBindings ${tag}`, () => Object.fromEntries(SC.relativeImportBindings(lines)), { op: 'relativeImportBindings', lines });
  const namespaces = [...new Set([...[...bindings].filter(([, b]) => b.imported === '*').map(([l]) => l), 'assert', 'path', 'JSON', 'Object'])];
  add(`extractNamespaceCalls ${tag}`, () => [...SC.extractNamespaceCalls(lines, new Set(namespaces)).values()], { op: 'extractNamespaceCalls', lines, namespaces });
  add(`addedLinesOfSection ${tag}`, () => SC.addedLinesOfSection(lines.map((l, k) => `${String(k + 1).padStart(5)}: ${k % 3 ? '+' : ' '}${l}`).join('\n')),
    { op: 'addedLinesOfSection', section: lines.map((l, k) => `${String(k + 1).padStart(5)}: ${k % 3 ? '+' : ' '}${l}`).join('\n') });
  // объявления: строки, похожие на начало объявления, и каждая 7-я
  let scans = 0;
  for (let k = 0; k < lines.length && scans < 150; k++) {
    if (!(/^\s*(?:export|function|const|let|var|class|async|static|public|private|interface|type|enum|[\w$]+\s*[(:=])/.test(lines[k]) || k % 7 === 0)) continue;
    scans++;
    add(`scanDeclaration ${tag}:${k}`, () => SC.scanDeclaration(lines, k), { op: 'scanDeclaration', linesRef: ref, startIdx: k });
    const m = /([A-Za-z_$][\w$]*)\s*[(=<]/.exec(lines[k]);
    const name = m?.[1] ?? '';
    add(`cardDeclaration ${tag}:${k}`, () => SC.cardDeclaration(lines, k, name), { op: 'cardDeclaration', linesRef: ref, startIdx: k, name });
  }
  const text = lines.join('\n');
  const names = [...SC.extractUsedIdentifiers(lines).keys()].slice(0, 60);
  for (const n of [...names, 'foo', 'get', 'static', 'export', 'type']) {
    add(`declarationTest ${tag}:${n}`, () => SC.declarationRegex(n).test(text), { op: 'declarationTest', name: n, textRef: ref });
    add(`hasLocalBinding ${tag}:${n}`, () => SC.hasLocalBinding(lines, n), { op: 'hasLocalBinding', linesRef: ref, name: n });
  }
}

// кейсы test/symbol-cards.test.mjs, которые не покрыты корпусом напрямую
for (const d of ['function foo(a) {', 'export async function foo() {', 'class foo {', 'const foo = (x) => x', '  async foo(a, b) {',
  '  foo: async (x) => x,', 'let foo = function () {', 'const r = foo(1);', 'foodle(1)', 'function foo$ (', 'x.foo: function',
  '\n\n  static get foo() {', 'export  foo(a): T {', 'foo(\n) {', 'foo\t(a)\t:\tT\t{']) {
  add(`declarationTest ${JSON.stringify(d)}`, () => SC.declarationRegex('foo').test(d), { op: 'declarationTest', name: 'foo', text: d });
}
for (const [lines, name] of [[['const { log, warn: w } = deps;'], 'log'], [['items.forEach((item, log) => {'], 'log'], [['} catch (log) {'], 'log'],
  [['const f = log => log + 1;'], 'log'], [['logger.info(dialog);', 'import { log } from "./logger.mjs";'], 'log'],
  [['const [a, log ] = x;'], 'log'], [['let {log}= y'], 'log'], [['fn(a): (log) => void {'], 'log']]) {
  add(`hasLocalBinding ${JSON.stringify(lines)}`, () => SC.hasLocalBinding(lines, name), { op: 'hasLocalBinding', lines, name });
}
for (const [caller, spec] of [['src/x.ts', './a.js'], ['src/x.ts', './lib/b.mjs'], ['src/x.ts', './util'], ['src/x.ts', '../shared/c'],
  ['src/x.ts', './missing'], ['x.ts', '../../escape'], ['src/x.ts', './a.m.js'], ['src/x.ts', './/a']]) {
  const fl = ['src/a.ts', 'src/lib/b.mjs', 'src/util/index.ts', 'shared/c.tsx', 'src/a.m.ts', 'src/a'];
  add(`resolveImportPath ${caller} ${spec}`, () => SC.resolveImportPath(caller, spec, (f) => fl.includes(f)),
    { op: 'resolveImportPath', caller, spec, files: fl });
}
for (const p of ['a.ts', 'a.TSX', 'a.b.mjs', 'README.md', 'x', '.ts', 'dir.ts/file', 'a.ts.bak', 'a.d.ts']) {
  add(`isSymbolCardPath ${p}`, () => SC.isSymbolCardPath(p), { op: 'isSymbolCardPath', path: p });
}
for (const f of [{ title: 'Функция `parseLimits` не определена' }, { body: 'В модуле `x.ts` экспорт `foo` is not exported' },
  { title: '`isCurrentUser` не защищён от null' }, { body: 'Отсутствует обработка случая отсутствия OrganizationMapping' },
  { title: 'НЕ   ОПРЕДЕЛЕНА функция `X`' }, { title: '`a.b` DOES NOT EXIST' }, { title: null, body: 5 }, {},
  { title: 'is undefined, see `q`' }, { title: '`x`; is not defined' }]) {
  add(`isAbsenceClaim ${JSON.stringify(f)}`, () => SC.isAbsenceClaim(f), { op: 'isAbsenceClaim', finding: f });
}

// resolveCardNode — граф аудита
const AUDIT_NODES = [
  { id: 'var', label: 'dup', source_file: 'lib/a.mjs', source_location: 'L1' },
  { id: 'fn', label: 'dup()', source_file: 'lib/b.mjs', source_location: 'L1' },
  { id: 'P', label: 'Parser', source_file: 'lib/parser.mjs', source_location: 'L1' },
  { id: 'p', label: 'parser()', source_file: 'lib/parse.mjs', source_location: 'L1' },
  { id: 'm', label: '.method()', source_file: 'lib/m.mjs', source_location: 'L1' },
  { id: 2, label: ' spaced()() ', source_file: 'x.mjs' },
];
const auditIdx = buildGraphIndex({ nodes: AUDIT_NODES, links: [] });
for (const n of ['dup', 'Parser', 'parser', 'method', 'spaced', '', 'none']) {
  add(`resolveCardNode ${n}`, () => { const r = SC.resolveCardNode(auditIdx, n); return { id: r.node?.id ?? null, reason: r.reason }; },
    { op: 'resolveCardNode', graph: { nodes: AUDIT_NODES, links: [] }, name: n });
}

// ── buildSymbolCards на синтетическом графе lib/ ──
const libFiles = files.filter((f) => /^lib\/[^/]+\.mjs$/.test(f.rel));
const nodes = [];
for (const f of libFiles) {
  f.lines.forEach((l, k) => {
    const m = /^export\s+(?:async\s+)?function\*?\s+([A-Za-z_$][\w$]*)|^export\s+(?:const|class)\s+([A-Za-z_$][\w$]*)/.exec(l);
    if (m) nodes.push({ id: `${f.rel}#${m[1] || m[2]}`, label: `${m[1] || m[2]}${m[1] ? '()' : ''}`, source_file: f.rel, source_location: `L${k + 1}` });
  });
}
// пара намеренно битых узлов: устаревшая строка, абсолютный путь, тест-источник
nodes.push({ id: 'stale', label: 'loadReviewConfig()', source_file: 'lib/util.mjs', source_location: 'L1' });
nodes.push({ id: 'abs', label: 'absFn()', source_file: resolve(REPO, 'lib/util.mjs'), source_location: 'L2' });
const graph = { nodes, links: [] };
const graphIndex = buildGraphIndex(graph);
const fileMap = Object.fromEntries(files.map((f) => [f.rel, f.lines]));
const callerFiles = files.filter((f) => /^(?:lib|bin|test)\/[^/]+\.mjs$/.test(f.rel) || f.rel === 'pipeline.mjs');
const sectionOf = (lines) => lines.map((l, k) => `${String(k + 1).padStart(5)}: +${l}`).join('\n');
const sections = Object.fromEntries(callerFiles.map((f) => [f.rel, sectionOf(f.lines)]));
// окна — первые 30 строк файла: часть объявлений становится «видимой»
const windows = Object.fromEntries(callerFiles.map((f) => [f.rel, f.lines.slice(0, 30).join('\n')]));
const paths = callerFiles.map((f) => f.rel);
for (const bodyMaxLines of [0, 5]) {
  const opts = {
    graphIndex, sections: new Map(Object.entries(sections)), windows: new Map(Object.entries(windows)),
    readLines: (p) => fileMap[p] ?? null, limits: { bodyMaxLines }, root: REPO,
  };
  const r = SC.buildSymbolCards(paths, opts);
  const jsOut = { byPath: [...r.byPath], metrics: r.metrics };
  add(`buildSymbolCards body=${bodyMaxLines}`, () => jsOut,
    { op: 'buildSymbolCards', paths, graph, sections, windows, files: fileMap, bodyMaxLines, root: REPO, appDir: '' });
  const byPathObj = Object.fromEntries(r.byPath);
  const allCards = [...r.byPath.values()].flat();
  add(`symbolCardsHashInput body=${bodyMaxLines}`, () => SC.symbolCardsHashInput(allCards), { op: 'symbolCardsHashInput', cards: allCards });
  for (const c of allCards.slice(0, 40)) add(`renderSymbolCard ${c.path}:${c.line}`, () => SC.renderSymbolCard(c), { op: 'renderSymbolCard', card: c });
  for (let k = 0; k < paths.length; k += 3) {
    const chunk = paths.slice(k, k + 4);
    for (const [max, maxChars] of [[20, 3000], [3, 400]]) {
      const visibleText = chunk.map((p) => windows[p]).join('\n');
      add(`selectChunkCards ${chunk.join(',')} ${max}/${maxChars}`,
        () => SC.selectChunkCards(r.byPath, chunk, { max, maxChars, visibleText }),
        { op: 'selectChunkCards', byPath: byPathObj, chunkPaths: chunk, max, maxChars, visibleText });
    }
  }
}
// appDir: граф без префикса каталога приложения
{
  const appNodes = [{ id: 'app', label: 'inApp()', source_file: 'server/x.ts', source_location: 'L1' }];
  const appFiles = { 'app/server/x.ts': ['export function inApp(a: number) {', '  return a;', '}'] };
  const sec = { 'app/client/y.ts': '    1: +const v = inApp(1);' };
  for (const appDir of ['app', './app/', '']) {
    const r = SC.buildSymbolCards(['app/client/y.ts'], {
      graphIndex: buildGraphIndex({ nodes: appNodes, links: [] }), sections: new Map(Object.entries(sec)), windows: new Map(),
      readLines: (p) => appFiles[p] ?? null, appDir, root: REPO,
    });
    add(`buildSymbolCards appDir=${appDir}`, () => ({ byPath: [...r.byPath], metrics: r.metrics }),
      { op: 'buildSymbolCards', paths: ['app/client/y.ts'], graph: { nodes: appNodes, links: [] }, sections: sec, windows: {}, files: appFiles, root: REPO, appDir });
  }
  const none = SC.buildSymbolCards(['a.ts'], { graphIndex: null, sections: new Map(), windows: new Map(), readLines: () => null });
  add('buildSymbolCards без графа', () => ({ byPath: [...none.byPath], metrics: none.metrics }),
    { op: 'buildSymbolCards', paths: ['a.ts'], graph: null, sections: {}, windows: {}, files: {}, root: REPO, appDir: '' });
}
// symbolCardsOutcome
{
  const chunks = [{ paths: ['a.ts'], symbolCardNames: ['parseLimits', 'z$'] }, { paths: ['b.ts'] }];
  const onCard = { file: 'a.ts', severity: 'major', title: 'вызов `parseLimits` без strict', verifierStatus: 'verified' };
  const otherFile = { file: 'b.ts', severity: 'minor', title: 'parseLimits где-то ещё' };
  const substring = { file: 'a.ts', severity: 'minor', title: 'parseLimitsX — другое имя' };
  const absent = { file: 'a.ts', title: '`helper` не определён', quote: 'z$(1)' };
  const llmFindings = [onCard, otherFile, substring, absent];
  const finalFindings = [onCard, otherFile, absent];
  add('symbolCardsOutcome', () => SC.symbolCardsOutcome({ chunks, llmFindings, finalFindings, inline: [onCard, absent] }),
    { op: 'symbolCardsOutcome', chunks, llmFindings, finalFindings, inline: [0, 2] });
}

// ── прогон ──
const rust = JSON.parse(execFileSync(BIN, { input: JSON.stringify({ op: 'batch', corpus: lineSets, calls: cases.map((c) => c.call) }), maxBuffer: 1 << 30 }).toString());
let compared = 0;
const failures = [];
const norm = (v) => JSON.parse(JSON.stringify(v ?? null));
cases.forEach((c, i) => {
  compared++;
  try {
    assert.deepEqual(norm(rust[i]), norm(c.js()));
  } catch (e) {
    failures.push({ label: c.label, js: JSON.stringify(norm(c.js())).slice(0, 600), rust: JSON.stringify(norm(rust[i])).slice(0, 600) });
  }
});
console.log(`corpus files: ${files.length}, graph nodes: ${nodes.length}`);
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
