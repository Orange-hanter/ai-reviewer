/**
 * Parity-прогон Фазы 2, шаг 2: Rust structure_lcov ↔ чистый JS fallback
 * lib/structure-lcov.mjs.
 *
 * Запуск: node rust/parity/structure-lcov.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/structure-lcov.mjs: resolveLcovBin() читает
 * AI_REVIEWER_STRUCTURE_LCOV_FORCE_JS на каждый вызов, поэтому js-сторона
 * прогоняется с FORCE_JS=1, rust-сторона — без него. Результаты сравниваются
 * после JSON round-trip. byPath (JS Map) сравнивается отдельно: на rust-стороне
 * он пересобран обёрткой из files.
 *
 * Корпус (векторы из test/structure.test.mjs + edge-cases):
 *   - базовый двухфайловый LCOV (totals, byPath, branches);
 *   - одноимённые FN: привязка по порядку (attribution 'order'), разошедшееся
 *     число FN/FNDA → hits null + unresolvable-warning, однозначные имена;
 *   - абсолютные SF: без repoRoot → warning, с repoRoot → относительный путь,
 *     за пределами repoRoot → warning, Windows-путь → unsafe warning;
 *   - мусорные поля DA/BRDA (пустые, не-числа, hex/экспонента, лишние запятые),
 *     FNDA с float-хитами, CRLF, EOF без end_of_record, дубликаты путей;
 *   - lcovObservations: file/function уровни, exact/extracted/inferred/ambiguous,
 *     unresolved по order/unresolvable, subjectRef fallback;
 *   - matchLcovFunction: exact/extracted/inferred/ambiguous/unresolved,
 *     нормализация имён (Obj.method, trailing '()', кавычки), constructor-kind.
 */

import assert from 'node:assert/strict';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_STRUCTURE_LCOV_FORCE_JS = '1';
const S = await import('../../lib/structure-lcov.mjs');

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
  process.env.AI_REVIEWER_STRUCTURE_LCOV_FORCE_JS = '1';
  try {
    return { ok: true, value: rt(fn(...args)) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_STRUCTURE_LCOV_FORCE_JS;
  }
};

