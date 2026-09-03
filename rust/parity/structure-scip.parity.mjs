/**
 * Parity-прогон Фазы 2, шаг 1: Rust structure_scip ↔ чистый JS fallback lib/structure-scip.mjs.
 *
 * Запуск: node rust/parity/structure-scip.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/structure-scip.mjs: resolveScipBin() читает
 * AI_REVIEWER_SCIP_FORCE_JS на каждый вызов, поэтому js-сторона прогоняется
 * с FORCE_JS=1, rust-сторона — без него (диспетчеризация проверяется тоже).
 * Результаты сравниваются после JSON round-trip: так они выглядят на выходе
 * Rust-бинаря и в проде (facts сериализуются в JSON).
 *
 * Корпус:
 *   1. decodeScipIndex: hand-built SCIP-буферы (single/multi/legacy-packed range,
 *      packed 3 и ≥4 значений, relationships со всеми флагами, externalSymbols,
 *      skipped fixed64/fixed32, дубликаты полей, невалидный UTF-8, порядок ключей);
 *   2. граничные случаи varint: 2^53-1 ок, 2^53 — ошибка, guard сдвига > 53,
 *      truncated varint/field, unsupported wire 3/4/6/7, пустой буфер — тексты
 *      ошибок сравниваются посимвольно (тесты матчат regex'ами);
 *   3. extractScipFacts: JSON-индексы (scope-фильтр, truncation maxEntities/maxEdges,
 *      определение в другом файле, небезопасные пути пропускаются, kind-маппинг,
 *      line 0 → null по `||`-семантике);
 *   4. mergeScipWithGraphify: регистрозависимость, неоднозначность, rewrite рёбер
 *      и контрактов (entrypoint/signature/прочие), дедуп с exact-приоритетом,
 *      capabilities (ранги строк, OR bool, перезапись несовпадающих типов).
 */

import assert from 'node:assert/strict';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_SCIP_FORCE_JS = '1';
const S = await import('../../lib/structure-scip.mjs');

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
  process.env.AI_REVIEWER_SCIP_FORCE_JS = '1';
  try {
    return { ok: true, value: rt(fn(...args)) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_SCIP_FORCE_JS;
  }
};

