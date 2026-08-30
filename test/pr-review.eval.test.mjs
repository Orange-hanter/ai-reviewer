// Eval harness + confidence gate (R1/R2). Запуск: node --test ops/pr-review.eval.test.mjs

import './helpers/reference-profile.mjs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { runEvalHarness, runFixtureCase, loadFixtures } from '../lib/eval.mjs';
import { clusterFindings, normalizeConfidence, resolveBlockers, createFindingsFormatter } from '../lib/findings.mjs';
import { selectVerifierCandidates, applyVerifierVerdicts } from '../lib/verifier.mjs';
import { loadReviewConfig } from '../lib/config.mjs';
import { routeDiff, isRenameOnlySection, classifyFilePath } from '../lib/router.mjs';
import { applyRefutation, decideRefutation, parseSpecialistAssignments, mergeSpecialistReferences } from '../lib/specialists.mjs';
import {
  eslintJsonToFindings, dedupeAgainstPrescan, selectLintablePaths, selectLintableEntries,
  normalizeAppPath, buildPrescanPathMap, resolvePrescanDiffKey,
  mapEslintRuleSeverity, buildPrescanPromptSection, formatPrescanLog,
} from '../lib/prescan.mjs';
import {
  astgrepStreamToFindings, mapAstgrepRuleSeverity, filterAstgrepFindings, runAstgrepPrescan,
} from '../lib/astgrep.mjs';
import { runCommand } from '../lib/prescan-shared.mjs';
import { runCombinedPrescan, createPrescanRunner } from '../lib/prescan-runner.mjs';
import {
  decideAutoTriage,
  buildTriageContext,
  findingLocationKey,
  parseInlineReviewTitle,
} from '../lib/triage.mjs';
import { matchesKnownFalsePositive, filterFalsePositiveFindings } from '../lib/findings.mjs';
import { parseFpPatternsText } from '../lib/fp-patterns.mjs';
import { sanitizeFixtureId, resolveFixtureDir, matchPrFilePatch } from '../feedback/promote-candidate.mjs';
import { groupFpEntries, fpAggregateKey, inferPatternId } from '../feedback/promote-patterns.mjs';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseDiff } from '../lib/diff.mjs';
import { createObservability, slimFindings, countBySeverity } from '../lib/observability.mjs';
import {
  collectFeedbackEntries, parseReviewCommentFinding, parseReviewCommentFindings,
  parseStickyFindings, isTrustedReviewAuthor, dedupeFeedbackEntries,
  buildFixtureCandidate, feedbackDedupeKey, harvestFeedback,
  appendFeedbackLog, readFeedbackLog, writeFixtureCandidates, appendFeedbackMarkdown,
  DEFAULT_FP_LABEL,
} from '../lib/feedback.mjs';
import {
  FEEDBACK_SCHEMA_VERSION,
  migrateFeedbackText,
  normalizeFeedbackEntry,
} from '../lib/feedback-schema.mjs';
import { runFeedbackBatch, selectFeedbackPulls } from '../bin/pr-review-feedback-batch.mjs';
import { SEV, MARKERS } from '../lib/constants.mjs';
import { createReviewPipeline } from '../pipeline.mjs';
import { buildReviewJson, buildSarif, writeReviewExports, selectSarifFindings, sarifArtifactUri, sarifAutomationGuid, SARIF_GUID_RE } from '../lib/export.mjs';
import { buildSummary, buildReasoningSection, buildStickyReasoningFooter } from '../lib/report.mjs';
import { access, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { spawnSync } from 'node:child_process';

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

test('normalizeConfidence: missing → 0.5 (не 1.0 — иначе обход гейта), clamp 0–1', () => {
  assert.equal(normalizeConfidence(undefined), 0.5);
  assert.equal(normalizeConfidence(0.95), 0.95);
  assert.equal(normalizeConfidence(1.5), 1);
  assert.equal(normalizeConfidence(-0.1), 0);
});

test('clusterFindings: low confidence critical не блокирует (R2)', () => {
  const gate = { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 };
  const findings = [{ file: 'a.ts', line: 1, severity: 'critical', confidence: 0.4, model: 'm1' }];
  clusterFindings(findings, gate);
  assert.equal(findings[0].consensusBlock, false);
  assert.equal(findings[0].confidenceBlock, false);
});

test('resolveBlockers: FP e2e фильтруется когда spec в changedPaths', () => {
  const gate = { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 };
  const raw = [{ file: 's.ts', line: 1, severity: 'major', confidence: 0.9, title: 'нет e2e', body: 'нет e2e spec', model: 'm1' }];
  const changedPaths = ['app/e2e/foo.spec.ts', 'server/s.ts'];
  const { wouldBlock, findings } = resolveBlockers(raw, changedPaths, gate);
  assert.equal(findings.length, 0, 'e2e FP must be filtered when spec path is in PR');
  assert.equal(wouldBlock, false);
  const unfiltered = resolveBlockers(raw, changedPaths, gate, () => {}, { skipFpFilter: true });
  assert.equal(unfiltered.findings.length, 1, 'without FP filter finding remains');
});

test('applyVerifierVerdicts: rejected убирает блокер-кандидата (R3)', () => {
  const gate = { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 };
  const findings = [{ file: 'a.ts', line: 1, severity: 'critical', confidence: 0.95, title: 'bug', model: 'm1' }];
  const candidates = selectVerifierCandidates(findings, gate);
  const { findings: out, stats } = applyVerifierVerdicts(findings, candidates, [
    { candidateIndex: 0, decision: 'rejected', reason: 'FP' },
  ]);
  assert.equal(out.length, 0);
  assert.equal(stats.rejected, 1);
});

test('applyVerifierVerdicts: downgrade critical → minor, не блок', () => {
  const gate = { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 };
  const findings = [{ file: 'a.ts', line: 1, severity: 'critical', confidence: 0.95, title: 'bug', model: 'm1' }];
  const candidates = selectVerifierCandidates(findings, gate);
  const { findings: out } = applyVerifierVerdicts(findings, candidates, [
    { candidateIndex: 0, decision: 'downgrade', severity: 'minor', reason: 'не критично' },
  ]);
  assert.equal(out.length, 1);
  assert.equal(out[0].severity, 'minor');
  const { wouldBlock } = resolveBlockers(out, ['a.ts'], gate, () => {}, { skipFpFilter: true });
  assert.equal(wouldBlock, false);
});

test('eval harness: все fixtures green', async () => {
  const { ok, metrics, results } = await runEvalHarness();
  assert.equal(metrics.total, 16, 'ожидаем 16 seed fixtures');
  if (!ok) {
    const failed = results.filter((r) => !r.pass).map((r) => `${r.id}: ${r.errors.join('; ')}`);
    assert.fail(`eval fixtures failed:\n${failed.join('\n')}`);
  }
  assert.equal(metrics.passed, 16);
});

test('loadReviewConfig: REVIEW_VERIFIER off', () => {
  const cfg = loadReviewConfig({ REVIEW_VERIFIER: '0', OLLAMA_API_KEY: 'x' });
  assert.equal(cfg.verifier.enabled, false);
});

test('loadReviewConfig: REVIEW_CONFIDENCE_MIN', () => {
  const cfg = loadReviewConfig({ REVIEW_CONFIDENCE_MIN: '0.65', OLLAMA_API_KEY: 'x' });
  assert.equal(cfg.gate.confidenceMin, 0.65);
});

test('eslintJsonToFindings R6: no-restricted-imports → critical', () => {
  const pathMap = buildPrescanPathMap([{ appPath: 'app/client/src/foo.ts', diffKey: 'app/client/src/foo.ts' }]);
  const findings = eslintJsonToFindings([{
    filePath: '/repo/app/client/src/foo.ts',
    messages: [{ ruleId: 'no-restricted-imports', severity: 2, message: 'layer', line: 3 }],
  }], { pathMap });
  assert.equal(findings.length, 1);
  assert.equal(findings[0].severity, 'critical');
  assert.equal(findings[0].prescan, true);
  assert.equal(findings[0].prescanBlock, true);
  assert.equal(findings[0].file, 'app/client/src/foo.ts');
});

test('eslintJsonToFindings R6: diffKey без app/ prefix', () => {
  const pathMap = buildPrescanPathMap([{ appPath: 'app/client/src/foo.ts', diffKey: 'client/src/foo.ts' }]);
  const findings = eslintJsonToFindings([{
    filePath: '/repo/Orakul/app/client/src/foo.ts',
    messages: [{ ruleId: 'no-restricted-imports', severity: 2, message: 'layer', line: 1 }],
  }], { pathMap });
  assert.equal(findings[0].file, 'client/src/foo.ts');
});

test('astgrepStreamToFindings R6+: raw-x-forwarded-for → critical block', () => {
  const pathMap = buildPrescanPathMap([{ appPath: 'app/server/auth.ts', diffKey: 'server/auth.ts' }]);
  const line = JSON.stringify({
    ruleId: 'raw-x-forwarded-for',
    severity: 'error',
    message: 'Use clientIp(req)',
    file: 'server/auth.ts',
    range: { start: { line: 41 } },
  });
  const findings = astgrepStreamToFindings(line, { pathMap, cwd: '/repo/app' });
  assert.equal(findings.length, 1);
  assert.equal(findings[0].severity, 'critical');
  assert.equal(findings[0].prescanBlock, true);
  assert.equal(findings[0].file, 'server/auth.ts');
  assert.equal(findings[0].model, 'astgrep-prescan');
});

test('mapAstgrepRuleSeverity: no-math-round-ts advisory → major', () => {
  assert.equal(mapAstgrepRuleSeverity('no-math-round-ts', 'warning', ['raw-x-forwarded-for']), 'major');
});

test('filterAstgrepFindings: no-math-round-ts только shared/server', () => {
  const findings = [
    { file: 'client/src/foo.tsx', ruleId: 'no-math-round-ts', line: 1 },
    { file: 'shared/dishCost.ts', ruleId: 'no-math-round-ts', line: 1 },
    { file: 'server/auth.ts', ruleId: 'raw-x-forwarded-for', line: 1 },
    { file: 'server/net.ts', ruleId: 'raw-x-forwarded-for', line: 1 },
  ];
  const out = filterAstgrepFindings(findings);
  assert.deepEqual(out.map((f) => f.file), ['shared/dishCost.ts', 'server/auth.ts']);
});

test('filterAstgrepFindings: layer rules scoped by directory', () => {
  const findings = [
    { file: 'client/src/foo.tsx', ruleId: 'client-imports-server', line: 1 },
    { file: 'shared/x.ts', ruleId: 'shared-imports-upper', line: 1 },
    { file: 'server/auth.ts', ruleId: 'backend-imports-client', line: 1 },
    { file: 'client/src/foo.tsx', ruleId: 'no-dangerously-set-inner-html', line: 1 },
    { file: 'server/db.ts', ruleId: 'no-prisma-raw-unsafe', line: 1 },
  ];
  assert.equal(filterAstgrepFindings(findings).length, 5);
});

test('matchesKnownFalsePositive: e2e-in-diff pattern', () => {
  const paths = ['app/e2e/supplier-delete.spec.ts'];
  assert.equal(matchesKnownFalsePositive('нет e2e теста на удаление', paths), true);
});

test('decideAutoTriage: superseded thread resolves', () => {
  const ctx = buildTriageContext({
    allFindings: [],
    blockers: [],
    changedPaths: ['server/auth.ts'],
    headSha: 'abc1234',
  });
  const d = decideAutoTriage({
    path: 'server/auth.ts',
    line: 10,
    comments: [{ body: '<!-- orakul-ai-review-inline -->\n🟡 **minor (m): old**', author: 'github-actions[bot]' }],
  }, ctx);
  assert.equal(d.action, 'resolve');
  assert.equal(d.reason, 'superseded');
});

test('decideAutoTriage: active blocker skipped', () => {
  const ctx = buildTriageContext({
    allFindings: [{ file: 'server/auth.ts', line: 10 }],
    blockers: [{ file: 'server/auth.ts', line: 10 }],
    changedPaths: [],
    headSha: 'abc',
  });
  const d = decideAutoTriage({
    path: 'server/auth.ts',
    line: 10,
    comments: [{ body: '<!-- orakul-ai-review-inline -->\n🔴 **critical (m): x**', author: 'github-actions[bot]' }],
  }, ctx);
  assert.equal(d.action, 'skip');
  assert.equal(d.reason, 'active_blocker');
});

test('decideAutoTriage: human reviewer thread skipped', () => {
  const ctx = buildTriageContext({ allFindings: [], blockers: [], changedPaths: [], headSha: 'abc' });
  const d = decideAutoTriage({
    path: 'server/auth.ts',
    line: 10,
    comments: [{ body: '<!-- orakul-ai-review-inline -->\n🟡 **minor: spoof**', author: 'human-dev' }],
  }, ctx);
  assert.equal(d.action, 'skip');
  assert.equal(d.reason, 'untrusted_author');
});

test('parseInlineReviewTitle: asterisk in title text', () => {
  const body = '🟡 **minor (m): Use round2 instead of Math.round(x*100)/100**';
  assert.match(parseInlineReviewTitle(body), /round2.*Math\.round/);
});

test('parseInlineReviewTitle: nested bold markers in title', () => {
  const body = '🟡 **minor (m): foo **bar** baz**';
  assert.equal(parseInlineReviewTitle(body), 'foo **bar** baz');
});

test('loadReviewConfig: MoA defaults', () => {
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x' });
  assert.equal(cfg.reviewMode, 'moa');
  assert.deepEqual([...cfg.moa.references], ['kimi-k2.7-code', 'glm-5.2', 'deepseek-v4-flash']);
  assert.equal(cfg.moa.aggregator, 'minimax-m3');
  assert.equal(cfg.verifier.model, 'deepseek-v4-pro');
  assert.equal(cfg.gate.consensusMin, 1);
  assert.equal(cfg.triage.enabled, true);
  assert.deepEqual([...cfg.inlineOn], ['critical', 'major']);
});

test('runAstgrepPrescan: sgconfig smoke on dishCost.ts', async () => {
  const bin = resolve('app/node_modules/.bin/ast-grep');
  try {
    await access(bin);
  } catch {
    return;
  }
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x' });
  const r = await runAstgrepPrescan(['app/shared/dishCost.ts'], cfg, () => {});
  assert.ok(
    r.findings.some((f) => f.ruleId === 'no-math-round-ts'),
    `expected no-math-round-ts, findings=${JSON.stringify(r.findings)} error=${r.error}`,
  );
  assert.equal(r.error, undefined);
});

test('raw-x-forwarded-for: 12 header/get/headers quote+case variants', async () => {
  const bin = resolve(REPO_ROOT, 'node_modules/.bin/ast-grep');
  const configPath = resolve(REPO_ROOT, 'ast-grep/sgconfig.yml');
  const fixture = resolve(REPO_ROOT, 'ast-grep/fixtures/xff-variants.ts');
  try {
    await access(bin);
    await access(fixture);
  } catch {
    return;
  }
  const { stdout } = await runCommand(
    bin,
    ['scan', '-c', configPath, '--json=stream', fixture],
    { cwd: REPO_ROOT },
  );
  const rows = stdout.trim().split('\n').filter(Boolean).map((line) => JSON.parse(line));
  assert.equal(rows.length, 12, `expected 12 xff variants, got ${rows.length}`);
  assert.ok(rows.every((r) => r.ruleId === 'raw-x-forwarded-for'));
});

test('runCommand: maxBuffer rejects once (no double settle)', async () => {
  await assert.rejects(
    () => runCommand(process.execPath, ['-e', "process.stdout.write('x'.repeat(500))"], { maxBuffer: 100 }),
    /exceeded 100 bytes/,
  );
});

test('resolveBlockers R6+: astgrep prescanBlock critical блокирует', () => {
  const gate = { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 };
  const prescanCfg = {
    enabled: true,
    blockEnabled: true,
    blockRules: ['no-restricted-imports'],
    astgrep: { blockRules: ['raw-x-forwarded-for'] },
  };
  const findings = [{
    file: 'server/auth.ts', line: 42, severity: 'critical', confidence: 1,
    title: 'ast-grep: raw-x-forwarded-for', body: 'spoofable',
    model: 'astgrep-prescan', prescan: true, prescanBlock: true, ruleId: 'raw-x-forwarded-for',
  }];
  const { wouldBlock } = resolveBlockers(findings, ['server/auth.ts'], gate, () => {}, { skipFpFilter: true, prescan: prescanCfg });
  assert.equal(wouldBlock, true);
});

test('formatPrescanLog: byTool breakdown', () => {
  const s = formatPrescanLog({ findings: [{}, {}], scanned: 3, byTool: { eslint: 1, astgrep: 1 } });
  assert.match(s, /eslint:1, ast-grep:1/);
});

test('runCombinedPrescan smoke: eslint when astgrep off', async () => {
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x', REVIEW_PRESCAN_ASTGREP: '0' });
  const r = await runCombinedPrescan(['app/shared/dishCost.ts'], cfg, () => {});
  assert.ok(r.byTool);
  assert.equal(typeof r.scanned, 'number');
});

test('createPrescanRunner: custom exec passthrough', async () => {
  const stub = { findings: [], promptSection: '', scanned: 3, byTool: { eslint: 0, astgrep: 0 } };
  const runner = createPrescanRunner({ prescan: { enabled: true } }, { exec: async () => stub });
  assert.equal((await runner.run([])).scanned, 3);
});

test('mergeLintableTotal: max across tools, ignores null/zero', async () => {
  const { mergeLintableTotal } = await import('../lib/prescan-shared.mjs');
  assert.equal(mergeLintableTotal(0, 5), 5);
  assert.equal(mergeLintableTotal(undefined, 3), 3);
  assert.equal(mergeLintableTotal(0, 0), undefined);
});

test('dedupeAgainstPrescan R6: same bucket → LLM dropped', () => {
  const prescan = [{ file: 'a.ts', line: 10, severity: 'critical', title: 'eslint', model: 'eslint-prescan' }];
  const llm = [{ file: 'a.ts', line: 11, severity: 'major', title: 'dup layer', model: 'm1' }];
  assert.equal(dedupeAgainstPrescan(llm, prescan).length, 0);
});

test('dedupeAgainstPrescan R6: app/ prefix alias', () => {
  const prescan = [{ file: 'app/client/x.ts', line: 10, severity: 'critical', model: 'eslint-prescan' }];
  const llm = [{ file: 'client/x.ts', line: 11, severity: 'major', title: 'dup', model: 'm1' }];
  assert.equal(dedupeAgainstPrescan(llm, prescan).length, 0);
});

// Политика с 2026-08-08: minor/nit больше не «всегда в свод». Маршрут считает доказуемость
// (actionability), а не ярлык модели — на трёх прогонах одного sha одна тема получала
// critical/major/minor, при этом среди minor были дефекты гейт-уровня (`-->` в футере,
// `sort` без LC_ALL=C). Блокировка merge по-прежнему по severity: это разные решения.
test('actionability: доказуемый minor уходит в строку, «голый» — в свод', async () => {
  const { routeFindings } = await import('../lib/actionability.mjs');
  const files = new Map([['server/auth.ts', { validLines: new Set([10, 20]) }]]);
  const proven = {
    file: 'server/auth.ts', line: 10, severity: 'minor', title: 'p', model: 'm1',
    confidence: 0.9, consensus: true, suggestion: 'fix()',
  };
  const bare = { file: 'server/auth.ts', line: 20, severity: 'nit', title: 'b', model: 'm1', confidence: 0.4 };
  const routed = routeFindings([proven, bare], files, {
    ctx: { fileClass: () => 'logic' },
  });
  const byTitle = Object.fromEntries(routed.map((r) => [r.finding.title, r]));
  assert.equal(byTitle.p.route, 'inline', 'консенсус + уверенность + фикс + чувствительный путь');
  assert.equal(byTitle.b.route, 'sticky', 'ничем не подкреплённый nit остаётся в своде');
  assert.ok(byTitle.p.finding.actionabilityReasons.length, 'в комментарий уйдёт объяснение, почему он в строке');
});

test('actionability: находка про невидимый чанку файл в строку не поднимается', async () => {
  const { routeFindings } = await import('../lib/actionability.mjs');
  const files = new Map([['server/auth.ts', { validLines: new Set([10]) }]]);
  const f = {
    file: 'server/auth.ts', line: 10, severity: 'minor', title: 'x', model: 'm1',
    confidence: 0.9, consensus: true, suggestion: 'fix()', outOfChunk: true,
  };
  const [routed] = routeFindings([f], files, { ctx: { fileClass: () => 'logic' } });
  assert.equal(routed.route, 'sticky', 'outOfChunk перевешивает остальные сигналы');
});

test('actionability: лимит инлайна берёт самые доказуемые, а не первые', async () => {
  const { routeFindings } = await import('../lib/actionability.mjs');
  const files = new Map([['server/auth.ts', { validLines: new Set([1, 2, 3]) }]]);
  const mk = (line, extra) => ({
    file: 'server/auth.ts', line, severity: 'minor', title: `t${line}`, model: 'm1',
    confidence: 0.9, consensus: true, ...extra,
  });
  const routed = routeFindings([mk(1), mk(2, { suggestion: 'fix()' }), mk(3)], files, {
    maxAdvisoryInline: 1,
    ctx: { fileClass: () => 'logic' },
  });
  const inline = routed.filter((r) => r.route === 'inline').map((r) => r.finding.title);
  assert.deepEqual(inline, ['t2'], 'выиграла находка с конкретным фиксом');
});

test('auto-triage: поднятую в строку advisory бот не резолвит молча', () => {
  const promoted = { file: 'server/auth.ts', line: 10, severity: 'minor', actionabilityScore: 6 };
  const plain = { file: 'server/auth.ts', line: 30, severity: 'minor' };
  const ctx = buildTriageContext({
    allFindings: [promoted, plain], blockers: [], blockOn: ['critical', 'major'],
    changedPaths: [], headSha: 'abc',
  });
  const mk = (line) => ({
    path: 'server/auth.ts', line,
    comments: [{ body: '<!-- orakul-ai-review-inline -->\n🟡 **minor (m): x**', author: 'github-actions[bot]' }],
  });
  const promotedDecision = decideAutoTriage(mk(10), ctx);
  assert.notEqual(promotedDecision.reason, 'advisory_only', 'поднятая по доказуемости не резолвится как advisory');
  assert.notEqual(promotedDecision.action, 'resolve', 'и вообще не резолвится ботом — тред ждёт человека');
  const plainDecision = decideAutoTriage(mk(30), ctx);
  assert.equal(plainDecision.reason, 'advisory_only');
  assert.equal(plainDecision.action, 'resolve', 'обычная advisory закрывается ботом, как раньше');
});

test('classify: доказуемая advisory уходит в inline через маршрутизацию', () => {
  const cfg = { inlineOn: ['critical', 'major'], actionability: { threshold: 4, maxAdvisoryInline: 5 } };
  const files = new Map([['server/auth.ts', { validLines: new Set([10, 20]) }]]);
  const proven = {
    file: 'server/auth.ts', line: 10, severity: 'minor', title: 'доказуемая', body: '',
    model: 'm1', confidence: 0.9, consensus: true, suggestion: 'fix()',
  };
  const bare = { file: 'server/auth.ts', line: 20, severity: 'nit', title: 'голая', body: '', model: 'm1', confidence: 0.3 };
  const { inline, outside } = createFindingsFormatter(cfg).classify([proven, bare], files, {
    fileClass: () => 'logic',
  });
  assert.equal(inline.length, 1, 'в строку ушла одна находка');
  assert.match(inline[0].body, /доказуемая/);
  assert.match(inline[0].body, /в строке, потому что/, 'в комментарии объяснено, почему advisory в строке');
  assert.deepEqual(outside.map((f) => f.title), ['голая']);
});

test('classify: minor/nit → sticky only (REVIEW_INLINE_ON)', () => {
  const diff = `diff --git a/app/client/src/foo.ts b/app/client/src/foo.ts
--- a/app/client/src/foo.ts
+++ b/app/client/src/foo.ts
@@ -1 +1 @@
+import x from 'bad';
`;
  const { files } = parseDiff(diff);
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x', REVIEW_INLINE_ON: 'critical,major' });
  const fmt = createFindingsFormatter(cfg);
  const findings = [
    { file: 'app/client/src/foo.ts', line: 1, severity: 'critical', title: 'block', body: 'x', model: 'm' },
    { file: 'app/client/src/foo.ts', line: 1, severity: 'minor', title: 'nitpick', body: 'y', model: 'm' },
  ];
  const { inline, outside } = fmt.classify(findings, files);
  assert.equal(inline.length, 1);
  assert.equal(outside.length, 1);
  assert.equal(outside[0].severity, 'minor');
});

test('classify: inline contains only the finding, without review metadata', () => {
  const diff = `diff --git a/app/client/src/foo.ts b/app/client/src/foo.ts
--- a/app/client/src/foo.ts
+++ b/app/client/src/foo.ts
@@ -1 +1 @@
+import x from 'bad';
`;
  const { files } = parseDiff(diff);
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x', REVIEW_MODE: 'moa' });
  const { inline } = createFindingsFormatter(cfg).classify([
    {
      file: 'app/client/src/foo.ts', line: 1, severity: 'major', title: 'unsafe import', body: 'Use the boundary.',
      model: 'moa-specialist(minimax-m3←security+correctness+architecture)',
    },
  ], files);
  assert.match(inline[0].body, /\*\*major: unsafe import\*\*/);
  assert.doesNotMatch(inline[0].body, /¹ MoA|модели и методика|подтверждено несколькими моделями/);
  assert.doesNotMatch(inline[0].body, /moa-specialist\(/);
});

test('fp-patterns.jsonl: e2e-in-diff matches', () => {
  assert.equal(matchesKnownFalsePositive('нет e2e теста на удаление', ['app/e2e/foo.spec.ts']), true);
  assert.equal(matchesKnownFalsePositive('file not in pr diff', ['server/auth.ts']), false);
});

test('groupFpEntries: aggregates recurring FP', () => {
  const mk = (pr, title) => ({
    signal: 'fp',
    pr,
    finding: { file: 'a.ts', severity: 'major', title, line: 1 },
  });
  const groups = groupFpEntries([mk(1, 'same'), mk(2, 'same')], { minCount: 2 });
  assert.equal(groups.length, 1);
  assert.equal(fpAggregateKey(mk(1, 'same')), 'a.ts|major|same');
  assert.equal(inferPatternId(mk(1, 'нет e2e теста')), 'e2e-in-diff');
});

test('feedback promote workflow keeps heredocs inside run block', async () => {
  const workflow = await readFile(
    resolve(REPO_ROOT, 'examples/ci/pr-review-feedback-promote.yml'),
    'utf8',
  );
  const lines = workflow.split('\n');
  let runIndent = null;
  let heredocMarker = null;

  for (const [index, line] of lines.entries()) {
    const indent = line.match(/^\s*/)?.[0].length ?? 0;
    if (runIndent != null && line.trim() && indent <= runIndent) {
      assert.equal(
        heredocMarker,
        null,
        `heredoc ${heredocMarker} escaped run block before line ${index + 1}`,
      );
      runIndent = null;
    }
    if (runIndent == null && /^\s*run:\s*\|[0-9+-]*\s*(?:#.*)?$/.test(line)) {
      runIndent = indent;
      continue;
    }
    if (runIndent == null || !line.trim()) continue;
    if (!heredocMarker) {
      const match = /<<-?\s*(?:'([^']+)'|"([^"]+)"|([A-Za-z_][A-Za-z0-9_]*))/.exec(line);
      heredocMarker = match?.[1] ?? match?.[2] ?? match?.[3] ?? null;
    } else if (line.trim() === heredocMarker) {
      heredocMarker = null;
    }
  }

  assert.equal(heredocMarker, null, `unclosed heredoc ${heredocMarker}`);
});

test('decideAutoTriage: advisory minor auto-resolves', () => {
  const ctx = buildTriageContext({
    allFindings: [{ file: 'server/auth.ts', line: 10, severity: 'minor' }],
    blockers: [],
    blockOn: ['critical', 'major'],
    changedPaths: [],
    headSha: 'abc',
  });
  const d = decideAutoTriage({
    path: 'server/auth.ts',
    line: 10,
    comments: [{ body: '<!-- orakul-ai-review-inline -->\n🟡 **minor (m): style**', author: 'github-actions[bot]' }],
  }, ctx);
  assert.equal(d.action, 'resolve');
  assert.equal(d.reason, 'advisory_only');
});

test('decideAutoTriage: advisory skipped when bucket has blocker', () => {
  const ctx = buildTriageContext({
    allFindings: [
      { file: 'server/auth.ts', line: 10, severity: 'critical' },
      { file: 'server/auth.ts', line: 11, severity: 'minor' },
    ],
    blockers: [{ file: 'server/auth.ts', line: 10, severity: 'critical' }],
    blockOn: ['critical', 'major'],
    changedPaths: [],
    headSha: 'abc',
  });
  const d = decideAutoTriage({
    path: 'server/auth.ts',
    line: 11,
    comments: [{ body: '<!-- orakul-ai-review-inline -->\n🟡 **minor (m): style**', author: 'github-actions[bot]' }],
  }, ctx);
  assert.equal(d.action, 'skip');
  assert.equal(d.reason, 'active_blocker');
});

test('decideAutoTriage: advisory skipped when bucket has blockOn finding', () => {
  const ctx = buildTriageContext({
    allFindings: [
      { file: 'server/auth.ts', line: 10, severity: 'major' },
      { file: 'server/auth.ts', line: 11, severity: 'minor' },
    ],
    blockers: [],
    blockOn: ['critical', 'major'],
    changedPaths: [],
    headSha: 'abc',
  });
  const d = decideAutoTriage({
    path: 'server/auth.ts',
    line: 11,
    comments: [{ body: '<!-- orakul-ai-review-inline -->\n🟡 **minor (m): style**', author: 'github-actions[bot]' }],
  }, ctx);
  assert.equal(d.action, 'skip');
  assert.notEqual(d.reason, 'advisory_only');
});

test('sanitizeFixtureId: rejects path traversal', () => {
  assert.equal(sanitizeFixtureId('fp-pr-1-ok'), 'fp-pr-1-ok');
  assert.equal(sanitizeFixtureId('../../evil'), null);
  assert.equal(resolveFixtureDir('/tmp/fixtures', 'fp-pr-1'), '/tmp/fixtures/fp-pr-1');
  assert.equal(resolveFixtureDir('/tmp/fixtures', '../evil'), null);
});

test('matchPrFilePatch: app/ prefix both ways', () => {
  const files = [{ filename: 'app/client/x.ts', patch: 'x' }];
  assert.equal(matchPrFilePatch(files, 'client/x.ts')?.filename, 'app/client/x.ts');
  assert.equal(matchPrFilePatch(files, 'app/client/x.ts')?.filename, 'app/client/x.ts');
});

test('filterFalsePositiveFindings: patternsFile forwarded', () => {
  const custom = parseFpPatternsText(
    '{"id":"t","regex":"unique-fp-marker-xyz","requiresPath":"server/.*"}',
  );
  const findings = [{ title: 'unique-fp-marker-xyz', body: '', file: 'a.ts', line: 1 }];
  const out = filterFalsePositiveFindings(findings, ['server/a.ts'], () => {}, { patterns: custom });
  assert.equal(out.length, 0);
});

test('classify R6: prescan finding matches app/ diff key', () => {
  const diff = `diff --git a/app/client/src/foo.ts b/app/client/src/foo.ts
--- a/app/client/src/foo.ts
+++ b/app/client/src/foo.ts
@@ -1 +1 @@
+import x from 'bad';
`;
  const { files } = parseDiff(diff);
  const fmt = createFindingsFormatter(loadReviewConfig({ OLLAMA_API_KEY: 'x' }));
  const findings = [{ file: 'app/client/src/foo.ts', line: 1, severity: 'critical', title: 'eslint', body: 'x', model: 'eslint-prescan' }];
  const { inline, outside } = fmt.classify(findings, files);
  assert.equal(inline.length, 1);
  assert.equal(outside.length, 0);
});

test('resolveBlockers R6: prescan major non-block rule не блокирует', () => {
  const gate = { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 2, confidenceMin: 0.8 };
  const prescanCfg = { enabled: true, blockEnabled: true, blockRules: ['no-restricted-imports'] };
  const findings = [{
    file: 'a.ts', line: 1, severity: 'major', confidence: 1, title: 'eslint: other-rule',
    model: 'eslint-prescan', prescan: true, prescanBlock: false,
  }];
  const { wouldBlock } = resolveBlockers(findings, ['a.ts'], gate, () => {}, { skipFpFilter: true, prescan: prescanCfg });
  assert.equal(wouldBlock, false);
});

test('resolveBlockers R6: REVIEW_PRESCAN_BLOCK off → advisory', () => {
  const gate = { blockOn: ['critical'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 };
  const prescanCfg = { enabled: true, blockEnabled: false, blockRules: ['no-restricted-imports'] };
  const findings = [{
    file: 'a.ts', line: 1, severity: 'critical', confidence: 1, title: 'eslint: no-restricted-imports',
    model: 'eslint-prescan', prescan: true, prescanBlock: true,
  }];
  const { wouldBlock } = resolveBlockers(findings, ['a.ts'], gate, () => {}, { skipFpFilter: true, prescan: prescanCfg });
  assert.equal(wouldBlock, false);
});

test('selectLintableEntries R6: truncation flag', () => {
  const paths = Array.from({ length: 35 }, (_, i) => `app/server/f${i}.ts`);
  const { entries, truncated, total } = selectLintableEntries(paths, 30);
  assert.equal(total, 35);
  assert.equal(entries.length, 30);
  assert.equal(truncated, true);
});

test('selectLintablePaths R6', () => {
  const paths = selectLintablePaths(['app/server/x.ts', 'docs/a.md', 'server/y.ts']);
  assert.deepEqual(paths, ['app/server/x.ts', 'app/server/y.ts']);
});

test('mapEslintRuleSeverity: max-len → nit', () => {
  assert.equal(mapEslintRuleSeverity('max-len', 1), 'nit');
});

test('loadReviewConfig: REVIEW_PRESCAN off', () => {
  assert.equal(loadReviewConfig({ REVIEW_PRESCAN: '0', OLLAMA_API_KEY: 'x' }).prescan.enabled, false);
});

test('loadReviewConfig: REVIEW_PRESCAN_ASTGREP off', () => {
  const cfg = loadReviewConfig({ REVIEW_PRESCAN_ASTGREP: '0', OLLAMA_API_KEY: 'x' });
  assert.equal(cfg.prescan.astgrep.enabled, false);
});

test('loadReviewConfig: empty REVIEW_PRESCAN_ASTGREP_BLOCK_RULES disables ast-grep block', () => {
  const cfg = loadReviewConfig({ REVIEW_PRESCAN_ASTGREP_BLOCK_RULES: '', OLLAMA_API_KEY: 'x' });
  assert.deepEqual([...cfg.prescan.astgrep.blockRules], []);
  const pathMap = buildPrescanPathMap([{ appPath: 'app/server/auth.ts', diffKey: 'server/auth.ts' }]);
  const line = JSON.stringify({
    ruleId: 'raw-x-forwarded-for', severity: 'error', message: 'x',
    file: 'server/auth.ts', range: { start: { line: 41 } },
  });
  const findings = astgrepStreamToFindings(line, {
    blockRules: new Set(cfg.prescan.astgrep.blockRules), pathMap, cwd: '/repo/app',
  });
  assert.equal(findings[0].prescanBlock, false);
});

test('decideRefutation R5: all rejected → drop', () => {
  assert.deepEqual(decideRefutation([
    { role: 'security', status: 'rejected' },
    { role: 'correctness', status: 'rejected' },
  ]), { action: 'drop' });
});

test('decideRefutation R5: downgraded to nit when claim says nit', () => {
  assert.deepEqual(decideRefutation([
    { role: 'security', status: 'downgraded', severity: 'nit' },
    { role: 'architecture', status: 'rejected' },
  ]), { action: 'downgrade', severity: 'nit' });
});

test('applyRefutation R5: strict — block-tier без claims → minor', () => {
  const gate = { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 };
  const findings = [{
    file: 'a.ts', line: 1, severity: 'critical', confidence: 0.95, title: 'no jury', model: 'moa-specialist',
  }];
  const { findings: out, stats } = applyRefutation(findings, () => {}, { strict: true, blockOn: gate.blockOn });
  assert.equal(out.length, 1);
  assert.equal(out[0].severity, 'minor');
  assert.equal(stats.downgradedNoClaims, 1);
  const { wouldBlock } = resolveBlockers(out, ['a.ts'], gate, () => {}, { skipFpFilter: true });
  assert.equal(wouldBlock, false);
});

test('applyRefutation R5: verified keeps block', () => {
  const gate = { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8 };
  const findings = [{
    file: 'a.ts', line: 1, severity: 'critical', confidence: 0.95, title: 'bug', model: 'm1',
    advisorClaims: [{ role: 'security', status: 'verified', reason: 'ok' }],
  }];
  const { findings: out } = applyRefutation(findings);
  assert.equal(out.length, 1);
  const { wouldBlock } = resolveBlockers(out, ['a.ts'], gate, () => {}, { skipFpFilter: true });
  assert.equal(wouldBlock, true);
});

test('mergeSpecialistReferences R5: merges multi-chunk refs', () => {
  const merged = mergeSpecialistReferences([
    { references: [{ role: 'security', model: 'm1', text: 'chunk0' }] },
    { references: [{ role: 'security', model: 'm1', text: 'chunk1' }] },
  ]);
  assert.equal(merged.length, 1);
  assert.equal(merged[0].chunkCount, 2);
  assert.match(merged[0].text, /chunk0/);
  assert.match(merged[0].text, /chunk1/);
});

test('parseSpecialistAssignments: semantic defaults (not positional MOA_REFERENCE)', () => {
  const a = parseSpecialistAssignments({}, ['m1', 'm2', 'm3']);
  assert.equal(a.length, 3);
  assert.equal(a[0].role, 'security');
  assert.equal(a[0].model, 'deepseek-v4-flash');
  assert.equal(a[1].model, 'kimi-k2.7-code');
  assert.equal(a[2].model, 'glm-5.2');
});

test('loadReviewConfig: specialist model mapping + independent aggregator/verifier', () => {
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x' });
  assert.equal(cfg.specialists.enabled, true);
  const map = Object.fromEntries(cfg.specialists.assignments.map((x) => [x.role, x.model]));
  assert.deepEqual(map, {
    security: 'deepseek-v4-flash',
    correctness: 'kimi-k2.7-code',
    architecture: 'glm-5.2',
  });
  assert.equal(cfg.moa.aggregator, 'minimax-m3');
  assert.equal(cfg.verifier.model, 'deepseek-v4-pro');
  const advisorModels = new Set(cfg.specialists.assignments.map((x) => x.model));
  assert.equal(advisorModels.has(cfg.moa.aggregator), false);
  assert.equal(advisorModels.has(cfg.verifier.model), false);
});

test('loadReviewConfig: REVIEW_SPECIALISTS off in moa', () => {
  const cfg = loadReviewConfig({ REVIEW_MODE: 'moa', REVIEW_SPECIALISTS: '0', OLLAMA_API_KEY: 'x' });
  assert.equal(cfg.specialists.enabled, false);
});

test('routeDiff R4: lockfile-only → skip', () => {
  const diff = `diff --git a/package-lock.json b/package-lock.json
--- a/package-lock.json
+++ b/package-lock.json
@@ -1 +1 @@
+  "version": "2"`;
  const route = routeDiff(['package-lock.json'], diff);
  assert.equal(route.route, 'skip');
});

test('routeDiff R4: docs-only → light', () => {
  const diff = `diff --git a/docs/guide.md b/docs/guide.md
--- a/docs/guide.md
+++ b/docs/guide.md
@@ -1 +1 @@
+# Title`;
  const route = routeDiff(['docs/guide.md'], diff);
  assert.equal(route.route, 'light');
});

test('routeDiff R4: server logic → full', () => {
  const diff = `diff --git a/app/server/api.ts b/app/server/api.ts
--- a/app/server/api.ts
+++ b/app/server/api.ts
@@ -1 +1 @@
+export const x = 1;`;
  assert.equal(routeDiff(['app/server/api.ts'], diff).route, 'full');
  assert.equal(routeDiff(['shared/foo.ts'], diff.replace(/server/g, 'shared')).route, 'full');
});

test('routeDiff R4: docs + server → full', () => {
  const route = routeDiff(['docs/a.md', 'app/server/x.ts'], 'diff');
  assert.equal(route.route, 'full');
});

test('routeDiff R4: rename-only → skip', () => {
  const diff = `diff --git a/app/shared/old.ts b/app/shared/new.ts
similarity index 100%
rename from app/shared/old.ts
rename to app/shared/new.ts
`;
  assert.equal(isRenameOnlySection(diff), true);
  assert.equal(routeDiff(['app/shared/new.ts'], diff).route, 'skip');
});

test('classifyFilePath: generated dist → skip', () => {
  assert.equal(classifyFilePath('dist/bundle.js'), 'skip');
});

test('loadReviewConfig: REVIEW_ROUTER off → full', () => {
  const cfg = loadReviewConfig({ REVIEW_ROUTER: '0', OLLAMA_API_KEY: 'x' });
  assert.equal(cfg.router.enabled, false);
  assert.equal(routeDiff(['package-lock.json'], '', cfg.router).route, 'full');
});

test('slimFindings R7: no body, title cap', () => {
  const slim = slimFindings([{
    file: 'a.ts', line: 1, severity: 'major', model: 'm1',
    title: 'x'.repeat(200), body: 'secret stack trace',
  }]);
  assert.equal(slim[0].title.length, 120);
  assert.equal(slim[0].body, undefined);
});

test('countBySeverity R7', () => {
  const c = countBySeverity([
    { severity: 'critical' }, { severity: 'major' }, { severity: 'major' }, { severity: 'nit' },
  ]);
  assert.deepEqual(c, { critical: 1, major: 2, minor: 0, nit: 1 });
});

test('createObservability R7: append JSONL + run_end', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-obs-'));
  const obs = createObservability({ observability: { enabled: true, dir, file: 'runs.jsonl' } });
  assert.ok(obs.runId);
  obs.logEvent('router', { route: 'full' });
  await obs.flush({ exitCode: 0, wouldBlock: false });
  const lines = (await readFile(join(dir, 'runs.jsonl'), 'utf8')).trim().split('\n').map(JSON.parse);
  assert.equal(lines.length, 2);
  assert.equal(lines[0].type, 'router');
  assert.equal(lines[0].runId, obs.runId);
  assert.equal(lines[1].type, 'run_end');
  assert.equal(lines[1].exitCode, 0);
  assert.ok(lines[1].duration_ms >= 0);
});

test('createObservability R7: disabled → no file', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-obs-off-'));
  const obs = createObservability({ observability: { enabled: false, dir, file: 'runs.jsonl' } });
  assert.equal(obs.enabled, false);
  obs.logEvent('test', {});
  await obs.flush({});
  await assert.rejects(readFile(join(dir, 'runs.jsonl')), { code: 'ENOENT' });
});

test('createObservability R7: serialized writes, run_end last', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-obs-order-'));
  const obs = createObservability({ observability: { enabled: true, dir, file: 'runs.jsonl' } });
  obs.logEvent('alpha', { n: 1 });
  obs.logEvent('beta', { n: 2 });
  await obs.flush({ exitCode: 0 });
  const lines = (await readFile(join(dir, 'runs.jsonl'), 'utf8')).trim().split('\n').map(JSON.parse);
  assert.equal(lines.length, 3);
  assert.deepEqual(lines.slice(0, 2).map((l) => l.type), ['alpha', 'beta']);
  assert.equal(lines[2].type, 'run_end');
});

