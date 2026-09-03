/** R11: auto-triage unresolved AI review threads (known FP + superseded findings). */

import { MARKERS, hasMarker } from './constants.mjs';
import { matchesKnownFalsePositive } from './findings.mjs';
import { DEFAULT_TRUSTED_AUTHORS, isTrustedReviewAuthor, parseTrustedAuthors } from './feedback.mjs';

const AUTO_TRIAGE_MARKER = '🤖 auto-triage';

export function findingLocationKey(file, line) {
  const ln = Number(line) || 0;
  return `${file}|${ln > 0 ? Math.floor((ln - 1) / 3) : 'out'}`;
}

/** Заголовок находки из тела inline-комментария движка. */
export function parseInlineReviewTitle(body) {
  const m = /^\s*[^\n]*\*\*(?:critical|major|minor|nit)(?:\s*\(.+\))?:\s*(.*)\*\*/im.exec(String(body || ''));
  return m ? m[1].trim() : '';
}

/**
 * Decide whether to auto-resolve a review thread.
 * @returns {{ action: 'resolve'|'skip', reason?: string, reply?: string }}
 */
export function decideAutoTriage(thread, ctx) {
  const firstComment = thread.comments?.[0];
  const first = firstComment?.body || '';
  if (!hasMarker(first, MARKERS.inline)) {
    return { action: 'skip', reason: 'not_ai_inline' };
  }
  const trustedAuthors = ctx.trustedAuthors?.length
    ? ctx.trustedAuthors
    : parseTrustedAuthors(DEFAULT_TRUSTED_AUTHORS);
  if (!isTrustedReviewAuthor(firstComment?.author, trustedAuthors, firstComment?.user)) {
    return { action: 'skip', reason: 'untrusted_author' };
  }

  const path = thread.path || '';
  const line = Number(thread.line) || 0;
  const key = findingLocationKey(path, line);
  const text = `${parseInlineReviewTitle(first)} ${first}`;

  if (ctx.blockerKeys?.has(key)) {
    return { action: 'skip', reason: 'active_blocker' };
  }

  if (matchesKnownFalsePositive(text, ctx.changedPaths || [], { patternsFile: ctx.patternsFile })) {
    return {
      action: 'resolve',
      reason: 'known_fp',
      reply: `${MARKERS.inline}\n✅ **auto-triage:** известный false positive (e2e-in-diff / file-in-diff pattern). SHA \`${ctx.headSha?.slice(0, 7) || '?'}\`.\n\n<sub>🤖 auto-triage</sub>`,
    };
  }

  if (ctx.advisoryOnly?.has(key)) {
    return {
      action: 'resolve',
      reason: 'advisory_only',
      reply: `${MARKERS.inline}\n✅ **auto-triage:** advisory (${ctx.blockOn?.join('/') || 'non-blocker'}) — не блокирует merge. SHA \`${ctx.headSha?.slice(0, 7) || '?'}\`.\n\n<sub>🤖 auto-triage</sub>`,
    };
  }

  if (!ctx.currentKeys?.has(key)) {
    return {
      action: 'resolve',
      reason: 'superseded',
      reply: `${MARKERS.inline}\n✅ **auto-triage:** finding не воспроизведён в последнем прогоне (исправлено или отфильтровано). SHA \`${ctx.headSha?.slice(0, 7) || '?'}\`.\n\n<sub>🤖 auto-triage</sub>`,
    };
  }

  return { action: 'skip', reason: 'still_active' };
}

/** Resolve eligible threads; fail-open. */
export async function runAutoTriage(github, ctx, log = () => {}) {
  if (!github?.listUnresolvedReviewThreads) return { resolved: 0, skipped: 0, errors: 0 };

  let threads;
  try {
    threads = await github.listUnresolvedReviewThreads();
  } catch (e) {
    log(`auto-triage: не удалось загрузить threads (fail-open): ${e.message}`);
    return { resolved: 0, skipped: 0, errors: 1 };
  }

  let resolved = 0;
  let skipped = 0;
  let errors = 0;

  for (const thread of threads) {
    const decision = decideAutoTriage(thread, ctx);
    if (decision.action !== 'resolve') {
      skipped++;
      continue;
    }
    try {
      const alreadyTriaged = (thread.comments || []).some((c) => c.body?.includes(AUTO_TRIAGE_MARKER));
      if (decision.reply && !alreadyTriaged) await github.replyReviewThread(thread.id, decision.reply);
      await github.resolveReviewThread(thread.id);
      resolved++;
      log(`auto-triage: resolved ${thread.path}:${thread.line} (${decision.reason})`);
    } catch (e) {
      errors++;
      log(`auto-triage: thread ${thread.id} fail-open: ${e.message}`);
    }
  }

  return { resolved, skipped, errors };
}

export function buildTriageContext({
  allFindings, blockers, changedPaths, headSha, trustedAuthors, blockOn, patternsFile,
}) {
  const currentKeys = new Set((allFindings || []).map((f) => findingLocationKey(f.file, f.line)));
  const blockerKeys = new Set((blockers || []).map((f) => findingLocationKey(f.file, f.line)));
  const tiers = new Set(blockOn || ['critical', 'major']);
  const blockOnKeys = new Set(
    (allFindings || []).filter((f) => tiers.has(f.severity)).map((f) => findingLocationKey(f.file, f.line)),
  );
  // Advisory, поднятая в инлайн по доказуемости, из авто-резолва исключается: смысл подъёма
  // в том, чтобы её ПРОЧИТАЛИ, а не в том, чтобы бот закрыл тред раньше человека. Остальные
  // advisory-нити резолвятся как прежде. Признак — флаг `actionabilityPromoted`, а не наличие
  // скора: скор теперь есть у каждой находки (он уходит в артефакт), и проверка на его
  // наличие исключила бы из авто-резолва вообще всё.
  const promotedKeys = new Set(
    (allFindings || [])
      .filter((f) => f.actionabilityPromoted)
      .map((f) => findingLocationKey(f.file, f.line)),
  );
  const advisoryOnly = new Set(
    (allFindings || [])
      .filter((f) => !tiers.has(f.severity))
      .map((f) => findingLocationKey(f.file, f.line))
      .filter((key) => !blockerKeys.has(key) && !blockOnKeys.has(key) && !promotedKeys.has(key)),
  );
  return {
    currentKeys,
    blockerKeys,
    advisoryOnly,
    blockOn: [...tiers],
    changedPaths: changedPaths || [],
    headSha,
    patternsFile,
    trustedAuthors: trustedAuthors?.length ? trustedAuthors : parseTrustedAuthors(DEFAULT_TRUSTED_AUTHORS),
  };
}