/** Rust-сторона через обёртку; текст ошибки вытаскиваем из {"error": ...} в stderr. */
const runRust = (fn, ...args) => {
  delete process.env.AI_REVIEWER_SCIP_FORCE_JS;
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
// Мини-энкодер protobuf wire-format для сборки SCIP-буферов
// ---------------------------------------------------------------------------

const varint = (value) => {
  const tmp = [];
  let v = value;
  do {
    let b = v % 128;
    v = Math.floor(v / 128);
    if (v > 0) b |= 0x80;
    tmp.push(b);
  } while (v > 0);
  return Buffer.from(tmp);
};
const tag = (field, wire) => varint(field * 8 + wire);
const varintField = (field, value) => Buffer.concat([tag(field, 0), varint(value)]);
const bytesField = (field, payload) => Buffer.concat([tag(field, 2), varint(payload.length), payload]);
const fixed64Field = (field) => Buffer.concat([tag(field, 1), Buffer.alloc(8, 0xAB)]);
const fixed32Field = (field) => Buffer.concat([tag(field, 5), Buffer.alloc(4, 0xCD)]);

// ---------------------------------------------------------------------------
// Корпус 1: decodeScipIndex на hand-built буферах
// ---------------------------------------------------------------------------

function buildFullIndex() {
  const singleRange = Buffer.concat([
    varintField(1, 4), // line=4 → 5
    varintField(2, 2), // startCharacter
    varintField(3, 9), // endCharacter
  ]);
  const multiRange = Buffer.concat([
    varintField(1, 1), // line=1 → 2
    varintField(2, 0), // startCharacter
    varintField(3, 3), // endLine=3 → 4
    varintField(4, 12), // endCharacter
  ]);
  const legacyPacked4 = Buffer.concat([varint(7), varint(0), varint(9), varint(5)]);
  const legacyPacked3 = Buffer.concat([varint(2), varint(3), varint(8)]);
  const legacyPackedShort = varint(1); // одиночное значение → range null

  const fooOcc = Buffer.concat([
    bytesField(2, Buffer.from('npm pkg 1 src/a.ts/foo().')),
    varintField(3, 1),
    bytesField(8, singleRange),
    // дубликат: first() берёт первый single range
    bytesField(8, multiRange),
  ]);
  const useOcc = Buffer.concat([
    bytesField(2, Buffer.from('npm pkg 1 src/a.ts/foo().')),
    varintField(3, 0), // не definition
    bytesField(9, multiRange),
  ]);
  const legacyOcc = Buffer.concat([
    bytesField(2, Buffer.from('npm pkg 1 src/a.ts/Legacy#.')),
    varintField(3, 1),
    bytesField(1, legacyPacked4),
  ]);
  const legacyOcc3 = Buffer.concat([
    bytesField(2, Buffer.from('npm pkg 1 src/a.ts/Legacy3#.')),
    varintField(3, 1),
    bytesField(1, legacyPacked3),
  ]);
  const legacyOccShort = Buffer.concat([
    bytesField(2, Buffer.from('npm pkg 1 src/a.ts/LegacyS#.')),
    varintField(3, 1),
    bytesField(1, legacyPackedShort),
  ]);

  const relationToBar = Buffer.concat([
    bytesField(1, Buffer.from('npm pkg 1 src/a.ts/Bar#.')),
    varintField(2, 1), // isReference
    varintField(3, 1), // isImplementation
    varintField(4, 0), // isTypeDefinition
    varintField(5, 1), // isDefinition
  ]);
  const fooSig = bytesField(5, Buffer.from('function foo(a: string): number'));
  const fooSymbol = Buffer.concat([
    bytesField(1, Buffer.from('npm pkg 1 src/a.ts/foo().')),
    bytesField(3, Buffer.from('doc line 1')),
    bytesField(3, Buffer.from('doc line 2')),
    bytesField(4, relationToBar),
    varintField(5, 17), // function
    bytesField(6, Buffer.from('foo')),
    bytesField(7, fooSig),
    bytesField(8, Buffer.from('npm pkg 1 src/a.ts/')),
    // wire 1/5 пропускаются без записи
    fixed64Field(9),
    fixed32Field(10),
  ]);
  const noDisplayNameSymbol = Buffer.concat([
    bytesField(1, Buffer.from('npm pkg 1 src/a.ts/noDisplay().')),
    varintField(5, 80), // static_method
  ]);
  const unknownKindSymbol = Buffer.concat([
    bytesField(1, Buffer.from('npm pkg 1 src/a.ts/weird().')),
    varintField(5, 200), // неизвестный kind → 'symbol'
    bytesField(6, Buffer.from('weird')),
  ]);
  // невалидный UTF-8 в displayName: обе стороны дают U+FFFD
  const invalidUtf8Symbol = Buffer.concat([
    bytesField(1, Buffer.from('npm pkg 1 src/a.ts/bad().')),
    varintField(5, 17),
    bytesField(6, Buffer.from([0x66, 0x6f, 0xC3, 0x28])),
  ]);

  const docA = Buffer.concat([
    bytesField(1, Buffer.from('src/a.ts')),
    bytesField(2, fooOcc),
    bytesField(2, useOcc),
    bytesField(2, legacyOcc),
    bytesField(2, legacyOcc3),
    bytesField(2, legacyOccShort),
    bytesField(3, fooSymbol),
    bytesField(3, noDisplayNameSymbol),
    bytesField(3, unknownKindSymbol),
    bytesField(3, invalidUtf8Symbol),
    bytesField(4, Buffer.from('typescript')),
  ]);

  const barDefOcc = Buffer.concat([
    bytesField(2, Buffer.from('npm pkg 1 src/a.ts/Bar#.')),
    varintField(3, 1),
    bytesField(8, Buffer.concat([varintField(1, 0), varintField(2, 0), varintField(3, 0)])),
  ]);
  const extRelation = Buffer.concat([
    bytesField(1, Buffer.from('npm pkg 1 ext/Ext#.')),
    varintField(2, 1),
  ]);
  const barSymbol = Buffer.concat([
    bytesField(1, Buffer.from('npm pkg 1 src/a.ts/Bar#.')),
    varintField(5, 7), // class
    bytesField(6, Buffer.from('Bar')),
    bytesField(4, extRelation),
  ]);
  const docB = Buffer.concat([
    bytesField(1, Buffer.from('src/b.ts')),
    bytesField(2, barDefOcc),
    bytesField(3, barSymbol),
  ]);

  const extSymbol = Buffer.concat([
    bytesField(1, Buffer.from('npm pkg 1 ext/Ext#.')),
    varintField(5, 21), // interface
    bytesField(3, Buffer.from('external docs')),
  ]);

  return Buffer.concat([
    bytesField(2, docA),
    bytesField(2, docB),
    bytesField(3, extSymbol),
    // metadata до документов: порядок полей не должен влиять
    bytesField(1, Buffer.from('scip-typescript 0.4.0')),
  ]);
}

check('decodeScipIndex: full hand-built index', S.decodeScipIndex, buildFullIndex());
check('decodeScipIndex: empty buffer', S.decodeScipIndex, Buffer.alloc(0));
check('decodeScipIndex: raw Uint8Array input', S.decodeScipIndex, new Uint8Array([0x10, 0x00]));

// ---------------------------------------------------------------------------
// Корпус 2: граничные случаи varint и ошибки (тексты совпадают посимвольно)
// ---------------------------------------------------------------------------

check('varint: 2^53-1 последнее безопасное', S.decodeScipIndex, Buffer.concat([tag(1, 0), varint(2 ** 53 - 1)]));
check('varint: 2^53 уже ошибка', S.decodeScipIndex, Buffer.concat([tag(1, 0), varint(2 ** 53)]));
check('varint: величина на сдвиге 49 (вектор из теста)', S.decodeScipIndex, Buffer.from([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]));
check('varint: guard сдвига > 53', S.decodeScipIndex, Buffer.concat([tag(1, 0), Buffer.from([0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01])]));
check('varint: truncated', S.decodeScipIndex, Buffer.from([0x80]));
check('varint: пустой с незавершённым тегом', S.decodeScipIndex, Buffer.from([0x0A]));
check('field: truncated length-delimited', S.decodeScipIndex, Buffer.from([0x0A, 0x05, 0x61]));
check('field: truncated fixed64 (вектор из теста)', S.decodeScipIndex, Buffer.from([0b00001001, 0x00, 0x00]));
check('field: truncated fixed32', S.decodeScipIndex, Buffer.from([0x0D, 0x00]));
for (const wire of [3, 4, 6, 7]) {
  check(`wire type ${wire} unsupported`, S.decodeScipIndex, Buffer.from([wire]));
}

// ---------------------------------------------------------------------------
// Корпус 3: extractScipFacts
// ---------------------------------------------------------------------------

const TWO_DOCS = {
  documents: [{
    relativePath: 'src/a.ts',
    language: 'typescript',
    occurrences: [{
      symbol: 'npm pkg 1 src/a.ts/foo().',
      roles: 1,
      range: { line: 1, endLine: 1 },
    }],
    symbols: [{
      symbol: 'npm pkg 1 src/a.ts/foo().',
      displayName: 'foo',
      kind: 17,
      signature: 'function foo(): void',
      relationships: [],
    }],
  }, {
    relativePath: 'src/b.ts',
    language: 'typescript',
    occurrences: [{
      symbol: 'npm pkg 1 src/b.ts/bar().',
      roles: 1,
      range: { line: 1, endLine: 1 },
    }],
    symbols: [{
      symbol: 'npm pkg 1 src/b.ts/bar().',
      displayName: 'bar',
      kind: 17,
      signature: 'function bar(): void',
      relationships: [],
    }],
  }],
};

const IMPLEMENTS_INDEX = {
  documents: [{
    relativePath: 'src/a.ts',
    language: 'typescript',
    occurrences: [
      { symbol: 'npm pkg 1 src/a.ts/foo().', roles: 1, range: { line: 10, endLine: 12 } },
      { symbol: 'npm pkg 1 src/a.ts/Bar#.', roles: 1, range: { line: 5, endLine: 5 } },
      // definition вне scope и вне файла символа: fallback definitions[0]
      { symbol: 'npm pkg 1 shared/Util#.'.replace('shared', 'src'), roles: 1, range: { line: 3, endLine: 3 } },
    ],
    symbols: [
      {
        symbol: 'npm pkg 1 src/a.ts/foo().',
        displayName: 'foo',
        kind: 17,
        signature: 'function foo(): void',
        relationships: [
          { symbol: 'npm pkg 1 src/a.ts/Bar#.', isImplementation: true, isReference: true, isDefinition: true },
          { symbol: 'npm pkg 1 ext/Missing#.', isImplementation: true },
        ],
      },
      { symbol: 'npm pkg 1 src/a.ts/Bar#.', displayName: 'Bar', kind: 7, signature: null, relationships: [] },
      { symbol: 'npm pkg 1 src/Util#.', displayName: '', kind: 53, signature: 'trait Util', relationships: [] },
      { symbol: 'npm pkg 1 src/a.ts/ghost().', displayName: 'ghost', kind: 17, signature: null, relationships: [] },
    ],
  }, {
    // небезопасный путь — документ пропускается целиком
    relativePath: '../escape.ts',
    occurrences: [],
    symbols: [{ symbol: 'x', displayName: 'x', kind: 17, signature: null, relationships: [] }],
  }, {
    // Windows-разделители и точка в пути
    relativePath: 'src\\deep\\..\\lib\\x.ts',
    occurrences: [{ symbol: 'npm pkg 1 src/lib/x.ts/x().', roles: 1, range: { line: 0, endLine: 0 } }],
    symbols: [{ symbol: 'npm pkg 1 src/lib/x.ts/x().', displayName: '', kind: 81, signature: null, relationships: [] }],
  }],
};

check('extractScipFacts: two docs (кейс из тестов)', S.extractScipFacts, TWO_DOCS);
check('extractScipFacts: implements/definition/много kind/unsafe paths/line 0', S.extractScipFacts, IMPLEMENTS_INDEX);
check('extractScipFacts: scope filter exact', S.extractScipFacts, IMPLEMENTS_INDEX, { scope: 'src/lib' });
check('extractScipFacts: scope filter trailing slash', S.extractScipFacts, IMPLEMENTS_INDEX, { scope: 'src/' });
check('extractScipFacts: scope без совпадений', S.extractScipFacts, IMPLEMENTS_INDEX, { scope: 'docs' });
check('extractScipFacts: maxEntities=0', S.extractScipFacts, IMPLEMENTS_INDEX, { maxEntities: 0 });
check('extractScipFacts: maxEntities=2', S.extractScipFacts, IMPLEMENTS_INDEX, { maxEntities: 2 });
check('extractScipFacts: maxEdges=0', S.extractScipFacts, IMPLEMENTS_INDEX, { maxEdges: 0 });
check('extractScipFacts: maxEdges=1', S.extractScipFacts, IMPLEMENTS_INDEX, { maxEdges: 1 });
check('extractScipFacts: null index', S.extractScipFacts, null);
check('extractScipFacts: index без documents', S.extractScipFacts, {});
check('extractScipFacts: decoded binary index', S.extractScipFacts, S.decodeScipIndex(buildFullIndex()));

// ---------------------------------------------------------------------------
// Корпус 4: mergeScipWithGraphify
// ---------------------------------------------------------------------------

const entity = (id, label, extra = {}) => ({
  observedId: id, lineageId: id, boundaryId: 'b1', label, qualifiedName: label,
  kind: 'function', path: 'src/a.ts', line: 1, endLine: 2, signature: null,
  community: 'c1', isGodNode: false, confidence: 'extracted',
  evidence: [`graphify:${id}`], externalIds: {}, ...extra,
});

const GRAPH_BASE = {
  entities: [entity('g_upper', 'User'), entity('g_lower', 'user')],
  dependencies: [{
    id: 'edge:g_upper:g_lower:calls', from: 'g_upper', to: 'g_lower', relation: 'calls',
    confidence: 'extracted', evidence: [],
  }],
  contracts: [{
    id: 'contract:entrypoint:g_upper', entityRef: 'g_upper', kind: 'entrypoint',
    targetRef: null, signature: null, confidence: 'extracted', evidence: [],
  }],
  warnings: ['graph warning'],
  truncated: false,
  capabilities: { symbols: 'inferred', topology: true, calls: true, notes: 'graph-note' },
};
const SCIP_CASE = {
  entities: [{ ...entity('s_upper', 'User'), confidence: 'exact', evidence: ['scip:User'], boundaryId: null, community: null }],
  dependencies: [],
  contracts: [],
  warnings: [],
  truncated: false,
  capabilities: { symbols: 'exact', imports: false, notes: null },
};

check('merge: регистрозависимость строгого ключа', S.mergeScipWithGraphify, SCIP_CASE, GRAPH_BASE);
check('merge: регистр не совпал, нестрогий ключ один — сливается', S.mergeScipWithGraphify, {
  ...SCIP_CASE,
  entities: [{ ...entity('s_lower', 'user', {}), confidence: 'exact', evidence: ['scip:user'], boundaryId: null, community: null }],
}, GRAPH_BASE);

const GRAPH_TWINS = {
  entities: [entity('g1', 'handler'), entity('g2', 'handler')],
  dependencies: [],
  contracts: [],
  warnings: [],
  truncated: false,
  capabilities: {},
};
check('merge: неоднозначный кандидат не сливается', S.mergeScipWithGraphify, {
  ...SCIP_CASE,
  entities: [{ ...entity('s1', 'handler'), confidence: 'exact', evidence: ['scip:handler'] }],
}, GRAPH_TWINS);

// rewrite рёбер/контрактов + дедуп с exact-приоритетом
const graphifyLike = {
  entities: [
    { observedId: 'ge1', lineageId: 'ge1', boundaryId: null, label: 'foo', qualifiedName: 'pkg/foo', kind: 'function', path: 'src/a.ts', line: 2, endLine: 2, signature: null, community: null, isGodNode: false, confidence: 'extracted', evidence: ['graphify:foo'], externalIds: {} },
    { observedId: 'ge2', lineageId: 'ge2', boundaryId: null, label: 'bar', qualifiedName: 'pkg/bar', kind: 'function', path: 'src/b.ts', line: 1, endLine: 1, signature: null, community: null, isGodNode: false, confidence: 'extracted', evidence: ['graphify:bar'], externalIds: {} },
  ],
  dependencies: [
    { id: 'old-e1', from: 'ge1', to: 'ge2', relation: 'calls', confidence: 'extracted', evidence: ['g'] },
    // то же (from,to,relation), что даст SCIP: exact перезаписывает
    { id: 'old-e2', from: 'geX', to: 'ge2', relation: 'calls', confidence: 'extracted', evidence: ['g2'] },
  ],
  contracts: [
    { id: 'old-c1', entityRef: 'ge1', kind: 'entrypoint', targetRef: null, signature: null, confidence: 'extracted', evidence: [] },
    { id: 'old-c2', entityRef: 'ge1', kind: 'implements', targetRef: 'ge2', signature: null, confidence: 'extracted', evidence: [] },
    { id: 'old-c3', entityRef: 'ge2', kind: 'signature', targetRef: null, signature: 'function bar(): void', confidence: 'extracted', evidence: [] },
  ],
  warnings: [],
  truncated: false,
  capabilities: { symbols: 'inferred', calls: true, positionEncoding: 'utf-16' },
};
const scipFactsLike = S.extractScipFacts(TWO_DOCS);
// SCIP-сущности с совпадающими path+label сливаются с ge1/ge2 → rewrite рёбер и контрактов
check('merge: rewrite edges/contracts + exact dedup + capabilities', S.mergeScipWithGraphify, scipFactsLike, graphifyLike);

// дедуп: не-exact НЕ перезаписывает exact
check('merge: non-exact не трогает exact', S.mergeScipWithGraphify, {
  entities: [], dependencies: [
    { id: 's1', from: 'a', to: 'b', relation: 'calls', confidence: 'exact', evidence: [] },
  ], contracts: [], warnings: [], truncated: false, capabilities: {},
}, {
  entities: [], dependencies: [
    { id: 'g1', from: 'a', to: 'b', relation: 'calls', confidence: 'extracted', evidence: [] },
  ], contracts: [], warnings: [], truncated: true, capabilities: { symbols: 'none' },
});

// null-поля SCIP не затирают graphify; evidence union отсортирован
check('merge: null-поля SCIP сохраняют graphify-значения', S.mergeScipWithGraphify, {
  entities: [{
    observedId: 's1', lineageId: 's1', boundaryId: null, label: 'keep', qualifiedName: 'q-scip',
    kind: 'method', path: 'src/a.ts', line: null, endLine: null, signature: null, community: null,
    isGodNode: null, confidence: 'exact', evidence: ['z-evidence', 'a-evidence', 'z-evidence'],
    externalIds: { scip: 'sym' },
  }],
  dependencies: [], contracts: [], warnings: [], truncated: false, capabilities: {},
}, {
  entities: [{
    observedId: 'g1', lineageId: 'g1', boundaryId: 'b1', label: 'keep', qualifiedName: 'q-graph',
    kind: 'function', path: 'src/a.ts', line: 7, endLine: 9, signature: 'sig', community: 'c',
    isGodNode: false, confidence: 'extracted', evidence: ['m-evidence'], externalIds: { graphify: 'n1' },
  }],
  dependencies: [], contracts: [], warnings: ['w1'], truncated: false, capabilities: { imports: true },
});

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
