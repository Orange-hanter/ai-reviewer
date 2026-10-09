/**
 * Parity-прогон Фазы 3, шага 6: Rust export ↔ чистый JS fallback.
 *
 * Запуск: node rust/parity/export.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/export.mjs: диспетчеризация читает
 * AI_REVIEWER_EXPORT_FORCE_JS на каждый вызов, поэтому js-сторона прогоняется
 * с FORCE_JS=1, rust-сторона — без него.
 *
 * SARIF — внешний контракт для GitHub code scanning, поэтому сравнение
 * ПОБАЙТОВОЕ по JSON.stringify(sarif, null, 2): порядок ключей, опускание
 * undefined-полей и UTF-16-обрезки — часть контракта. buildReviewJson
 * сравнивается по канонизированному JSON (undefined-поля выпадают) с
 * вырезанными generatedAt — часы живут на JS-стороне и на совпадение не
 * претендуют.
 *
 * Матрица:
 *   1. sarifAutomationGuid — seed/tool, стабильность, regex GUID;
 *   2. sarifArtifactUri — encodeURIComponent-сегменты, unicode, trim, empty;
 *   3. slimFinding (через buildReviewJson findings/blockers) — обрезки
 *      title/body, bodyTruncated, advisorClaims, actionability-поля;
 *   4. extractReasoningEntries — consensus/moa, references (text/error),
 *      excerptLen/errorMax, truncation;
 *   5. buildReviewJson — дефолты (reviewMode/counts/total ??), router, verdict,
 *      llmErrors, prescan, blast, swarm-вложение (strong/scope/notes);
 *   6. selectSarifFindings — blockers preferred / consensusBlock fallback / sarifAll;
 *   7. buildSarif — ПОБАЙТОВО: 0/N findings, severities→levels, неизвестная
 *      severity→minor, line=0 без региона, repositoryUri+guid, profile из
 *      репозитория (default + orakul: свои toolName/sarifRulePrefix/repositoryUri).
 */

import { existsSync, readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

const BIN = resolve(REPO, 'rust/target/debug/export');
if (!existsSync(BIN)) {
  console.error(`нет бинаря ${BIN} — сначала cargo build`);
  process.exit(1);
}

process.env.AI_REVIEWER_EXPORT_FORCE_JS = '1';
const E = await import('../../lib/export.mjs');
const { setProfile, resetProfile } = await import('../../lib/profile.mjs');

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_EXPORT_FORCE_JS = '1';
  try {
    return { ok: true, value: fn(...args) };
  } finally {
    delete process.env.AI_REVIEWER_EXPORT_FORCE_JS;
  }
};

/** Канонизация: undefined-поля выпадают (JSON их не несёт). */
const j = (x) => JSON.parse(JSON.stringify(x));

/** buildReviewJson: generatedAt (top-level и swarm) — часы, вырезаем перед сравнением. */
const stripClock = (x) => {
  const c = j(x);
  delete c.generatedAt;
  if (c.swarm) delete c.swarm.generatedAt;
  return c;
};

let compared = 0;
const failures = [];

function compare(label, jsResult, rustResult) {
  compared += 1;
  try {
    assert.deepEqual(j(rustResult), j(jsResult));
  } catch (e) {
    failures.push({
      label,
      js: JSON.stringify(jsResult)?.slice(0, 400),
      rust: JSON.stringify(rustResult)?.slice(0, 400),
    });
  }
}

/// Побайтовое сравнение SARIF (внешний контракт GitHub code scanning).
function compareSarifBytes(label, jsSarif, rustSarif) {
  compared += 1;
  const jsBytes = JSON.stringify(jsSarif, null, 2);
  const rustBytes = JSON.stringify(rustSarif, null, 2);
  if (jsBytes !== rustBytes) {
    let i = 0;
    while (i < Math.min(jsBytes.length, rustBytes.length) && jsBytes[i] === rustBytes[i]) i++;
    failures.push({
      label,
      js: `…${jsBytes.slice(Math.max(0, i - 60), i + 80)}`,
      rust: `…${rustBytes.slice(Math.max(0, i - 60), i + 80)}`,
      divergeAt: i,
    });
  }
}

// ---------------------------------------------------------------------------
// Корпус 1: sarifAutomationGuid
// ---------------------------------------------------------------------------

{
  const seeds = [
    'https://github.com/o/r', 'https://github.com/acme/widgets', '',
    'x'.repeat(200), 'https://github.com/o/r?query=1&other=2',
  ];
  const tools = ['orakul-ai-review', 'ai-review', 't', 'émoji-🔍-tool'];
  for (const seed of seeds) {
    for (const tool of tools) {
      compare(`guid ${JSON.stringify(seed)}/${tool}`,
        runJs(E.sarifAutomationGuid, seed, tool).value,
        E.sarifAutomationGuid(seed, tool));
    }
  }
  const g = E.sarifAutomationGuid('https://github.com/o/r');
  assert.ok(E.SARIF_GUID_RE.test(g), 'guid удовлетворяет SARIF_GUID_RE');
}

