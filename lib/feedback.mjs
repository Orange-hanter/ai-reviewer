/** R8: GitHub feedback → JSONL log + fixture candidates (👍/👎, label ai-review-fp). */

import { createHash } from 'node:crypto';
import { appendFile, mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { MARKERS, hasMarker } from './constants.mjs';
import { extractJsonObject } from './json.mjs';
import { FEEDBACK_SCHEMA_VERSION, normalizeFeedbackEntry } from './feedback-schema.mjs';
import { forgeOf } from './forge.mjs';

export const DEFAULT_FP_LABEL = 'ai-review-fp';
/** Куда движок кладёт кандидатов в fixtures, если проект не переопределил путь. */
export const DEFAULT_CANDIDATES_DIR = '.ai-review/feedback/candidates';
export const DEFAULT_TRUSTED_AUTHORS = 'github-actions[bot]';
const UNKNOWN_LINE = 0;

// Старый формат включал модель в заголовок; новый намеренно оставляет только
// severity+title, чтобы не повторять MoA-цепочку в каждом inline-комментарии.
// Жадный захват до последнего `):` сохраняет совместимость с моделями в скобках.
const FINDING_RE = /\*\*(critical|major|minor|nit)(?:\s*\((.+)\))?:\s*([^*]+)\*\*/g;

export function findingKey(f) {
  return `${f.file || ''}|${f.line || UNKNOWN_LINE}|${f.severity || ''}|${f.title || ''}`;
}

export function feedbackDedupeKey(entry) {
  const userPart = entry.source === 'reaction' ? (entry.user || '') : '';
  return `${entry.repo}#${entry.pr}:${entry.source}:${entry.signal}:${findingKey(entry.finding || {})}:${entry.commentId || ''}:${userPart}`;
}

export function parseTrustedAuthors(raw = DEFAULT_TRUSTED_AUTHORS) {
  return String(raw).split(',').map((s) => s.trim()).filter(Boolean);
}

/**
 * Allowlist авторов с поправкой на хостинг (RFC 16). Клиент с `selfLogin` (GitLab: бот —
 * access token с логином `project_<id>_bot_<hash>`, признака бота у автора заметки нет)
 * при дефолтном allowlist доверяет владельцу токена: дефолт `github-actions[bot]` там не
 * встречается, и без подстановки auto-triage и harvest молча пропускали бы всё.
 * Явно заданный allowlist не трогаем.
 */
export async function resolveTrustedAuthors(raw, client) {
  const list = parseTrustedAuthors(raw ?? DEFAULT_TRUSTED_AUTHORS);
  if ((raw ?? DEFAULT_TRUSTED_AUTHORS) !== DEFAULT_TRUSTED_AUTHORS) return list;
  if (typeof client?.selfLogin !== 'function') return list;
  try {
    const me = await client.selfLogin();
    return me ? [me] : list;
  } catch {
    return list;
  }
}

/** Доверенный автор inline/sticky — только exact allowlist + GitHub user.type Bot. */
export function isTrustedReviewAuthor(login, trustedAuthors, user) {
  if (!trustedAuthors?.length) return true;
  if (!login) return false;
  const l = login.toLowerCase();
  if (!trustedAuthors.some((a) => l === String(a).toLowerCase())) return false;
  if (user?.type && user.type !== 'Bot') return false;
  return true;
}

/** Все findings из grouped inline comment body. */
export function parseReviewCommentFindings(comment) {
  if (!hasMarker(comment?.body, MARKERS.inline)) return [];
  const file = comment.path || '';
  const line = Number(comment.line || comment.original_line) || UNKNOWN_LINE;
  const out = [];
  for (const m of comment.body.matchAll(FINDING_RE)) {
    out.push({
      file,
      line,
      severity: m[1],
      title: m[3].trim(),
      model: m[2]?.trim() || '',
    });
  }
  return out;
}

/** Первый finding (backward compat). */
export function parseReviewCommentFinding(comment) {
  const all = parseReviewCommentFindings(comment);
  return all[0] || null;
}

/** Findings из машинного футера sticky (`<ns>-data:`). */
export function parseStickyFindings(body) {
  if (!body) return [];
  const m = MARKERS.dataRe.exec(body);
  if (!m) return [];
  try {
    const data = JSON.parse(m[1].trim());
    return Array.isArray(data.findings) ? data.findings : [];
  } catch {
    // Footer обрезан лимитом GitHub — пробуем ремонт JSON (как в readPriorFindings).
    try {
      const data = extractJsonObject(m[1], { onRepaired: () => {} });
      return Array.isArray(data.findings) ? data.findings : [];
    } catch {
      return [];
    }
  }
}

function reactionSignal(content) {
  if (content === '+1') return 'tp';
  if (content === '-1') return 'fp';
  return null;
}

/**
 * Собирает feedback-записи из labels, inline reactions, sticky findings.
 * @param {{ repo, pr, labels, inlineComments, stickyFindings, reactionsByCommentId, fpLabel?, trustedAuthors? }} input
 */
export function collectFeedbackEntries(input) {
  const {
    repo, pr, labels = [], inlineComments = [], stickyFindings = [],
    reactionsByCommentId = new Map(), fpLabel = DEFAULT_FP_LABEL,
    trustedAuthors = [],
  } = input;
  const entries = [];
  const ts = new Date().toISOString();
  const hasFpLabel = labels.some((l) => (typeof l === 'string' ? l : l.name) === fpLabel);

  if (hasFpLabel) {
    for (const f of stickyFindings) {
      entries.push({
        ts, repo, pr, source: 'label', signal: 'fp', label: fpLabel,
        finding: { ...f },
      });
    }
  }

  for (const c of inlineComments) {
    if (!hasMarker(c.body, MARKERS.inline)) continue;
    if (!isTrustedReviewAuthor(c.user?.login, trustedAuthors, c.user)) continue;
    const findings = parseReviewCommentFindings(c);
    if (!findings.length) continue;
    const reactions = reactionsByCommentId.get(String(c.id)) || [];
    for (const r of reactions) {
      const signal = reactionSignal(r.content);
      if (!signal) continue;
      for (const finding of findings) {
        entries.push({
          ts, repo, pr, source: 'reaction', signal, reaction: r.content,
          commentId: c.id, finding,
          user: r.user?.login,
        });
      }
    }
  }

  return entries.map((entry) => ({ schemaVersion: FEEDBACK_SCHEMA_VERSION, ...entry }));
}

export function dedupeFeedbackEntries(existing, incoming) {
  const seen = new Set((existing || []).map(feedbackDedupeKey));
  const out = [...(existing || [])];
  for (const e of incoming || []) {
    const k = feedbackDedupeKey(e);
    if (seen.has(k)) continue;
    seen.add(k);
    out.push(e);
  }
  return out;
}

export async function readFeedbackLog(logPath, { log = () => {} } = {}) {
  let raw;
  try {
    raw = await readFile(logPath, 'utf8');
  } catch {
    return [];
  }
  const rows = [];
  for (const [i, line] of raw.trim().split('\n').filter(Boolean).entries()) {
    try {
      const normalized = normalizeFeedbackEntry(JSON.parse(line));
      if (normalized) rows.push(normalized);
      else log(`feedback log skip invalid line ${i + 1}: unsupported schema or fields`);
    } catch (e) {
      log(`feedback log skip corrupt line ${i + 1}: ${e.message}`);
    }
  }
  return rows;
}

let feedbackLogQueue = Promise.resolve();

export async function appendFeedbackLog(logPath, entries, { log = () => {} } = {}) {
  if (!entries?.length) return { appended: 0 };
  const normalized = entries.map(normalizeFeedbackEntry).filter(Boolean);
  if (normalized.length !== entries.length) {
    log(`feedback log skipped ${entries.length - normalized.length} invalid entr${entries.length - normalized.length === 1 ? 'y' : 'ies'}`);
  }
  if (!normalized.length) return { appended: 0 };
  const run = async () => {
    try {
      await mkdir(dirname(logPath), { recursive: true });
      const lines = normalized.map((entry) => `${JSON.stringify(entry)}\n`).join('');
      await appendFile(logPath, lines, 'utf8');
      return { appended: normalized.length };
    } catch (e) {
      log(`feedback log write failed: ${e.message}`);
      return { appended: 0, error: e.message };
    }
  };
  const resultPromise = feedbackLogQueue.then(run, run);
  feedbackLogQueue = resultPromise.catch(() => {});
  return resultPromise;
}

function fixtureSlug(entry) {
  const title = entry.finding?.title || 'finding';
  const line = entry.finding?.line || UNKNOWN_LINE;
  const ascii = title.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/(^-|-$)/g, '').slice(0, 24);
  if (ascii.length >= 3) return `${ascii}-L${line}`;
  const hash = createHash('sha256').update(title).digest('hex').slice(0, 8);
  return `h${hash}-L${line}`;
}

