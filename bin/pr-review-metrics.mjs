#!/usr/bin/env node
// Агрегация метрик observability JSONL → stdout.
// Запуск: npx pr-review-metrics [path.jsonl]
// (default: observability-путь активного профиля — локально или из CI artifact)
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { parseJsonl, aggregateMetrics, formatMetricsReport } from '../lib/metrics.mjs';
import { loadReviewConfig } from '../lib/config.mjs';

const cfg = loadReviewConfig();
const path = process.argv[2] || join(cfg.observability.dir, cfg.observability.file);
try {
  const text = await readFile(path, 'utf8');
  console.log(formatMetricsReport(aggregateMetrics(parseJsonl(text)), path));
} catch (e) {
  console.error(`[pr-review.metrics] не удалось прочитать ${path}: ${e.message}`);
  process.exit(1);
}
