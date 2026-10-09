#!/usr/bin/env node
// R8: harvest feedback с хостинга (GitHub/GitLab, RFC 16) (👍/👎, label ai-review-fp) → JSONL + fixture candidates.
// Запуск: npx pr-review-feedback [--dry-run] [--pr N] [--repo owner/repo]

import { pathToFileURL } from 'node:url';
import { loadReviewConfig, parseCli } from '../lib/config.mjs';
import { createForgeClient, forgeOf, forgeCredsHint } from '../lib/forge.mjs';
import { harvestFeedback, formatHarvestReport } from '../lib/feedback.mjs';

async function main() {
  const argv = process.argv.slice(2);
  const dryRun = argv.includes('--dry-run');
  const env = { ...process.env };
  if (dryRun) env.REVIEW_FEEDBACK_WRITE = '0';

  const cli = parseCli(argv);
  const cfg = loadReviewConfig(env, cli);

  if (!cfg.feedback.enabled) {
    console.log('REVIEW_FEEDBACK=0 — harvest отключён');
    process.exit(0);
  }
  const forge = forgeOf(cfg);
  if (!forge.token || !forge.repo || !forge.prNumber) {
    console.error(`[pr-review.feedback] нужны ${forgeCredsHint(cfg)} (или --repo/--pr)`);
    process.exit(1);
  }

  const log = (m) => console.log(m);
  const github = createForgeClient(cfg, log);
  const result = await harvestFeedback(cfg, github, { log });
  console.log(formatHarvestReport(result));
  process.exit(result.ok === false ? 1 : 0);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((e) => {
    console.error('[pr-review.feedback]', e.stack || e.message);
    process.exit(1);
  });
}
