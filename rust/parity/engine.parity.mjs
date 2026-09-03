/**
 * Parity-прогон Фазы 3, шага 2: Rust engine ↔ чистый JS fallback.
 *
 * Запуск: node rust/parity/engine.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ createReviewEngine (lib/engine.mjs): оркестрация в обеих
 * ветках одна и та же (JS), rust-ветка дополнительно гоняет чистые части
 * (скоуп priors, demote, aggregate) через бинарь rust/target/debug/engine.
 * Диспетчеризация — env AI_REVIEWER_ENGINE_FORCE_JS (читается на каждый вызов),
 * поэтому js-сторона прогоняется с FORCE_JS=1, rust-сторона — без него.
 * LLM фейкается инжектным ollama-объектом (как в test/pr-review.engine.test.mjs),
 * prompt — инжектным билдером: DI-точки движка не трогаем, сравниваем только
 * чистую логику. Входы/выходы канонизируются JSON round-trip (`j()`).
 *
 * Матрица:
 *   1. consensus, все модели ок: агрегация находок по чанкам, дедуп внутри
 *      модели, verdict-ранк (changes_requested побеждает), summary — первое непустое;
 *   2. consensus, primary падает → fallback-модель, fallbackFor, лог дедупа;
 *   3. consensus, все падают → error у каждой модели;
 *   4. дедлайн истёк → skipped, LLM не зовётся, fallback не запускается;
 *   5. scoped chunks: priorFindings режутся по файлам чанка, слепой блокер
 *      понижается до nit (demoteOutOfChunk), partialPaths/unknownPaths;
 *   6. MoA + specialists: advisors prose → jury JSON с advisorClaims, label
 *      атрибуция, references смержены; все advisors недоступны → error;
 *   7. MoA fallback: aggregator недоступен → резервная модель, juryBypassed;
 *   8. aggregate-края: вердикт-тай, missing verdict, все чанки skipped.
 */

import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

if (!existsSync(resolve(REPO, 'rust/target/debug/engine'))) {
  console.error('нет бинаря rust/target/debug/engine — сначала cargo build');
  process.exit(1);
}

const { createReviewEngine } = await import('../../index.mjs');

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

/** Прогон reviewAllModels на обеих ветках: js — FORCE_JS=1, rust — бинарь. */
async function checkReview(label, cfg, { ollama, prompt, chunks, ctx }) {
  const run = async (forceJs) => {
    const logs = [];
    if (forceJs) process.env.AI_REVIEWER_ENGINE_FORCE_JS = '1';
    else delete process.env.AI_REVIEWER_ENGINE_FORCE_JS;
    try {
      const engine = createReviewEngine(cfg, {
        ollama, prompt, log: (...a) => logs.push(a.join(' ')), observability: null,
      });
      const reviews = await engine.reviewAllModels(chunks, ctx);
      return { reviews, logs };
    } finally {
      delete process.env.AI_REVIEWER_ENGINE_FORCE_JS;
    }
  };
  const js = await run(true);
  const rust = await run(false);
  compare(`${label} reviews`, js.reviews, rust.reviews);
  compare(`${label} logs`, js.logs, rust.logs);
}

// ---------------------------------------------------------------------------
// Конфиги и фейки (по образцу test/pr-review.engine.test.mjs)
// ---------------------------------------------------------------------------

const ENGINE_CFG = {
  reviewMode: 'consensus',
  models: { list: ['m1', 'm2'], fallback: ['mfb'] },
  moa: { aggregator: 'agg', label: 'moa', references: [], refMaxTokens: 100, refTemp: 0.6, aggTemp: 0.4 },
  limits: { concurrency: 2 },
  specialists: { enabled: false, assignments: [] },
  ollama: { temperature: 0.4 },
  gate: {},
};
const PROMPT = { buildMessages: () => [{ role: 'user', content: 'x' }] };
const CHUNKS = [{ annotated: 'd1', fileContents: '' }, { annotated: 'd2', fileContents: '' }];

// ---------------------------------------------------------------------------
// 1. Все модели ок: агрегация, дедуп внутри модели, verdict-ранк
// ---------------------------------------------------------------------------

await checkReview('consensus all ok', ENGINE_CFG, {
  ollama: {
    chatJson: async (msgs, model) => ({
      summary: model === 'm1' ? '' : 'обзор',
      verdict: msgs[0].content === 'x' ? 'comment' : 'approve',
      findings: [
        // одна тема из обоих чанков — схлопывается внутри модели
        { file: 'a.ts', line: 1, severity: 'minor', title: 'Дефект', confidence: 0.5 },
        { file: `${model}.ts`, line: 2, severity: 'major', title: `у ${model}`, confidence: 0.9 },
      ],
    }),
  },
  prompt: PROMPT,
  chunks: CHUNKS,
  ctx: { pr: {} },
});

