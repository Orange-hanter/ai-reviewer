// Юнит-тесты чистых функций pr-review.mjs (регрессоопасная логика: парсинг диффа, JSON-repair,
// чанкинг, фильтр ложных находок, консенсус). Запуск: node --test ops/pr-review.test.mjs
//
// Консенсус зависит от MODELS/CONSENSUS_MIN (читаются из env при импорте), поэтому фиксируем
// два ревьюера ДО импорта модуля (динамический import после установки env).
process.env.OLLAMA_MODELS = 'm1,m2';

import { test } from 'node:test';
import assert from 'node:assert/strict';

const {
  parseDiff, extractChangedSymbols, splitSegmentByHunks, buildChunks,
  extractJsonObject, filterFalsePositiveFindings, clusterFindings,
} = await import('../bin/pr-review.mjs');

const SAMPLE_DIFF = `diff --git a/foo.ts b/foo.ts
--- a/foo.ts
+++ b/foo.ts
@@ -1,2 +1,3 @@
 const a = 1;
-const b = 2;
+const b = 3;
+export function foo() { return a; }
`;

test('parseDiff: validLines/changed/annotated', () => {
  const { files, annotated, changedText } = parseDiff(SAMPLE_DIFF);
  const rec = files.get('foo.ts');
  assert.ok(rec, 'файл распознан');
  for (const ln of [1, 2, 3]) assert.ok(rec.validLines.has(ln), `строка ${ln} валидна для inline`);
  assert.equal(rec.changed.length, 3);               // -b, +b, +foo
  assert.match(annotated, /^\s+1: /m);               // префикс с номером строки RIGHT-стороны
  assert.ok(changedText.includes('export function foo'));
});

test('extractChangedSymbols: только экспортируемые ≥3 симв', () => {
  const changed = `export function foo() {}
export const barBaz = async () => {}
export class Widget {}
export interface Shape {}
const localHelper = () => {}
export const x = 1;`;   // arrow — да; голый const без () — нет; x короткий — нет
  const syms = extractChangedSymbols(changed);
  assert.ok(syms.includes('foo'));
  assert.ok(syms.includes('barBaz'));
  assert.ok(syms.includes('Widget'));
  assert.ok(syms.includes('Shape'));
  assert.ok(!syms.includes('localHelper'), 'локальные не берём');
  assert.ok(!syms.includes('x'), 'короткие (<3) не берём');
});

test('extractJsonObject: чистый и в fence', () => {
  assert.deepEqual(extractJsonObject('{"findings":[]}'), { findings: [] });
  assert.deepEqual(extractJsonObject('```json\n{"a":1}\n```'), { a: 1 });
});

test('extractJsonObject: repair оборванного хвоста', () => {
  // оборванная строка + незакрытые скобки
  const o = extractJsonObject('{"findings":[{"file":"a.ts","line":10,"body":"partial no close');
  assert.equal(o.findings[0].file, 'a.ts');
  // висячая запятая перед обрывом
  const o2 = extractJsonObject('{"findings":[{"file":"a.ts","line":1},');
  assert.equal(o2.findings.length, 1);
  // фигурная скобка ВНУТРИ строки не должна ломать баланс
  const o3 = extractJsonObject('{"findings":[{"body":"use rm -rf { danger"');
  assert.ok(o3.findings[0].body.includes('danger'));
});

test('extractJsonObject: безнадёжно битый бросает truncated', () => {
  assert.throws(() => extractJsonObject('{"findings":[{"line":'), (e) => e.truncated === true);
  assert.throws(() => extractJsonObject('нет json тут'), /нет JSON/);
});

test('splitSegmentByHunks: мелкий сегмент = 1 кусок, крупный режется с повтором заголовка', () => {
  const small = 'diff --git a/f b/f\n+++ b/f\n@@ -1 +1 @@\n+x';
  assert.deepEqual(splitSegmentByHunks(small, 10_000), [small]);

  const seg = [
    'diff --git a/f.ts b/f.ts', '--- a/f.ts', '+++ b/f.ts',
    '@@ -1,1 +1,1 @@', '+aaaaaaaaaaaaaaaaaaaa',
    '@@ -5,1 +5,1 @@', '+bbbbbbbbbbbbbbbbbbbb',
  ].join('\n');
  const header = 'diff --git a/f.ts b/f.ts\n--- a/f.ts\n+++ b/f.ts';
  const pieces = splitSegmentByHunks(seg, header.length + 25);   // влезает по одному хунку
  assert.equal(pieces.length, 2);
  for (const p of pieces) assert.ok(p.startsWith('diff --git a/f.ts'), 'заголовок в каждой части');
});

test('buildChunks: мелкий PR = 1 чанк, контент прикреплён', () => {
  const { annotated } = parseDiff(SAMPLE_DIFF);
  const contentMap = new Map([['foo.ts', '### foo.ts\n```\nconst a=1;\n```']]);
  const limits = { maxDiffBytes: 200_000, maxContextBytes: 180_000, chunkBytes: 40_000, maxChunks: 40 };
  const chunks = buildChunks(annotated, contentMap, limits);
  assert.equal(chunks.length, 1);
  assert.ok(chunks[0].fileContents.includes('### foo.ts'));
});

test('filterFalsePositiveFindings: гасит ложные e2e и file-absent, оставляет реальные', () => {
  const changedPaths = ['app/e2e/foo.spec.ts', 'server/foo.ts'];
  const findings = [
    { file: 'server/foo.ts', line: 5, severity: 'minor', title: 'нет e2e тестов', body: '' },
    { file: 'server/foo.ts', line: 6, severity: 'minor', title: 'проблема', body: 'файл foo.ts отсутствует в диффе' },
    { file: 'server/foo.ts', line: 7, severity: 'major', title: 'реальный баг', body: 'null pointer при пустом вводе' },
  ];
  const out = filterFalsePositiveFindings(findings, changedPaths);
  const titles = out.map((f) => f.title);
  assert.ok(!titles.includes('нет e2e тестов'), 'ложный e2e отфильтрован');
  assert.ok(!titles.includes('проблема'), 'ложный file-absent отфильтрован');
  assert.ok(titles.includes('реальный баг'), 'реальная находка сохранена');
});

test('clusterFindings: консенсус (2 модели) и single-блок для critical', () => {
  const gate = { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 2, confidenceMin: 0.8 };
  const findings = [
    { file: 'a.ts', line: 10, severity: 'major', model: 'm1', confidence: 0.9 },
    { file: 'a.ts', line: 11, severity: 'major', model: 'm2', confidence: 0.9 },
    { file: 'b.ts', line: 1, severity: 'major', model: 'm1', confidence: 0.9 },
    { file: 'c.ts', line: 1, severity: 'critical', model: 'm1', confidence: 0.9 },
  ];
  clusterFindings(findings, gate);
  const byFile = Object.fromEntries(findings.map((f) => [f.file, f]));
  assert.equal(byFile['a.ts'].consensusBlock, true, 'major×2 модели → блок');
  assert.equal(byFile['b.ts'].consensusBlock, false, 'major×1 модель → не блок');
  assert.equal(byFile['c.ts'].consensusBlock, true, 'critical×1 модель → блок (single-tier)');
});