/** Черновик eval-fixture из FP-записи. */
export function buildFixtureCandidate(entry) {
  const slug = fixtureSlug(entry);
  const id = `fp-pr-${entry.pr}-${slug}`.slice(0, 64);
  const file = entry.finding?.file;
  if (!file) return null;
  return {
    suggestedId: id,
    fromFeedback: {
      ts: entry.ts,
      pr: entry.pr,
      repo: entry.repo,
      source: entry.source,
      signal: entry.signal,
      reaction: entry.reaction,
      label: entry.label,
      commentId: entry.commentId,
    },
    meta: {
      id,
      description: `FP harvest PR #${entry.pr}: ${entry.finding?.title || 'finding'} (${entry.source}/${entry.signal})`,
      expected: { shouldBlock: false, blockerCount: 0 },
    },
    findings: [{
      file,
      line: entry.finding?.line || 1,
      severity: entry.finding?.severity || 'major',
      confidence: 0.95,
      title: entry.finding?.title || 'замечание',
      body: 'auto-import from GitHub feedback (R8) — дополните diff.patch',
      model: entry.finding?.model || 'harvest',
    }],
    diffNote: `Скопируйте unified diff из PR #${entry.pr} в fixtures/${id}/diff.patch`,
  };
}

export async function writeFixtureCandidates(dir, entries, { log = () => {} } = {}) {
  const fpEntries = (entries || []).filter((e) => e.signal === 'fp');
  if (!fpEntries.length) return { written: 0, skipped: 0 };
  await mkdir(dir, { recursive: true });
  let written = 0;
  let skipped = 0;
  for (const entry of fpEntries) {
    const candidate = buildFixtureCandidate(entry);
    if (!candidate) {
      skipped++;
      continue;
    }
    const path = resolve(dir, `${candidate.suggestedId}.json`);
    try {
      await writeFile(path, `${JSON.stringify(candidate, null, 2)}\n`, { flag: 'wx' });
      written++;
    } catch (e) {
      if (e.code === 'EEXIST') skipped++;
      else log(`candidate write skip ${candidate.suggestedId}: ${e.message}`);
    }
  }
  return { written, skipped };
}

