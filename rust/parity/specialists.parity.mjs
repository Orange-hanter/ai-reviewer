/**
 * Parity-прогон Фазы 3, шага 2: Rust specialists ↔ чистый JS fallback.
 *
 * Запуск: node rust/parity/specialists.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/specialists.mjs: resolveSpecialistsBin()
 * читает AI_REVIEWER_SPECIALISTS_FORCE_JS на каждый вызов, поэтому js-сторона
 * прогоняется с FORCE_JS=1, rust-сторона — без него (диспетчеризация и
 * наложение мутаций applyRefutation проверяются тоже). Входы канонизируются
 * JSON round-trip (`j()`), состояние находок после applyRefutation сравнивается
 * отдельно (мутации на месте — семантика разделяемых ссылок), логи — по строкам.
 *
 * Корпус:
 *   1. parseSpecialistAssignments: дефолты, bulk-переопределение, per-role
 *      override, мусорные части, пустые значения, moaReferences legacy;
 *   2. normalizeAdvisorClaims: фильтр ролей/статусов, severity у downgraded,
 *      пустой/не-массив → undefined (null в JSON);
 *   3. decideRefutation: матрица статусов (all-rejected, verified, downgraded
 *      nit/minor, пусто, посторонние статусы);
 *   4. applyRefutation: jury-матрица — drop/downgrade/keep/verified, strict
 *      noClaims на block-tier, prescan проходит, severity не из blockOn,
 *      не-строгий режим; сверяются return, мутации на входе и логи;
 *   5. mergeSpecialistReferences: мердж по чанкам, chunkCount, tag [chunk N],
 *      error не затирает непустой text, role-falsy → advisor в ключе;
 *   6. formatRefutationLog: шаблон, downgradedNoClaims-хвост.
 */

import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

if (!existsSync(resolve(REPO, 'rust/target/debug/specialists'))) {
  console.error('нет бинаря rust/target/debug/specialists — сначала cargo build');
  process.exit(1);
}

const S = await import('../../lib/specialists.mjs');

const j = (x) => (x === undefined ? null : JSON.parse(JSON.stringify(x))); // undefined → null, как в контракте бинаря

let compared = 0;
const failures = [];

function compare(label, jsResult, rustResult) {
  compared += 1;
  try {
    assert.deepEqual(j(rustResult), j(jsResult));
  } catch (e) {
    failures.push({
      label,
      js: JSON.stringify(jsResult)?.slice(0, 500),
      rust: JSON.stringify(rustResult)?.slice(0, 500),
    });
  }
}

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_SPECIALISTS_FORCE_JS = '1';
  try {
    return { ok: true, value: fn(...args) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_SPECIALISTS_FORCE_JS;
  }
};

// ---------------------------------------------------------------------------
// 1. parseSpecialistAssignments
// ---------------------------------------------------------------------------

const ASSIGN_ENVS = [
  {},
  { MOA_SPECIALIST_MODELS: 'security:sec-x,correctness:corr-x,architecture:arch-x' },
  { MOA_SPECIALIST_MODELS: 'security:sec-x,architecture : arch-y,bogus:no,no-colon,:empty-role' },
  { MOA_SPECIALIST_MODELS: '' },
  { MOA_SPECIALIST_MODELS: 'security:sec-bulk', MOA_SPECIALIST_SECURITY_MODEL: 'sec-role' },
  { MOA_SPECIALIST_MODELS: 'security:sec-bulk', MOA_SPECIALIST_SECURITY_MODEL: '   ' },
  { MOA_SPECIALIST_CORRECTNESS_MODEL: 'corr-only' },
  { MOA_SPECIALIST_MODELS: 'security:sec-1,security:sec-2' },
];
for (const [i, env] of ASSIGN_ENVS.entries()) {
  for (const refs of [[], ['r-a', 'r-b', 'r-c']]) {
    const js = runJs(S.parseSpecialistAssignments, env, refs).value;
    const rust = S.parseSpecialistAssignments(env, refs);
    compare(`parseSpecialistAssignments env#${i} refs=${refs.length}`, js, rust);
  }
}

// ---------------------------------------------------------------------------
// 2. normalizeAdvisorClaims
// ---------------------------------------------------------------------------