test('createObservability R7: concurrent logEvent before flush', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-obs-conc-'));
  const obs = createObservability({ observability: { enabled: true, dir, file: 'runs.jsonl' } });
  obs.logEvent('x');
  obs.logEvent('y');
  await obs.flush({ exitCode: 0 });
  const types = (await readFile(join(dir, 'runs.jsonl'), 'utf8')).trim().split('\n').map((l) => JSON.parse(l).type);
  assert.equal(types[types.length - 1], 'run_end');
  assert.deepEqual(types.slice(0, -1).sort(), ['x', 'y']);
});

test('pipeline R7: router light dry-run writes run_end', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-pipe-'));
  const diffPath = join(dir, 'doc.patch');
  await writeFile(diffPath, `diff --git a/docs/guide.md b/docs/guide.md
--- a/docs/guide.md
+++ b/docs/guide.md
@@ -1 +1 @@
+# Title
`);

  const cfg = loadReviewConfig({
    OLLAMA_API_KEY: 'test',
    REVIEW_OBSERVABILITY_DIR: dir,
    REVIEW_OBSERVABILITY_FILE: 'pipe.jsonl',
    REVIEW_EXPORT_DIR: join(dir, 'export'),
    REVIEW_EXPORT_DRY_RUN: '1',
  }, { dryRun: true, diffFile: diffPath, repo: '', prNumber: 0 });

  const code = await createReviewPipeline(cfg).run();
  assert.equal(code, 0);

  const lines = (await readFile(join(dir, 'pipe.jsonl'), 'utf8')).trim().split('\n').map(JSON.parse);
  assert.ok(lines.some((l) => l.type === 'router' && l.route === 'light'));
  assert.equal(lines[lines.length - 1].type, 'run_end');
  assert.equal(lines[lines.length - 1].exitCode, 0);

  const jsonExport = JSON.parse(await readFile(join(dir, 'export', 'orakul-ai-review.json'), 'utf8'));
  assert.equal(jsonExport.router.route, 'light');
  assert.equal(jsonExport.findings.length, 0);
  const sarif = JSON.parse(await readFile(join(dir, 'export', 'orakul-ai-review.sarif.json'), 'utf8'));
  assert.equal(sarif.version, '2.1.0');
  assert.equal(sarif.runs[0].results.length, 0);
});

