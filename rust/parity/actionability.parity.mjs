/**
 * Parity-прогон Фазы 1, шага 5: Rust actionability ↔ чистый JS fallback.
 *
 * Запуск: node rust/parity/actionability.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртки lib/actionability.mjs и lib/findings.mjs:
 * resolveActionabilityBin() читает AI_REVIEWER_ACTIONABILITY_FORCE_JS на каждый
 * вызов, поэтому js-сторона прогоняется с FORCE_JS=1, rust-сторона — без него
 * (диспетчеризация + наложение мутаций проверяются тоже). Входные находки
 * канонизируются JSON round-trip (`j()`), чтобы undefined-поля не давали ложных
 * расхождений deepStrictEqual.
 *
 * Корпус:
 *   1. ACTIONABILITY_SIGNALS ↔ op `signals` (защита от рассинхрона весов);
 *   2. scoreActionability: каждая находка каждого fixtures-кейса × наборы ctx
 *      (priorKeys/prescanKeys/fileClass/sensitive из профиля orakul) + литеральные
 *      edge-кейсы сигналов (verified/rejected, jury, blast, outOfChunk, docs…);
 *   3. routeFindings: return + мутации на находках, на картах файлов из parseDiff;
 *      inlineOn/threshold/maxAdvisoryInline/strict; лимит и стабильность тай-брейков;
 *   4. createFindingsFormatter.classify/findingBlock (обёртка lib/findings.mjs →
 *      бинарь actionability): группировка, marker профиля, SEV-эмодзи, suggestion;
 *   5. профиль с layout.sensitive (profiles/orakul.json) — обе ветки под ним,
 *      в т.ч. регекс с lookahead (в Rust его берёт fancy-regex, RFC 20).
 */

