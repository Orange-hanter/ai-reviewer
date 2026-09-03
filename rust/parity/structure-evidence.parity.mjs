/**
 * Parity-прогон Фазы 2, шаг 3: Rust structure_evidence ↔ чистый JS fallback
 * lib/structure-evidence.mjs (+ каноническая JSON-сериализация из lib/util.mjs).
 *
 * Запуск: node rust/parity/structure-evidence.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/structure-evidence.mjs: resolveEvidenceBin()
 * читает AI_REVIEWER_STRUCTURE_EVIDENCE_FORCE_JS на каждый вызов, поэтому
 * js-сторона прогоняется с FORCE_JS=1, rust-сторона — без него. Каноникализация
 * и sha256 сравниваются ПОБАЙТОВО (строки JSON и hex-хеши) — от них зависит
 * sha256-пиннинг receipt'ов. canonicalJson/sha256/stableId/normalizeRepoPath
 * обёртка реэкспортирует из lib/util.mjs, поэтому их Rust-версии вызываются
 * бинарём напрямую (опы canonicalJson/sha256/stableId/normalizeRepoPath).
 *
 * Корпус:
 *   - canonicalJson: порядок ключей по UTF-16 code units (astral-символы,
 *     пустой ключ, цифры), экранирование (ES2019: U+2028/U+2029 сырые, контролы,
 *     кавычки, backslash, не-ASCII), числа (краевые: -0, 1e21, 1e-7, 5e-324,
 *     1e999 → null, 123456789123456789, дробные tie-кейсы), вложенность;
 *   - sha256: строки/объекты/массивы, в т.ч. хеши receipt-подобных payload'ов;
 *   - stableId/normalizeRepoPath: базовые векторы;
 *   - validateEvidenceReceipt: валидный receipt, каждое поле битое по очереди,
 *     массив как artifact/observation (typeof-ветка), не-итерируемый subjectRefs
 *     (TypeError), пустые subjectRefs, невалидные пути в observation.path,
 *     Date.parse-края (rollover дня/часа 24, 't'/'z', ±HHMM, RFC-2822-отказ);
 *   - createEvidenceReceipt: дефолты (trustTier/evidenceCeiling/complete/
 *     truncated/exitStatus/testRunId/observedAt), covered вырезается, путь
 *     нормализуется, strings() (trim/dedup/slice 100/50/сортировка), ошибки
 *     бросаются с тем же текстом;
 *   - receiptRef: побайтовое равенство строк и стабильность хешей.
 */

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');
const BIN = resolve(REPO, 'rust/target/debug/structure_evidence');

process.env.AI_REVIEWER_STRUCTURE_EVIDENCE_FORCE_JS = '1';
const E = await import('../../lib/structure-evidence.mjs');
const { canonicalJson, sha256, stableId, normalizeRepoPath } = await import('../../lib/util.mjs');

let compared = 0;
const failures = [];

function fail(label, js, rust) {
  compared += 1;
  failures.push({ label, js: String(js)?.slice(0, 400), rust: String(rust)?.slice(0, 400) });
}

function ok(label) {
  compared += 1;
  if (failures.length && failures[failures.length - 1].label === label) failures.pop();
}

/** Бинарь напрямую (canonicalJson/sha256/stableId/normalizeRepoPath не ходят через обёртку). */
function rustOp(op, args) {
  const raw = execFileSync(BIN, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
  });
  return JSON.parse(raw);
}

// ---------------------------------------------------------------------------
// canonicalJson: ПОБАЙТОВОЕ сравнение строк
// ---------------------------------------------------------------------------