test('pipeline R7: crash still emits run_end', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-crash-'));
  const diffPath = join(dir, 'app.patch');
  await writeFile(diffPath, `diff --git a/app/server/api.ts b/app/server/api.ts
--- a/app/server/api.ts
+++ b/app/server/api.ts
@@ -1 +1 @@
+export const x = 1;
`);

  const cfg = loadReviewConfig({
    OLLAMA_API_KEY: 'test',
    REVIEW_MODE: 'consensus',
    OLLAMA_MODELS: 'm1',
    REVIEW_ROUTER: '0',
    REVIEW_PRESCAN: '0',
    REVIEW_VERIFIER: '0',
    REVIEW_OBSERVABILITY_DIR: dir,
    REVIEW_OBSERVABILITY_FILE: 'crash.jsonl',
  }, { dryRun: true, diffFile: diffPath, repo: '', prNumber: 0 });

  const pipeline = createReviewPipeline(cfg, {
    graphify: { buildBlastRadius: async () => [] },
    prescan: { run: async () => ({ findings: [], promptSection: '', scanned: 0, skipped: 0 }) },
    github: { readPriorFindings: async () => [] },
    engine: {
      reviewAllModels: async () => { throw new Error('boom-test'); },
      verifyFindings: async () => [],
    },
  });

  await assert.rejects(() => pipeline.run(), /boom-test/);

  const lines = (await readFile(join(dir, 'crash.jsonl'), 'utf8')).trim().split('\n').map(JSON.parse);
  const end = lines[lines.length - 1];
  assert.equal(end.type, 'run_end');
  assert.equal(end.crashed, true);
  assert.match(end.error, /boom-test/);
});