// ---------------------------------------------------------------------------
// 2. Primary падает → fallback; 3. Все падают
// ---------------------------------------------------------------------------

await checkReview('consensus primary fails → fallback', ENGINE_CFG, {
  ollama: {
    chatJson: async (_msgs, model) => {
      if (model === 'm1') throw new Error('модель лежит');
      return { summary: 'ok', verdict: 'approve', findings: [] };
    },
  },
  prompt: PROMPT,
  chunks: [CHUNKS[0]],
  ctx: { pr: {} },
});

await checkReview('consensus all fail', ENGINE_CFG, {
  ollama: { chatJson: async () => { throw new Error('down'); } },
  prompt: PROMPT,
  chunks: CHUNKS,
  ctx: { pr: {} },
});

// ---------------------------------------------------------------------------
// 4. Дедлайн истёк: ни одного LLM-вызова, skipped, fallback не зовётся
// ---------------------------------------------------------------------------

{
  const mkOllama = (counter) => ({
    chatJson: async () => { counter.n++; throw new Error('must not be called'); },
  });
  const run = async (forceJs, counter) => {
    if (forceJs) process.env.AI_REVIEWER_ENGINE_FORCE_JS = '1';
    else delete process.env.AI_REVIEWER_ENGINE_FORCE_JS;
    try {
      const engine = createReviewEngine(ENGINE_CFG, { ollama: mkOllama(counter), prompt: PROMPT, log: () => {}, observability: null });
      return await engine.reviewAllModels(CHUNKS, { pr: {}, deadline: Date.now() - 1000 });
    } finally {
      delete process.env.AI_REVIEWER_ENGINE_FORCE_JS;
    }
  };
  const jsCounter = { n: 0 };
  const rustCounter = { n: 0 };
  const js = await run(true, jsCounter);
  const rust = await run(false, rustCounter);
  compare('deadline reviews', js, rust);
  assert.equal(jsCounter.n + rustCounter.n, 0, 'после дедлайна LLM не вызывается');
}

// ---------------------------------------------------------------------------
// 5. Scoped chunks: скоуп priors + demoteOutOfChunk
// ---------------------------------------------------------------------------

const SCOPED_CHUNKS = [
  { annotated: 'd1', fileContents: '', paths: ['a.ts'], partialPaths: [] },
  { annotated: 'd2', fileContents: '', paths: ['b.ts'], partialPaths: [] },
];

await checkReview('scoped priors + demote', ENGINE_CFG, {
  ollama: {
    chatJson: async () => ({
      summary: 's',
      verdict: 'changes_requested',
      findings: [
        { file: 'b.ts', line: 3, severity: 'critical', title: 'слепой блокер', confidence: 0.9 },
        { file: 'a.ts', line: 1, severity: 'major', title: 'виден', confidence: 0.9 },
      ],
    }),
  },
  prompt: PROMPT,
  chunks: [SCOPED_CHUNKS[0]],
  ctx: {
    pr: {},
    changedPaths: ['a.ts', 'b.ts'],
    priorFindings: [
      { file: 'a.ts', line: 1, severity: 'major', title: 'по чанку' },
      { file: 'b.ts', line: 2, severity: 'major', title: 'не по чанку' },
      { file: '', line: 0, severity: 'minor', title: 'без файла' },
    ],
  },
});

// частично видимый файл — свой; unknownPaths отключает скоуп и демоут
await checkReview('partial paths + unknownPaths', ENGINE_CFG, {
  ollama: {
    chatJson: async () => ({
      summary: 's', verdict: 'approve',
      findings: [{ file: 'b.ts', line: 3, severity: 'critical', title: 'x', confidence: 0.9 }],
    }),
  },
  prompt: PROMPT,
  chunks: [{ annotated: 'd', fileContents: '', paths: ['a.ts'], partialPaths: ['a.ts'] }],
  ctx: { pr: {}, changedPaths: ['a.ts', 'b.ts'], priorFindings: [{ file: 'b.ts', line: 2, severity: 'major', title: 'out' }] },
});

await checkReview('unknownPaths disables scope/demote', ENGINE_CFG, {
  ollama: {
    chatJson: async () => ({
      summary: 's', verdict: 'approve',
      findings: [{ file: 'zzz.ts', line: 3, severity: 'critical', title: 'x', confidence: 0.9 }],
    }),
  },
  prompt: PROMPT,
  chunks: [{ annotated: 'd', fileContents: '', paths: ['a.ts'], partialPaths: [], unknownPaths: true }],
  ctx: { pr: {}, changedPaths: ['a.ts'], priorFindings: [{ file: 'zzz.ts', line: 2, severity: 'major', title: 'out' }] },
});