export function formatFeedbackMarkdown(entry, candidatesDir = DEFAULT_CANDIDATES_DIR) {
  const candidate = buildFixtureCandidate(entry);
  const loc = entry.finding?.file
    ? `\`${entry.finding.file}${entry.finding.line ? `:${entry.finding.line}` : ''}\``
    : '(no location)';
  const src = entry.source === 'reaction'
    ? `reaction ${entry.reaction === '-1' ? '👎' : '👍'} на inline #${entry.commentId}`
    : `label \`${entry.label}\``;
  const candLine = candidate
    ? `- Signal: **${entry.signal}** · candidate: \`${candidatesDir}/${candidate.suggestedId}.json\`\n`
    : `- Signal: **${entry.signal}** (no fixture — missing file path)\n`;
  return `### ${entry.ts.slice(0, 10)} · PR #${entry.pr} (R8 auto)\n` +
    `- Источник: ${src}${entry.user ? ` (@${entry.user})` : ''}\n` +
    `- Finding: [${entry.finding?.severity}] ${loc} «${entry.finding?.title || ''}»\n` +
    candLine;
}

export async function appendFeedbackMarkdown(mdPath, entries, { log = () => {}, candidatesDir = DEFAULT_CANDIDATES_DIR } = {}) {
  const fp = (entries || []).filter((e) => e.signal === 'fp');
  if (!fp.length) return { appended: 0 };
  try {
    let md;
    try {
      md = await readFile(mdPath, 'utf8');
    } catch (e) {
      if (e.code !== 'ENOENT') throw e;
      md = `# AI-review FP journal (R8 auto-init)\n\n`;
    }
    const block = `\n${fp.map((e) => formatFeedbackMarkdown(e, candidatesDir)).join('\n')}`;
    md = md.trimEnd() + block + '\n';
    await writeFile(mdPath, md, 'utf8');
    return { appended: fp.length };
  } catch (e) {
    log(`feedback markdown append failed: ${e.message}`);
    return { appended: 0, error: e.message };
  }
}

export function formatHarvestReport(result) {
  const lines = [
    '=== pr-review feedback harvest (R8) ===',
    `PR #${result.pr} · ${result.repo}`,
    `Новых записей: ${result.newEntries.length} (persisted: ${result.persistedEntries ?? result.newEntries.length}, всего в логе: ${result.totalInLog})`,
    `Fixture candidates: ${result.candidatesWritten}`,
  ];
  for (const e of result.newEntries) {
    lines.push(`- [${e.signal}] ${e.source} · ${e.finding?.file}:${e.finding?.line} «${e.finding?.title}»`);
  }
  if (!result.newEntries.length) lines.push('- (нет нового feedback)');
  if (result.warnings?.length) {
    lines.push('', 'Warnings:');
    for (const w of result.warnings) lines.push(`- ${w}`);
  }
  if (result.writeErrors?.length) {
    lines.push('', 'Write errors:');
    for (const err of result.writeErrors) lines.push(`- ${err}`);
  }
  return lines.join('\n');
}