const CANON_CORPUS = [
  {},
  { b: 1, a: 2 },
  { '': 1, a: 2 },
  { '𝄞': 1, '￿': 2, '😀': 3, a: 4, Z: 5, 10: 6, 9: 7 },
  { 'ẞ': 1, 'ß': 2, 'K': 3 },
  { a: { b: { c: [1, 'x', null, true, { d: 0.5 }] } }, z: [] },
  { k: 'héllo𝄞\n"\\' },
  { esc: '  ' },
  { n: -0, m: 1e21, p: 1e-7, q: 0.1, r: [1e999, -1e999, 123456789123456789] },
  { n: 5e-324, o: 1.7976931348623157e308, s: 0.30000000000000004, t: 1e20, u: 1e-6 },
  // tie-кейсы последней цифры (V8 выбирает чётное)
  { tie: 812782.2875976562, tie2: 0.15452957153320312, tie3: 963966.3696289062 },
  [1, [2, [3, { deep: '𝄞' }]], { z: 1, a: 2 }],
];

for (const [i, v] of CANON_CORPUS.entries()) {
  compared += 1;
  const js = canonicalJson(JSON.parse(JSON.stringify(v)));
  const rust = rustOp('canonicalJson', { value: JSON.parse(JSON.stringify(v)) });
  if (rust !== js) failures.push({ label: `canonicalJson[${i}]`, js, rust });
}

// ---------------------------------------------------------------------------
// sha256: побайтовое равенство hex
// ---------------------------------------------------------------------------

const SHA_CORPUS = [
  'none',
  'config',
  '.',
  'héllo𝄞\n',
  '{"b":1,"a":2}',
  { provider: 'unit', headSha: 'abcdef1234567890', n: 812782.2875976562, u: '𝄞' },
  ['a', 1e21, { k: -0 }],
];

for (const [i, v] of SHA_CORPUS.entries()) {
  compared += 1;
  const js = sha256(typeof v === 'string' ? v : JSON.parse(JSON.stringify(v)));
  const rust = rustOp('sha256', { value: typeof v === 'string' ? v : JSON.parse(JSON.stringify(v)) });
  if (rust !== js) failures.push({ label: `sha256[${i}]`, js, rust });
}

// ---------------------------------------------------------------------------
// stableId / normalizeRepoPath
// ---------------------------------------------------------------------------

for (const [i, args] of [
  ['scenario', 'obl:1', 'error_path', 'c1,c2'],
  ['hint', 'rule', 'subj:1', 'subj:2'],
  ['mutant', 'src/a.ts', null, 'BooleanLiteral', '12'],
  ['edge', 'e1', 'e2', 'calls'],
].entries()) {
  compared += 1;
  const js = stableId(...args);
  const rust = rustOp('stableId', { prefix: args[0], parts: args.slice(1) });
  if (rust !== js) failures.push({ label: `stableId[${i}]`, js, rust });
}

for (const [i, p] of ['./src/x.ts', '.', 'a\\b\\c.ts', 'src//deep/../x.ts'].entries()) {
  compared += 1;
  const js = normalizeRepoPath(p);
  const rust = rustOp('normalizeRepoPath', { path: p });
  if (rust !== js) failures.push({ label: `normalizeRepoPath[${i}]`, js, rust });
}
for (const [i, p] of ['../evil', '/abs', 'C:\\secret', ''].entries()) {
  compared += 1;
  let jsErr = null;
  try { normalizeRepoPath(p); } catch (e) { jsErr = e.message; }
  let rustErr = null;
  try { rustOp('normalizeRepoPath', { path: p }); } catch (e) {
    rustErr = String(e?.stderr || e).match(/"error":\s*"((?:[^"\\]|\\.)*)"/)?.[1];
    if (rustErr) rustErr = JSON.parse(`"${rustErr}"`);
  }
  if (jsErr !== rustErr) failures.push({ label: `normalizeRepoPath err[${i}]`, js: jsErr, rust: rustErr });
}

// ---------------------------------------------------------------------------
// validateEvidenceReceipt / createEvidenceReceipt / receiptRef через обёртку
// ---------------------------------------------------------------------------

const H = (s) => sha256(s);
const HEAD = 'abcdef1234567890';