import { readFileSync, readdirSync, existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_ACTIONABILITY_FORCE_JS = '1';
const A = await import('../../lib/actionability.mjs');
const F = await import('../../lib/findings.mjs');
const { parseDiff } = await import('../../lib/diff.mjs');
const { setProfile, resetProfile } = await import('../../lib/profile.mjs');

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_ACTIONABILITY_FORCE_JS = '1';
  try {
    return { ok: true, value: fn(...args) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_ACTIONABILITY_FORCE_JS;
  }
};

/** Канонизация: undefined-поля выпадают (JSON их не несёт). */
const j = (x) => JSON.parse(JSON.stringify(x));

let compared = 0;
const failures = [];

function compare(label, jsResult, rustResult) {
  compared += 1;
  try {
    assert.deepEqual(rustResult, jsResult);
  } catch (e) {
    failures.push({
      label,
      js: JSON.stringify(jsResult)?.slice(0, 400),
      rust: JSON.stringify(rustResult)?.slice(0, 400),
    });
  }
}

// ---------------------------------------------------------------------------
// Корпус 1: ACTIONABILITY_SIGNALS ↔ op signals (напрямую к бинарю, у обёртки
// константа осталась в JS)
// ---------------------------------------------------------------------------

{
  const bin = resolve(REPO, 'rust/target/debug/actionability');
  if (!existsSync(bin)) {
    console.error(`нет бинаря ${bin} — сначала cargo build`);
    process.exit(1);
  }
  const rustSignals = JSON.parse(
    execFileSync(bin, [], { input: JSON.stringify({ op: 'signals' }), encoding: 'utf8' }),
  );
  compare('ACTIONABILITY_SIGNALS', j(A.ACTIONABILITY_SIGNALS), rustSignals);
}

// ---------------------------------------------------------------------------
// Хелперы проверок
// ---------------------------------------------------------------------------

function checkScore(label, finding, ctx) {
  const js = j(runJs(A.scoreActionability, j(finding), ctx).value);
  const rust = j(A.scoreActionability(j(finding), ctx));
  compare(`scoreActionability ${label}`, js, rust);
}

/** Мутационный оп: прогон на копиях, сравнение и возврата, и состояния находок. */
function checkRoute(label, findings, files, cfg) {
  const jsInput = j(findings);
  const js = j(runJs(A.routeFindings, jsInput, files, cfg).value);
  const jsState = j(jsInput);
  const rustInput = j(findings);
  const rust = j(A.routeFindings(rustInput, files, cfg));
  const rustState = j(rustInput);
  compare(`routeFindings ${label} return`, js, rust);
  compare(`routeFindings ${label} mutations`, jsState, rustState);
}

function checkClassify(label, cfg, findings, files, ctx) {
  const jsInput = j(findings);
  const js = j(runJs((c) => F.createFindingsFormatter(c).classify(jsInput, files, ctx), cfg).value);
  const jsState = j(jsInput);
  const rustInput = j(findings);
  const rust = j(F.createFindingsFormatter(cfg).classify(rustInput, files, ctx));
  const rustState = j(rustInput);
  compare(`classify ${label} return`, js, rust);
  compare(`classify ${label} mutations`, jsState, rustState);
  // findingBlock на каждой находке (в пост-мутационном состоянии)
  for (const [i, f] of rustInput.entries()) {
    const jsBlock = runJs((ff) => F.createFindingsFormatter(cfg).findingBlock(ff), j(jsState[i])).value;
    const rustBlock = F.createFindingsFormatter(cfg).findingBlock(f);
    compare(`findingBlock ${label}#${i}`, jsBlock, rustBlock);
  }
}

// ---------------------------------------------------------------------------
// Карты файлов из parseDiff (тот же дифф, что в тестах findings)
// ---------------------------------------------------------------------------

const SAMPLE_DIFF = 'diff --git a/foo.ts b/foo.ts\n--- a/foo.ts\n+++ b/foo.ts\n@@ -1,2 +1,3 @@\n const a = 1;\n-const b = 2;\n+const b = 3;\n+export function foo() { return a; }\n';
const AUTH_DIFF = 'diff --git a/server/auth.ts b/server/auth.ts\n--- a/server/auth.ts\n+++ b/server/auth.ts\n@@ -1,3 +1,4 @@\n const x = 1;\n+const y = parseInt(raw);\n const z = 2;\n+export function check() { return x; }\n';
const { files: sampleFiles } = parseDiff(SAMPLE_DIFF);
const { files: authFiles } = parseDiff(AUTH_DIFF);

// ---------------------------------------------------------------------------
// Корпус 2: scoreActionability — находки fixtures × наборы ctx
// ---------------------------------------------------------------------------

const fixtureDirs = readdirSync(resolve(REPO, 'fixtures'), { withFileTypes: true })
  .filter((d) => d.isDirectory())
  .map((d) => d.name);

const CTXS = {
  empty: {},
  prior: { priorKeys: new Set(['foo.ts:1', 'server/auth.ts:2']) },
  prescan: { prescanKeys: new Set(['foo.ts:1']) },
  fileClassLogic: { fileClass: (p) => (p.endsWith('.md') ? 'docs' : 'logic') },
  fileClassRouter: { fileClass: (p) => (p.includes('docs/') ? 'docs' : 'logic') },
  all: {
    priorKeys: new Set(['foo.ts:2']),
    prescanKeys: new Set(['foo.ts:1', 'server/auth.ts:3']),
    fileClass: (p) => (p.endsWith('.ts') ? 'logic' : 'other'),
  },
};

const allFixtureFindings = [];
for (const dir of fixtureDirs) {
  let raw;
  try {
    raw = JSON.parse(readFileSync(resolve(REPO, 'fixtures', dir, 'findings.json'), 'utf8'));
  } catch { continue; }
  for (const f of raw.findings || []) allFixtureFindings.push({ dir, f });
}

for (const { dir, f } of allFixtureFindings) {
  for (const [ctxName, ctx] of Object.entries(CTXS)) {
    checkScore(`${dir} ${f.title?.slice(0, 30)} [${ctxName}]`, f, ctx);
  }
}

// литеральные edge-кейсы сигналов
const edgeFindings = [
  { file: 'server/auth.ts', line: 10, severity: 'minor', title: 'base' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', verifierStatus: 'verified' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', verifierStatus: 'rejected' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', verifierStatus: 'downgraded' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', advisorClaims: [{ status: 'verified' }, { status: 'verified' }] },
  { file: 'server/auth.ts', line: 10, severity: 'minor', advisorClaims: [{ status: 'verified' }, { status: 'rejected' }] },
  { file: 'server/auth.ts', line: 10, severity: 'minor', advisorClaims: [] },
  { file: 'server/auth.ts', line: 10, severity: 'minor', advisorClaims: 'not-array' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', verifierStatus: 'rejected', advisorClaims: [{ status: 'verified' }] },
  // RFC 06: refutationStatus гасит jury-штраф только при точном 'downgraded'
  { file: 'server/auth.ts', line: 10, severity: 'minor', refutationStatus: 'downgraded', advisorClaims: [{ status: 'downgraded' }, { status: 'verified' }] },
  { file: 'server/auth.ts', line: 10, severity: 'minor', refutationStatus: 'kept', advisorClaims: [{ status: 'downgraded' }, { status: 'verified' }] },
  { file: 'server/auth.ts', line: 10, severity: 'minor', refutationStatus: 'downgraded_no_claims', advisorClaims: [{ status: 'rejected' }, { status: 'verified' }] },
  { file: 'server/auth.ts', line: 10, severity: 'minor', refutationStatus: 'downgraded', verifierStatus: 'downgraded', advisorClaims: [{ status: 'downgraded' }] },
  { file: 'server/auth.ts', line: 10, severity: 'minor', refutationStatus: null, advisorClaims: [{ status: 'rejected' }, { status: 'verified' }] },
  { file: 'server/auth.ts', line: 10, severity: 'minor', consensus: true },
  { file: 'server/auth.ts', line: 10, severity: 'minor', consensus: false },
  { file: 'server/auth.ts', line: 10, severity: 'minor', confidence: 0.8 },
  { file: 'server/auth.ts', line: 10, severity: 'minor', confidence: 0.79 },
  { file: 'server/auth.ts', line: 10, severity: 'minor', confidence: '0.9' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', confidence: null },
  { file: 'server/auth.ts', line: 10, severity: 'minor', confidence: 'abc' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', suggestion: 'fix()' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', suggestion: '' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', blastCaller: true },
  { file: 'server/auth.ts', line: 10, severity: 'minor', source: 'blast-radius' },
  { file: 'server/auth.ts', line: 10, severity: 'minor', source: 'llm', blastCaller: true },
  { file: 'server/auth.ts', line: 10, severity: 'minor', outOfChunk: true },
  { file: 'docs/guide.md', line: 1, severity: 'minor', title: 'docs' },
  { file: 'app/server/auth/roles.ts', line: 1, severity: 'minor' },
  { file: 'app/rbac.ts', line: 1, severity: 'minor' },
  { file: 'db/migrations/001_x.sql', line: 1, severity: 'minor' },
  { file: '.github/workflows/ci.yml', line: 1, severity: 'minor' },
  { file: 'src/util.ts', line: 1, severity: 'minor' },
  { file: 'src/util.ts', line: '3', severity: 'minor' },
  { file: '', line: 1, severity: 'minor' },
  { line: 1, severity: 'minor' },
  { file: 'billing/pricing.ts', line: 1, severity: 'minor', confidence: 0.9, consensus: true, suggestion: 'x', outOfChunk: true },
];
for (const [i, f] of edgeFindings.entries()) {
  for (const [ctxName, ctx] of Object.entries(CTXS)) {
    checkScore(`edge#${i} [${ctxName}]`, f, ctx);
  }
}

// ---------------------------------------------------------------------------
// Корпус 3: routeFindings
// ---------------------------------------------------------------------------

const mkMinor = (line, extra = {}) => ({
  file: 'server/auth.ts', line, severity: 'minor', title: `t${line}`, model: 'm1',
  confidence: 0.9, consensus: true, ...extra,
});

checkRoute('fixtures batch', allFixtureFindings.map((x) => x.f).slice(0, 12), sampleFiles, {});
checkRoute('empty', [], sampleFiles, {});
checkRoute('no files map', [mkMinor(1)], null, {});
checkRoute('defaults', [mkMinor(10), mkMinor(20, { confidence: 0.4, consensus: false })], authFiles, {});
checkRoute('inlineOn minor', [mkMinor(1), mkMinor(2)], authFiles, { inlineOn: ['critical', 'major', 'minor'] });
checkRoute('inlineOn empty', [mkMinor(1)], authFiles, { inlineOn: [] });
checkRoute('threshold high', [mkMinor(1)], authFiles, { threshold: 100 });
checkRoute('threshold zero', [mkMinor(1, { confidence: 0.1, consensus: false, suggestion: null })], authFiles, { threshold: 0 });
checkRoute('maxAdvisory 1 stable ties', [mkMinor(1), mkMinor(2), mkMinor(3)], authFiles, { maxAdvisoryInline: 1 });
checkRoute('maxAdvisory 0', [mkMinor(1)], authFiles, { maxAdvisoryInline: 0 });
checkRoute('strict blocks unverified', [
  { file: 'server/auth.ts', line: 10, severity: 'major', confidence: 0.95, model: 'm' },
  { file: 'server/auth.ts', line: 20, severity: 'major', confidence: 0.95, model: 'm', prescan: true },
  { file: 'server/auth.ts', line: 30, severity: 'major', confidence: 0.95, model: 'm', verifierStatus: 'verified' },
  { file: 'server/auth.ts', line: 40, severity: 'minor', confidence: 0.95, model: 'm' },
], authFiles, { strict: true });
checkRoute('unknown severity not promoted', [
  { file: 'server/auth.ts', line: 10, severity: 'weird', confidence: 0.9, consensus: true, suggestion: 'x' },
  { file: 'server/auth.ts', line: 20, severity: null, confidence: 0.9, consensus: true },
], authFiles, {});
checkRoute('out of diff lines', [
  { file: 'server/auth.ts', line: 999, severity: 'major', confidence: 0.95 },
  { file: 'gone.ts', line: 1, severity: 'major', confidence: 0.95 },
  { file: 'server/auth.ts', line: 0, severity: 'major', confidence: 0.95 },
  { file: 'server/auth.ts', line: 2, severity: 'critical', confidence: 0.95 },
], authFiles, {});
checkRoute('ctx signals', [
  { file: 'foo.ts', line: 1, severity: 'minor', model: 'm', confidence: 0.5 },
  { file: 'foo.ts', line: 2, severity: 'nit', model: 'm' },
], sampleFiles, {
  ctx: {
    priorKeys: new Set(['foo.ts:1']),
    prescanKeys: new Set(['foo.ts:1', 'foo.ts:2']),
    fileClass: () => 'logic',
  },
});
checkRoute('string line coercion', [
  { file: 'foo.ts', line: '1', severity: 'major', confidence: 0.9, model: 'm' },
  { file: 'foo.ts', line: 'x', severity: 'major', confidence: 0.9, model: 'm' },
], sampleFiles, {});

// ---------------------------------------------------------------------------
// Корпус 4: classify / findingBlock через createFindingsFormatter
// ---------------------------------------------------------------------------

const FMT_CFG = { inlineOn: ['critical', 'major'], actionability: { threshold: 4, maxAdvisoryInline: 5 } };

checkClassify('proven + bare', FMT_CFG, [
  { file: 'server/auth.ts', line: 10, severity: 'minor', title: 'доказуемая', body: '', model: 'm1', confidence: 0.9, consensus: true, suggestion: 'fix()' },
  { file: 'server/auth.ts', line: 20, severity: 'nit', title: 'голая', body: '', model: 'm1', confidence: 0.3 },
], authFiles, { fileClass: () => 'logic' });

checkClassify('outOfChunk stays sticky', FMT_CFG, [
  { file: 'server/auth.ts', line: 10, severity: 'minor', title: 'x', model: 'm1', confidence: 0.9, consensus: true, suggestion: 'fix()', outOfChunk: true },
], authFiles, { fileClass: () => 'logic' });

checkClassify('group by file:line', FMT_CFG, [
  { file: 'foo.ts', line: 1, severity: 'critical', title: 'a', body: 'x', model: 'm' },
  { file: 'foo.ts', line: 1, severity: 'major', title: 'b', body: 'y', model: 'm', suggestion: 's' },
  { file: 'foo.ts', line: 3, severity: 'minor', title: 'c', body: 'z', model: 'm' },
], sampleFiles, {});

checkClassify('no ctx, no body/suggestion', { inlineOn: ['critical'] }, [
  { file: 'foo.ts', line: 1, severity: 'critical', title: 'block', model: 'm' },
  { file: 'foo.ts', line: 1, severity: 'minor', title: 'nitpick', model: 'm' },
], sampleFiles);

checkClassify('strict gate cfg', { ...FMT_CFG, gate: { strict: true } }, [
  { file: 'foo.ts', line: 1, severity: 'major', confidence: 0.95, model: 'm' },
  { file: 'foo.ts', line: 2, severity: 'major', confidence: 0.95, model: 'm', prescan: true },
], sampleFiles, {});

checkClassify('weird severity emoji', FMT_CFG, [
  { file: 'foo.ts', line: 1, severity: 'weird', title: '', body: 'b', model: 'm' },
  { file: 'foo.ts', line: 2, severity: 'nit', title: 'n', body: null, model: 'm' },
], sampleFiles, {});

checkClassify('advisory limit 1', { ...FMT_CFG, actionability: { threshold: 4, maxAdvisoryInline: 1 } }, [
  mkMinor(1), mkMinor(2, { suggestion: 'fix()' }), mkMinor(3),
], authFiles, { fileClass: () => 'logic' });

// ---------------------------------------------------------------------------
// Корпус 5: профиль с layout.sensitive (orakul) + негативный случай
// ---------------------------------------------------------------------------

setProfile(JSON.parse(readFileSync(resolve(REPO, 'profiles/orakul.json'), 'utf8')));
try {
  checkScore('orakul sensitive ops/pr-review', { file: 'ops/pr-review/gate.sh', line: 1, severity: 'major', model: 'm' }, {});
  checkScore('orakul non-sensitive', { file: 'src/util.ts', line: 1, severity: 'major', model: 'm' }, {});
  checkRoute('orakul profile route', [
    { file: 'ops/pr-review/gate.sh', line: 1, severity: 'minor', title: 't', model: 'm', confidence: 0.9, consensus: true },
    { file: 'ops/pr-merge/x.sh', line: 2, severity: 'minor', title: 't2', model: 'm', confidence: 0.9 },
  ], null, {});
  checkClassify('orakul classify', FMT_CFG, [
    { file: 'ops/pr-review/gate.sh', line: 1, severity: 'minor', title: 'gate', body: 'b', model: 'm', confidence: 0.9, consensus: true },
  ], null, {});

  // Регекс с lookahead: Rust компилирует его fancy-regex (RFC 20, решение 4),
  // деградации на JS больше нет — обе ветки считают сигнал одинаково.
  setProfile({ name: 't', layout: { sensitive: 'ops/(?=pr)' } });
  checkScore('lookahead sensitive', { file: 'ops/pr/x.sh', line: 1, severity: 'major', model: 'm' }, {});
  checkScore('lookahead sensitive miss', { file: 'ops/other/x.sh', line: 1, severity: 'major', model: 'm' }, {});
  checkRoute('lookahead sensitive route', [
    { file: 'ops/pr/x.sh', line: 1, severity: 'minor', title: 't', model: 'm', confidence: 0.9, consensus: true },
  ], null, {});
} finally {
  resetProfile();
}

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

console.log(`fixtures findings scored: ${allFixtureFindings.length}`);
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
