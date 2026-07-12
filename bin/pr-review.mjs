#!/usr/bin/env node
// CLI: AI code review (MoA / consensus). Локально: pr-review --dry-run --diff path/to.patch

export {
  parseDiff, extractChangedSymbols, splitSegmentByHunks, buildChunks, readChangedFiles,
  extractJsonObject,
  filterFalsePositiveFindings, clusterFindings, normalizeFinding, normalizeConfidence, resolveBlockers,
  runEvalHarness, loadFixtures, runFixtureCase, formatEvalReport,
  buildMoAReferenceSection,
  loadReviewConfig, parseCli,
  createReviewEngine,
  buildSummary,
  buildReviewJson,
  buildSarif,
  createReviewPipeline, runReview,
} from '../index.mjs';

import { pathToFileURL } from 'node:url';
import { runReview } from '../pipeline.mjs';

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  runReview().catch((e) => {
    console.error('[pr-review]', e.stack || e.message);
    process.exit(0);
  });
}