test('loadReviewConfig: REVIEW_OBSERVABILITY off', () => {
  assert.equal(loadReviewConfig({ REVIEW_OBSERVABILITY: '0', OLLAMA_API_KEY: 'x' }).observability.enabled, false);
});

test('loadReviewConfig: REVIEW_EXPORT defaults on (R9)', () => {
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x' });
  assert.equal(cfg.export.enabled, true);
  assert.equal(cfg.export.json, true);
  assert.equal(cfg.export.sarif, true);
  assert.equal(cfg.export.sarifAll, false);
  assert.equal(cfg.export.dryRun, false);
  assert.equal(cfg.export.reasoningInSticky, true);
});

test('buildSarif R9: severity → SARIF level (blockers only by default)', () => {
  const payload = buildReviewJson({
    pr: { number: 1, title: 't', html_url: 'u' },
    headSha: 'abc1234',
    reviewMode: 'consensus',
    wouldBlock: true,
    findings: [
      { file: 'app/server/a.ts', line: 10, severity: 'critical', title: 'IDOR', body: 'x', model: 'm1', confidence: 0.9, consensusBlock: true },
      { file: 'app/server/b.ts', line: 2, severity: 'nit', title: 'nitpick', body: '', model: 'm2', confidence: 0.85 },
    ],
    blockers: [
      { file: 'app/server/a.ts', line: 10, severity: 'critical', title: 'IDOR', body: 'x', model: 'm1', confidence: 0.9, consensusBlock: true },
    ],
    reviews: [],
  });
  const sarif = buildSarif(payload);
  assert.equal(sarif.runs[0].results.length, 1);
  assert.equal(sarif.runs[0].results[0].level, 'error');
  assert.equal(sarif.runs[0].results[0].message.text, 'IDOR');
  assert.equal(sarif.runs[0].results[0].locations[0].physicalLocation.artifactLocation.uri, 'app/server/a.ts');
  assert.equal(sarif.runs[0].automationDetails.guid, undefined);
  const sarifAll = buildSarif(payload, { sarifAll: true, repositoryUri: 'https://github.com/o/r' });
  assert.equal(sarifAll.runs[0].results.length, 2);
  assert.match(sarifAll.runs[0].automationDetails.guid, SARIF_GUID_RE);
  assert.equal(sarifAll.runs[0].automationDetails.guid, sarifAutomationGuid('https://github.com/o/r'));
});

