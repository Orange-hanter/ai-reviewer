/**
 * Parity-прогон Фазы 3, шага 6: Rust metrics ↔ чистый JS fallback.
 *
 * Запуск: node rust/parity/metrics.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртки lib/metrics.mjs и lib/observability.mjs:
 * диспетчеризация читает AI_REVIEWER_METRICS_FORCE_JS на каждый вызов,
 * поэтому js-сторона прогоняется с FORCE_JS=1, rust-сторона — без него.
 *
 * Матрица:
 *   1. parseJsonl — битые строки, пустые, пробелы, вложенный JSON;
 *   2. countBySeverity — только известные severities, пустой вход;
 *   3. slimFindings — лимит, title-cap, prescan, без body;
 *   4. aggregateMetrics — каждый тип события: run_end (wouldBlock/llmFailed/
 *      crashed), router, plan (tokens/union путей/graph_fresh/skip_reasons,
 *      cross_community только > 0), llm_call (byModel-агрегат, duration_ms
 *      отрицательный/строковый), fp_filter, verifier (+skip/error),
 *      stability (null/строка/конечное число, recent-кап 5), verdict
 *      (blocked строго === true, смены на одном sha, repeatedShas на 2-м
 *      прогоне), time_budget; порядок byModel — первое появление;
 *   5. formatMetricsReport — ПОБАЙТОВО по отчёту: source/без source, тренд
 *      стабильности, Graph state, skip reasons по убыванию, модели по
 *      убыванию calls, avg toFixed(1), '—' на пустых долях;
 *   6. подкоманда `pr-review metrics` ↔ bin/pr-review-metrics.mjs (RFC 16, T29) —
 *      stdout побайтово и код возврата: явный путь, путь по умолчанию из env
 *      и из профиля, пустой аргумент, отсутствующий файл (exit 1 + префикс stderr).
 */

import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

const BIN = resolve(REPO, 'rust/target/debug/metrics');
if (!existsSync(BIN)) {
  console.error(`нет бинаря ${BIN} — сначала cargo build`);
  process.exit(1);
}

process.env.AI_REVIEWER_METRICS_FORCE_JS = '1';
const M = await import('../../lib/metrics.mjs');
const O = await import('../../lib/observability.mjs');

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_METRICS_FORCE_JS = '1';
  try {
    return { ok: true, value: fn(...args) };
  } finally {
    delete process.env.AI_REVIEWER_METRICS_FORCE_JS;
  }
};

/** Канонизация: undefined-поля выпадают (JSON их не несёт). */
const j = (x) => JSON.parse(JSON.stringify(x));

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

/// Побайтовое сравнение отчёта (текст, видимый в CI-логе).
function compareBytes(label, jsText, rustText) {
  compared += 1;
  if (jsText !== rustText) {
    const jsLines = jsText.split('\n');
    const rustLines = rustText.split('\n');
    const diff = [];
    for (let i = 0; i < Math.max(jsLines.length, rustLines.length); i++) {
      if (jsLines[i] !== rustLines[i]) {
        diff.push(`line ${i + 1}:\n  js:   ${jsLines[i]}\n  rust: ${rustLines[i]}`);
        if (diff.length >= 3) break;
      }
    }
    failures.push({ label, js: diff.join('\n') || '(length differs)' });
  }
}

// ---------------------------------------------------------------------------
// Корпус 1: parseJsonl
// ---------------------------------------------------------------------------

{
  const texts = [
    '',
    '\n\n',
    '{"type":"run_end"}\nне-json\n\n{"type":"router","route":"skip"}\n',
    '  {"a":1}  \n{"b":{"c":[1,2]}}\n{"broken\n',
    '{"n":1.5}\r\n{"win":"x"}\r\n',
    'null\ntrue\n42\n"str"\n[1]\n',
  ];
  for (const [i, text] of texts.entries()) {
    compare(`parseJsonl #${i}`, runJs(M.parseJsonl, text).value, M.parseJsonl(text));
  }
}

// ---------------------------------------------------------------------------
// Корпус 2: countBySeverity + slimFindings
// ---------------------------------------------------------------------------

{
  const findingSets = [
    [],
    [{ severity: 'critical' }, { severity: 'major' }, { severity: 'major' }, { severity: 'nit' }],
    [{ severity: 'minor' }, { severity: 'unknown' }, {}, { severity: 'CRITICAL' }, { severity: 5 }],
    null,
    [{ severity: 'critical', title: 'x'.repeat(300), file: 'a.ts', line: 1, model: 'm1', prescan: 'eslint', body: 'secret' }],
  ];
  for (const [i, findings] of findingSets.entries()) {
    compare(`countBySeverity #${i}`, runJs(O.countBySeverity, findings).value, O.countBySeverity(findings));
    compare(`slimFindings #${i}`, runJs(O.slimFindings, findings).value, O.slimFindings(findings));
  }
  // лимит
  const many = Array.from({ length: 60 }, (_, i) => ({ file: `f${i}.ts`, line: i, severity: 'nit', title: `t${i}` }));
  compare('slimFindings limit default', runJs(O.slimFindings, many).value, O.slimFindings(many));
  compare('slimFindings limit 3', runJs(O.slimFindings, many, 3).value, O.slimFindings(many, 3));
}

