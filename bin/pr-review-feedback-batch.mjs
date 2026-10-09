#!/usr/bin/env node
/** Scheduled/manual batch harvest for recent PR review feedback. */

import { pathToFileURL } from 'node:url';
import { loadReviewConfig, parseCli } from '../lib/config.mjs';
import { createForgeClient, forgeOf } from '../lib/forge.mjs';
import { harvestFeedback } from '../lib/feedback.mjs';

function positiveInt(value, fallback) {
  const n = Number(value);
  return Number.isInteger(n) && n > 0 ? n : fallback;
}

/** Select recent human PRs for scheduled harvest, or one explicit PR. */
export function selectFeedbackPulls(pulls, {
  explicitPr = 0,
  now = Date.now(),
  sinceDays = 14,
  limit = 30,
} = {}) {
  const one = positiveInt(explicitPr, 0);
  if (one) return [one];
  const cutoff = now - positiveInt(sinceDays, 14) * 86_400_000;
  return (Array.isArray(pulls) ? pulls : [])
    .filter((pr) =>
      positiveInt(pr?.number, 0) &&
      pr?.user?.type !== 'Bot' &&
      Date.parse(pr?.updated_at || '') >= cutoff)
    .slice(0, positiveInt(limit, 30))
    .map((pr) => Number(pr.number));
}

export async function runFeedbackBatch(
  env = process.env,
  argv = process.argv.slice(2),
  deps = {},
) {
  const dryRun = argv.includes('--dry-run');
  const runEnv = { ...env, ...(dryRun ? { REVIEW_FEEDBACK_WRITE: '0' } : {}) };
  const cli = parseCli(argv);
  const probeCfg = loadReviewConfig(runEnv, { ...cli, prNumber: 0, dryRun });
  const { kind, repo, token } = forgeOf(probeCfg);
  if (!repo || !token) {
    throw new Error(kind === 'gitlab'
      ? 'batch harvest требует CI_PROJECT_ID и AI_REVIEW_GITLAB_TOKEN'
      : 'batch harvest требует GITHUB_REPOSITORY и GITHUB_TOKEN');
  }

  const log = deps.log || ((message) => console.log(message));
  // deps.createGithubClient — прежнее имя точки инъекции (RFC 16, Р5).
  const makeClient = deps.createForgeClient || deps.createGithubClient || createForgeClient;
  const harvest = deps.harvestFeedback || harvestFeedback;
  const baseCli = { ...cli, repo, prNumber: 0, dryRun };
  const baseCfg = loadReviewConfig(runEnv, baseCli);
  const discovery = makeClient(baseCfg, log);
  const explicitPr = positiveInt(cli.prNumber || runEnv.PR_NUMBER || (kind === 'gitlab' ? runEnv.CI_MERGE_REQUEST_IID : 0), 0);
  const pulls = explicitPr
    ? []
    : kind === 'gitlab'
      ? await discovery.listRecentChanges({ limit: 100 })
      : await discovery.gh(
        `/repos/${repo}/pulls?state=all&sort=updated&direction=desc&per_page=100`,
      );
  const prNumbers = selectFeedbackPulls(pulls, {
    explicitPr,
    sinceDays: positiveInt(runEnv.REVIEW_FEEDBACK_SINCE_DAYS, 14),
    limit: positiveInt(runEnv.REVIEW_FEEDBACK_PR_LIMIT, 30),
  });

  const results = [];
  for (const prNumber of prNumbers) {
    const cfg = loadReviewConfig(runEnv, { ...baseCli, prNumber });
    const github = makeClient(cfg, log);
    try {
      results.push(await harvest(cfg, github, { log }));
    } catch (error) {
      results.push({
        pr: prNumber,
        repo,
        ok: false,
        newEntries: [],
        persistedEntries: 0,
        candidatesWritten: 0,
        warnings: [],
        writeErrors: [String(error?.message || error)],
      });
    }
  }

  return {
    repo,
    dryRun,
    selected: prNumbers.length,
    processed: results.length,
    newEntries: results.reduce((sum, result) => sum + (result.newEntries?.length || 0), 0),
    persistedEntries: results.reduce((sum, result) => sum + (result.persistedEntries || 0), 0),
    failed: results.filter((result) => result.ok === false).length,
    results,
  };
}

function formatBatchReport(result) {
  const lines = [
    '=== pr-review feedback batch ===',
    `repo: ${result.repo}`,
    `selected: ${result.selected}, processed: ${result.processed}, failed: ${result.failed}`,
    `new entries: ${result.newEntries}, persisted: ${result.persistedEntries}`,
  ];
  for (const item of result.results) {
    lines.push(`- PR #${item.pr}: ${item.ok === false ? 'failed' : 'ok'}; new=${item.newEntries?.length || 0}`);
  }
  return lines.join('\n');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  runFeedbackBatch().then((result) => {
    console.log(formatBatchReport(result));
    process.exit(result.failed ? 1 : 0);
  }).catch((error) => {
    console.error('[pr-review.feedback-batch]', error.stack || error.message);
    process.exit(1);
  });
}