test('sarifArtifactUri R9: percent-encodes path segments', () => {
  assert.equal(sarifArtifactUri('app/server/a.ts'), 'app/server/a.ts');
  assert.equal(sarifArtifactUri('app/my file.ts'), 'app/my%20file.ts');
  assert.equal(sarifArtifactUri(''), 'unknown');
});

test('buildReviewJson R9: redacts long body in export', () => {
  const longBody = 'x'.repeat(800);
  const payload = buildReviewJson({
    pr: { number: 1, title: 't', html_url: '' },
    headSha: 'abc',
    reviewMode: 'consensus',
    wouldBlock: false,
    findings: [{ file: 'a.ts', line: 1, severity: 'major', title: 't', body: longBody, model: 'm1' }],
    blockers: [],
    reviews: [],
  });
  assert.equal(payload.findings[0].body.length, 400);
  assert.equal(payload.findings[0].bodyTruncated, true);
});

test('buildSummary R9: consensus model summaries in sticky', () => {
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x', REVIEW_MODE: 'consensus' });
  const reviews = [
    { model: 'glm-5.2', summary: 'Race in handler when two requests overlap.', findings: [] },
    { model: 'deepseek-v4-flash', summary: 'Same race pattern near line 42.', findings: [] },
  ];
  const { md } = buildSummary(cfg, {
    reviews, ok: reviews, allFindings: [], outside: [], blastBlocks: [], files: new Map(),
    pr: { number: 1, title: 't' }, headSha: 'deadbeef', note: '', blockerClusters: [],
    verifierStats: null, refutationStats: null, prescanResult: null,
  });
  assert.match(md, /Model reasoning \(consensus\)/);
  assert.match(md, /Race in handler/);
  assert.match(md, /"reasoning":\[/);
});

test('buildSummary R9: MoA reasoning chain in sticky + footer', () => {
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x', REVIEW_MODE: 'moa' });
  const reviews = [{
    model: 'moa',
    findings: [{
      file: 'app/server/a.ts', line: 1, severity: 'major', title: 'bug', body: 'b', model: 'moa',
      advisorClaims: [{ role: 'security', status: 'verified', reason: 'endpoint open' }],
    }],
    references: [{ role: 'security', model: 'ds', text: 'Advisor sees SSRF risk in handler.' }],
  }];
  const allFindings = reviews[0].findings;
  const { md } = buildSummary(cfg, {
    reviews, ok: reviews, allFindings, outside: [], blastBlocks: [], files: new Map([['a.ts', {}]]),
    pr: { number: 1, title: 't' }, headSha: 'deadbeef', note: '', blockerClusters: [],
    verifierStats: null, refutationStats: null, prescanResult: null,
  });
  assert.match(md, /MoA reasoning/);
  assert.match(md, /Advisor sees SSRF/);
  assert.match(md, /Jury \(advisor → aggregator\)/);
  assert.match(md, /¹ MoA specialists:/);
  const stickyFindings = parseStickyFindings(md);
  assert.equal(stickyFindings.length, 1);
  assert.match(md, /"reasoning":\[/);
});

test('writeReviewExports R9: JSON + SARIF files', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-export-'));
  const cfg = loadReviewConfig({
    OLLAMA_API_KEY: 'x',
    REVIEW_EXPORT_DIR: dir,
  });
  const payload = buildReviewJson({
    pr: { number: 42, title: 'x', html_url: '' },
    headSha: 'sha',
    reviewMode: 'consensus',
    wouldBlock: false,
    findings: [],
    blockers: [],
    reviews: [],
  });
  const out = await writeReviewExports(cfg, payload);
  assert.ok(out.written.length >= 2);
  const json = JSON.parse(await readFile(join(dir, 'orakul-ai-review.json'), 'utf8'));
  assert.equal(json.pr.number, 42);
  assert.ok(json.swarm);
  const swarm = JSON.parse(await readFile(join(dir, 'orakul-ai-review-swarm.json'), 'utf8'));
  assert.equal(swarm.role, 'external-verdict');
  assert.equal(swarm.wouldBlock, false);
});

test('buildReasoningSection R9: empty without refs/claims/summaries', () => {
  assert.equal(buildReasoningSection([], [], { enabled: true }).text, '');
});

test('buildStickyReasoningFooter R9: consensus excerpts', () => {
  const footer = buildStickyReasoningFooter(
    [{ model: 'm1', summary: 'summary text here' }],
    'consensus',
  );
  assert.equal(footer.length, 1);
  assert.match(footer[0].excerpt, /summary text/);
});

test('selectSarifFindings R9: blockers preferred over advisory', () => {
  const payload = buildReviewJson({
    pr: { number: 1 }, headSha: 'a', reviewMode: 'consensus', wouldBlock: true,
    findings: [
      { file: 'a.ts', line: 1, severity: 'nit', title: 'n', body: '', model: 'm' },
      { file: 'b.ts', line: 2, severity: 'critical', title: 'c', body: '', model: 'm', consensusBlock: true },
    ],
    blockers: [{ file: 'b.ts', line: 2, severity: 'critical', title: 'c', body: '', model: 'm', consensusBlock: true }],
    reviews: [],
  });
  assert.equal(selectSarifFindings(payload, false).length, 1);
  assert.equal(selectSarifFindings(payload, true).length, 2);
});

test('pipeline R9: LLM failure writes export with llmFailed', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-llm-fail-'));
  const diffPath = join(dir, 'app.patch');
  await writeFile(diffPath, `diff --git a/app/server/a.ts b/app/server/a.ts
--- a/app/server/a.ts
+++ b/app/server/a.ts
@@ -1 +1 @@
+export const x = 1;
`);

  const cfg = loadReviewConfig({
    OLLAMA_API_KEY: 'test',
    REVIEW_MODE: 'consensus',
    OLLAMA_MODELS: 'm1',
    REVIEW_ROUTER: '0',
    REVIEW_PRESCAN: '0',
    REVIEW_VERIFIER: '0',
    REVIEW_EXPORT_DIR: join(dir, 'export'),
    REVIEW_EXPORT_DRY_RUN: '1',
  }, { dryRun: true, diffFile: diffPath, repo: '', prNumber: 0 });

  const pipeline = createReviewPipeline(cfg, {
    graphify: { buildBlastRadius: async () => [] },
    prescan: { run: async () => ({ findings: [], promptSection: '', scanned: 0, skipped: 0 }) },
    github: { readPriorFindings: async () => [] },
    engine: {
      reviewAllModels: async () => [{ model: 'm1', error: 'model down' }],
      verifyFindings: async () => [],
    },
  });

  const code = await pipeline.run();
  assert.equal(code, 1);
  const json = JSON.parse(await readFile(join(dir, 'export', 'orakul-ai-review.json'), 'utf8'));
  assert.equal(json.llmFailed, true);
  assert.equal(json.llmErrors[0].model, 'm1');
});

