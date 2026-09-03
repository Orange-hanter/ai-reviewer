/**
 * Parity-прогон Фазы 2, шаг 2: Rust structure_graphify ↔ чистый JS fallback
 * lib/structure-graphify.mjs.
 *
 * Запуск: node rust/parity/structure-graphify.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/structure-graphify.mjs (FORCE_JS переключает
 * стороны на каждый вызов), результаты — после JSON round-trip.
 *
 * Корпус (векторы из test/structure.test.mjs + edge-cases):
 *   - GRAPH из тестов: entities/contracts/boundaries/dependencies, kind-инференс
 *     (_callable/_callable_class/implements/method), community/crossCommunity;
 *   - layout appDir/layerDirs, module-префиксы с чёрным списком, deepest-prefix
 *     boundary, scope-фильтр (exact/trailing slash/без совпадений);
 *   - includeAmbiguous, отсечение maxEntities (ДО boundaries) и maxEdges
 *     (после сортировки) с warnings;
 *   - gods из analysis (строки и объекты), file_type != 'code' пропускается,
 *     небезопасный source_file пропускается, raw.edges как alias links;
 *   - entrypoint-дедуп при дублирующихся observedId (кейс из теста);
 *   - testCases: test/|__tests__|e2e|*.test.*|*.spec.* пути, kind function/method,
 *     line = entity.line || 1;
 *   - buildStructureViews: objectTree (contains/method-children по label),
 *     functionTree (группировка по пути, сорт по line), boundaryTree entityRefs,
 *     dependencyGraph.
 */

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_STRUCTURE_GRAPHIFY_FORCE_JS = '1';
const S = await import('../../lib/structure-graphify.mjs');

let compared = 0;
const failures = [];

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
  process.env.AI_REVIEWER_STRUCTURE_GRAPHIFY_FORCE_JS = '1';
  try {
    return { ok: true, value: rt(fn(...args)) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_STRUCTURE_GRAPHIFY_FORCE_JS;
  }
};

