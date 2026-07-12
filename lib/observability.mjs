/** R7: JSONL observability для pr-review runs. */

import { appendFile, mkdir } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { randomBytes } from 'node:crypto';

const MAX_SLIM = 40;
const TITLE_MAX = 120;
export const PR_TITLE_LOG_MAX = 80;

export function countBySeverity(findings) {
  const c = { critical: 0, major: 0, minor: 0, nit: 0 };
  for (const f of findings || []) {
    if (c[f.severity] != null) c[f.severity]++;
  }
  return c;
}

/** Компактные findings для лога (без body — без секретов/шума). */
export function slimFindings(findings, limit = MAX_SLIM) {
  return (findings || []).slice(0, limit).map((f) => ({
    file: f.file,
    line: f.line,
    severity: f.severity,
    model: f.model,
    title: String(f.title || '').slice(0, TITLE_MAX),
    prescan: f.prescan || undefined,
  }));
}

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
    const dir = resolve(process.cwd(), obsCfg.dir || '.graphify/graphify-out/observability');
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