// ---------------------------------------------------------------------------
// Корпус 2: sarifArtifactUri
// ---------------------------------------------------------------------------

{
  const files = [
    'app/server/a.ts', 'app/my file.ts', '', '   ', 'dir/м файл.ts',
    'a-b_c.d!e~f*g\'h(i).ts', 'a&b=c+d.ts', 'ü/ñ/中.ts', 'a//b', '/lead',
    'a?b#c.ts', 'a%b.ts', 'trail/', 'a\\b.ts', null, undefined,
  ];
  for (const f of files) {
    compare(`artifactUri ${JSON.stringify(f)}`,
      runJs(E.sarifArtifactUri, f).value,
      E.sarifArtifactUri(f));
  }
}

// ---------------------------------------------------------------------------
// Корпус 3: extractReasoningEntries
// ---------------------------------------------------------------------------

{
  const reviewSets = [
    { reviewMode: 'consensus', reviews: [{ model: 'm1', summary: 'ok' }] },
    { reviewMode: 'moa', reviews: [{ model: 'agg', summary: 'ok' }] },
    {
      reviewMode: 'moa',
      reviews: [{
        model: 'agg',
        references: [
          { role: 'security', model: 's1', chunkCount: 2, text: 'text a' },
          { role: 'correctness', model: 'c1', error: 'boom: ' + 'x'.repeat(300) },
          { role: 'architecture', model: 'a1', text: '' },
          { error: 'no model' },
        ],
      }],
    },
    {
      reviewMode: 'consensus',
      reviews: [
        { model: 'm1', summary: 's'.repeat(1000) },
        { model: 'm2', error: 'fail', summary: 'ignored' },
        { model: 'm3' },
      ],
    },
    { reviewMode: 'consensus', reviews: [] },
    { reviewMode: 'consensus', reviews: null },
  ];
  for (const [i, { reviews, reviewMode }] of reviewSets.entries()) {
    compare(`reasoning #${i}`,
      runJs(E.extractReasoningEntries, reviews, reviewMode).value,
      E.extractReasoningEntries(reviews, reviewMode));
    compare(`reasoning #${i} short excerpt`,
      runJs(E.extractReasoningEntries, reviews, reviewMode, { excerptLen: 10, errorMax: 5 }).value,
      E.extractReasoningEntries(reviews, reviewMode, { excerptLen: 10, errorMax: 5 }));
  }
}

// ---------------------------------------------------------------------------
// Корпус 4: buildReviewJson
// ---------------------------------------------------------------------------

{
  const inputs = [
    {},
    { pr: { number: 1, title: 't' }, headSha: 'abc' },
    {
      pr: { number: 7, title: 'x'.repeat(300), html_url: 'https://github.com/o/r/pull/7' },
      headSha: 'a'.repeat(64),
      reviewMode: 'moa', route: { route: 'full', reason: 'big' },
      wouldBlock: true, verdict: { blocked: 2, total: 3 },
      llmFailed: true, llmErrors: ['e1', 'e2'],
      counts: { critical: 1 }, total: 0,
      findings: [{
        file: 'a.ts', line: 2, severity: 'critical', title: 'IDOR', body: 'x'.repeat(700),
        model: 'm1', confidence: 0.9, consensusBlock: true, prescan: 'eslint',
        advisorClaims: [{ role: 'security', status: 'confirmed', reason: 'r'.repeat(300) }],
        refutationStatus: 'upheld', fp: 'abc123', verifierStatus: 'verified',
        actionabilityScore: 6, actionabilityPromoted: true,
      }],
      blockers: [{ file: 'b.ts', line: 0, severity: 'major', title: 'b' }],
      reviews: [{ model: 'm1', summary: 's'.repeat(800) }],
      verifierStats: { verified: 1 }, refutationStats: { upheld: 1 },
      prescanResult: { findings: [1, 2], scanned: 5, skipped: 1, byTool: { eslint: 2 }, error: null },
    },
    {
      blastMetrics: {
        symbols_total: 10, symbols_resolved: 5, affected_nonempty: 2, graph_status: 'fresh',
        affected_files: ['z.ts', 'a.ts'], confidence: { EXTRACTED: 1 }, cross_community: 3,
      },
      changedPaths: ['a.ts', 'in-diff.ts'],
      blockers: [
        { file: 'a.ts', line: 1, severity: 'critical', title: 'prescan blocker', prescan: true },
        { file: 'b.ts', line: 2, severity: 'major', title: 'verified blocker', verifierStatus: 'verified' },
      ],
      wouldBlock: true,
      sourceHint: 'blast-radius',
    },
    { blastMetrics: 'broken', prescanResult: { findings: null } },
  ];
  for (const [i, input] of inputs.entries()) {
    compare(`buildReviewJson #${i}`,
      stripClock(runJs(E.buildReviewJson, input).value),
      stripClock(E.buildReviewJson(input)));
  }
}

// ---------------------------------------------------------------------------
// Корпус 5: selectSarifFindings + buildSarif (ПОБАЙТОВО)
// ---------------------------------------------------------------------------