test('pipeline R9: dry-run skips export without REVIEW_EXPORT_DRY_RUN', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-no-export-'));
  const diffPath = join(dir, 'doc.patch');
  await writeFile(diffPath, `diff --git a/docs/guide.md b/docs/guide.md
--- a/docs/guide.md
+++ b/docs/guide.md
@@ -1 +1 @@
+# Title
`);

  const cfg = loadReviewConfig({
    OLLAMA_API_KEY: 'test',
    REVIEW_EXPORT_DIR: join(dir, 'export'),
  }, { dryRun: true, diffFile: diffPath, repo: '', prNumber: 0 });

  await createReviewPipeline(cfg).run();
  await assert.rejects(() => readFile(join(dir, 'export', 'orakul-ai-review.json'), 'utf8'));
});

test('parseReviewCommentFinding R8: production emoji + severity', () => {
  const body = `${MARKERS.inline}\n${SEV.major} **major (glm-5.2): SQL injection risk**\n\nbody`;
  const f = parseReviewCommentFinding({ path: 'server/a.ts', line: 10, body });
  assert.equal(f.severity, 'major');
  assert.match(f.title, /SQL injection/);
});

test('parseReviewCommentFindings R8: grouped inline', () => {
  const body = `${MARKERS.inline}\n${SEV.critical} **critical (m1): leak**\n\na\n\n---\n\n${SEV.major} **major (m2): race**`;
  const fs = parseReviewCommentFindings({ path: 'server/s.ts', line: 5, body });
  assert.equal(fs.length, 2);
  assert.equal(fs[0].title, 'leak');
  assert.equal(fs[1].title, 'race');
});

test('parseReviewCommentFindings R8: compact inline heading without model', () => {
  const body = `${MARKERS.inline}\n**major: race**\n\nbody`;
  const fs = parseReviewCommentFindings({ path: 'server/s.ts', line: 5, body });
  assert.deepEqual(fs, [{ file: 'server/s.ts', line: 5, severity: 'major', title: 'race', model: '' }]);
  assert.equal(parseInlineReviewTitle(body), 'race');
});

test('collectFeedbackEntries R8: reaction expands grouped findings', () => {
  const body = `${MARKERS.inline}\n**critical (m1): a**\n\n---\n\n**major (m2): b**`;
  const c = { id: 1, path: 'x.ts', line: 1, body, user: { login: 'github-actions[bot]', type: 'Bot' } };
  const reactionsByCommentId = new Map([['1', [{ content: '-1', user: { login: 'dev' } }]]]);
  const entries = collectFeedbackEntries({
    repo: 'o/r', pr: 1, inlineComments: [c], reactionsByCommentId,
    trustedAuthors: ['github-actions[bot]'],
  });
  assert.equal(entries.length, 2);
  assert.equal(entries[0].finding.title, 'a');
  assert.equal(entries[1].finding.title, 'b');
});

test('collectFeedbackEntries R8: untrusted inline skipped', () => {
  const body = `${MARKERS.inline}\n**major (m): spoof**`;
  const c = { id: 2, path: 'x.ts', line: 1, body, user: { login: 'evil-user', type: 'User' } };
  const reactionsByCommentId = new Map([['2', [{ content: '-1', user: { login: 'dev' } }]]]);
  const entries = collectFeedbackEntries({
    repo: 'o/r', pr: 1, inlineComments: [c], reactionsByCommentId,
    trustedAuthors: ['github-actions[bot]'],
  });
  assert.equal(entries.length, 0);
});

test('isTrustedReviewAuthor R8', () => {
  assert.equal(isTrustedReviewAuthor('github-actions[bot]', ['github-actions[bot]'], { type: 'Bot' }), true);
  assert.equal(isTrustedReviewAuthor('evil', ['github-actions[bot]'], { type: 'User' }), false);
  assert.equal(isTrustedReviewAuthor('github-actions[bot]', ['github-actions[bot]'], { type: 'User' }), false);
  assert.equal(isTrustedReviewAuthor('fake[bot]', ['github-actions[bot]'], { type: 'Bot' }), false);
  assert.equal(isTrustedReviewAuthor('any', []), true);
});

test('parseStickyFindings R8: orakul-ai-review-data footer', () => {
  const body = `<!-- orakul-ai-review -->\n<!-- orakul-ai-review-data:${JSON.stringify({
    findings: [{ file: 'app/x.ts', line: 3, severity: 'major', title: 'bug' }],
  })} -->`;
  const fs = parseStickyFindings(body);
  assert.equal(fs.length, 1);
  assert.equal(fs[0].file, 'app/x.ts');
});

test('collectFeedbackEntries R8: label ai-review-fp with sticky', () => {
  const entries = collectFeedbackEntries({
    repo: 'o/r', pr: 42, labels: [{ name: DEFAULT_FP_LABEL }],
    stickyFindings: [{ file: 'a.ts', line: 1, severity: 'major', title: 'x' }],
  });
  assert.equal(entries.length, 1);
  assert.equal(entries[0].signal, 'fp');
  assert.equal(entries[0].source, 'label');
});

test('collectFeedbackEntries R8: reaction 👎 on inline', () => {
  const body = `${MARKERS.inline}\n**critical (m1): leak**`;
  const c = { id: 99, path: 'server/s.ts', line: 5, body, user: { login: 'github-actions[bot]', type: 'Bot' } };
  const reactionsByCommentId = new Map([['99', [{ content: '-1', user: { login: 'dev' } }]]]);
  const entries = collectFeedbackEntries({
    repo: 'o/r', pr: 1, inlineComments: [c], reactionsByCommentId,
    trustedAuthors: ['github-actions[bot]'],
  });
  assert.equal(entries.length, 1);
  assert.equal(entries[0].signal, 'fp');
  assert.equal(entries[0].source, 'reaction');
  assert.equal(entries[0].user, 'dev');
});

test('dedupeFeedbackEntries R8', () => {
  const base = [{ repo: 'o/r', pr: 1, source: 'label', signal: 'fp', finding: { file: 'a', line: 1, title: 't' } }];
  const dup = [{ ...base[0], ts: '2026-01-02' }];
  const merged = dedupeFeedbackEntries(base, dup);
  assert.equal(merged.length, 1);
  assert.ok(feedbackDedupeKey(base[0]));
});

test('buildFixtureCandidate R8', () => {
  const c = buildFixtureCandidate({
    pr: 7, repo: 'o/r', ts: '2026-07-05T00:00:00Z', source: 'reaction', signal: 'fp',
    finding: { file: 'server/x.ts', line: 2, severity: 'major', title: 'False alarm' },
  });
  assert.match(c.suggestedId, /^fp-pr-7-/);
  assert.equal(c.meta.expected.shouldBlock, false);
  assert.equal(c.findings[0].file, 'server/x.ts');
});

