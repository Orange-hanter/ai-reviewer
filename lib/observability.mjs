/**
 * Фаза 3, шаг 6: lib/observability.mjs — JSONL observability для pr-review runs.
 *
 * Чистые функции (countBySeverity/slimFindings) — диспетчер над Rust-бинарём
 * rust/target/debug/metrics (тот же бинарь, что lib/metrics.mjs:
 * AI_REVIEWER_METRICS_BIN / AI_REVIEWER_METRICS_FORCE_JS=1). createObservability
 * намеренно остался чистым JS: runId (Date.now + randomBytes), ts (new Date)
 * и appendFile — часы и fs живут на JS-стороне.
 *
 * Если бинарь не существует (чистый клон до первого `cargo build`), обёртка
 * деградирует на эталонный JS fallback; упавший бинарь — ошибка наружу,
 * молчаливая подмена скрывала бы регрессии (как в lib/report.mjs).
 */

import { appendFile, mkdir } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { randomBytes } from 'node:crypto';
import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { getProfile } from './profile.mjs';
import { defaultRustBin } from './rust-bin.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_METRICS_BIN = defaultRustBin('metrics');

function resolveMetricsBin() {
  if (process.env.AI_REVIEWER_METRICS_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_METRICS_BIN || '').trim() || DEFAULT_METRICS_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveMetricsBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

const MAX_SLIM = 40;
const TITLE_MAX = 120;
export const PR_TITLE_LOG_MAX = 80;

export function countBySeverity(findings) {
  const rust = runRust('countBySeverity', { findings: findings ?? null });
  if (rust !== null) return rust;
  return countBySeverityJs(findings);
}

/** Компактные findings для лога (без body — без секретов/шума). */
export function slimFindings(findings, limit = MAX_SLIM) {
  const rust = runRust('slimFindings', { findings: findings ?? null, limit });
  if (rust !== null) return rust;
  return slimFindingsJs(findings, limit);
}

// ---------------------------------------------------------------------------
// Эталонный JS fallback (копия прежней реализации, активна без бинаря / FORCE_JS)
// ---------------------------------------------------------------------------

function countBySeverityJs(findings) {
  const c = { critical: 0, major: 0, minor: 0, nit: 0 };
  for (const f of findings || []) {
    if (c[f.severity] != null) c[f.severity]++;
  }
  return c;
}

function slimFindingsJs(findings, limit = MAX_SLIM) {
  return (findings || []).slice(0, limit).map((f) => ({
    file: f.file,
    line: f.line,
    severity: f.severity,
    model: f.model,
    title: String(f.title || '').slice(0, TITLE_MAX),
    prescan: f.prescan || undefined,
  }));
}

// ---------------------------------------------------------------------------
// createObservability — чистый JS (fs/часы/рандом живут на JS-стороне)
// ---------------------------------------------------------------------------

function newRunId() {
  return `${Date.now().toString(36)}-${randomBytes(4).toString('hex')}`;
}

/** Создаёт logger run; fail-open при ошибках записи. Запись сериализована — run_end всегда последний. */
export function createObservability(cfg, { log = () => {} } = {}) {
  const obsCfg = cfg.observability || {};
  const enabled = obsCfg.enabled !== false;
  const runId = newRunId();
  const startedAt = Date.now();
  let writePath = null;
  let writeChain = Promise.resolve();
  let dirReady = false;

  if (enabled) {
    const dir = resolve(process.cwd(), obsCfg.dir || getProfile().paths.observabilityDir);
    writePath = resolve(dir, obsCfg.file || 'pr-review-runs.jsonl');
  }

  function enqueueWrite(record) {
    writeChain = writeChain.then(async () => {
      if (!enabled || !writePath) return;
      try {
        if (!dirReady) {
          await mkdir(dirname(writePath), { recursive: true });
          dirReady = true;
        }
        await appendFile(writePath, `${JSON.stringify(record)}\n`, 'utf8');
      } catch (e) {
        log(`observability write failed (fail-open): ${e.message}`);
      }
    });
    return writeChain;
  }

  function logEvent(type, data = {}) {
    if (!enabled) return;
    void enqueueWrite({
      ts: new Date().toISOString(),
      runId,
      type,
      ...data,
    });
  }

  async function flush(final = {}) {
    if (!enabled) return;
    await enqueueWrite({
      ts: new Date().toISOString(),
      runId,
      type: 'run_end',
      duration_ms: Date.now() - startedAt,
      ...final,
    });
  }

  /** Дождаться всех append (для тестов). */
  async function drain() {
    await writeChain;
  }

  return { runId, startedAt, enabled, logEvent, flush, drain };
}
