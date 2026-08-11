#!/usr/bin/env node
// CLI полного pipeline. Offline-прогон: pr-review --dry-run --diff path/to.patch
// PR-прогон: GITHUB_TOKEN=… PR_NUMBER=… GITHUB_REPOSITORY=owner/repo pr-review

import { pathToFileURL } from 'node:url';
import { runReview } from '../pipeline.mjs';

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  // exit 0 на любом сбое самого движка: ревью советует, инфра-сбой не блокирует merge.
  // Блокировка — только осознанный exit 1 внутри runReview на подтверждённых блокерах.
  runReview().catch((e) => {
    console.error('[pr-review]', e.stack || e.message);
    process.exit(0);
  });
}