// ---------------------------------------------------------------------------
// Корпус 3: aggregateMetrics
// ---------------------------------------------------------------------------

{
  const corpora = [
    [],
    [
      { type: 'run_end', exitCode: 1, wouldBlock: true },
      { type: 'run_end', exitCode: 0, llmFailed: true },
      { type: 'run_end', crashed: 'yes' },
      { type: 'router', route: 'skip' },
      { type: 'router', route: 'full' },
      { type: 'router', route: 'full' },
      { type: 'router' },
      { type: 'plan', blast_radius: { symbols_total: 4, symbols_resolved: 2, affected_nonempty: 1, graph_fresh: false, skip_reason: 'mixed', skip_reasons: { graph_stale: 1, symbol_unresolved: 2 } } },
      { type: 'plan', blast_radius: { symbols_total: '10', symbols_resolved: 'bad', cross_community: -1, affected_files: ['b.ts', 'a.ts', 'b.ts', 5, ''], graph_fresh: true, skip_reasons: { graph_stale: '2' } } },
      { type: 'plan' },
      { type: 'plan', blast_radius: 'broken' },
      { type: 'llm_call', callType: 'specialist', model: 'm1', ok: true, duration_ms: 2000, prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 },
      { type: 'llm_call', callType: 'specialist', model: 'm1', ok: false, duration_ms: 4000 },
      { type: 'llm_call', callType: 'json', model: 'm2', ok: true, duration_ms: -5 },
      { type: 'llm_call', callType: 'json', model: 'm2', duration_ms: 'abc' },
      { type: 'llm_call', ok: true, duration_ms: '500' },
      { type: 'fp_filter', removed: 3 },
      { type: 'fp_filter', removed: 'bad' },
      { type: 'verifier', verified: 2, rejected: 1, downgraded: 0, unverified: 1 },
      { type: 'verifier', verified: '2' },
      { type: 'verifier_skip' },
      { type: 'verifier_error' },
      { type: 'stability', kept: 5, added: 1, gone: 1, stability: 0.8333333 },
      { type: 'stability', kept: 4, stability: null },
      { type: 'stability', stability: '0.9' },
      { type: 'stability', stability: 1 },
      { type: 'verdict', blocked: true, sha: 'shaA' },
      { type: 'verdict', blocked: false, sha: 'shaA' },
      { type: 'verdict', blocked: true, sha: 'shaA' },
      { type: 'verdict', blocked: 'yes', sha: 'shaB' },
      { type: 'verdict', blocked: true },
      { type: 'time_budget', skippedChunks: 5 },
      { type: 'time_budget', skippedChunks: 'bad' },
      { type: 'unknown_event', x: 1 },
      { type: 5 },
      {},
    ],
    // recent-кап > 5 и повторные sha
    [
      { type: 'stability', stability: 0.1 },
      { type: 'stability', stability: 0.2 },
      { type: 'stability', stability: 0.3 },
      { type: 'stability', stability: 0.4 },
      { type: 'stability', stability: 0.5 },
      { type: 'stability', stability: 0.6 },
      { type: 'verdict', blocked: true, sha: 's1' },
      { type: 'verdict', blocked: true, sha: 's1' },
      { type: 'verdict', blocked: true, sha: 's1' },
      { type: 'verdict', blocked: true, sha: 's1' },
    ],
  ];
  for (const [i, records] of corpora.entries()) {
    compare(`aggregateMetrics #${i}`,
      runJs(M.aggregateMetrics, records).value,
      M.aggregateMetrics(records));
  }
}

// ---------------------------------------------------------------------------
// Корпус 4: formatMetricsReport (ПОБАЙТОВО)
// ---------------------------------------------------------------------------

{
  const aggSources = [
    M.aggregateMetrics([]),
    M.aggregateMetrics([
      { type: 'run_end', wouldBlock: true },
      { type: 'run_end', wouldBlock: false, llmFailed: true, crashed: true },
      { type: 'router', route: 'full' },
      { type: 'router', route: 'full' },
      { type: 'router', route: 'light' },
      { type: 'plan', blast_radius: { symbols_total: 4, symbols_resolved: 2, affected_nonempty: 1, graph_fresh: false, skip_reasons: { no_deps: 3, stale: 1, mixed: 3 } } },
      { type: 'plan', blast_radius: { symbols_total: 2, graph_fresh: true, skip_reasons: { stale: 1 } } },
      { type: 'llm_call', callType: 'json', model: 'm1', ok: true, total_tokens: 10, duration_ms: 1500 },
      { type: 'llm_call', callType: 'json', model: 'm1', ok: false, duration_ms: 500 },
      { type: 'llm_call', callType: 'prose', model: 'm2', ok: true, duration_ms: 2500 },
      { type: 'fp_filter', removed: 7 },
      { type: 'verifier', verified: 1, rejected: 2, downgraded: 1, unverified: 3 },
      { type: 'verifier_skip' },
      { type: 'verifier_error' },
      { type: 'stability', kept: 10, added: 2, gone: 3, stability: 0.8 },
      { type: 'stability', kept: 9, added: 1, gone: 1, stability: 0.6666666 },
      { type: 'verdict', blocked: true, sha: 'a' },
      { type: 'verdict', blocked: false, sha: 'a' },
      { type: 'time_budget', skippedChunks: 12 },
    ]),
  ];
  for (const [i, agg] of aggSources.entries()) {
    for (const source of ['', 'runs.jsonl', 'путь/с пробелом.jsonl']) {
      compareBytes(`formatMetricsReport #${i} source=${JSON.stringify(source)}`,
        runJs(M.formatMetricsReport, agg, source).value,
        M.formatMetricsReport(agg, source));
    }
  }
  // форматирование независимо от стороны агрегации: JS-agg → Rust-format
  {
    process.env.AI_REVIEWER_METRICS_FORCE_JS = '1';
    const jsAgg = M.aggregateMetrics([{ type: 'run_end', wouldBlock: true }, { type: 'time_budget', skippedChunks: 2 }]);
    delete process.env.AI_REVIEWER_METRICS_FORCE_JS;
    compareBytes('formatMetricsReport js-agg → rust-format',
      runJs(M.formatMetricsReport, jsAgg, 's').value,
      M.formatMetricsReport(jsAgg, 's'));
  }
}

