/** Versioned, backward-compatible schema helpers for fp-feedback.jsonl. */

export const FEEDBACK_SCHEMA_VERSION = 1;

const SOURCES = new Set(['label', 'reaction']);
const SIGNALS = new Set(['fp', 'tp']);
const SEVERITIES = new Set(['critical', 'major', 'minor', 'nit']);
const LEGACY_TS = '1970-01-01T00:00:00.000Z';
const LEGACY_REPO = 'unknown/unknown';

function optionalString(value) {
  return typeof value === 'string' && value ? value : undefined;
}

function normalizeFinding(raw) {
  if (!raw || typeof raw !== 'object') return null;
  return {
    file: typeof raw.file === 'string' ? raw.file : '',
    line: Math.max(0, Number(raw.line) || 0),
    severity: SEVERITIES.has(raw.severity) ? raw.severity : 'minor',
    title: optionalString(raw.title) || 'замечание',
    ...(optionalString(raw.model) ? { model: raw.model } : {}),
    ...(optionalString(raw.body) ? { body: raw.body } : {}),
  };
}

/**
 * Accepts legacy rows without schemaVersion and fills fields that old fixtures
 * omitted. Unsupported future schema versions are rejected instead of being
 * silently rewritten.
 */
export function normalizeFeedbackEntry(raw) {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  const version = raw.schemaVersion == null ? FEEDBACK_SCHEMA_VERSION : Number(raw.schemaVersion);
  if (version !== FEEDBACK_SCHEMA_VERSION) return null;
  const pr = Number(raw.pr);
  const finding = normalizeFinding(raw.finding);
  if (!Number.isInteger(pr) || pr <= 0 || !SOURCES.has(raw.source) || !SIGNALS.has(raw.signal) || !finding) {
    return null;
  }

  const out = {
    schemaVersion: FEEDBACK_SCHEMA_VERSION,
    ts: optionalString(raw.ts) || LEGACY_TS,
    repo: optionalString(raw.repo) || LEGACY_REPO,
    pr,
    source: raw.source,
    signal: raw.signal,
    finding,
  };
  for (const key of ['label', 'reaction', 'user']) {
    const value = optionalString(raw[key]);
    if (value) out[key] = value;
  }
  if (raw.commentId != null && Number.isFinite(Number(raw.commentId))) {
    out.commentId = Number(raw.commentId);
  }
  return out;
}

/** Convert arbitrary JSONL text to canonical schema-v1 JSONL. */
export function migrateFeedbackText(text) {
  const rows = [];
  const stats = { total: 0, valid: 0, migrated: 0, invalid: 0 };
  for (const line of String(text || '').split('\n')) {
    if (!line.trim()) continue;
    stats.total++;
    let raw;
    try {
      raw = JSON.parse(line);
    } catch {
      stats.invalid++;
      continue;
    }
    const normalized = normalizeFeedbackEntry(raw);
    if (!normalized) {
      stats.invalid++;
      continue;
    }
    stats.valid++;
    if (JSON.stringify(raw) !== JSON.stringify(normalized)) stats.migrated++;
    rows.push(normalized);
  }
  return {
    entries: rows,
    stats,
    text: rows.length ? `${rows.map((row) => JSON.stringify(row)).join('\n')}\n` : '',
  };
}