// ---------------------------------------------------------------------------
// 6-7. MoA + specialists: jury, label-атрибуция, references, fallback
// ---------------------------------------------------------------------------

const MOA_CFG = {
  reviewMode: 'moa',
  models: { list: [], fallback: ['mfb'] },
  moa: { aggregator: 'agg-model', label: 'moa', references: ['ref-a'], refMaxTokens: 100, refTemp: 0.6, aggTemp: 0.4 },
  limits: { concurrency: 4 },
  specialists: {
    enabled: true,
    assignments: [
      { role: 'security', model: 'sec-m' },
      { role: 'correctness', model: 'corr-m' },
      { role: 'architecture', model: 'arch-m' },
    ],
  },
  ollama: { temperature: 0.4 },
  gate: {},
};
const MOA_PROMPT = {
  buildMessages: () => [{ role: 'user', content: 'base' }],
  buildSpecialistMessages: (role, base) => [...base, { role: 'system', content: `specialist:${role}` }],
  buildSpecialistAggregatorMessages: (base, refs) => [
    ...base,
    { role: 'user', content: `jury refs=${refs.map((r) => r.role).join(',')}` },
  ],
  buildReferenceMessages: (base) => base,
  buildAggregatorMessages: (base) => base,
  buildVerifierMessages: ({ candidates }) => [{ role: 'user', content: `verify ${candidates.length}` }],
};
const MOA_CHUNK = [{ annotated: 'd1', fileContents: 'code', paths: ['a.ts'], partialPaths: [] }];

await checkReview('MoA specialists jury', MOA_CFG, {
  ollama: {
    chatText: async (_msgs, model) => `prose from ${model}`,
    chatJson: async (_msgs, model) => {
      if (model === 'mfb') return { summary: 'fb', verdict: 'comment', findings: [] };
      assert.equal(model, 'agg-model');
      return {
        summary: 'jury',
        verdict: 'changes_requested',
        findings: [{
          file: 'a.ts', line: 3, severity: 'critical', title: 'IDOR', body: 'open', confidence: 0.9,
          advisorClaims: [
            { role: 'security', status: 'verified', reason: 'real' },
            { role: 'correctness', status: 'rejected', reason: 'n/a' },
            { role: 'architecture', status: 'downgraded', severity: 'minor', reason: 'layer' },
          ],
        }],
      };
    },
  },
  prompt: MOA_PROMPT,
  chunks: MOA_CHUNK,
  ctx: { pr: {} },
});

await checkReview('MoA all advisors down → error', MOA_CFG, {
  ollama: {
    chatText: async () => { throw new Error('down'); },
    chatJson: async () => ({ findings: [] }),
  },
  prompt: MOA_PROMPT,
  chunks: MOA_CHUNK,
  ctx: { pr: {} },
});

await checkReview('MoA aggregator fails → fallback (juryBypassed)', MOA_CFG, {
  ollama: {
    chatText: async (_msgs, model) => {
      if (model === 'arch-m') throw new Error('arch down');
      return `prose from ${model}`;
    },
    chatJson: async (_msgs, model) => {
      if (model === 'agg-model') throw new Error('agg down');
      assert.equal(model, 'mfb');
      return { summary: 'fb', verdict: 'comment', findings: [{ file: 'a.ts', line: 1, severity: 'minor', title: 'fb finding', confidence: 0.5 }] };
    },
  },
  prompt: MOA_PROMPT,
  chunks: MOA_CHUNK,
  ctx: { pr: {} },
});

// ---------------------------------------------------------------------------
// 8. Aggregate-края: verdict-тай (первый побеждает), missing verdict, all-skipped
// ---------------------------------------------------------------------------

await checkReview('verdict ties keep first', {
  ...ENGINE_CFG,
  models: { list: ['m1'], fallback: [] },
}, {
  ollama: {
    chatJson: async (msgs) => ({
      summary: msgs[0].content,
      // оба чанка дают ранк 1 (comment) — стабильный sort берёт первый
      verdict: 'comment',
      findings: [],
    }),
  },
  prompt: { buildMessages: ({ annotated }) => [{ role: 'user', content: annotated }] },
  chunks: [{ annotated: 'v1', fileContents: '' }, { annotated: 'v2', fileContents: '' }],
  ctx: { pr: {} },
});

await checkReview('missing verdict aggregated as-is', {
  ...ENGINE_CFG,
  models: { list: ['m1'], fallback: [] },
}, {
  ollama: {
    chatJson: async () => ({ summary: 's', findings: [] }), // verdict отсутствует
  },
  prompt: PROMPT,
  chunks: [CHUNKS[0]],
  ctx: { pr: {} },
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