const CLAIMS_RAW = [
  null,
  undefined,
  'not-array',
  [],
  [
    { role: 'security', status: 'verified', reason: 'ok' },
    { role: 'bogus', status: 'verified' },
    { role: 'correctness', status: 'downgraded', severity: 'nit' },
    { role: 'architecture', status: 'downgraded', severity: 'major' },
    { role: 'architecture', status: 'pending' },
    { role: 'security', status: 'rejected' },
    null,
    { status: 'verified' },
  ],
  [{ role: 'security', status: 'verified', reason: 42 }],
];
for (const [i, raw] of CLAIMS_RAW.entries()) {
  const js = runJs(S.normalizeAdvisorClaims, raw).value;
  const rust = S.normalizeAdvisorClaims(raw);
  compare(`normalizeAdvisorClaims #${i}`, js, rust);
}

// ---------------------------------------------------------------------------
// 3. decideRefutation — матрица статусов
// ---------------------------------------------------------------------------

const CLAIM_MATRICES = [
  [],
  [{ status: 'rejected' }, { status: 'rejected' }],
  [{ status: 'rejected' }, { status: 'verified' }],
  [{ status: 'verified' }, { status: 'downgraded', severity: 'nit' }],
  [{ status: 'downgraded', severity: 'minor' }],
  [{ status: 'downgraded', severity: 'nit' }, { status: 'downgraded', severity: 'minor' }],
  [{ status: 'downgraded' }], // severity не minor/nit (не нормализованный вход)
  [{ status: 'pending' }],
  [{ status: 'rejected' }, { status: 'downgraded' }],
];
for (const [i, claims] of CLAIM_MATRICES.entries()) {
  const js = runJs(S.decideRefutation, claims).value;
  const rust = S.decideRefutation(claims);
  compare(`decideRefutation #${i}`, js, rust);
}

// ---------------------------------------------------------------------------
// 4. applyRefutation — сверяем return, мутации входа и логи
// ---------------------------------------------------------------------------

const mkFinding = (severity, extra = {}) => ({
  file: 'src/auth.ts', line: 10, severity, title: 'Утечка', ...extra,
});

const REFUTATION_CASES = [
  {
    label: 'matrix strict',
    strict: true,
    blockOn: ['critical', 'major'],
    findings: [
      mkFinding('critical', { prescan: true }),
      mkFinding('major', { advisorClaims: [
        { role: 'security', status: 'rejected', reason: 'fp' },
        { role: 'correctness', status: 'rejected' },
      ] }),
      mkFinding('major', { advisorClaims: [
        { role: 'security', status: 'downgraded', severity: 'nit' },
        { role: 'architecture', status: 'rejected' },
      ] }),
      mkFinding('major', { advisorClaims: [
        { role: 'security', status: 'verified', reason: 'real' },
        { role: 'correctness', status: 'rejected' },
      ] }),
      mkFinding('critical'), // strict: нет claims → downgrade minor
      mkFinding('minor'), // вне blockOn — не трогается даже в strict
    ],
  },
  {
    label: 'not strict keeps block tier without claims',
    strict: false,
    blockOn: ['critical', 'major'],
    findings: [mkFinding('critical'), mkFinding('major', { advisorClaims: [{ role: 'security', status: 'verified' }] })],
  },
  {
    label: 'custom blockOn + missing fields',
    strict: true,
    blockOn: ['critical'],
    findings: [
      { severity: 'critical', title: 'без файла/строки' },
      { file: '', line: 0, severity: 'major', title: 'major вне blockOn' },
      { file: 'a.ts', severity: 'weird', title: 'неизвестный severity strict' },
    ],
  },
  {
    label: 'claims not array + empty',
    strict: true,
    blockOn: ['critical', 'major'],
    findings: [
      mkFinding('major', { advisorClaims: 42 }), // нет .length → noClaims, как в JS
      mkFinding('major', { advisorClaims: [] }),
      // строковый advisorClaims: JS падает с TypeError ('x'.length truthy →
      // .map по строке) — вход недостижим после normalizeFinding, в корпус не
      // берём (отклонение зафиксировано в шапке rust/src/specialists.rs).
    ],
  },
];

