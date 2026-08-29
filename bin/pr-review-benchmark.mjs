#!/usr/bin/env node
// Offline benchmark report: score gold results from JSON/JSONL без LLM.
// Вход: JSON-массив, JSONL прогонов, или `--gold-dir` для meta-only dry scoring stub.
// Пример: node bin/pr-review-benchmark.mjs results.jsonl
//         node bin/pr-review-benchmark.mjs --findings findings.json --meta gold/tp-real-bugs-unescape/meta.json
import { readFile, readdir } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { scoreGoldCase, aggregateBenchmark, scoreFromRecord } from '../lib/benchmark.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const GOLD_DIR = process.env.REVIEW_GOLD_DIR
  ? resolve(process.cwd(), process.env.REVIEW_GOLD_DIR)
  : join(__dir, '../gold');

function parseInput(text) {
  const trimmed = String(text || '').trim();
  if (!trimmed) return [];
  if (trimmed.startsWith('[')) {
    const arr = JSON.parse(trimmed);
    return Array.isArray(arr) ? arr.map(scoreFromRecord) : [];
  }
  return trimmed.split('\n').map((l) => l.trim()).filter(Boolean).map((l) => {
    try { return scoreFromRecord(JSON.parse(l)); } catch { return null; }
  }).filter(Boolean);
}

async function main() {
  const args = process.argv.slice(2);
  const metaIdx = args.indexOf('--meta');
  const findingsIdx = args.indexOf('--findings');
  const goldDirFlag = args.includes('--gold-dir');
  const file = args.find((a) => !a.startsWith('--') && a !== args[metaIdx + 1] && a !== args[findingsIdx + 1]);

  let results = [];

  if (metaIdx >= 0 && findingsIdx >= 0) {
    const meta = JSON.parse(await readFile(args[metaIdx + 1], 'utf8'));
    const payload = JSON.parse(await readFile(args[findingsIdx + 1], 'utf8'));
    const findings = payload.findings || payload;
    const wouldBlock = payload.wouldBlock === true;
    results = [{
      id: meta.id || '?',
      type: meta.type,
      expect: meta.expect,
      wouldBlock,
      findingsCount: findings.length,
      score: scoreGoldCase(meta, { wouldBlock, findings }),
    }];
  } else if (file) {
    results = parseInput(await readFile(file, 'utf8'));
  } else if (goldDirFlag) {
    const ids = (await readdir(GOLD_DIR, { withFileTypes: true }))
      .filter((e) => e.isDirectory()).map((e) => e.name).sort();
    for (const id of ids) {
      const meta = JSON.parse(await readFile(join(GOLD_DIR, id, 'meta.json'), 'utf8'));
      results.push({
        id,
        type: meta.type,
        expect: meta.expect,
        wouldBlock: null,
        findingsCount: 0,
        score: scoreGoldCase(meta, { wouldBlock: meta.expect?.wouldBlock ?? false, findings: [] }),
        note: 'meta-only stub (передайте findings через --findings или JSONL прогона)',
      });
    }
  } else {
    console.error(`Usage:
  pr-review-benchmark <results.jsonl|json>
  pr-review-benchmark --meta gold/.../meta.json --findings run.json
  pr-review-benchmark --gold-dir`);
    process.exit(1);
  }

  const agg = aggregateBenchmark(results);
  console.log('=== pr-review benchmark ===');
  console.log(`Cases: ${agg.passed}/${agg.total} passed (${agg.passRate != null ? `${Math.round(agg.passRate * 100)}%` : '—'})`);
  if (agg.wouldBlock.expected) {
    console.log(`wouldBlock match: ${agg.wouldBlock.matched}/${agg.wouldBlock.expected} (${Math.round((agg.wouldBlock.rate || 0) * 100)}%)`);
  }
  if (agg.recall.targets) {
    console.log(`title recall (substring): ${agg.recall.hits}/${agg.recall.targets} (${Math.round((agg.recall.rate || 0) * 100)}%)`);
  }
  if (agg.precision.fpViolations) {
    console.log(`mustNotFind violations: ${agg.precision.fpViolations}`);
  }
  for (const r of results) {
    const mark = r.score?.ok ? '✅' : '❌';
    console.log(`${mark} ${r.id}${r.note ? ` (${r.note})` : ''}`);
    for (const hint of r.score?.precisionHints || []) console.log(`   ✗ ${hint}`);
  }
  process.exit(results.every((r) => r.score?.ok) ? 0 : 1);
}

main().catch((e) => {
  console.error('[benchmark]', e.message);
  process.exit(1);
});