async function fetchReactionsPool(comments, github, concurrency, { log = () => {} } = {}) {
  const reactionsByCommentId = new Map();
  const pool = Math.max(1, concurrency || 6);
  let idx = 0;
  async function worker() {
    while (idx < comments.length) {
      const i = idx++;
      const c = comments[i];
      try {
        const reactions = await github.listPullCommentReactions(c.id);
        reactionsByCommentId.set(String(c.id), reactions);
      } catch (e) {
        log(`reactions for comment ${c.id} skip: ${e.message}`);
      }
    }
  }
  await Promise.all(Array.from({ length: Math.min(pool, comments.length) }, () => worker()));
  return reactionsByCommentId;
}

/** Harvest с хостинга (GitHub/GitLab): labels + inline 👎/👍 + sticky findings. */
export async function harvestFeedback(cfg, github, { log = () => {} } = {}) {
  const { feedback } = cfg;
  const { repo, prNumber: pr } = forgeOf(cfg);
  if (!repo || !pr) throw new Error('harvest требует REPO и PR_NUMBER');

  const trustedAuthors = await resolveTrustedAuthors(feedback.trustedAuthors, github);

  const [labels, inlineComments, sticky] = await Promise.all([
    github.listIssueLabels(),
    github.listInlineComments(),
    github.readStickyReview(),
  ]);

  const warnings = [];
  let stickyFindings = [];
  if (sticky?.body) {
    if (isTrustedReviewAuthor(sticky.user?.login, trustedAuthors, sticky.user)) {
      stickyFindings = parseStickyFindings(sticky.body);
    } else {
      warnings.push(`sticky comment by untrusted author ${sticky.user?.login || '?'} — skipped for label harvest`);
    }
  }

  const hasFpLabel = labels.some((l) => l.name === feedback.label);
  if (hasFpLabel && !stickyFindings.length) {
    warnings.push(`label ${feedback.label} present but no trusted sticky findings — label harvest skipped`);
  }

  const trustedInline = inlineComments.filter(
    (c) => hasMarker(c.body, MARKERS.inline) && isTrustedReviewAuthor(c.user?.login, trustedAuthors, c.user),
  );
  const reactionsByCommentId = await fetchReactionsPool(
    trustedInline, github, feedback.reactionConcurrency, { log },
  );

  const incoming = collectFeedbackEntries({
    repo, pr, labels, inlineComments: trustedInline, stickyFindings, reactionsByCommentId,
    fpLabel: feedback.label, trustedAuthors,
  });

  const logPath = resolve(process.cwd(), feedback.logFile);
  const existing = await readFeedbackLog(logPath, { log });
  const merged = dedupeFeedbackEntries(existing, incoming);
  const newEntries = merged.slice(existing.length);

  let candidatesWritten = 0;
  let persistedEntries = 0;
  const writeErrors = [];

  if (feedback.write && newEntries.length) {
    const logR = await appendFeedbackLog(logPath, newEntries, { log });
    if (logR.error) writeErrors.push(`JSONL: ${logR.error}`);
    else if (logR.appended !== newEntries.length) {
      writeErrors.push(`JSONL: appended ${logR.appended}/${newEntries.length}`);
    } else {
      persistedEntries = logR.appended;
    }

    if (feedback.writeCandidates && !writeErrors.length) {
      const r = await writeFixtureCandidates(resolve(process.cwd(), feedback.candidatesDir), newEntries, { log });
      candidatesWritten = r.written;
    }
    if (feedback.appendMarkdown && !writeErrors.length) {
      const mdR = await appendFeedbackMarkdown(resolve(process.cwd(), feedback.mdPath), newEntries, {
        log, candidatesDir: feedback.candidatesDir,
      });
      if (mdR.error) writeErrors.push(`markdown: ${mdR.error}`);
    }
  }

  return {
    pr, repo,
    newEntries,
    persistedEntries: feedback.write ? persistedEntries : 0,
    totalInLog: merged.length,
    candidatesWritten,
    warnings,
    writeErrors,
    dryRun: !feedback.write,
    ok: writeErrors.length === 0,
  };
}