{
  const payloads = [
    { findings: [], blockers: [] },
    {
      findings: [{ file: 'a.ts', line: 1, severity: 'critical', title: 'IDOR', model: 'm1', confidence: 0.9, consensusBlock: true }],
      blockers: [],
    },
    {
      findings: [
        { file: 'a.ts', line: 1, severity: 'critical', title: 'c', consensusBlock: false },
        { file: 'b.ts', line: 0, severity: 'weird', title: 'w' },
        { file: 'c.ts', line: 3, severity: 'nit', title: 'n'.repeat(600), prescan: 'astgrep' },
      ],
      blockers: [{ file: 'd.ts', line: 4, severity: 'major', title: 'm' }],
    },
    {
      pr: { number: 42 },
      findings: [
        { file: 'app/my dir/f.ts', line: 9, severity: 'minor', title: 'minor finding', confidence: 0.5, consensusBlock: true },
        { file: 'e.ts', line: 5, severity: 'critical', title: 'crit' },
      ],
      blockers: [],
    },
  ];
  const profiles = [
    { label: 'default-profile', profile: undefined, repositoryUri: undefined },
    { label: 'with-repo-uri', profile: undefined, repositoryUri: 'https://github.com/o/r' },
  ];
  for (const [pi, payload] of payloads.entries()) {
    for (const { label, profile, repositoryUri } of profiles) {
      const opts = { repositoryUri };
      if (profile) opts.profile = profile;
      compareSarifBytes(
        `buildSarif payload#${pi} ${label} blockers`,
        runJs(E.buildSarif, payload, opts).value,
        E.buildSarif(payload, opts),
      );
      compareSarifBytes(
        `buildSarif payload#${pi} ${label} all`,
        runJs(E.buildSarif, payload, { ...opts, sarifAll: true }).value,
        E.buildSarif(payload, { ...opts, sarifAll: true }),
      );
      compare(`selectSarifFindings payload#${pi} ${label}`,
        runJs(E.selectSarifFindings, payload, false).value,
        E.selectSarifFindings(payload, false));
      compare(`selectSarifFindings payload#${pi} ${label} all`,
        runJs(E.selectSarifFindings, payload, true).value,
        E.selectSarifFindings(payload, true));
    }
  }
}

// ---------------------------------------------------------------------------
// Корпус 6: buildSarif через buildReviewJson (полный путь pipeline)
// ---------------------------------------------------------------------------

{
  const input = {
    pr: { number: 12, title: 'PR title', html_url: 'https://github.com/o/r/pull/12' },
    headSha: 'deadbeef',
    reviewMode: 'consensus',
    wouldBlock: true,
    findings: [
      { file: 'src/a.ts', line: 10, severity: 'critical', title: 'rce', body: 'b', model: 'm1', confidence: 0.99, consensusBlock: true, verifierStatus: 'verified' },
      { file: 'src/b.ts', line: 20, severity: 'major', title: 'bug', body: 'b2', model: 'm2', confidence: 0.7, consensusBlock: true },
    ],
    blockers: [],
    reviews: [{ model: 'm1', summary: 'sum' }],
  };
  const jsPayload = runJs(E.buildReviewJson, input).value;
  const rustPayload = E.buildReviewJson(input);
  compareSarifBytes('buildSarif via buildReviewJson',
    runJs(E.buildSarif, jsPayload, { repositoryUri: 'https://github.com/o/r' }).value,
    E.buildSarif(rustPayload, { repositoryUri: 'https://github.com/o/r' }));
}

// ---------------------------------------------------------------------------
// Корпус 7: профиль orakul (свои toolName/sarifRulePrefix/repositoryUri)
// ---------------------------------------------------------------------------

{
  setProfile(JSON.parse(readFileSync(resolve(REPO, 'profiles/orakul.json'), 'utf8')));
  try {
    const payload = {
      pr: { number: 3 },
      findings: [{ file: 'a.ts', line: 1, severity: 'major', title: 't', consensusBlock: true }],
      blockers: [],
    };
    compareSarifBytes('buildSarif orakul profile',
      runJs(E.buildSarif, payload, {}).value,
      E.buildSarif(payload, {}));
    compare('toolName orakul',
      runJs(E.toolName).value,
      E.toolName());
    compareSarifBytes('buildSarif orakul profile explicit repo',
      runJs(E.buildSarif, payload, { repositoryUri: 'https://github.com/acme/other' }).value,
      E.buildSarif(payload, { repositoryUri: 'https://github.com/acme/other' }));
  } finally {
    resetProfile();
  }
}

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

if (failures.length) {
  console.error(`FAIL: ${failures.length} расхождений из ${compared} сравнений`);
  for (const f of failures.slice(0, 12)) {
    console.error(`--- ${f.label}`);
    if (f.divergeAt !== undefined) {
      console.error(`  diverge@${f.divergeAt}\n  js:   ${f.js}\n  rust: ${f.rust}`);
    } else {
      console.error(`  js:   ${f.js}\n  rust: ${f.rust}`);
    }
  }
  process.exit(1);
}
console.log(`OK: export parity — ${compared} сравнений, расхождений нет`);
