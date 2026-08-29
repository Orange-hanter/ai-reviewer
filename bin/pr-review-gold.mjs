#!/usr/bin/env node
// Shadow eval ПОЛНОГО pipeline (с LLM) на gold-диффах с разметкой TP/FP (R-eval, backlog P0-3).
// В отличие от eval.mjs (fixtures, post-processing без LLM) — здесь вызываются модели.
// Запуск: OLLAMA_API_KEY=… npx pr-review-gold [--case <id>]
// Blast-radius в gold-прогонах заглушен (детерминизм; на находки влияет слабо).
import { readdir, readFile } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadReviewConfig } from '../lib/config.mjs';
import { createReviewPipeline } from '../pipeline.mjs';
import { MARKERS } from '../lib/constants.mjs';
import { scoreGoldCase } from '../lib/benchmark.mjs';

const GOLD_DIR = process.env.REVIEW_GOLD_DIR
  ? resolve(process.cwd(), process.env.REVIEW_GOLD_DIR)
  : join(dirname(fileURLToPath(import.meta.url)), '../gold');
const onlyIdx = process.argv.indexOf('--case');
const only = onlyIdx >= 0 ? process.argv[onlyIdx + 1] : null;

function footerFindings(text) {
  const m = MARKERS.dataRe.exec(text);
  if (!m) return [];
  try { return JSON.parse(m[1].trim()).findings || []; } catch { return []; }
}

async function runCase(id) {
  const dir = join(GOLD_DIR, id);
  const meta = JSON.parse(await readFile(join(dir, 'meta.json'), 'utf8'));
  const env = {
    ...process.env,
    REVIEW_OBSERVABILITY: '0',
    REVIEW_EXPORT: '0',
    REVIEW_AUTO_TRIAGE: '0',
  };
  const cfg = loadReviewConfig(env, { dryRun: true, diffFile: join(dir, 'diff.patch'), repo: '', prNumber: 0 });
  const captured = [];
  const origLog = console.log;
  console.log = (...a) => { captured.push(a.join(' ')); };
  const t0 = Date.now();
  let code;
  try {
    const pipeline = createReviewPipeline(cfg, {
      graphify: { buildBlastRadius: async () => [] },
      die: (m) => { throw new Error(`die: ${m}`); },
    });
    code = await pipeline.run();
  } finally {
    console.log = origLog;
  }
  return { meta, code, findings: footerFindings(captured.join('\n')), wallMs: Date.now() - t0 };
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
for (const id of ids) {
  process.stdout.write(`[gold] ${id} … `);
  try {
    const { meta, code, findings, wallMs } = await runCase(id);
    const wouldBlock = code === 1;
    const score = scoreGoldCase(meta, { wouldBlock, findings });
    const errors = score.precisionHints;
    const stat = errors.length ? '❌' : '✅';
    console.log(`${stat} wouldBlock=${code === 1} findings=${findings.length} wall=${(wallMs / 1000).toFixed(0)}с`);
    for (const e of errors) {
      console.log(`   ✗ ${e}`);
      failed++;
    }
  } catch (e) {
    console.log(`💥 ${e.message}`);
    failed++;
  }
}
console.log(failed ? `⛔ gold: ${failed} несовпадение(й)` : '✅ gold: все кейсы совпали с разметкой');
process.exit(failed ? 1 : 0);