const runRust = (fn, ...args) => {
  delete process.env.AI_REVIEWER_STRUCTURE_LCOV_FORCE_JS;
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

/** parseLcov с отдельным сравнением byPath (Map не выживает в JSON round-trip). */
function checkParse(label, text, options) {
  const run = (forceJs) => {
    if (forceJs) process.env.AI_REVIEWER_STRUCTURE_LCOV_FORCE_JS = '1';
    else delete process.env.AI_REVIEWER_STRUCTURE_LCOV_FORCE_JS;
    try {
      const result = S.parseLcov(text, options);
      return { ok: true, value: result };
    } catch (e) {
      return { ok: false, error: String(e?.message || e) };
    }
  };
  const js = run(true);
  const rust = run(false);
  if (js.ok !== rust.ok) {
    compared += 1;
    failures.push({ label, js: JSON.stringify(js)?.slice(0, 400), rust: JSON.stringify(rust)?.slice(0, 400) });
    return;
  }
  const { byPath: jsByPath, ...jsRest } = js.value ?? {};
  const { byPath: rustByPath, ...rustRest } = rust.value ?? {};
  compare(`${label} (result)`, rt(jsRest), rt(rustRest));
  compared += 1;
  try {
    assert.deepEqual(rt([...rustByPath.entries()]), rt([...jsByPath.entries()]));
  } catch (e) {
    failures.push({
      label: `${label} (byPath)`,
      js: JSON.stringify(rt([...jsByPath.entries()]))?.slice(0, 600),
      rust: JSON.stringify(rt([...rustByPath.entries()]))?.slice(0, 600),
    });
  }
}

// ---------------------------------------------------------------------------
// Корпус: parseLcov
// ---------------------------------------------------------------------------

const LCOV_TWO_FILES = `TN:
SF:src/auth.ts
FN:10,validate
FNDA:2,validate
DA:10,2
DA:11,0
BRDA:11,0,0,0
end_of_record
SF:src/login.ts
FN:4,login
FNDA:1,login
DA:4,1
end_of_record
`;

checkParse('parseLcov: базовый двухфайловый', LCOV_TWO_FILES, { repoRoot: '/repo' });
checkParse('parseLcov: без repoRoot', LCOV_TWO_FILES);

checkParse('parseLcov: одноимённые FN, привязка по порядку', [
  'SF:src/a.ts',
  'FN:10,handler',
  'FN:50,handler',
  'FNDA:7,handler',
  'FNDA:0,handler',
  'DA:10,7',
  'DA:50,0',
  'end_of_record',
].join('\n'));

checkParse('parseLcov: FN/FNDA разошлись — неизвестно исполнение', [
  'SF:src/a.ts',
  'FN:10,handler',
  'FN:50,handler',
  'FNDA:7,handler',
  'DA:10,7',
  'DA:50,0',
  'end_of_record',
].join('\n'));

checkParse('parseLcov: однозначные имена', [
  'SF:src/a.ts',
  'FN:10,alpha',
  'FN:50,beta',
  'FNDA:7,alpha',
  'FNDA:0,beta',
  'DA:10,7',
  'end_of_record',
].join('\n'));

checkParse('parseLcov: абсолютный SF без repoRoot', 'TN:\nSF:/repo/src/x.ts\nDA:1,1\nend_of_record\n');
checkParse('parseLcov: абсолютный SF с repoRoot', 'TN:\nSF:/repo/src/x.ts\nDA:1,1\nend_of_record\n', { repoRoot: '/repo' });
checkParse('parseLcov: абсолютный SF вне repoRoot', 'SF:/other/x.ts\nDA:1,1\nend_of_record\n', { repoRoot: '/repo' });
checkParse('parseLcov: абсолютный SF с repoRoot со slash/точками', 'SF:/repo/../repo/./src/x.ts\nDA:1,1\nend_of_record\n', { repoRoot: '/repo/' });
checkParse('parseLcov: Windows-путь отклонён', 'SF:C:\\secret\\x.ts\nDA:1,1\nend_of_record\n');
checkParse('parseLcov: SF с выходом через ..', 'SF:../escape.ts\nDA:1,1\nend_of_record\n');
checkParse('parseLcov: пустой SF', 'SF:\nDA:1,1\nend_of_record\n');

checkParse('parseLcov: мусорные DA/BRDA', 'TN:\nSF:src/x.ts\nDA:,5\nDA:3,\nDA:4,2\nBRDA:4,0,0,\nBRDA:,0,0,1\nend_of_record\n');
checkParse('parseLcov: hex/экспонента/пробелы/лишние запятые', [
  'SF:src/x.ts',
  'DA:0x10,1',
  'DA:1e2,1',
  'DA: 5 , 1 ',
  'DA:5,0,1',
  'DA:7,1.5',
  'FNDA:2.5,fn',
  'FN:1e2,fn',
  'BRDA:9,0,0,-',
  'BRDA:9,0,1,abc',
  'BRDA:9,0,2,3.5',
  'FN:3,x',
  'FN:,y',
  'FN:4,',
  'FNDA:3,z',
  'end_of_record',
].join('\n'));
checkParse('parseLcov: CRLF и запись без end_of_record на EOF', 'SF:src/a.ts\r\nDA:1,1\r\n');
checkParse('parseLcov: дубликаты путей', 'SF:src/a.ts\nDA:1,1\nend_of_record\nSF:src/b.ts\nDA:2,1\nend_of_record\nSF:src/a.ts\nDA:1,0\nend_of_record\n');
checkParse('parseLcov: запись без единой DA', 'SF:src/empty.ts\nend_of_record\n');
checkParse('parseLcov: пустой текст', '');
checkParse('parseLcov: поле перед первым SF игнорируется', 'TN:version\nLH:0\nSF:src/a.ts\nDA:1,1\nend_of_record\n');
checkParse('parseLcov: строки вне записи игнорируются', 'SF:src/a.ts\nDA:1,1\nend_of_record\nDA:99,9\nFN:9,late\n');

// ---------------------------------------------------------------------------
// Корпус: lcovObservations
// ---------------------------------------------------------------------------

const lcovOrder = S.parseLcov([
  'SF:src/a.ts',
  'FN:10,handler',
  'FN:50,handler',
  'FNDA:7,handler',
  'FNDA:0,handler',
  'DA:10,7',
  'DA:50,0',
  'end_of_record',
].join('\n'));

const lcovUnresolved = S.parseLcov([
  'SF:src/a.ts',
  'FN:10,handler',
  'FN:50,handler',
  'FNDA:7,handler',
  'DA:10,7',
  'DA:50,0',
  'end_of_record',
].join('\n'));

const entitiesMap = () => new Map([['src/a.ts', [
  { observedId: 'E10', kind: 'function', label: 'handler', line: 10 },
  { observedId: 'E50', kind: 'function', label: 'handler', line: 50 },
]]]);

check('lcovObservations: order-атрибуция + маппинг на сущности', S.lcovObservations, lcovOrder, entitiesMap());
check('lcovObservations: unresolved — ambiguous, hitCount null', S.lcovObservations, lcovUnresolved);
check('lcovObservations: пустой lcov', S.lcovObservations, null);
check('lcovObservations: без entityByPath', S.lcovObservations, lcovOrder);
check('lcovObservations: близкие строки — inferred (±3)', S.lcovObservations,
  S.parseLcov('SF:src/a.ts\nFN:10,run\nFNDA:1,run\nDA:10,1\nend_of_record\n'),
  new Map([['src/a.ts', [{ observedId: 'E1', kind: 'method', label: 'obj.run', line: 12 }]]]));

// ---------------------------------------------------------------------------
// Корпус: matchLcovFunction
// ---------------------------------------------------------------------------

const ENTS = [
  { kind: 'constructor', label: 'Widget', line: 3, observedId: 'ctor' },
  { kind: 'function', label: 'Widget', line: 3, observedId: 'fn' },
  { kind: 'function', label: 'obj.run', line: 12, observedId: 'e1' },
  { kind: 'function', label: 'obj.run', line: 14, observedId: 'e2' },
  { kind: 'method', label: 'Obj.helper()', line: 20, observedId: 'e3' },
  { kind: 'variable', label: 'data', line: 3, observedId: 'e4' },
  { kind: 'function', label: 'Quoted', line: 8, observedId: 'e5' },
];

check('matchLcovFunction: exact по имени и строке', S.matchLcovFunction, { line: 3, name: 'Widget' }, ENTS);
check('matchLcovFunction: extracted — единственная callable на строке', S.matchLcovFunction, { line: 12, name: 'renamed' }, ENTS);
check('matchLcovFunction: inferred — имя совпало в ±3', S.matchLcovFunction, { line: 13, name: 'run' }, ENTS);
check('matchLcovFunction: ambiguous — два кандидата в ±3', S.matchLcovFunction, { line: 13, name: 'obj.run' }, ENTS);
check('matchLcovFunction: unresolved — никого', S.matchLcovFunction, { line: 100, name: 'missing' }, ENTS);
check('matchLcovFunction: нормализация Obj.helper()', S.matchLcovFunction, { line: 20, name: 'helper()' }, ENTS);
check('matchLcovFunction: кавычки срезаются', S.matchLcovFunction, { line: 8, name: '"quoted"' }, ENTS);
check('matchLcovFunction: variable-kind не callable', S.matchLcovFunction, { line: 3, name: 'data' }, ENTS);
check('matchLcovFunction: пустые сущности', S.matchLcovFunction, { line: 3, name: 'Widget' }, []);

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