// ---------------------------------------------------------------------------
// Корпус 6: подкоманда pr-review metrics ↔ bin/pr-review-metrics.mjs
// ---------------------------------------------------------------------------

{
  const CLI = resolve(REPO, 'rust/target/debug/pr-review');
  if (!existsSync(CLI)) {
    console.error(`нет бинаря ${CLI} — сначала cargo build`);
    process.exit(1);
  }
  const JS_BIN = resolve(REPO, 'bin/pr-review-metrics.mjs');
  const work = mkdtempSync(join(tmpdir(), 'metrics-cli-parity-'));
  const log = [
    '{"type":"run_end","wouldBlock":true}',
    'не-json',
    '{"type":"router","route":"full"}',
    '{"type":"llm_call","callType":"json","model":"m1","ok":true,"total_tokens":10,"duration_ms":1500}',
    '{"type":"stability","kept":10,"added":2,"gone":3,"stability":0.8}',
    '{"type":"verdict","blocked":true,"sha":"a"}',
    '{"type":"verdict","blocked":false,"sha":"a"}',
    '',
  ].join('\n');
  writeFileSync(join(work, 'runs.jsonl'), log);
  mkdirSync(join(work, 'obs'));
  writeFileSync(join(work, 'obs', 'custom.jsonl'), log);
  mkdirSync(join(work, '.ai-review', 'observability'), { recursive: true });
  writeFileSync(join(work, '.ai-review', 'observability', 'pr-review-runs.jsonl'), '{"type":"router","route":"skip"}\n');

  // чистое окружение: REVIEW_* родителя не должны менять путь по умолчанию
  const baseEnv = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('REVIEW_')));
  const cases = [
    { label: 'явный относительный путь', args: ['runs.jsonl'] },
    { label: 'явный абсолютный путь', args: [join(work, 'runs.jsonl')] },
    { label: 'путь по умолчанию из профиля', args: [] },
    { label: 'пустой аргумент → путь по умолчанию', args: [''] },
    { label: 'путь по умолчанию из env', args: [], env: { REVIEW_OBSERVABILITY_DIR: 'obs', REVIEW_OBSERVABILITY_FILE: 'custom.jsonl' } },
    { label: 'лишние аргументы игнорируются', args: ['runs.jsonl', 'extra'] },
    { label: 'отсутствующий файл', args: ['nope/runs.jsonl'] },
  ];
  for (const c of cases) {
    const env = { ...baseEnv, ...c.env };
    const js = spawnSync(process.execPath, [JS_BIN, ...c.args], {
      cwd: work, encoding: 'utf8', env: { ...env, AI_REVIEWER_METRICS_FORCE_JS: '1', AI_REVIEWER_CONFIG_FORCE_JS: '1' },
    });
    const rust = spawnSync(CLI, ['metrics', ...c.args], { cwd: work, encoding: 'utf8', env });
    compare(`cli ${c.label}: код возврата`, js.status, rust.status);
    compareBytes(`cli ${c.label}: stdout`, js.stdout, rust.stdout);
    if (js.status !== 0) {
      // текст системной ошибки у Node и Rust разный — сверяем префикс до ': '
      const prefix = (t) => t.split('\n').find((l) => l.startsWith('[pr-review.metrics]'))?.replace(/: .*$/, '') ?? t;
      compare(`cli ${c.label}: stderr`, prefix(js.stderr), prefix(rust.stderr));
    }
  }
  rmSync(work, { recursive: true, force: true });
}

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

if (failures.length) {
  console.error(`FAIL: ${failures.length} расхождений из ${compared} сравнений`);
  for (const f of failures.slice(0, 12)) {
    console.error(`--- ${f.label}\n  js:   ${f.js}\n  rust: ${f.rust}`);
  }
  process.exit(1);
}
console.log(`OK: metrics parity — ${compared} сравнений, расхождений нет`);