test('appendFeedbackLog R8: JSONL round-trip', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'r8-log-'));
  const logPath = join(dir, 'fp.jsonl');
  const entry = {
    ts: '2026-07-05', repo: 'o/r', pr: 1, source: 'reaction', signal: 'fp',
    finding: { file: 'a.ts', line: 1, severity: 'major', title: 't' },
  };
  await appendFeedbackLog(logPath, [entry]);
  const rows = await readFeedbackLog(logPath);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].pr, 1);
});

test('feedback schema v1: legacy rows normalize without losing identity', () => {
  const legacy = {
    ts: '2026-07-05T00:00:00.000Z',
    repo: 'o/r',
    pr: 7,
    source: 'reaction',
    signal: 'fp',
    commentId: 42,
    user: 'reviewer',
    finding: { file: 'server/a.ts', line: 9, severity: 'major', title: 'false alarm', model: 'm1' },
  };
  const normalized = normalizeFeedbackEntry(legacy);
  assert.equal(normalized.schemaVersion, FEEDBACK_SCHEMA_VERSION);
  assert.equal(normalized.pr, 7);
  assert.equal(normalized.commentId, 42);
  assert.equal(normalized.finding.title, 'false alarm');
});

test('feedback schema v1: migration reports invalid rows and is idempotent', () => {
  const legacy = JSON.stringify({
    ts: '2026-07-05T00:00:00.000Z',
    repo: 'o/r',
    pr: 7,
    source: 'label',
    signal: 'fp',
    finding: { file: 'server/a.ts', line: 9, severity: 'major', title: 'false alarm' },
  });
  const first = migrateFeedbackText(`${legacy}\nnot-json\n`);
  assert.equal(first.stats.total, 2);
  assert.equal(first.stats.migrated, 1);
  assert.equal(first.stats.invalid, 1);
  assert.match(first.text, /"schemaVersion":1/);
  const second = migrateFeedbackText(first.text);
  assert.equal(second.stats.migrated, 0);
  assert.equal(second.stats.valid, 1);
  assert.equal(second.text, first.text);
});

test('feedback schema v1: JSON Schema artifact matches runtime version', async () => {
  const schema = JSON.parse(await readFile(
    resolve(REPO_ROOT, 'feedback/fp-feedback.schema.json'),
    'utf8',
  ));
  assert.equal(schema.properties.schemaVersion.const, FEEDBACK_SCHEMA_VERSION);
  assert.ok(schema.required.includes('finding'));
});

test('feedback batch: explicit PR wins; schedule filters old and bot PRs', () => {
  const pulls = [
    { number: 9, updated_at: '2026-07-09T00:00:00.000Z', user: { type: 'User' } },
    { number: 8, updated_at: '2026-07-08T00:00:00.000Z', user: { type: 'Bot' } },
    { number: 7, updated_at: '2026-06-01T00:00:00.000Z', user: { type: 'User' } },
  ];
  assert.deepEqual(
    selectFeedbackPulls(pulls, {
      now: Date.parse('2026-07-10T00:00:00.000Z'),
      sinceDays: 14,
      limit: 30,
    }),
    [9],
  );
  assert.deepEqual(selectFeedbackPulls(pulls, { explicitPr: 42 }), [42]);
});

test('feedback batch: explicit PR is harvested through injected client', async () => {
  const seen = [];
  const result = await runFeedbackBatch(
    {
      GITHUB_TOKEN: 'token',
      GITHUB_REPOSITORY: 'o/r',
      REVIEW_FEEDBACK_WRITE: '0',
    },
    ['--dry-run', '--pr', '42'],
    {
      log: () => {},
      createGithubClient: () => ({}),
      harvestFeedback: async (cfg) => {
        seen.push(cfg.github.prNumber);
        return {
          ok: true,
          pr: cfg.github.prNumber,
          newEntries: [{ signal: 'fp' }],
          persistedEntries: 0,
        };
      },
    },
  );
  assert.deepEqual(seen, [42]);
  assert.equal(result.selected, 1);
  assert.equal(result.newEntries, 1);
  assert.equal(result.failed, 0);
});

const yamlIndent = (line) => line.length - line.trimStart().length;

// Контрактные ассерты по workflow цепляются за структуру (скаляр по пути, шаг → его
// shell-скрипт), а не за сырой YAML: regex вида /…\n\s+…/ ломался от безобидной смены
// отступов, хотя поведение не менялось. Полноценный yaml-парсер брать некуда — ops-тесты
// работают без npm-зависимостей, поэтому разбираем нужное подмножество по отступам.