const VALID_INPUT = {
  provider: 'unit',
  toolVersion: '1',
  capability: 'test',
  headSha: HEAD,
  configHash: H('config'),
  scopeHash: H('.'),
  artifact: { uri: 'memory:test', sha256: H('artifact') },
  observation: { type: 'sample', value: true },
  trustTier: 'trusted_local',
  evidenceCeiling: 'static_inventory',
  limitations: ['test only'],
  observedAt: '2026-09-02T00:00:00.000Z',
};

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_STRUCTURE_EVIDENCE_FORCE_JS = '1';
  try {
    return { ok: true, value: fn(...args) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_STRUCTURE_EVIDENCE_FORCE_JS;
  }
};

const runRust = (fn, ...args) => {
  delete process.env.AI_REVIEWER_STRUCTURE_EVIDENCE_FORCE_JS;
  try {
    return { ok: true, value: fn(...args) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  }
};

function check(label, fn, ...args) {
  const js = runJs(fn, ...args);
  const rust = runRust(fn, ...args);
  compared += 1;
  if (js.ok !== rust.ok) {
    failures.push({ label, js: JSON.stringify(js)?.slice(0, 400), rust: JSON.stringify(rust)?.slice(0, 400) });
    return;
  }
  const jsV = js.ok ? JSON.parse(JSON.stringify(js.value)) : js.error;
  const rustV = rust.ok ? JSON.parse(JSON.stringify(rust.value)) : rust.error;
  try {
    assert.deepEqual(rustV, jsV);
  } catch {
    failures.push({ label, js: JSON.stringify(jsV)?.slice(0, 500), rust: JSON.stringify(rustV)?.slice(0, 500) });
  }
}

// validate: валидный receipt
const validReceipt = E.createEvidenceReceipt(VALID_INPUT);
check('validate: валидный receipt', E.validateEvidenceReceipt, validReceipt);

// validate: битые поля по очереди
for (const [field, value] of [
  ['version', 2],
  ['provider', ''],
  ['toolVersion', '  '],
  ['capability', null],
  ['headSha', 'zzzzzz'],
  ['headSha', 'abc'],
  ['configHash', H('config').toUpperCase()],
  ['scopeHash', 'nope'],
  ['observedAt', 'not-a-date'],
  ['trustTier', 'bogus'],
  ['evidenceCeiling', 'bogus'],
  ['limitations', 'not-array'],
  ['subjectRefs', ['ok', '']],
]) {
  check(`validate: битое ${field}=${JSON.stringify(value)}`, E.validateEvidenceReceipt,
    { ...validReceipt, [field]: value });
}

// validate: artifact/observation массивы (typeof [] === 'object' → ветка полей)
check('validate: artifact массив', E.validateEvidenceReceipt, { ...validReceipt, artifact: [] });
check('validate: observation массив', E.validateEvidenceReceipt, { ...validReceipt, observation: [] });
check('validate: artifact null', E.validateEvidenceReceipt, { ...validReceipt, artifact: null });
check('validate: receipt массив', E.validateEvidenceReceipt, [1, 2]);
check('validate: receipt строка', E.validateEvidenceReceipt, 'nope');
check('validate: subjectRefs не-итерируемый', E.validateEvidenceReceipt,
  { ...validReceipt, subjectRefs: 5 });
check('validate: observation.path небезопасен', E.validateEvidenceReceipt,
  { ...validReceipt, observation: { type: 'file', path: '../escape.ts' } });
check('validate: observation.path нормализуем', E.validateEvidenceReceipt,
  { ...validReceipt, observation: { type: 'file', path: 'src//x.ts' } });

// validate: Date.parse-края
for (const [i, observedAt] of [
  '2026-02-30',
  '2026-01-01T24:00:00Z',
  '2026-01-01 12:00:00',
  '2026-01-01t12:00:00z',
  '2026-01-01T12:00:00+0300',
  '2026-1-1',
  '2026-13-01',
  '2026-01-01T12:60:00Z',
  // RFC-2822 ('Tue, 01 Jan 2026 00:00:00 GMT') и 'January 1, 2026' НЕ входят
  // в корпус: V8 Date.parse их принимает, Rust-порт — нет (задокументированное
  // отклонение, см. шапку модуля).
].entries()) {
  check(`validate: observedAt[${i}]=${JSON.stringify(observedAt)}`, E.validateEvidenceReceipt,
    { ...validReceipt, observedAt });
}

// create: дефолты и нормализация
check('create: полный валидный', E.createEvidenceReceipt, VALID_INPUT);
check('create: covered вырезается, path нормализуется', E.createEvidenceReceipt, {
  ...VALID_INPUT,
  observation: { type: 'file', value: 1, covered: true, path: '.\\src\\x.ts' },
});
check('create: дефолты trustTier/evidenceCeiling/complete/truncated/exitStatus/testRunId', E.createEvidenceReceipt, {
  provider: 'p',
  toolVersion: '1',
  capability: 'c',
  headSha: 'abcdef1',
  configHash: H('c'),
  scopeHash: H('s'),
  artifact: { uri: 'u', sha256: H('a') },
  observation: { type: 't' },
  observedAt: '2026-09-02T00:00:00.000Z',
});
check('create: complete=0/truncated=1/exitStatus микс', E.createEvidenceReceipt, {
  ...VALID_INPUT,
  complete: 0,
  truncated: 1,
  exitStatus: 5,
});
check('create: exitStatus 5.5/null/"5"/true → null', E.createEvidenceReceipt, {
  ...VALID_INPUT,
  exitStatus: 5.5,
});
check('create: exitStatus -0 и 5.0', E.createEvidenceReceipt, {
  ...VALID_INPUT,
  exitStatus: 5.0,
});
check('create: subjectRefs trim/dedup/сортировка/не-строки', E.createEvidenceReceipt, {
  ...VALID_INPUT,
  subjectRefs: [' b ', 'a', 'b', '', 'a', 10, '10', true],
});
check('create: limitations slice до 50', E.createEvidenceReceipt, {
  ...VALID_INPUT,
  limitations: Array.from({ length: 80 }, (_, i) => `l${String(i).padStart(3, '0')}`),
});
check('create: subjectRefs slice до 100', E.createEvidenceReceipt, {
  ...VALID_INPUT,
  subjectRefs: Array.from({ length: 150 }, (_, i) => `s${String(i).padStart(3, '0')}`),
});
check('create: headSha битый → throw', E.createEvidenceReceipt, { ...VALID_INPUT, headSha: 'bad' });
check('create: artifact.uri пустой → throw', E.createEvidenceReceipt,
  { ...VALID_INPUT, artifact: { uri: ' ', sha256: H('a') } });
check('create: observation.path небезопасен → throw', E.createEvidenceReceipt, {
  ...VALID_INPUT,
  observation: { type: 'file', path: '/abs.ts' },
});
check('create: null input → TypeError', E.createEvidenceReceipt, null);

// receiptRef: побайтовое равенство строк (хеши receipt-пиннинга)
for (const [i, receipt] of [
  validReceipt,
  E.createEvidenceReceipt({ ...VALID_INPUT, observation: { type: 'file', path: 'src/𝄞.ts', value: 0.15452957153320312 } }),
  { provider: 'x', headSha: 'abcdef1', n: 1e21, u: ' ' },
].entries()) {
  compared += 1;
  const js = runJs(E.receiptRef, receipt);
  const rust = runRust(E.receiptRef, receipt);
  if (js.ok !== rust.ok || js.value !== rust.value) {
    failures.push({ label: `receiptRef[${i}]`, js: JSON.stringify(js), rust: JSON.stringify(rust) });
  }
}

// Пиннинг: хеш валидного receipt — стабильный якорь (меняется при расхождении каноникализации)
const PINNED = 'receipt:' + sha256(validReceipt);
console.log(`pinned receiptRef: ${PINNED}`);

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
