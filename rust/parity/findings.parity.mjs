/**
 * Parity-прогон Фазы 1, шаг 3: Rust findings ↔ чистый JS fallback lib/findings.mjs.
 *
 * Запуск: node rust/parity/findings.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/findings.mjs: resolveFindingsBin() читает
 * AI_REVIEWER_FINDINGS_FORCE_JS на каждый вызов, поэтому js-сторона прогоняется
 * с FORCE_JS=1, rust-сторона — без него (диспетчеризация + наложение мутаций
 * проверяются тоже). Входные находки канонизируются JSON round-trip
 * (`j()`), чтобы undefined-поля не давали ложных расхождений deepStrictEqual.
 *
 * Корпус:
 *   1. findings.json всех кейсов fixtures/: normalizeFinding / sameFindingKey /
 *      dedupeSameModelFindings / clusterFindings (5 гейтов) / resolveBlockers;
 *   2. литеральные edge-кейсы из test/pr-review.test.mjs и pr-review.eval.test.mjs:
 *      confidence-коэрсии, requireQuote, advisorClaims, blast soft-gate, strict,
 *      prescan-политика, 3-строчные бакеты, перенос по fingerprint, legacy ±3;
 *   3. fingerprint/attachFingerprints/filterStalePriorFindings на text-картах
 *      из parseDiff (тот же дифф, что в тестах) + ручные карты с коллизиями;
 *   4. сравнение log-сообщений для дедупа, carry-over и FP-фильтра;
 *   5. FP-фильтр (RFC 04, T07): matchesKnownFalsePositive/filterFalsePositiveFindings
 *      на всех находках fixtures/ с changedPaths из diff.patch кейса + литеральные
 *      кейсы паттернов + свой набор (opts.patterns) и пустой набор.
 *
 * createFindingsFormatter/classify после Фазы 1, шага 5 портированы в бинарь
 * actionability и сравниваются в rust/parity/actionability.parity.mjs.
 */

import { readFileSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_FINDINGS_FORCE_JS = '1';
const F = await import('../../lib/findings.mjs');
const { parseDiff } = await import('../../lib/diff.mjs');

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_FINDINGS_FORCE_JS = '1';
  try {
    return { ok: true, value: fn(...args) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_FINDINGS_FORCE_JS;
  }
};

/** Канонизация входа: undefined-поля выпадают (JSON их не несёт). */
const j = (x) => JSON.parse(JSON.stringify(x));

let compared = 0;
const failures = [];

function compare(label, jsResult, rustResult, compareLogs) {
  compared += 1;
  try {
    assert.deepEqual(rustResult, jsResult);
  } catch (e) {
    failures.push({
      label,
      why: compareLogs || 'value mismatch',
      js: JSON.stringify(jsResult)?.slice(0, 400),
      rust: JSON.stringify(rustResult)?.slice(0, 400),
    });
  }
}

// ---------------------------------------------------------------------------
// Операции: обе стороны через обёртку, вход — глубокие копии
// ---------------------------------------------------------------------------

function checkNormalizeConfidence(label, raw) {
  const js = runJs(F.normalizeConfidence, raw).value;
  const rust = F.normalizeConfidence(raw);
  compare(`normalizeConfidence ${label}`, js, rust);
}

function checkNormalizeFinding(label, f, model, opts) {
  const js = j(runJs(F.normalizeFinding, j(f), model, opts).value);
  const rust = j(F.normalizeFinding(j(f), model, opts));
  compare(`normalizeFinding ${label}`, js, rust);
}

function checkSameFindingKey(label, f) {
  const js = runJs(F.sameFindingKey, j(f)).value;
  const rust = F.sameFindingKey(j(f));
  compare(`sameFindingKey ${label}`, js, rust);
}

/** Мутационные опы: прогон на копиях, сравнение и возврата, и состояния находок. */
function checkDedupe(label, findings) {
  const input = j(findings);
  const jsLogs = [];
  const js = runJs(F.dedupeSameModelFindings, input, (m) => jsLogs.push(m)).value;
  const rustLogs = [];
  const rust = F.dedupeSameModelFindings(j(findings), (m) => rustLogs.push(m));
  compare(`dedupeSameModelFindings ${label} logs`, jsLogs, rustLogs);
  compare(`dedupeSameModelFindings ${label}`, js, rust);
}

function checkCluster(label, findings, gate) {
  const jsInput = j(findings);
  const js = runJs(F.clusterFindings, jsInput, gate).value;
  const jsState = j(jsInput);
  const rustInput = j(findings);
  const rust = F.clusterFindings(rustInput, gate);
  const rustState = j(rustInput);
  compare(`clusterFindings ${label} return`, js, rust);
  compare(`clusterFindings ${label} mutations`, jsState, rustState);
}

function checkResolveBlockers(label, findings, changedPaths, gate, opts = {}) {
  const jsInput = j(findings);
  const jsLogs = [];
  const js = runJs(F.resolveBlockers, jsInput, changedPaths, gate, (m) => jsLogs.push(m), opts).value;
  const jsState = j(jsInput);
  const rustInput = j(findings);
  const rustLogs = [];
  const rust = F.resolveBlockers(rustInput, changedPaths, gate, (m) => rustLogs.push(m), opts);
  const rustState = j(rustInput);
  compare(`resolveBlockers ${label} logs`, jsLogs, rustLogs);
  compare(`resolveBlockers ${label} return`, js, rust);
  compare(`resolveBlockers ${label} mutations`, jsState, rustState);
}

function checkSoftConsensus(label, blockTier) {
  const js = runJs(F.softConsensusForBlastCluster, j(blockTier)).value;
  const rust = F.softConsensusForBlastCluster(j(blockTier));
  compare(`softConsensusForBlastCluster ${label}`, js, rust);
}

function checkFingerprint(label, text, line) {
  const rec = text ? { text: new Map(Object.entries(text).map(([k, v]) => [Number(k), v])) } : null;
  const js = runJs(F.findingFingerprint, rec, line).value;
  const rust = F.findingFingerprint(rec, line);
  compare(`findingFingerprint ${label}`, js, rust);
}

function checkAttachFingerprints(label, findings, files) {
  const jsInput = j(findings);
  const js = runJs(F.attachFingerprints, jsInput, files).value;
  const jsState = j(jsInput);
  const rustInput = j(findings);
  const rust = F.attachFingerprints(rustInput, files);
  const rustState = j(rustInput);
  compare(`attachFingerprints ${label} return`, js, rust);
  compare(`attachFingerprints ${label} mutations`, jsState, rustState);
}

function checkStalePrior(label, priors, files) {
  const jsLogs = [];
  const js = runJs(F.filterStalePriorFindings, j(priors), files, (m) => jsLogs.push(m)).value;
  const rustLogs = [];
  const rust = F.filterStalePriorFindings(j(priors), files, (m) => rustLogs.push(m));
  compare(`filterStalePriorFindings ${label} logs`, jsLogs, rustLogs);
  compare(`filterStalePriorFindings ${label}`, j(js), j(rust));
}

function checkPrescanPolicy(label, findings, prescanCfg) {
  const jsClusters = j(findings).map((g) => g.map((f) => ({ ...f })));
  runJs(F.applyPrescanBlockPolicy, jsClusters, prescanCfg);
  const rustClusters = j(findings).map((g) => g.map((f) => ({ ...f })));
  F.applyPrescanBlockPolicy(rustClusters, prescanCfg);
  compare(`applyPrescanBlockPolicy ${label}`, jsClusters, rustClusters);
}

// ---------------------------------------------------------------------------
// Корпус 1: fixtures
// ---------------------------------------------------------------------------

const fixtureDirs = readdirSync(resolve(REPO, 'fixtures'), { withFileTypes: true })
  .filter((d) => d.isDirectory() && readFileSyncSafe(resolve(REPO, 'fixtures', d.name, 'findings.json')))
  .map((d) => d.name);

function readFileSyncSafe(p) {
  try { readFileSync(p, 'utf8'); return true; } catch { return false; }
}

const GATES = {
  default: { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 },
  consensus2: { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 2, confidenceMin: 0.8 },
  strict: { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8, strict: true },
  noConfMin: { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1 },
  conf065: { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.65 },
  singleOnly: { blockOn: [], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 },
};

const fixtureFindings = [];
for (const dir of fixtureDirs) {
  const raw = JSON.parse(readFileSync(resolve(REPO, 'fixtures', dir, 'findings.json'), 'utf8'));
  const findings = raw.findings || [];
  fixtureFindings.push({ dir, findings });
  const paths = [...new Set(findings.map((f) => f.file).filter(Boolean))];
  for (const [gateName, gate] of Object.entries(GATES)) {
    checkCluster(`${dir} [${gateName}]`, findings, gate);
    checkResolveBlockers(`${dir} [${gateName}] skipFp`, findings, paths, gate, { skipFpFilter: true });
  }
  checkDedupe(`${dir}`, findings);
  checkResolveBlockers(`${dir} with-fp-filter`, findings, paths, GATES.default, {});
  findings.forEach((f, i) => checkNormalizeFinding(`${dir}#${i}`, f, f.model || 'm', {}));
  findings.forEach((f, i) => checkSameFindingKey(`${dir}#${i}`, f));
}

// ---------------------------------------------------------------------------
// Корпус 2: литеральные edge-кейсы из тестов
// ---------------------------------------------------------------------------

for (const [i, raw] of [
  undefined, null, '', 'abc', 0.9, 1.5, -0.1, 0, 1, true, false,
  '0.9', '  ', '1e3', '.5', '5.', '+0.5', 'Infinity', '-Infinity', '0x10', '1_000', {}, [],
].entries()) {
  checkNormalizeConfidence(`#${i} ${JSON.stringify(raw)?.slice(0, 40)}`, raw);
}

checkNormalizeFinding('defaults', { file: 'a.ts', line: '5', severity: 'weird', title: 7 }, 'm1', {});
checkNormalizeFinding('requireQuote critical no quote', { severity: 'critical' }, 'm', { requireQuote: true });
checkNormalizeFinding('requireQuote critical with quote', { severity: 'critical', quote: '  x ' }, 'm', { requireQuote: true });
checkNormalizeFinding('requireQuote minor no quote', { severity: 'minor' }, 'm', { requireQuote: true });
checkNormalizeFinding('null finding', null, 'm', {});
checkNormalizeFinding('string finding', 'nope', 'm', {});
checkNormalizeFinding('advisorClaims mixed', {
  file: 'a.ts', line: 1, severity: 'major', advisorClaims: [
    { role: 'security', status: 'verified', reason: 'ok' },
    { role: 'bogus', status: 'verified' },
    { role: 'correctness', status: 'downgraded', severity: 'nit' },
    { role: 'architecture', status: 'pending' },
    { role: 'architecture', status: 'rejected', reason: 5 },
  ],
}, 'm', {});
checkNormalizeFinding('suggestion non-string', { file: 'a.ts', suggestion: 5, quote: '' }, 'm', {});

checkSameFindingKey('markdown chars', { model: 'm', file: 'a.ts', line: 1, severity: 'major', title: '  `Bug` —  «Утечка»...  ' });
checkSameFindingKey('case-sensitive path', { model: 'm', file: 'A.TS', line: 1, severity: 'major', title: 'x' });
checkSameFindingKey('string line', { model: 'm', file: 'a.ts', line: '3', severity: 'major', title: 'x' });
checkSameFindingKey('missing fields', {});

checkDedupe('same model formatted dup', [
  { model: 'm', file: 'a.ts', line: 1, severity: 'major', title: 'Bug', body: 'x' },
  { model: 'm', file: 'a.ts', line: 1, severity: 'major', title: '`bug`.', body: 'y' },
  { model: 'm', file: 'a.ts', line: 1, severity: 'major', title: 'bug', dupCount: 4 },
  { model: 'm2', file: 'a.ts', line: 1, severity: 'major', title: 'bug' },
  { model: 'm2', file: 'a.ts', line: 2, severity: 'major', title: 'bug' },
]);
checkDedupe('empty', []);

const blast = (status, source) => ({
  file: 'a.ts', line: 1, severity: 'major', confidence: 0.9, model: 'blast',
  source, verifierStatus: status,
});
checkCluster('blast unverified soft', [blast('unverified', 'blast-radius')], GATES.default);
checkCluster('blast verified soft', [blast('verified', 'blast-radius')], GATES.default);
checkCluster('blast mixed source', [blast('unverified', 'blast-radius'), {
  file: 'a.ts', line: 1, severity: 'major', confidence: 0.9, model: 'llm',
}], GATES.default);
checkCluster('blast no source field', [{ file: 'a.ts', line: 1, severity: 'major', confidence: 0.9, model: 'b' }], GATES.default);
checkCluster('strict prescan saves', [
  { file: 'a.ts', line: 1, severity: 'major', confidence: 0.95, model: 'm1', prescan: true },
  { file: 'a.ts', line: 1, severity: 'major', confidence: 0.95, model: 'm2' },
], GATES.strict);
checkCluster('strict no prescan no verified', [
  { file: 'a.ts', line: 1, severity: 'major', confidence: 0.95, model: 'm1', verifierStatus: 'unverified' },
  { file: 'a.ts', line: 1, severity: 'major', confidence: 0.95, model: 'm2' },
], GATES.strict);
checkCluster('buckets and line coercion', [
  { file: 'a.ts', line: 0, severity: 'major', model: 'm1', confidence: 0.9 },
  { file: 'a.ts', line: '1', severity: 'major', model: 'm2', confidence: 0.9 },
  { file: 'a.ts', line: 3, severity: 'major', model: 'm3', confidence: 0.9 },
  { file: 'a.ts', line: 4, severity: 'major', model: 'm4', confidence: 0.9 },
  { file: 'a.ts', line: -2, severity: 'major', model: 'm5', confidence: 0.9 },
], GATES.default);
checkCluster('confidence edges', [
  { file: 'a.ts', line: 1, severity: 'critical', model: 'm1' },
  { file: 'a.ts', line: 1, severity: 'critical', model: 'm2', confidence: 'abc' },
  { file: 'a.ts', line: 1, severity: 'critical', model: 'm3', confidence: 0.8 },
], GATES.default);
checkCluster('clusterModels order', [
  { file: 'b.ts', line: 9, severity: 'minor', model: 'z' },
  { file: 'b.ts', line: 8, severity: 'minor', model: 'a' },
  { file: 'b.ts', line: 9, severity: 'minor', model: 'z' },
  { file: 'b.ts', line: 8, severity: 'minor', model: 'm' },
], GATES.default);
checkCluster('empty', [], GATES.default);

checkSoftConsensus('empty', []);
checkSoftConsensus('no source', [{ severity: 'major' }]);
checkSoftConsensus('all blast unverified', [blast('unverified', 'blast-radius'), blast('rejected', 'blast-radius')]);
checkSoftConsensus('all blast one verified', [blast('unverified', 'blast-radius'), blast('verified', 'blast-radius')]);

checkPrescanPolicy('non-block rule', [[
  { file: 'a.ts', line: 1, severity: 'critical', model: 'p', prescan: true, prescanBlock: false, consensusBlock: true },
]], { enabled: true, blockEnabled: true });
checkPrescanPolicy('blockEnabled false', [[
  { file: 'a.ts', line: 1, severity: 'critical', model: 'p', prescan: true, prescanBlock: true, consensusBlock: true },
]], { enabled: true, blockEnabled: false });
checkPrescanPolicy('disabled', [[
  { file: 'a.ts', line: 1, severity: 'critical', model: 'p', prescan: true, prescanBlock: false, consensusBlock: true },
]], { enabled: false });
checkPrescanPolicy('null cfg', [[
  { file: 'a.ts', line: 1, severity: 'critical', model: 'p', prescan: true, consensusBlock: true },
]], null);
checkPrescanPolicy('non-prescan untouched', [[
  { file: 'a.ts', line: 1, severity: 'critical', model: 'p', consensusBlock: true },
]], { enabled: true, blockEnabled: false });

checkResolveBlockers('fp e2e filtered', [
  { file: 's.ts', line: 1, severity: 'major', confidence: 0.9, title: 'нет e2e', body: 'нет e2e spec', model: 'm1' },
], ['app/e2e/foo.spec.ts', 'server/s.ts'], GATES.default, {});
checkResolveBlockers('fp observability scoped', [
  { file: 'ops/observability/hermes/prometheus.yml', line: 1, severity: 'major', title: 'Alert использует отсутствующие метрики Docker', body: '', model: 'm' },
], ['ops/observability/hermes/prometheus.yml'], GATES.default, {});
checkResolveBlockers('prescan astgrep blocks', [{
  file: 'server/auth.ts', line: 42, severity: 'critical', confidence: 1,
  title: 'ast-grep: raw-x-forwarded-for', body: 'spoofable',
  model: 'astgrep-prescan', prescan: true, prescanBlock: true,
}], ['server/auth.ts'], GATES.default, {
  skipFpFilter: true,
  prescan: { enabled: true, blockEnabled: true, blockRules: ['no-restricted-imports'], astgrep: { blockRules: ['raw-x-forwarded-for'] } },
});
checkResolveBlockers('prescan non-block rule', [{
  file: 'a.ts', line: 1, severity: 'major', confidence: 1, title: 'eslint: other-rule',
  model: 'eslint-prescan', prescan: true, prescanBlock: false,
}], ['a.ts'], GATES.consensus2, { skipFpFilter: true, prescan: { enabled: true, blockEnabled: true, blockRules: ['no-restricted-imports'] } });
checkResolveBlockers('prescan block off advisory', [{
  file: 'a.ts', line: 1, severity: 'critical', confidence: 1, title: 'eslint: no-restricted-imports',
  model: 'eslint-prescan', prescan: true, prescanBlock: true,
}], ['a.ts'], { blockOn: ['critical'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 },
{ skipFpFilter: true, prescan: { enabled: true, blockEnabled: false, blockRules: ['no-restricted-imports'] } });

// ---------------------------------------------------------------------------
// Корпус 3: fingerprint / attachFingerprints / filterStalePriorFindings
// ---------------------------------------------------------------------------

const SAMPLE_DIFF = 'diff --git a/foo.ts b/foo.ts\n--- a/foo.ts\n+++ b/foo.ts\n@@ -1,2 +1,3 @@\n const a = 1;\n-const b = 2;\n+const b = 3;\n+export function foo() { return a; }\n';
const { files: sampleFiles } = parseDiff(SAMPLE_DIFF);
const fooText = Object.fromEntries(sampleFiles.get('foo.ts').text);

checkFingerprint('sample line 1', fooText, 1);
checkFingerprint('sample line 3', fooText, 3);
checkFingerprint('missing line', fooText, 99);
checkFingerprint('null text', null, 2);
checkFingerprint('line 0', fooText, 0);
checkFingerprint('line string', fooText, '2');
checkFingerprint('whitespace only line', { 1: '   ' }, 1);
checkFingerprint('whitespace collapse', { 1: '  const   a =   1;  ' }, 1);
checkFingerprint('unicode', { 1: 'const привет = "😀";' }, 1);
checkFingerprint('tab and nbsp', { 1: '\tconst a = 1;\u00a0' }, 1);
checkFingerprint('feff', { 1: '\uFEFFconst a = 1;' }, 1);

checkAttachFingerprints('sample', [
  { file: 'foo.ts', line: 1 }, { file: 'foo.ts', line: 99 }, { file: 'gone.ts', line: 1 },
], sampleFiles);
checkAttachFingerprints('rec without text', [{ file: 'x.ts', line: 1 }],
  new Map([['x.ts', { validLines: new Set([1]) }]]));
checkAttachFingerprints('empty findings', [], sampleFiles);
checkAttachFingerprints('null files', [{ file: 'foo.ts', line: 1 }], null);

// carry-over: тот же двухпрогонный сценарий, что в test/pr-review.test.mjs (PR #520)
const diffV1 = 'diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1,4 +1,4 @@\n const head = 0;\n+const bug = parseInt(raw);\n+const stable = compute(head);\n const tail = 1;\n';
const diffV2 = 'diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1,5 +1,6 @@\n const head = 0;\n+const extra = 0;\n+const bug = parseInt(raw, 10);\n+const stable = compute(head);\n const tail = 1;\n';
const runOne = parseDiff(diffV1);
const runTwo = parseDiff(diffV2);
const priorsV1 = [
  { file: 'a.ts', line: 2, severity: 'major', title: 'parseInt без основания' },
  { file: 'a.ts', line: 3, severity: 'minor', title: 'compute без проверки' },
];
F.attachFingerprints(priorsV1, runOne.files);
checkStalePrior('carry-over v1→v2', priorsV1, runTwo.files);
checkStalePrior('carry-over v1→v1', priorsV1, runOne.files);

// legacy ±3 и смешанный набор причин (как в test 'filterStalePriorFindings')
const legacyPriors = [
  { file: 'foo.ts', line: 2, severity: 'major', title: 'рядом с текущим hunk' },
  { file: 'foo.ts', line: 6, severity: 'major', title: 'в пределах ±3 от строки 3' },
  { file: 'foo.ts', line: 40, severity: 'major', title: 'далеко от hunks — устарела' },
  { file: 'other.ts', line: 1, severity: 'major', title: 'файл вышел из диффа' },
  { file: 'foo.ts', line: 0, severity: 'major', title: 'без строки — не валидируется' },
];
checkStalePrior('legacy mixed', legacyPriors, sampleFiles);
checkStalePrior('empty priors', [], sampleFiles);

// коллизии: типовая строка `}` на нескольких строках — tie → меньший номер
const collisionDiff = 'diff --git a/c.ts b/c.ts\n--- a/c.ts\n+++ b/c.ts\n@@ -1,5 +1,5 @@\n function a() {\n }\n function b() {\n }\n+const x = 1;\n';
const { files: collisionFiles } = parseDiff(collisionDiff);
const fpBrace = F.findingFingerprint(collisionFiles.get('c.ts'), 2);
checkStalePrior('collision tie', [
  { file: 'c.ts', line: 2, fp: fpBrace },
  { file: 'c.ts', line: 4, fp: fpBrace },
  { file: 'c.ts', line: 2, fp: 'gone12345678' },
  { file: 'c.ts', line: 2, title: 'legacy near' },
], collisionFiles);

// ---------------------------------------------------------------------------
// Корпус 5: FP-фильтр на всех находках fixtures/ с changedPaths из diff.patch
// ---------------------------------------------------------------------------

function checkMatchesFp(label, text, changedPaths, opts) {
  const js = runJs(F.matchesKnownFalsePositive, text, changedPaths, opts).value;
  const rust = F.matchesKnownFalsePositive(text, changedPaths, opts);
  compare(`matchesKnownFalsePositive ${label}`, js, rust);
}

function checkFilterFp(label, findings, changedPaths, opts) {
  const jsLogs = [];
  const js = runJs(F.filterFalsePositiveFindings, j(findings), changedPaths, (m) => jsLogs.push(m), opts).value;
  const rustLogs = [];
  const rust = F.filterFalsePositiveFindings(j(findings), changedPaths, (m) => rustLogs.push(m), opts);
  compare(`filterFalsePositiveFindings ${label} logs`, jsLogs, rustLogs);
  compare(`filterFalsePositiveFindings ${label}`, j(js), j(rust));
}

let fpFindingsChecked = 0;
for (const { dir, findings } of fixtureFindings) {
  // changedPaths — реальные пути диффа кейса (requiresPath паттернов смотрит именно на них)
  let diffPaths = [];
  try {
    const { files } = parseDiff(readFileSync(resolve(REPO, 'fixtures', dir, 'diff.patch'), 'utf8'));
    diffPaths = [...files.keys()];
  } catch {
    diffPaths = [...new Set(findings.map((f) => f.file).filter(Boolean))];
  }
  for (const [i, f] of findings.entries()) {
    checkMatchesFp(`${dir}#${i}`, `${f.title} ${f.body}`, diffPaths, {});
    fpFindingsChecked += 1;
  }
  checkFilterFp(`${dir}`, findings, diffPaths, {});
}

// Литеральные кейсы FP-паттернов из test/pr-review.test.mjs
for (const [text, path] of [
  ['нет e2e теста на удаление', 'app/e2e/foo.spec.ts'],
  ['file not in pr diff', 'server/auth.ts'],
  ['Alert использует отсутствующие метрики Docker', 'ops/observability/hermes/prometheus.yml'],
  ['Alert использует отсутствующие метрики Docker', 'app/server/api.ts'],
  ['typedef Finding не описывает blast-radius маркеры, а softBlast дублирует проверку через два поля', 'lib/findings.mjs'],
  ['Смена контракта aggregateMetrics.byModel: Map → Object', 'lib/metrics.mjs'],
  ['Гонка при записи snapshot: два процесса перезаписывают файл', 'lib/structure.mjs'],
  ['', 'a.ts'],
  ['undefined undefined', 'a.ts'],
]) {
  checkMatchesFp(`literal "${text.slice(0, 40)}"`, text, [path], {});
}

// Свой набор паттернов (opts.patterns) — передаётся исходниками регексов
const customPatterns = (await import('../../lib/fp-patterns.mjs')).parseFpPatternsText(
  '{"id":"custom","regex":"известный ложняк","requiresPath":"^server/"}\n' +
  '{"id":"basename","regex":"файл","matchChangedBasename":true}\n',
);
checkFilterFp('custom patterns', [
  { file: 'server/a.ts', line: 1, severity: 'major', title: 'известный ложняк', body: '', model: 'm' },
  { file: 'server/a.ts', line: 2, severity: 'major', title: 'файл a.ts не в диффе', body: '', model: 'm' },
  { file: 'server/a.ts', line: 3, severity: 'major', title: 'настоящая', body: '', model: 'm' },
], ['server/a.ts'], { patterns: customPatterns });
checkFilterFp('empty patterns', [
  { file: 'server/a.ts', line: 1, severity: 'major', title: 'нет e2e тестов', body: '', model: 'm' },
], ['app/e2e/a.spec.ts'], { patterns: [] });
checkFilterFp('patternsFile absent → пакетный набор', [
  { file: 'server/a.ts', line: 1, severity: 'major', title: 'нет e2e тестов', body: '', model: 'm' },
], ['app/e2e/a.spec.ts'], { patternsFile: resolve(REPO, 'no-such-file.jsonl') });

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

console.log(`fixtures: ${fixtureDirs.length} dirs`);
console.log(`FP-фильтр: находок сверено ${fpFindingsChecked}`);
console.log(`parity cases compared: ${compared}`);
if (failures.length) {
  console.log(`FAILURES: ${failures.length}`);
  for (const f of failures.slice(0, 20)) {
    console.log('---', f.label);
    console.log('  why :', f.why);
    console.log('  js  :', f.js);
    console.log('  rust:', f.rust);
  }
  process.exit(1);
}
console.log('PARITY OK');