/** Снять кавычки и хвостовой комментарий со скалярного значения YAML. */
const yamlScalarValue = (raw) => raw.replace(/\s+#.*$/, '').trim().replace(/^['"]|['"]$/g, '');

/**
 * Скаляр по пути вложенных мэппингов: `concurrency.group` → 'pr-review-feedback'.
 * Поддерживается только блочная форма записи; inline mapping (`{a: b}`) и блочные скаляры
 * (`|`, `>`) на пути не парсятся — на них хелпер падает с явным сообщением, а не возвращает
 * мусор, который выглядел бы как «ассерт зелёный».
 */
function workflowScalar(workflow, path) {
  let lines = workflow.split('\n');
  let indent = 0;
  let raw = '';
  const keys = path.split('.');
  for (const [depth, key] of keys.entries()) {
    // Сравнение строкой, а не собранным regex: ключ с `.`/`*`/`[` иначе стал бы шаблоном.
    const start = lines.findIndex((line) => {
      if (yamlIndent(line) !== indent) return false;
      const trimmed = line.trim();
      const tail = trimmed.startsWith(`${key}:`) ? trimmed.slice(key.length + 1) : null;
      return tail !== null && (tail === '' || /^\s/.test(tail));
    });
    assert.ok(start >= 0, `ключ «${key}» из «${path}» не найден в workflow`);
    raw = yamlScalarValue(lines[start].trim().slice(key.length + 1));
    const isLeaf = depth === keys.length - 1;
    assert.ok(
      isLeaf ? !raw.startsWith('{') && !/^[|>]/.test(raw) : raw === '',
      `«${key}» в «${path}» записан неподдерживаемой формой YAML (${raw || '<пусто>'}) — хелпер разбирает только блочные мэппинги со скалярными листьями`,
    );
    const rest = lines.slice(start + 1);
    const end = rest.findIndex((line) => line.trim() && yamlIndent(line) <= indent);
    lines = end === -1 ? rest : rest.slice(0, end);
    indent += 2;
  }
  return raw;
}

/** Shell-скрипт шага по его `name:` — с дедентом, без окружающего YAML. */
function workflowStepScript(workflow, stepName) {
  const lines = workflow.split('\n');
  const start = lines.findIndex((line) => {
    const match = /^- name:\s*(.+)$/.exec(line.trim());
    return match ? yamlScalarValue(match[1]) === stepName : false;
  });
  assert.ok(start >= 0, `шаг «${stepName}» не найден в workflow`);
  const stepIndent = yamlIndent(lines[start]);
  let runIndent = null;
  const script = [];
  for (const line of lines.slice(start + 1)) {
    if (line.trim() && yamlIndent(line) <= stepIndent) break; // начался следующий шаг
    if (runIndent === null) {
      if (/^\s*run:\s*\|[0-9+-]*\s*(?:#.*)?$/.test(line)) runIndent = yamlIndent(line);
      continue;
    }
    if (line.trim() && yamlIndent(line) <= runIndent) break; // run-блок кончился
    script.push(line);
  }
  assert.ok(runIndent !== null, `у шага «${stepName}» нет run-блока`);
  const body = script.filter((line) => line.trim());
  // Пустой run-блок дал бы Math.min() === Infinity и молчаливую пустую строку — то есть
  // «зелёные» ассерты по скрипту, которого нет.
  assert.ok(body.length > 0, `run-блок шага «${stepName}» пустой`);
  const dedent = Math.min(...body.map(yamlIndent));
  return script.map((line) => line.slice(dedent)).join('\n').trim();
}

test('feedback workflow: scheduled batch persists through a bot PR', async () => {
  const workflow = await readFile(
    resolve(REPO_ROOT, 'examples/ci/pr-review-feedback.yml'),
    'utf8',
  );
  const guardScript = workflowStepScript(workflow, 'Guard against duplicate feedback PR');
  const harvestScript = workflowStepScript(workflow, 'Migrate log + harvest feedback batch');
  const prScript = workflowStepScript(workflow, 'Create feedback data PR if new records');

  // Guard считает своими только фиксированную ветку и легаси-суффикс из цифр: по голому
  // префиксу чужая bot/ai-review-feedback-experiment заблокировала бы harvest навсегда.
  assert.match(guardScript, /ref === "bot\/ai-review-feedback"/);
  assert.match(guardScript, /\^bot\\\/ai-review-feedback-\\d\+\$/);
  assert.doesNotMatch(guardScript, /startsWith\("bot\/ai-review-feedback"\)/);

  assert.match(workflow, /schedule:/);
  assert.match(workflow, /github\.event_name == 'schedule'/);
  // Harvest гоняет движок из пина (отдельный чекаут), а не код из рабочего дерева.
  assert.match(harvestScript, /\.ai-reviewer-engine\/bin\/pr-review-feedback-batch\.mjs/);
  assert.equal(workflowScalar(workflow, 'permissions.actions'), 'write');
  assert.equal(workflowScalar(workflow, 'permissions.contents'), 'write');
  // Имя ветки фиксированное (без ${GITHUB_RUN_ID}): суффикс копил по ветке в день, 14 висячих.
  assert.match(prScript, /BRANCH="bot\/ai-review-feedback"/);
  assert.doesNotMatch(prScript, /bot\/ai-review-feedback-\$\{GITHUB_RUN_ID\}/);
  // Одна ветка + force-push ⇒ одна группа concurrency на все триггеры (иначе cron и
  // labeled-событие затирают друг друга), и обе стороны сравнения проходят миграцию —
  // иначе смена схемы делает каждую старую запись «новой».
  assert.equal(workflowScalar(workflow, 'concurrency.group'), 'pr-review-feedback');
  assert.equal(workflowScalar(workflow, 'concurrency.cancel-in-progress'), 'false');
  assert.match(prScript, /--file "\$RUNNER_TEMP\/head\.jsonl" --write/);

  // Сравниваем МНОЖЕСТВА записей побайтово: без `LC_ALL=C` смена локали раннера
  // переставила бы строки и `comm` снова насчитал бы «новые» на пустом месте, а без `-u`
  // дубль в логе считался бы новой записью.
  assert.match(prScript, /jq -cS \. "\$RUNNER_TEMP\/head\.jsonl" \| LC_ALL=C sort -u/);
  assert.match(prScript, /jq -cS \. "\$LOG" \| LC_ALL=C sort -u/);

  // PR открываем по НОВЫМ ЗАПИСЯМ, а не по байтовому диффу (переупаковка схемы — не повод).
  // Проверяем по смыслу скрипта, а не по форматированию: решение считается через `comm -13`,
  // а весь `git diff` и ранний выход живут ПОСЛЕ ветки «новых записей нет» — то есть diff
  // только поясняет сообщение и не решает, открывать ли PR. Условие матчим по смыслу
  // («if … $NEW … 0»), чтобы переход на [[ … ]] или (( … )) тест не ронял.
  const prLines = prScript.split('\n');
  const decision = prLines.findIndex((line) => /^NEW=.*\bcomm -13\b/.test(line.trim()));
  assert.ok(decision >= 0, 'решение об открытии PR должно считаться через comm -13');
  // Ближайший gate ПОСЛЕ подсчёта: чужая (например, вложенная) ветка с тем же условием
  // не должна подменять собой ту, что охраняет ранний выход.
  const after = prLines.slice(decision + 1);
  const gateOffset = after.findIndex((line) => /^if\b.*\bNEW\b.*(?:-eq|==?)\s*0/.test(line.trim()));
  assert.ok(gateOffset >= 0, 'ранний выход должен стоять под условием «новых записей нет»');
  const gate = decision + 1 + gateOffset;
  // Границы ветки — по вложенности `if`/`fi`, а не «всё, что ниже по тексту»: строка после
  // закрывающего `fi` формально идёт после gate, но выполняется уже при $NEW > 0.
  // Открывающим считается только shell-форма `if …; then` — иначе `if (…) {` из инлайнового
  // node-скрипта ниже наращивал бы глубину, которую нечем закрыть (в JS нет `fi`).
  let depth = 0;
  let gateEnd = -1;
  for (let index = gate; index < prLines.length; index++) {
    const trimmed = prLines[index].trim();
    if (/^if\b.*;\s*then$/.test(trimmed)) depth += 1;
    if (/^fi\b/.test(trimmed) && (depth -= 1) === 0) {
      gateEnd = index;
      break;
    }
  }
  assert.ok(gateEnd > gate, 'ветка «новых записей нет» не закрыта — разбор скрипта сломан');
  for (const [index, line] of prLines.entries()) {
    const at = `строка ${index + 1} шага create-PR`;
    const insideGate = index > gate && index < gateEnd;
    if (/\bgit diff\b/.test(line)) {
      assert.ok(insideGate, `${at}: git diff вне ветки $NEW == 0 снова сделает переупаковку схемы поводом для PR`);
    }
    if (/^exit\s+0\b/.test(line.trim())) {
      assert.ok(insideGate, `${at}: ранний exit 0 вне ветки $NEW == 0`);
    }
  }

  // gh CLI на self-hosted раннере отсутствует — PR и dispatch через REST API (node).
  assert.match(prScript, /\/actions\/workflows\/ci\.yml\/dispatches/);
  assert.match(prScript, /\/actions\/workflows\/pr-review\.yml\/dispatches/);
  assert.doesNotMatch(workflow, /\bgh (pr|workflow) /);
  assert.doesNotMatch(workflow, /HEAD:main/);
  // Обе диспатчимые workflow — на стороне потребителя (его ci.yml и pr-review.yml обязаны
  // иметь workflow_dispatch, см. docs/integration.md); здесь проверяем только шаблон.
});

// Ассерты выше проверяют ФОРМУ шага; этот тест — его ПОВЕДЕНИЕ: те же три команды, взятые
// из workflow, прогоняются на подготовленных логах. Иначе «PR только при новых записях»
// держится на regex, и подмена сравнения прошла бы мимо теста.
test('feedback workflow: подсчёт новых записей выполняется по множеству записей', async (t) => {
  if (spawnSync('jq', ['--version']).error) {
    t.skip('jq недоступен — прогон конвейера сравнения пропущен');
    return;
  }
  const workflow = await readFile(
    resolve(REPO_ROOT, 'examples/ci/pr-review-feedback.yml'),
    'utf8',
  );
  const prScript = workflowStepScript(workflow, 'Create feedback data PR if new records');
  const compare = prScript.split('\n').filter((line) => /^(?:jq -cS|NEW=)/.test(line.trim()));
  assert.equal(compare.length, 3, 'ожидались три строки сравнения (before, after, NEW) из шага create-PR');
  // Из workflow в bash уходят только эти три формы команд — тест исполняет контракт, а не
  // произвольный текст файла: любая другая строка обязана сначала попасть в этот список.
  const allowed = [
    /^jq -cS \. "\$RUNNER_TEMP\/head\.jsonl" \| LC_ALL=C sort -u > "\$RUNNER_TEMP\/before\.jsonl"$/,
    /^jq -cS \. "\$LOG" \| LC_ALL=C sort -u > "\$RUNNER_TEMP\/after\.jsonl"$/,
    /^NEW=\$\(LC_ALL=C comm -13 "\$RUNNER_TEMP\/before\.jsonl" "\$RUNNER_TEMP\/after\.jsonl" \| wc -l \| tr -d ' '\)$/,
  ];
  for (const [index, line] of compare.entries()) {
    assert.match(line.trim(), allowed[index], `строка сравнения ${index + 1} изменилась — обнови ожидаемую форму осознанно`);
  }

  const dir = await mkdtemp(join(tmpdir(), 'feedback-new-'));
  const log = join(dir, 'log.jsonl');
  const countNew = async (head, current) => {
    await writeFile(join(dir, 'head.jsonl'), head, 'utf8');
    await writeFile(log, current, 'utf8');
    const res = spawnSync('bash', ['-c', `set -euo pipefail\n${compare.join('\n')}\necho "$NEW"`], {
      env: { ...process.env, RUNNER_TEMP: dir, LOG: log },
      encoding: 'utf8',
      timeout: 30_000, // зависший jq не должен вешать весь прогон тестов
    });
    assert.equal(res.status, 0, res.stderr);
    const parsed = Number(res.stdout.trim());
    assert.ok(Number.isInteger(parsed), `конвейер вернул не число: ${JSON.stringify(res.stdout)}`);
    return parsed;
  };

  const a = '{"pr":1,"finding":{"file":"a.ts","title":"t"}}';
  const aRepacked = '{"finding":{"title":"t","file":"a.ts"},"pr":1}'; // та же запись, другой порядок ключей
  const b = '{"pr":2,"finding":{"file":"b.ts","title":"u"}}';
  assert.equal(await countNew(`${a}\n`, `${a}\n`), 0, 'без изменений новых записей нет');
  assert.equal(await countNew(`${a}\n`, `${aRepacked}\n`), 0, 'переупаковка ключей — не новая запись (та самая регрессия 2026-08-01)');
  assert.equal(await countNew(`${a}\n`, `${a}\n${b}\n`), 1, 'настоящая новая запись должна детектироваться');
  assert.equal(await countNew(`${a}\n`, `${a}\n${a}\n`), 0, 'дубль уже известной записи не считается новой');
  assert.equal(await countNew('', `${a}\n`), 1, 'bootstrap: пустой HEAD и запись в логе — одна новая');
});

test('writeFixtureCandidates R8: skip existing', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'r8-cand-'));
  const entry = {
    pr: 3, repo: 'o/r', ts: '2026-07-05', source: 'label', signal: 'fp', label: DEFAULT_FP_LABEL,
    finding: { file: 'b.ts', line: 4, severity: 'major', title: 'dup test' },
  };
  const r1 = await writeFixtureCandidates(dir, [entry]);
  const r2 = await writeFixtureCandidates(dir, [entry]);
  assert.equal(r1.written, 1);
  assert.equal(r2.written, 0);
});

test('readFeedbackLog R8: skip corrupt line', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'r8-corrupt-'));
  const logPath = join(dir, 'fp.jsonl');
  const good = JSON.stringify({ pr: 1, source: 'label', signal: 'fp', finding: { file: 'a.ts', line: 1, title: 't' } });
  await writeFile(logPath, `{bad}\n${good}\n`, 'utf8');
  const rows = await readFeedbackLog(logPath);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].pr, 1);
});

test('appendFeedbackMarkdown R8: appends at file end', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'r8-md-'));
  const mdPath = join(dir, 'fp.md');
  await writeFile(mdPath, '## Журнал\n\n### old entry\n', 'utf8');
  const entry = {
    ts: '2026-07-05T12:00:00Z', pr: 1, source: 'reaction', signal: 'fp', reaction: '-1',
    commentId: 1, user: 'dev',
    finding: { file: 'a.ts', line: 2, severity: 'major', title: 'False alarm' },
  };
  await appendFeedbackMarkdown(mdPath, [entry]);
  const md = await readFile(mdPath, 'utf8');
  assert.match(md, /### old entry/);
  assert.ok(md.indexOf('### old entry') < md.indexOf('R8 auto'));
});

test('buildFixtureCandidate R8: null without file', () => {
  assert.equal(buildFixtureCandidate({
    pr: 1, finding: { line: 1, title: 'x' },
  }), null);
});

test('dedupeFeedbackEntries R8: distinct reaction users', () => {
  const base = [{
    repo: 'o/r', pr: 1, source: 'reaction', signal: 'fp', commentId: 9, user: 'alice',
    finding: { file: 'a.ts', line: 1, severity: 'major', title: 't' },
  }];
  const other = [{ ...base[0], user: 'bob' }];
  assert.equal(dedupeFeedbackEntries(base, other).length, 2);
});

test('harvestFeedback R8: mock github dry-run', async () => {
  const cfg = loadReviewConfig({
    OLLAMA_API_KEY: 'x',
    REVIEW_FEEDBACK_WRITE: '0',
    REVIEW_FEEDBACK_LOG: 'ops/pr-review/feedback/test-fp.jsonl',
  }, { repo: 'o/r', prNumber: 5 });
  const github = {
    listIssueLabels: async () => [],
    listInlineComments: async () => [{
      id: 1, path: 'a.ts', line: 3,
      body: `${MARKERS.inline}\n**major (m): bug**`,
      user: { login: 'github-actions[bot]', type: 'Bot' },
    }],
    readStickyReview: async () => null,
    listPullCommentReactions: async () => [{ content: '-1', user: { login: 'dev' } }],
  };
  const r = await harvestFeedback(cfg, github);
  assert.equal(r.newEntries.length, 1);
  assert.equal(r.dryRun, true);
  assert.equal(r.ok, true);
});

test('loadReviewConfig: REVIEW_FEEDBACK off', () => {
  assert.equal(loadReviewConfig({ REVIEW_FEEDBACK: '0', OLLAMA_API_KEY: 'x' }).feedback.enabled, false);
  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'x' });
  assert.equal(cfg.feedback.label, 'ai-review-fp');
  assert.match(cfg.feedback.logFile, /fp-feedback\.jsonl$/);
});
