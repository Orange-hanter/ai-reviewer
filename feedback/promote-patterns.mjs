/** Aggregate fp-feedback.jsonl → promote patterns + fixtures when ≥N same FP. */

import { appendFile, readFile } from 'node:fs/promises';
import { buildFixtureCandidate, readFeedbackLog as readVersionedFeedbackLog } from '../lib/feedback.mjs';
import { parseFpPatternsText } from '../lib/fp-patterns.mjs';
import { promoteCandidateToFixture } from './promote-candidate.mjs';

/** Group key without line — same title+file+severity across PRs. */
export function fpAggregateKey(entry) {
  const f = entry.finding || {};
  return `${f.file || ''}|${f.severity || ''}|${f.title || ''}`;
}

export async function readFeedbackLog(logPath, { log = () => {} } = {}) {
  return readVersionedFeedbackLog(logPath, { log });
}

export function groupFpEntries(entries, { minCount = 2 } = {}) {
  const groups = new Map();
  for (const e of entries) {
    if (e.signal !== 'fp') continue;
    const k = fpAggregateKey(e);
    if (!groups.has(k)) groups.set(k, []);
    groups.get(k).push(e);
  }
  return [...groups.entries()].filter(([, list]) => list.length >= minCount);
}

/** Heuristic: map recurring FP title to known pattern id. */
export function inferPatternId(entry) {
  const text = `${entry.finding?.title || ''} ${entry.finding?.body || ''}`.toLowerCase();
  if (/(?:e2e|playwright)/.test(text) && /(?:нет|отсутств|missing|no\s+e2e)/.test(text)) {
    return 'e2e-in-diff';
  }
  if (/(?:файл|file).{0,20}(?:не\s+в|not\s+in|absent|missing)/.test(text)) {
    return 'file-absent-in-diff';
  }
  return null;
}

const KNOWN_PATTERNS = {
  'e2e-in-diff': {
    id: 'e2e-in-diff',
    description: 'e2e spec в диффе, но модель пишет «нет e2e»',
    regex: '(?:e2e|playwright)[^.]{0,25}(?:^|\\s)(?:нет|отсутствует|без|не\\s+добавлен[а-я]*)(?=\\s|$)|(?:^|\\s)(?:нет|отсутствует|без|не\\s+добавлен[а-я]*)(?=\\s)[^.]{0,25}(?:e2e|playwright)|\\b(?:no|missing)\\s+(?:e2e|playwright)(?:\\s+tests?)?\\b|\\b(?:e2e|playwright)\\s+tests?\\s+(?:not\\s+added|missing|absent)\\b',
    requiresPath: '(?:^|/)e2e/.*\\.spec\\.ts$',
  },
  'file-absent-in-diff': {
    id: 'file-absent-in-diff',
    description: 'модель: файл не в PR, но basename есть в тексте и в changedPaths',
    regex: '(?:^|\\s)файл(?=\\s).{0,30}?(?:отсутствует\\s+в|не\\s+(?:входит\\s+в|найден\\s+в|в))\\s+(?:дифф\\w*|pr\\b)|\\bfile\\b.{0,30}?(?:(?:not\\s+in|absent\\s+from|not\\s+found\\s+in|missing\\s+from|missing\\s+in)\\s+(?:the\\s+)?(?:pr|diff))\\b',
    matchChangedBasename: true,
  },
};

export async function appendPatternIfMissing(patternsFile, patternId, log = () => {}) {
  const known = KNOWN_PATTERNS[patternId];
  if (!known) return { appended: false, reason: 'unknown_pattern' };

  let existing = [];
  try {
    existing = parseFpPatternsText(await readFile(patternsFile, 'utf8'));
  } catch { /* new file */ }
  if (existing.some((p) => p.id === patternId)) {
    return { appended: false, reason: 'exists' };
  }

  await appendFile(patternsFile, `${JSON.stringify(known)}\n`);
  log(`appended fp pattern ${patternId}`);
  return { appended: true, patternId };
}

/**
 * @returns {{ patterns: object[], fixtures: object[], groups: number }}
 */
export async function promoteFromFeedbackLog({
  logFile,
  patternsFile,
  fixturesDir,
  minCount = 2,
  repo,
  token,
  log = () => {},
} = {}) {
  const entries = await readFeedbackLog(logFile, { log });
  const groups = groupFpEntries(entries, { minCount });
  const patterns = [];
  const fixtures = [];

  for (const [, list] of groups) {
    const sample = list[list.length - 1];
    const patternId = inferPatternId(sample);
    if (patternId) {
      const pr = await appendPatternIfMissing(patternsFile, patternId, log);
      if (pr.appended) patterns.push(pr);
    }

    const candidate = buildFixtureCandidate(sample);
    if (!candidate) continue;
    const r = await promoteCandidateToFixture(candidate, {
      fixturesDir,
      pr: sample.pr,
      repo,
      token,
      log,
    });
    fixtures.push({ ...r, aggregateKey: fpAggregateKey(sample), count: list.length });
  }

  return { patterns, fixtures, groups: groups.length };
}

export function formatPromoteReport(result) {
  const lines = [
    '=== feedback promote (aggregate) ===',
    `groups ≥ threshold: ${result.groups}`,
    `patterns appended: ${result.patterns.length}`,
    `fixtures promoted: ${result.fixtures.filter((f) => f.ok).length}`,
    `fixtures failed: ${result.fixtures.filter((f) => !f.ok && !f.skipped).length}`,
    `fixtures skipped: ${result.fixtures.filter((f) => f.skipped).length}`,
  ];
  for (const f of result.fixtures) {
    lines.push(`- ${f.id}: ${f.ok ? 'ok' : f.skipped ? 'skip' : `fail ${f.error || f.errors?.join('; ')}`}`);
  }
  return lines.join('\n');
}
