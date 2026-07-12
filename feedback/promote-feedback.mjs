#!/usr/bin/env node
/** CLI: promote feedback candidates → eval fixtures (+ optional aggregate from JSONL). */

import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';
import { loadReviewConfig, parseCli } from '../lib/config.mjs';
import { pkgPath } from '../lib/package-root.mjs';
import { promoteCandidatesFromDir } from './promote-candidate.mjs';
import { promoteFromFeedbackLog, formatPromoteReport } from './promote-patterns.mjs';

async function main() {
  const argv = process.argv.slice(2);
  const aggregate = argv.includes('--aggregate');
  const cli = parseCli(argv);
  const cfg = loadReviewConfig(process.env, cli);
  const log = (m) => console.log(m);

  const repo = cfg.github.repo || process.env.GITHUB_REPOSITORY;
  const token = cfg.github.token || process.env.GITHUB_TOKEN;
  const pr = cfg.github.prNumber;
  const fixturesDir = resolve(process.cwd(), process.env.REVIEW_FIXTURES_DIR || pkgPath('fixtures'));
  const candidatesDir = resolve(process.cwd(), cfg.feedback.candidatesDir);
  const logFile = resolve(process.cwd(), cfg.feedback.logFile);
  const patternsFile = resolve(process.cwd(), cfg.feedback.patternsFile);

  if (aggregate) {
    const result = await promoteFromFeedbackLog({
      logFile,
      patternsFile,
      fixturesDir,
      minCount: cfg.feedback.promoteMinFpCount,
      repo,
      token,
      log,
    });
    console.log(formatPromoteReport(result));
    const failed = result.fixtures.some((f) => !f.ok && !f.skipped);
    process.exit(failed ? 1 : 0);
  }

  const result = await promoteCandidatesFromDir(candidatesDir, {
    fixturesDir,
    pr,
    repo,
    token,
    log,
  });
  console.log('=== promote candidates ===');
  console.log(`promoted: ${result.promoted.length}, skipped: ${result.skipped.length}, failed: ${result.failed.length}`);
  for (const r of [...result.promoted, ...result.skipped, ...result.failed]) {
    console.log(`- ${r.id}: ${r.ok ? 'ok' : r.skipped ? 'skip' : r.error || r.errors?.join('; ')}`);
  }
  process.exit(result.failed.length ? 1 : 0);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((e) => {
    console.error('[promote-feedback]', e.stack || e.message);
    process.exit(1);
  });
}
