#!/usr/bin/env node
// Shadow eval ПОЛНОГО pipeline (с LLM) на gold-диффах с разметкой TP/FP (R-eval, backlog P0-3).
// В отличие от eval.mjs (fixtures, post-processing без LLM) — здесь вызываются модели.
// Запуск: OLLAMA_API_KEY=… npx pr-review-gold [--case <id>] [--jsonl out.jsonl]
// Каталог набора — REVIEW_GOLD_DIR; диффы публичных кейсов тянутся по пину в REVIEW_GOLD_CACHE.
// Blast-radius в gold-прогонах заглушен (детерминизм; на находки влияет слабо).
import { readdir, readFile, writeFile, mkdir } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadReviewConfig } from '../lib/config.mjs';
import { createReviewPipeline } from '../pipeline.mjs';
import { MARKERS } from '../lib/constants.mjs';
import { scoreGoldCase } from '../lib/benchmark.mjs';
import { ensureCaseDiff } from '../lib/gold-fetch.mjs';
import { buildGraphIndex } from '../lib/graphify.mjs';

const GOLD_DIR = process.env.REVIEW_GOLD_DIR
  ? resolve(process.cwd(), process.env.REVIEW_GOLD_DIR)
  : join(dirname(fileURLToPath(import.meta.url)), '../gold');
// Кэш диффов публичных кейсов: чужой код не вендорится, тянется по пину (RFC 03).
const CACHE_DIR = resolve(process.cwd(), process.env.REVIEW_GOLD_CACHE || '.ai-review/gold-public');
const onlyIdx = process.argv.indexOf('--case');
const only = onlyIdx >= 0 ? process.argv[onlyIdx + 1] : null;
const jsonlIdx = process.argv.indexOf('--jsonl');
const jsonlPath = jsonlIdx >= 0 ? resolve(process.cwd(), process.argv[jsonlIdx + 1]) : null;

let graphIndexCache;
/** Индекс graph.json (REVIEW_GOLD_GRAPH или GRAPH) — один раз на прогон; нет файла → null. */
async function goldGraphIndex(cfg) {
  if (graphIndexCache !== undefined) return graphIndexCache;
  const path = resolve(process.cwd(), process.env.REVIEW_GOLD_GRAPH || cfg.graphify.graph);
  try {
    graphIndexCache = buildGraphIndex(JSON.parse(await readFile(path, 'utf8')));
  } catch (e) {
    console.error(`[gold] карточки символов без графа (${path}: ${e.code || e.message})`);
    graphIndexCache = null;
  }
  return graphIndexCache;
}

function footerFindings(text) {
  const m = MARKERS.dataRe.exec(text);
  if (!m) return [];
  try { return JSON.parse(m[1].trim()).findings || []; } catch { return []; }
}

async function runCase(id) {
  const dir = join(GOLD_DIR, id);
  const meta = JSON.parse(await readFile(join(dir, 'meta.json'), 'utf8'));
  const diff = await ensureCaseDiff(dir, meta, { cacheDir: CACHE_DIR });
  const env = {
    ...process.env,
    // REVIEW_GOLD_OBSERVABILITY_DIR — JSONL прогона (токены, plan.symbolCards) для сравнения
    // конфигураций; по умолчанию выключено, как и было.
    ...(process.env.REVIEW_GOLD_OBSERVABILITY_DIR
      ? { REVIEW_OBSERVABILITY: '1', REVIEW_OBSERVABILITY_DIR: resolve(process.cwd(), process.env.REVIEW_GOLD_OBSERVABILITY_DIR) }
      : { REVIEW_OBSERVABILITY: '0' }),
    REVIEW_EXPORT: '0',
    REVIEW_AUTO_TRIAGE: '0',
  };
  const cfg = loadReviewConfig(env, { dryRun: true, diffFile: diff.path, repo: '', prNumber: 0 });
  const captured = [];
  const origLog = console.log;
  console.log = (...a) => { captured.push(a.join(' ')); };
  const t0 = Date.now();
  let code;
  try {
    const pipeline = createReviewPipeline(cfg, {
      // Blast-radius заглушен, но при карточках символов (RFC 14) индекс графа нужен — без
      // него сравнение REVIEW_SYMBOL_CARDS=0/1 дало бы два одинаковых прогона.
      graphify: { buildBlastRadius: async () => ({ blocks: [], metrics: null, graphIndex: cfg.limits.symbolCards ? await goldGraphIndex(cfg) : null }) },
      die: (m) => { throw new Error(`die: ${m}`); },
    });
    code = await pipeline.run();
  } finally {
    console.log = origLog;
  }
  return {
    meta,
    code,
    findings: footerFindings(captured.join('\n')),
    wallMs: Date.now() - t0,
    diffSource: diff.source,
  };
}

if (!process.env.LLM_API_KEY && !process.env.OLLAMA_API_KEY) {
  console.error('[gold] нет LLM_API_KEY / OLLAMA_API_KEY');
  process.exit(1);
}

const ids = (await readdir(GOLD_DIR, { withFileTypes: true }))
  .filter((e) => e.isDirectory())
  .map((e) => e.name)
  .filter((id) => !only || id === only)
  .sort();
if (!ids.length) {
  console.error(`[gold] кейсы не найдены${only ? `: ${only}` : ''}`);
  process.exit(1);
}

let failed = 0;
// Записи прогона в формате, который читает `scoreFromRecord` в pr-review-benchmark:
// без этого раннер и скорер остаются несвязанными половинами замера.
const records = [];
for (const id of ids) {
  process.stdout.write(`[gold] ${id} … `);
  try {
    const { meta, code, findings, wallMs, diffSource } = await runCase(id);
    const wouldBlock = code === 1;
    const score = scoreGoldCase(meta, { wouldBlock, findings });
    const errors = score.precisionHints;
    const stat = errors.length ? '❌' : '✅';
    console.log(`${stat} wouldBlock=${code === 1} findings=${findings.length} wall=${(wallMs / 1000).toFixed(0)}с`);
    records.push({ id, meta, findings, wouldBlock, code, wallMs, diffSource });
    for (const e of errors) {
      console.log(`   ✗ ${e}`);
      failed++;
    }
  } catch (e) {
    console.log(`💥 ${e.message}`);
    records.push({ id, error: e.message });
    failed++;
  }
}
if (jsonlPath) {
  // Каталог создаём здесь: прогон стоит десятки минут, и терять его на отсутствующем
  // `.ai-review/` в самом конце — недопустимо.
  await mkdir(dirname(jsonlPath), { recursive: true });
  await writeFile(jsonlPath, `${records.map((r) => JSON.stringify(r)).join('\n')}\n`, 'utf8');
  console.log(`[gold] записи прогона → ${jsonlPath}`);
}
console.log(failed ? `⛔ gold: ${failed} несовпадение(й)` : '✅ gold: все кейсы совпали с разметкой');
process.exit(failed ? 1 : 0);