for (const c of REFUTATION_CASES) {
  const jsLogs = [];
  const jsInput = j(c.findings);
  const js = runJs(S.applyRefutation, jsInput, (...a) => jsLogs.push(a.join(' ')), { strict: c.strict, blockOn: c.blockOn }).value;
  const jsState = j(jsInput);

  const rustLogs = [];
  const rustInput = j(c.findings);
  const rust = S.applyRefutation(rustInput, (...a) => rustLogs.push(a.join(' ')), { strict: c.strict, blockOn: c.blockOn });
  const rustState = j(rustInput);

  compare(`applyRefutation ${c.label} return`, js, rust);
  compare(`applyRefutation ${c.label} mutations`, jsState, rustState);
  compare(`applyRefutation ${c.label} logs`, jsLogs, rustLogs);
}

// ---------------------------------------------------------------------------
// 5. mergeSpecialistReferences
// ---------------------------------------------------------------------------

const MERGE_CASES = [
  [],
  [null, {}, { references: null }, { references: 'nope' }],
  [
    { references: [{ role: 'security', model: 's', text: 'one' }, { role: 'correctness', model: 'c', text: 'cee' }] },
    { references: [
      { role: 'security', model: 's', text: 'two' },
      { role: 'correctness', model: 'c', error: 'down' },
      { model: 'x', text: 'no role' },
      { role: 'security', model: 's2', error: 'down2' },
    ] },
    { references: [{ role: 'security', model: 's', text: 'three' }] },
  ],
  [
    // text falsy у первой записи → подменяется; пустой text не затирается error'ом
    { references: [{ role: 'security', model: 's', text: '' }] },
    { references: [{ role: 'security', model: 's', text: 'real', error: 'also-error' }] },
  ],
];
for (const [i, chunkResults] of MERGE_CASES.entries()) {
  const js = runJs(S.mergeSpecialistReferences, chunkResults).value;
  const rust = S.mergeSpecialistReferences(chunkResults);
  compare(`mergeSpecialistReferences #${i}`, js, rust);
}

// ---------------------------------------------------------------------------
// 6. formatRefutationLog
// ---------------------------------------------------------------------------

const STAT_CASES = [
  { kept: 0, dropped: 0, downgraded: 0, noClaims: 0 },
  { kept: 3, dropped: 1, downgraded: 2, noClaims: 4 },
  { kept: 1, dropped: 0, downgraded: 1, noClaims: 0, downgradedNoClaims: 1 },
  { kept: 1, dropped: 0, downgraded: 0, noClaims: 2, downgradedNoClaims: 0 },
];
for (const [i, stats] of STAT_CASES.entries()) {
  const js = runJs(S.formatRefutationLog, stats).value;
  const rust = S.formatRefutationLog(stats);
  compare(`formatRefutationLog #${i}`, js, rust);
}

// ---------------------------------------------------------------------------
// 7. RFC 21: blast-находка в strict-refutation
//    без skipBlast — обычная сверка (обе стороны понижают до minor);
//    со skipBlast — rust-owned (RFC 20): JS-копия параметра не знает, сверяется
//    только ожидаемое поведение бинаря.
// ---------------------------------------------------------------------------

let rustOwned = 0;
{
  const blast = () => [{ file: 'b.ts', line: 3, severity: 'major', title: 't', source: 'blast-radius', prescan: false }];
  const opts = { strict: true, blockOn: ['critical', 'major'] };
  const jsIn = blast();
  const js = runJs(S.applyRefutation, jsIn, () => {}, opts).value;
  const rustIn = blast();
  const rust = S.applyRefutation(rustIn, () => {}, opts);
  compare('applyRefutation blast без skipBlast return', js, rust);
  compare('applyRefutation blast без skipBlast mutations', jsIn, rustIn);

  rustOwned += 1;
  console.log('rust-owned (не сверяется с JS): applyRefutation blast + skipBlast');
  const own = blast();
  const r = S.applyRefutation(own, () => {}, { ...opts, skipBlast: true });
  try {
    assert.equal(own[0].severity, 'major');
    assert.equal(own[0].refutationStatus, 'not_applicable');
    assert.equal(r.stats.skippedBlast, 1);
  } catch (e) {
    failures.push({ label: 'rust-owned applyRefutation blast + skipBlast', js: '—', rust: JSON.stringify({ f: own[0], stats: r.stats }) });
  }
}

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

console.log(`parity cases compared: ${compared}, rust-owned: ${rustOwned}`);
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