const runRust = (fn, ...args) => {
  delete process.env.AI_REVIEWER_STRUCTURE_GRAPHIFY_FORCE_JS;
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
// Корпус: extractGraphifyFacts
// ---------------------------------------------------------------------------

const GRAPH = {
  nodes: [
    { id: 'api', label: 'validate()', file_type: 'code', source_file: 'src/auth.ts', source_location: 'L10', _callable: true, community: 1 },
    { id: 'caller1', label: 'login()', file_type: 'code', source_file: 'src/login.ts', source_location: 'L4', _callable: true, community: 2 },
    { id: 'caller2', label: 'refresh()', file_type: 'code', source_file: 'src/refresh.ts', source_location: 'L5', _callable: true, community: 2 },
    { id: 'iface', label: 'TokenValidator', file_type: 'code', source_file: 'src/contracts.ts', source_location: 'L1', _callable_class: true, community: 1 },
    { id: 'impl', label: 'JwtValidator', file_type: 'code', source_file: 'src/auth.ts', source_location: 'L2', _callable_class: true, community: 1 },
    { id: 'test', label: 'reject invalid token()', file_type: 'code', source_file: 'test/auth.test.ts', source_location: 'L3', _callable: true, community: 3 },
  ],
  links: [
    { source: 'caller1', target: 'api', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'caller2', target: 'api', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'test', target: 'api', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'impl', target: 'iface', relation: 'implements', confidence: 'EXTRACTED' },
  ],
};

check('extractGraphifyFacts: GRAPH из тестов', S.extractGraphifyFacts, GRAPH);
check('extractGraphifyFacts: layout appDir/layerDirs', S.extractGraphifyFacts, GRAPH, {
  layout: { appDir: 'src', layerDirs: ['auth'] },
});
check('extractGraphifyFacts: layout layerDirs без совпадений', S.extractGraphifyFacts, GRAPH, {
  layout: { appDir: 'src', layerDirs: ['web'] },
});
check('extractGraphifyFacts: layout без layerDirs', S.extractGraphifyFacts, GRAPH, {
  layout: { appDir: 'src' },
});
check('extractGraphifyFacts: scope exact', S.extractGraphifyFacts, GRAPH, { scope: 'src' });
check('extractGraphifyFacts: scope trailing slash', S.extractGraphifyFacts, GRAPH, { scope: 'src/' });
check('extractGraphifyFacts: scope без совпадений', S.extractGraphifyFacts, GRAPH, { scope: 'docs' });
check('extractGraphifyFacts: maxEntities=0', S.extractGraphifyFacts, GRAPH, { maxEntities: 0 });
check('extractGraphifyFacts: maxEntities=3', S.extractGraphifyFacts, GRAPH, { maxEntities: 3 });
check('extractGraphifyFacts: maxEdges=0', S.extractGraphifyFacts, GRAPH, { maxEdges: 0 });
check('extractGraphifyFacts: maxEdges=2', S.extractGraphifyFacts, GRAPH, { maxEdges: 2 });

const MESSY = {
  nodes: [
    // method-target и '.'-префикс → method; method-source → class_like
    { id: 'obj', label: 'obj', file_type: 'code', source_file: 'src/o.ts', source_location: 'src/o.ts:1' },
    { id: 'm1', label: '.hidden', file_type: 'code', source_file: 'src/o.ts', source_location: 'o.ts:2' },
    { id: 'cls', label: 'Cls', file_type: 'code', source_file: 'src/o.ts', source_location: ':3' },
    { id: 'sym', label: 'plain', file_type: 'code', source_file: 'src/o.ts', source_location: 'L4' },
    // file_type не code → пропуск; небезопасный путь → пропуск; id null → пропуск
    { id: 'doc', label: 'doc', file_type: 'doc', source_file: 'src/doc.md' },
    { id: 'esc', label: 'esc', file_type: 'code', source_file: '../escape.ts' },
    { id: null, label: 'noid', file_type: 'code', source_file: 'src/o.ts' },
    // '' и 0 — falsy file_type → НЕ пропускаются
    { id: 'zero', label: 'zero()', file_type: '', source_file: 'src/o.ts' },
    // norm_label, community 0, gods
    { id: 'god', label: 'God', norm_label: 'pkg/God', file_type: 'code', source_file: 'src/god.ts', community: 0 },
  ],
  links: [
    { source: 'obj', target: 'm1', relation: 'method', confidence: 'INFERRED' },
    { source: 'cls', target: 'obj', relation: 'inherits', confidence: 'ambiguous' },
    { source: 'cls', target: 'm1', relation: 'implements', confidence: 'EXTRACTED' },
    { source: 'cls', target: 'zero', relation: 'contains', confidence: 'HEURISTIC' },
    { source: 'm1', target: 'sym', relation: 'references', confidence: 'UNKNOWN' },
    { source: 'sym', target: 'm1', relation: 'implements', confidence: 'ambiguous' },
  ],
};
const GODS = { gods: ['god', { label: 'GOD2' }, '', 42] };

check('extractGraphifyFacts: kind-инференс, gods, мусорные узлы', S.extractGraphifyFacts, MESSY, { analysis: GODS });
check('extractGraphifyFacts: includeAmbiguous', S.extractGraphifyFacts, MESSY, { analysis: GODS, includeAmbiguous: true });
check('extractGraphifyFacts: без analysis', S.extractGraphifyFacts, MESSY);
check('extractGraphifyFacts: raw.edges как alias links', S.extractGraphifyFacts, { nodes: MESSY.nodes, edges: MESSY.links });
check('extractGraphifyFacts: raw без nodes/links', S.extractGraphifyFacts, null);
check('extractGraphifyFacts: {}', S.extractGraphifyFacts, {});

// кейс из теста: два узла с одинаковым observedId → entrypoint не дублируется
check('extractGraphifyFacts: entrypoint-дедуп по observedId', S.extractGraphifyFacts, {
  nodes: [
    { id: 'dup1', label: 'run()', file_type: 'code', source_file: 'src/run.ts', source_location: 'L10', _callable: true },
    { id: 'dup2', label: 'run()', file_type: 'code', source_file: 'src/run.ts', source_location: 'L10', _callable: true },
  ],
  links: [],
});

// graph-only graphify-отчёт с node_modules/дист-префиксами
check('extractGraphifyFacts: чёрный список module-префиксов', S.extractGraphifyFacts, {
  nodes: [
    { id: 'a', label: 'a()', file_type: 'code', source_file: 'node_modules/dep/index.js', _callable: true },
    { id: 'b', label: 'b()', file_type: 'code', source_file: 'dist/out.js', _callable: true },
    { id: 'c', label: 'c()', file_type: 'code', source_file: 'src/app/c.ts', _callable: true },
    { id: 'd', label: 'd()', file_type: 'code', source_file: 'single.ts', _callable: true },
  ],
  links: [],
});

// ---------------------------------------------------------------------------
// Корпус: buildStructureViews
// ---------------------------------------------------------------------------

check('buildStructureViews: facts из GRAPH', S.buildStructureViews, S.extractGraphifyFacts(GRAPH, {
  layout: { appDir: 'src', layerDirs: ['auth'] },
}));
check('buildStructureViews: пустые facts', S.buildStructureViews, { entities: [], dependencies: [], boundaries: [] });
check('buildStructureViews: без полей', S.buildStructureViews, {});

const withChildren = {
  entities: [
    { observedId: 'c1', label: 'Cls', kind: 'class_like', path: 'src/a.ts', line: 1 },
    { observedId: 'f2', label: 'b()', kind: 'method', path: 'src/a.ts', line: 9 },
    { observedId: 'f1', label: 'a()', kind: 'function', path: 'src/a.ts', line: 5 },
    { observedId: 'g1', label: 'top()', kind: 'function', path: 'src/b.ts', line: 2 },
  ],
  dependencies: [
    { id: 'e1', from: 'c1', to: 'f1', relation: 'contains' },
    { id: 'e2', from: 'c1', to: 'f2', relation: 'method' },
    { id: 'e3', from: 'c1', to: 'missing', relation: 'contains' },
    { id: 'e4', from: 'c1', to: 'f1', relation: 'calls' },
  ],
  boundaries: [{ id: 'boundary:repository', name: 'repository', kind: 'repository', paths: ['.'] }],
};
check('buildStructureViews: objectTree-children, висячие ссылки, вызовы не children', S.buildStructureViews, withChildren);

// canonical facts из fixtures (реальный payload протокола)
for (const name of ['structure-facts.v1.json']) {
  const payload = JSON.parse(await readFile(resolve(REPO, 'fixtures/structure', name), 'utf8'));
  check(`buildStructureViews: ${name}`, S.buildStructureViews, payload.facts);
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
