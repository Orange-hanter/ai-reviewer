#!/usr/bin/env node
// Eval harness (R1): golden fixtures без LLM → filter/cluster/gate + отчёт.

import { pathToFileURL } from 'node:url';
import { runEvalHarness, DEFAULT_FIXTURES_DIR } from '../lib/eval.mjs';

async function main() {
  const argv = process.argv.slice(2);
  const fixturesDir = argv.includes('--fixtures')
    ? argv[argv.indexOf('--fixtures') + 1]
    : DEFAULT_FIXTURES_DIR;
  const asJson = argv.includes('--json');

  const out = await runEvalHarness({ fixturesDir });
  if (asJson) {
    console.log(JSON.stringify({ ok: out.ok, metrics: out.metrics, results: out.results }, null, 2));
  } else {
    console.log(out.report);
  }
  process.exit(out.ok ? 0 : 1);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((e) => {
    console.error('[pr-review.eval]', e.stack || e.message);
    process.exit(1);
  });
}
