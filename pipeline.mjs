import { readFile } from 'node:fs/promises';
import { loadReviewConfig, parseCli } from './lib/config.mjs';
import { createLogger, publicErrorLabel } from './lib/util.mjs';
import { parseDiff, collectBlastSymbols, readChangedFiles, readOutsideFile, buildChunks, collectAddedPaths, collectDeletedPaths, splitDiffByParsedPath } from './lib/diff.mjs';
import { createLlmClient } from './lib/llm.mjs';
import { createCostTracker, efficiencyMetrics } from './lib/cost.mjs';
import { emitBlastRadiusFindings } from './lib/blast-findings.mjs';
import { createPromptBuilder, buildChangedFilesSection } from './lib/prompt.mjs';
import { createGraphifyClient, blastUnavailableReason } from './lib/graphify.mjs';
import { createGithubClient } from './lib/github.mjs';
import { withIcons } from './lib/icons.mjs';
import { createReviewEngine } from './lib/engine.mjs';
import { resolveBlockers, createFindingsFormatter, filterFalsePositiveFindings, filterStalePriorFindings, attachFingerprints } from './lib/findings.mjs';
import { buildRepoPathIndex, listRepoFiles } from './lib/absent-path.mjs';
import { BLAST_FINDING_SOURCE, MARKERS } from './lib/constants.mjs';
import { selectVerifierCandidates, applyVerifierVerdicts } from './lib/verifier.mjs';
import { routeFindings, findingLocationKey, ACTIONABILITY_SIGNALS } from './lib/actionability.mjs';
import { buildVerifierFileContents, windowChunkFileContents } from './lib/context-window.mjs';
import {
  buildSymbolCards, createLineReader, selectChunkCards, symbolCardsHashInput, symbolCardsOutcome, isAbsenceClaim,
} from './lib/symbol-cards.mjs';
import { applyRefutation, formatRefutationLog } from './lib/specialists.mjs';
import { createPrescanRunner, dedupeAgainstPrescan, formatPrescanLog } from './lib/prescan.mjs';
import { routeDiff, formatRouteLog, extractFileDiffSections, classifyFilePath, collectReviewSkipped, formatReviewSkippedNote, isCodePath, collectProfileProtected } from './lib/router.mjs';
import { createObservability, slimFindings, countBySeverity, PR_TITLE_LOG_MAX } from './lib/observability.mjs';
import { buildSummary, buildRouterSummary, buildLlmUnavailableSummary } from './lib/report.mjs';
import { buildReviewJson, writeReviewExports } from './lib/export.mjs';
import { parseTrustedAuthors } from './lib/feedback.mjs';
import { buildTriageContext, runAutoTriage } from './lib/triage.mjs';
import {
  fileInputHashes, configFingerprint, readIncrementalState, writeIncrementalState,
  planIncremental, markCarried, formatIncrementalNote,
} from './lib/incremental.mjs';

/** Soft-fail пайплайна без process.exit — для долгоживущих потребителей (MCP). */
export class PipelineSoftFail extends Error {
  constructor(message) {
    super(message);
    this.name = 'PipelineSoftFail';
  }
}

function logFindingsStage(obs, stage, findings) {
  if (!obs?.enabled) return;
  obs.logEvent('findings_stage', {
    stage,
    count: findings.length,
    bySeverity: countBySeverity(findings),
    sample: slimFindings(findings),
  });
}

// Обратная карта «человеческая формулировка сигнала → ключ»: в саму находку маршрут пишет
// actionabilityReasons текстами (их читает рендер инлайна), а метрике нужен стабильный ключ.
const REASON_KEY_BY_WHY = new Map(
  Object.entries(ACTIONABILITY_SIGNALS).map(([key, sig]) => [sig.why, key]),
);

/**
 * Распределение actionability-скоров (docs/review-quality.md §3, P3.2). `inline=N · outside=M`
 * не отвечает на вопрос «порог мешает или сигналов нет» — гистограмма отвечает: пик у порога
 * означает первое, пик в нуле — второе. Топ причин считается по находкам, оставшимся в своде:
 * это список того, чего им не хватило.
 */
function logActionabilityDistribution(obs, { findings, inlineCount, outside, threshold }) {
  if (!obs?.enabled) return;
  const histogram = {};
  for (const f of findings || []) {
    const s = f?.actionabilityScore;
    if (!Number.isFinite(s)) continue;
    histogram[s] = (histogram[s] || 0) + 1;
  }
  const reasonCounts = new Map();
  for (const f of outside || []) {
    for (const why of f?.actionabilityReasons || []) {
      const key = REASON_KEY_BY_WHY.get(why) || why;
      reasonCounts.set(key, (reasonCounts.get(key) || 0) + 1);
    }
  }
  const topOutsideReasons = [...reasonCounts.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .slice(0, 5)
    .map(([key, count]) => ({ key, count }));
  obs.logEvent('actionability', {
    threshold,
    inline: inlineCount,
    outside: (outside || []).length,
    histogram,
    topOutsideReasons,
  });
}

/** Sticky при `REVIEW_BLAST_REQUIRED=1` и непосчитанном blast-radius (LLM не вызывался). */
function buildBlastUnavailableSummary(reason) {
  return `${MARKERS.review}\n## 🤖 Авто код-ревью\n\n⛔ Blast-radius недоступен (\`${reason}\`): code graph Graphify не построен или не читается.\n\n`
    + '<sub>Без графа гейт не видит callers вне диффа — merge заблокирован до LLM-ревью '
    + '(fail-closed, `REVIEW_BLAST_REQUIRED=1`). Проверьте шаг сборки графа (`graphify update`), '
    + '`GRAPH` и `GRAPHIFY_BIN`; осознанное ослабление — `REVIEW_BLAST_REQUIRED=0`. Детали — в логе CI job.</sub>';
}

/** Совместимость с тестовыми/custom Graphify-клиентами старого API, возвращавшими массив. */
function normalizeBlastRadius(result, symbols) {
  if (Array.isArray(result)) {
    return {
      blocks: result,
      metrics: {
        symbols_total: symbols.length,
        symbols_resolved: null,
        affected_nonempty: result.length,
        graph_fresh: null,
        graph_status: 'unknown',
        skip_reason: 'client_did_not_report',
        skip_reasons: { client_did_not_report: 1 },
      },
    };
  }
  return {
    blocks: Array.isArray(result?.blocks) ? result.blocks : [],
    graphIndex: result?.graphIndex ?? null,
    metrics: result?.metrics ?? {
      symbols_total: symbols.length,
      symbols_resolved: null,
      affected_nonempty: 0,
      graph_fresh: null,
      graph_status: 'unknown',
      skip_reason: 'client_did_not_report',
      skip_reasons: { client_did_not_report: 1 },
    },
  };
}

async function persistExports(cfg, payload, log, obs) {
  if (!cfg.export?.enabled) return null;
  if (cfg.cli?.dryRun && !cfg.export.dryRun) return null;
  try {
    const out = await writeReviewExports(cfg, payload, log);
    obs?.logEvent('export', {
      ok: true,
      files: out?.written?.length ?? 0,
      sarifResults: out?.sarifResults ?? 0,
    });
    return out;
  } catch (e) {
    log('export R9 failed (fail-open):', e.message);
    obs?.logEvent('export', { ok: false, error: String(e.message || e).slice(0, 200) });
    return null;
  }
}

/**
 * Verifier не дал вердикта по находкам уровня блока из-за сбоя (RFC 18): исключение первого
 * вызова, ни одного валидного вердикта, тайм-бюджет. Такая находка не может считаться
 * подтверждённым блокером — гейт обещает блок только с вердиктом второго судьи. Она
 * понижается до minor с `verifierStatus: unavailable`, а судьбу прогона решает
 * REVIEW_LLM_REQUIRED в pipeline: degraded-блок (дефолт) или fail-open.
 *
 * Раньше исход зависел от вида сбоя: исключение оставляло находку critical (блок по слову
 * агрегатора, без строки verifier в sticky), пустой ответ понижал её до minor (merge проходил).
 * REVIEW_VERIFIER_STRICT=0 — прежнее поведение: потребитель доверяет агрегатору сам.
 */
function markVerifierUnavailable(findings, pending, reason, log, obs) {
  for (const f of pending) {
    f.verifierReason = `verifier недоступен (${reason}) — было ${f.severity}, понижено до minor`;
    f.severity = 'minor';
    f.verifierStatus = 'unavailable';
  }
  log(`verifier недоступен (${reason}): ${pending.length} находок уровня блока не проверены → minor`);
  obs.logEvent('verifier_unavailable', { reason, unverified: pending.length });
  return { reason, unverified: pending.length };
}

/** Стадия verifier Pass B: кандидаты critical/major → второй проход. Сбой — см. markVerifierUnavailable. */
async function runVerifierStage(allFindings, {
  engine, gate, verifier, deadline, contentMap, annotated, blastBlocks, pr, changedFilesSection,
  actionability, actionabilityCtx, files, symbolCardsByPath = null, limits = {},
  readOutside = null,
}, log, obs) {
  const empty = { findings: allFindings, stats: null, unavailable: null };
  if (!verifier.enabled) return empty;
  const isBlockTier = (f) => gate.blockOn.includes(f.severity) && !f.prescan;
  // RFC 21: blast-кандидат судится по строке вызова, а файл вызова — вне PR. Читаем до
  // отбора: нечитаемый файл — нет доказательства, такой кандидат не занимает слот verifier'а.
  const outside = new Map();
  if (gate.blastVerify !== '0' && readOutside) {
    const blastFiles = new Set(allFindings
      .filter((f) => f.source === BLAST_FINDING_SOURCE && isBlockTier(f) && f.file && !contentMap.has(f.file))
      .map((f) => f.file));
    for (const path of blastFiles) {
      const entry = await readOutside(path).catch(() => null);
      if (entry) outside.set(path, entry);
    }
    let unreadable = 0;
    for (const f of allFindings) {
      if (f.source !== BLAST_FINDING_SOURCE || !isBlockTier(f) || !blastFiles.has(f.file) || outside.has(f.file)) continue;
      f.severity = 'minor';
      f.verifierStatus = 'unverified';
      f.verifierReason = 'файл вызывающего кода не прочитан — несовместимость не проверить, понижено до minor';
      unreadable++;
    }
    if (blastFiles.size) {
      log(`verifier: файлов вызывающего кода вне PR прочитано ${outside.size} из ${blastFiles.size}${unreadable ? `, ${unreadable} blast-находок → minor (unverified)` : ''}`);
      obs.logEvent('verifier_blast_callers', { files: blastFiles.size, read: outside.size, unreadable });
    }
  }
  // Сбой судит только strict-режим verifier'а: при REVIEW_VERIFIER_STRICT=0 находка без
  // вердикта и так остаётся как есть — потребитель выбрал доверять агрегатору.
  const unavailable = (pending, reason) => (verifier.strictBlocks && pending.length
    ? { ...empty, unavailable: markVerifierUnavailable(allFindings, pending, reason, log, obs) }
    : empty);
  if (deadline && deadline - Date.now() < 120_000) {
    log('verifier pass B пропущен: тайм-бюджет на исходе (<2 мин в запасе)');
    obs.logEvent('verifier_skip', { reason: 'time_budget' });
    return unavailable(allFindings.filter(isBlockTier), 'time_budget');
  }
  // Advisory-кандидаты на подтверждение: доказуемые minor/nit, которые претендуют на строку
  // в PR. Verifier здесь работает не на гейт (severity их не блокирует), а на право занять
  // внимание человека — в инлайн должно уходить проверенное.
  //
  // Порог ДОПУСКА сюда ниже порога маршрута (cfg.actionability.verifyMin, дефолт
  // «порог − вес(verified)» = 1): совпадение порогов было курицей и яйцом — чтобы minor
  // дошёл до verifier'а за +3, он должен был уже набрать 4 без него, то есть уже уходить
  // в строку, и verifier для advisory работал только на понижение (docs/review-quality.md
  // §2.2, P1). Окончательный порог маршрута НЕ трогаем: он считается заново в classify,
  // уже с вердиктом verifier'а на руках. Цену держит лимит REVIEW_VERIFY_ADVISORY.
  const verifyThreshold = Number.isFinite(actionability?.verifyMin)
    ? actionability.verifyMin
    : actionability?.threshold;
  const advisoryPool = actionability?.verifyAdvisory
    ? routeFindings(allFindings, files, {
      inlineOn: gate.blockOn,
      threshold: verifyThreshold,
      maxAdvisoryInline: actionability.verifyAdvisory,
      strict: gate.strict,
      ctx: actionabilityCtx || {},
    }).filter((s) => s.promoted).map((s) => s.finding)
    : [];
  // Блокеры сверх капа одного вызова не понижаются молча, а идут следующими батчами
  // (RFC 17): 13-й critical иначе переставал блокировать только потому, что был 13-м.
  // Кап одного вызова держит размер промпта, число батчей — цену прогона.
  const batchCap = Math.max(1, verifier.maxCandidates);
  const maxBatches = Math.max(1, verifier.maxBatches ?? 1);
  const candidates = selectVerifierCandidates(allFindings, gate, batchCap * maxBatches, {
    candidates: advisoryPool,
    max: actionability?.verifyAdvisory || 0,
  });
  if (advisoryPool.length) {
    // Насколько пониженный порог расширил допуск — измеримая величина, а не вера в P1.
    const admitted = advisoryPool.filter((f) => (f.actionabilityScore ?? 0) < (actionability?.threshold ?? 0)).length;
    log(`verifier: +${advisoryPool.length} advisory-кандидат(ов) по доказуемости (порог допуска ${verifyThreshold} vs маршрут ${actionability?.threshold}; из них ${admitted} прошли только по пониженному порогу)`);
    obs.logEvent('verifier_advisory', {
      pool: advisoryPool.length,
      admittedByLowerBar: admitted,
      verifyThreshold,
      routeThreshold: actionability?.threshold ?? null,
    });
  }
  if (!candidates.length) {
    obs.logEvent('verifier_skip', { reason: 'no_candidates' });
    return empty;
  }
  // Первый батч — как прежний единственный вызов: до капа блокеров плюс advisory.
  // Остальные — только блокеры: advisory не стоят лишнего вызова.
  const blockTier = candidates.filter(isBlockTier);
  const advisory = candidates.filter((f) => !isBlockTier(f));
  const batches = [[...blockTier.slice(0, batchCap), ...advisory]];
  for (let i = batchCap; i < blockTier.length; i += batchCap) batches.push(blockTier.slice(i, i + batchCap));
  log(`verifier pass B: ${candidates.length} кандидат(ов) ${gate.blockOn.join('/')} · батчей ${batches.length} · модель ${verifier.model}`);

  async function verifyBatch(batch, i) {
    // Verifier получает только файлы/секции диффа с кандидатами, а не весь PR —
    // иначе на большом PR это самый жирный промпт пайплайна.
    const candFiles = new Set(batch.map((c) => c.file).filter(Boolean));
    // Blast-кандидат: его файл вне диффа, а доказательство смены контракта — в хунке, где
    // меняется его символ. Секции с этим именем идут verifier'у вместе с секциями кандидатов.
    const blastSymbols = batch.filter((c) => c.source === BLAST_FINDING_SOURCE && c.blastSymbol)
      .map((c) => c.blastSymbol);
    // …и не файлы целиком, а окна ±contextMargin вокруг строк кандидатов: verifier смотрит
    // одну-две строки на файл, а платил за 1500 строк — причём другой моделью, то есть без
    // шанса переиспользовать кэш префикса advisor'ов (lib/context-window.mjs).
    const ctxWindow = buildVerifierFileContents(contentMap, batch, {
      margin: verifier.contextMargin, outside, outsideMargin: verifier.blastCallerMargin ?? 20,
    });
    const fileContents = ctxWindow.text;
    const sections = [...extractFileDiffSections(annotated).entries()]
      .filter(([p, s]) => candFiles.has(p) || blastSymbols.some((sym) => s.includes(sym)))
      .map(([, s]) => s);
    const annotatedScoped = sections.length ? sections.join('\n') : annotated;
    const tag = batches.length > 1 ? ` [батч ${i + 1}/${batches.length}]` : '';
    log(`verifier ctx${tag}: ${candFiles.size} файл(ов) с кандидатами из ${contentMap.size}, контекст ${ctxWindow.before}→${ctxWindow.after} симв. (окна ±${verifier.contextMargin}), дифф ${annotatedScoped.length}/${annotated.length} симв.`);
    // Карточки символов файлов-кандидатов (RFC 14, решение 4) — только под флагом: это вход
    // судьи, чей вердикт решает блокировку.
    const verifierCards = verifier.symbolCards && symbolCardsByPath?.size
      ? selectChunkCards(symbolCardsByPath, [...candFiles], {
        max: limits.symbolCardsMax, maxChars: limits.symbolCardsMaxChars, visibleText: `${annotatedScoped}\n${fileContents}`,
      }).cards
      : [];
    if (verifierCards.length) log(`verifier: карточек символов ${verifierCards.length}`);
    return engine.verifyFindings(batch, {
      annotated: annotatedScoped, fileContents, blastBlocks, pr, changedFilesSection,
      ...(verifierCards.length ? { symbolCards: verifierCards } : {}),
    });
  }

  // Сначала все вызовы, потом одно применение: applyVerifierVerdicts мутирует находки, и
  // сбой посреди батчей не должен оставить набор наполовину применённым.
  const sent = [];
  const verdicts = [];
  let batchesSent = 0;
  let stopReason = '';
  for (let i = 0; i < batches.length; i++) {
    if (i > 0 && deadline && deadline - Date.now() < 120_000) {
      stopReason = 'time_budget';
      log(`verifier: тайм-бюджет — проверено ${sent.length} из ${candidates.length} кандидатов`);
      break;
    }
    let batchVerdicts;
    try {
      batchVerdicts = await verifyBatch(batches[i], i);
    } catch (e) {
      if (i === 0) {
        log('verifier pass failed:', e.message);
        obs.logEvent('verifier_error', { error: e.message?.slice(0, 200) });
        return unavailable(blockTier, 'error');
      }
      // Первый батч прошёл: его вердикты не выбрасываем, непроверенный хвост — как сверх
      // капа (ниже). Иначе сбой N-го вызова отменял бы уже сделанную проверку.
      log(`verifier: батч ${i + 1}/${batches.length} упал (${e.message}) — хвост не проверен`);
      obs.logEvent('verifier_error', { batch: i, error: e.message?.slice(0, 200) });
      stopReason = 'batch_error';
      break;
    }
    for (const v of batchVerdicts || []) {
      const idx = v?.candidateIndex;
      if (Number.isInteger(idx) && idx >= 0 && idx < batches[i].length) {
        verdicts.push({ ...v, candidateIndex: idx + sent.length });
      }
    }
    sent.push(...batches[i]);
    batchesSent++;
  }
  // Ни одного валидного вердикта (пустой ответ, чужие индексы, не тот ключ) — это сбой
  // судьи, а не его решение: тот же исход, что у исключения.
  if (!verdicts.length && sent.some(isBlockTier)) {
    obs.logEvent('verifier_error', { error: 'no valid verdicts' });
    return unavailable(blockTier, 'empty');
  }
  const applied = applyVerifierVerdicts(allFindings, sent, verdicts, log, {
    strictUnverified: verifier.strictBlocks,
  });
  // Хвост, не дошедший из-за сбоя или тайм-бюджета, — недоступность, а не кап.
  let tailUnavailable = null;
  if (stopReason) {
    const sentSet = new Set(sent);
    const tail = applied.findings.filter((f) => isBlockTier(f) && !sentSet.has(f));
    tailUnavailable = unavailable(tail, stopReason).unavailable;
  }
  // Strict: eligible-находки, не дошедшие до verifier'а (сверх капа × батчей, тайм-бюджет,
  // сбой хвостового батча), блокировать без верификации не должны (бэклог #7) → minor.
  let overflowN = 0;
  if (verifier.strictBlocks) {
    const sentSet = new Set(sent);
    const why = `сверх капа ${batchCap} × ${maxBatches}`;
    for (const f of applied.findings) {
      if (isBlockTier(f) && !sentSet.has(f)) {
        f.severity = 'minor';
        f.verifierStatus = 'overflow';
        f.verifierReason = `не проверено verifier'ом (${why}) — понижено до minor`;
        overflowN++;
      }
    }
    if (overflowN) {
      applied.stats.overflow = overflowN;
      log(`verifier strict: ${overflowN} находок не проверено (${why}) → minor (без верификации не блокируют)`);
    }
  }
  applied.stats.batches = batchesSent;
  log(`verifier итог: verified=${applied.stats.verified} rejected=${applied.stats.rejected} downgraded=${applied.stats.downgraded} missing=${applied.stats.missing}`);
  // RFC 21: вердикты по источнику — blast-находки судятся по другому правилу, и замер
  // shadow-режима читает именно их.
  const bySource = {};
  for (const f of sent) {
    const src = f.source === BLAST_FINDING_SOURCE ? BLAST_FINDING_SOURCE : 'llm';
    const st = f.verifierStatus || 'none';
    bySource[src] = bySource[src] || {};
    bySource[src][st] = (bySource[src][st] || 0) + 1;
  }
  applied.stats.bySource = bySource;
  obs.logEvent('verifier', applied.stats);
  logFindingsStage(obs, 'after_verifier', applied.findings);
  return { findings: applied.findings, stats: applied.stats, unavailable: tailUnavailable };
}

/** Публикует inline и чистит устаревшие только после успешного post. */
async function publishInline(github, { inline, headSha, maxInline, cleanupWithoutPost = false }, log) {
  const inlineToPost = inline.slice(0, maxInline);
  if (inline.length > inlineToPost.length) {
    log(`inline-комментов ${inline.length} > лимита ${maxInline} — постим первые ${maxInline} (полные счётчики в sticky)`);
  }
  if (!inlineToPost.length && cleanupWithoutPost) {
    const cleared = await github.deleteStaleInline();
    return { posted: 0, total: inline.length, cleared };
  }
  // post-then-delete: сначала новый review, потом чистка старых инлайнов (кроме только что
  // запощенного) — при сбое постинга старые комментарии не теряются.
  const reviewId = await github.postReview({ inline: inlineToPost, headSha });
  // reviewId=null (GitHub не вернул id) → чистку пропускаем: иначе сотрём только что запощенное.
  const cleared = reviewId
    ? await github.deleteStaleInline({ exceptReviewId: reviewId })
    : (log('review id не получен — пропускаем чистку старых инлайнов'), 0);
  return { posted: inlineToPost.length, total: inline.length, cleared };
}

/** Стадия публикации: post-then-delete inline → upsert sticky. */
async function publishReview(github, { inline, md, headSha, maxInline }, log) {
  const published = await publishInline(github, { inline, headSha, maxInline }, log);
  const op = await github.upsertSticky(md);
  return { ...published, op };
}

async function notifyTelegram(cfg, log, { pr, counts, total, htmlUrl, reason }) {
  const { token, chat } = cfg.telegram;
  if (!token || !chat) return;
  const text = `🔍 Авто-ревью PR #${pr.number}: «${pr.title}»\n` +
    `${total} замечаний — 🔴${counts.critical} 🟠${counts.major} 🟡${counts.minor} 🔵${counts.nit}` +
    `${reason ? `\n⚠️ ${reason}` : ''}\n${htmlUrl}`;
  try {
    const res = await fetch(`https://api.telegram.org/bot${token}/sendMessage`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ chat_id: chat, text, disable_web_page_preview: true }),
    });
    if (!res.ok) log('telegram ping failed (не критично):', res.status, await res.text().catch(() => ''));
  } catch (e) { log('telegram ping failed (не критично):', e.message); }
}

/**
 * FP-фильтр → beforeResolve → resolveBlockers: единый путь для fail-closed и основного pipeline.
 * Возвращает findings, clusters, blockers, список снятых FP-паттернами находок и результат beforeResolve.
 */
export async function runGateResolution({ findings: inputFindings, changedPaths, gate, prescanCfg, patternsFile, repoIndex, log, beforeResolve }) {
  const beforeFp = inputFindings;
  const beforeFpCount = beforeFp.length;
  const afterFp = filterFalsePositiveFindings(beforeFp, changedPaths, log, { patternsFile, repoIndex });
  const afterFpCount = afterFp.length;
  const afterFpSet = new Set(afterFp);
  const fpRemoved = beforeFp.filter((f) => !afterFpSet.has(f));
  const intermediate = beforeResolve ? await beforeResolve(afterFp) : { findings: afterFp };
  const resolved = resolveBlockers(intermediate.findings, changedPaths, gate, log, {
    skipFpFilter: true, prescan: prescanCfg,
  });
  return { ...intermediate, ...resolved, fpRemoved, beforeFpCount, afterFpCount };
}

/** Собирает пайплайн из конфига; зависимости инжектируются для тестов. */
export function createReviewPipeline(cfg, deps = {}) {
  const log = deps.log ?? createLogger();
  const die = deps.die ?? ((msg) => { log('SOFT-FAIL:', msg); process.exit(0); });

  const costTracker = deps.costTracker ?? createCostTracker(cfg.cost?.priceMap);
  const ollama = deps.ollama ?? createLlmClient(cfg, log, { costTracker });
  const prompt = deps.prompt ?? createPromptBuilder();
  const graphify = deps.graphify ?? createGraphifyClient(cfg);
  // Пиктограммы — на границе публикации: рендер и логи остаются с эмодзи (lib/icons.mjs).
  const github = withIcons(deps.github ?? createGithubClient(cfg, log), cfg.render?.iconBaseUrl);
  const findingsFmt = deps.findingsFmt ?? createFindingsFormatter(cfg);
  const prescan = deps.prescan ?? createPrescanRunner(cfg, { log });
  const telegramNotifier = deps.notifyTelegram ?? notifyTelegram;
  const repoFiles = deps.repoFiles ?? (() => listRepoFiles());
  // Файл вызывающего кода blast-кандидата (RFC 21) — вне PR, читается отдельно от contentMap.
  const readOutside = deps.readOutsideFile ?? ((path) => readOutsideFile(path, log));
  // Строки файла рабочего дерева для карточек символов (RFC 14); в тестах — подмена.
  const readLines = deps.readLines ?? createLineReader();

  async function finishObs(obs, data) {
    if (!obs?.enabled) return;
    // Эффективность считаем здесь, а не в пяти местах присваивания finalObs: единственная
    // точка выхода прогона, и costTracker к этому моменту видел все вызовы — включая те,
    // что случились на ветках fail-open.
    const efficiency = efficiencyMetrics(costTracker.snapshot(), data);
    await obs.flush(efficiency ? { ...data, ...efficiency } : data);
  }

  /** @returns {Promise<number>} exit code */
  async function run() {
    const detailed = await runDetailed();
    return detailed.exitCode;
  }

  /**
   * Тот же прогон, но с артефактом в памяти — MCP и тесты не зависят от записи на диск
   * (dry-run без REVIEW_EXPORT_DRY_RUN файл не пишет).
   * @returns {Promise<{exitCode: number, artifact: object|null, exportResult: object|null, wouldBlock: boolean, degraded: boolean, reason?: string}>}
   */
  async function runDetailed() {
    const { cli, reviewMode, ollama: oCfg, moa, models, gate, limits, verifier, specialists } = cfg;
    const { dryRun, diffFile } = cli;
    const postCreds = cfg.github.repo && cfg.github.prNumber && cfg.github.token;
    let lastArtifact = null;
    let lastExport = null;

    async function persistAndCapture(payload) {
      lastArtifact = payload;
      lastExport = await persistExports(cfg, payload, log, obs);
      return lastExport;
    }

    function detailedResult(exitCode, extra = {}) {
      const wouldBlock = extra.wouldBlock !== undefined
        ? extra.wouldBlock
        : (lastArtifact ? Boolean(lastArtifact.wouldBlock) : exitCode === 1);
      return {
        exitCode,
        artifact: lastArtifact,
        exportResult: lastExport,
        wouldBlock,
        degraded: Boolean(extra.degraded),
        reason: extra.reason,
      };
    }

    const obs = createObservability(cfg, { log });
    let finalObs = { exitCode: 0 };

    try {
      if (!oCfg.key) die('нет LLM API key (задайте LLM_API_KEY или OLLAMA_API_KEY)');
      if (reviewMode === 'moa' && !moa.references.length) die('MOA_REFERENCE_MODELS пуст — задай хотя бы один advisor');
      if (!diffFile && !postCreds) die('нет REPO/PR_NUMBER/GITHUB_TOKEN для загрузки диффа');
      if (!dryRun && !postCreds) die('постинг требует REPO/PR_NUMBER/GITHUB_TOKEN — для локального прогона добавь --dry-run');

      const engine = deps.engine ?? createReviewEngine(cfg, { ollama, prompt, log, observability: obs });

      let pr = { number: cfg.github.prNumber, title: '(local diff)', html_url: '' };
      let headSha = '';
      let diff;

      if (diffFile) {
        diff = await readFile(diffFile, 'utf8');
        headSha = String(deps.headSha || cli.headSha || '').trim();
        log(`дифф из файла ${diffFile} (${diff.length} байт)${headSha ? ` · head ${headSha.slice(0, 7)}` : ''}`);
      } else {
        const loaded = await github.loadDiff();
        if (!loaded) die('не удалось загрузить дифф PR (нет REPO/PR/token или сбой GitHub API)');
        if (loaded.draft) die('PR — черновик, пропускаем');
        pr = loaded.pr;
        headSha = loaded.headSha;
        diff = loaded.diff;
        log(`PR #${pr.number} «${pr.title}» · дифф ${diff.length} байт · head ${headSha.slice(0, 7)}`);
      }

      if (!diff.trim()) die('пустой дифф');

      const { files, annotated, changedText } = parseDiff(diff);
      const changedPaths = [...files.keys()];
      // Класс пути считает Rust-бинарь через отдельный процесс на вызов, а спрашивают его
      // четверо (отбор skip-путей, actionability по каждой находке, план чанков, маршрутизация
      // ролей) — мемоизация превращает десятки запусков процесса в один на файл.
      const fileClassCache = new Map();
      const fileClass = (path) => {
        if (!fileClassCache.has(path)) fileClassCache.set(path, classifyFilePath(path));
        return fileClassCache.get(path);
      };
      // Пути, которые не уходят в LLM-чанки: lockfile/generated (класс skip) и
      // `layout.reviewSkip` профиля — сгенерированное, что CI потребителя сверяет с
      // источником. Модели читают такие файлы кусками и спорят с отступами на границе
      // чанка, а дефект в них — всегда дефект генератора, видимый в его исходнике.
      // Prescan, blast-radius и карта PR их по-прежнему видят: меняется только вход LLM.
      // Профиль и имена файлов приходят из checkout'а PR, поэтому код по расширению,
      // `.github/`, пин движка, сам профиль и `layout.sensitive` не исключаются никогда
      // (reviewSkipProtection в lib/router.mjs) — совпадение с ними только пишется в лог.
      const isCodeCache = new Map();
      const isCode = (path) => {
        if (!isCodeCache.has(path)) isCodeCache.set(path, isCodePath(path));
        return isCodeCache.get(path);
      };
      let reviewSkipped = collectReviewSkipped(changedPaths, {
        reviewSkip: cfg.profile?.layout?.reviewSkip || null,
        fileClass,
        isCode,
        protectedPaths: collectProfileProtected(cfg),
        sensitive: cfg.profile?.layout?.sensitive || null,
      });
      if (reviewSkipped.ignored.length) {
        log(`skip-пути: ${reviewSkipped.ignored.length} совпадений проигнорировано — защищённые пути остаются во входе LLM`,
          reviewSkipped.ignored.map((x) => `${x.path} (${x.by}: ${x.reason})`));
      }
      // Секции диффа под LLM ключуются ТЕМ ЖЕ путём, что `files`/`changedPaths` (parseDiff).
      // Путь PR без своей секции — сужать нельзя: выкинули бы чужую секцию вместе с
      // реальным файлом. Fail-closed: skip-пути отключаются, модели получают полный дифф.
      const diffByPath = splitDiffByParsedPath(annotated);
      const sectionPaths = new Set(diffByPath.map((s) => s.path).filter(Boolean));
      const unmatchedPaths = changedPaths.filter((p) => !sectionPaths.has(p));
      if (unmatchedPaths.length && reviewSkipped.all.size) {
        log(`skip-пути: у ${unmatchedPaths.length} файл(ов) PR нет своей секции в диффе — сужение отключено, моделям уходит полный дифф`, unmatchedPaths);
        reviewSkipped = { byProfile: [], builtIn: [], ignored: reviewSkipped.ignored, all: new Set() };
      }
      const route = routeDiff(changedPaths, diff, { ...cfg.router, skipPaths: reviewSkipped.byProfile });
      log(`router R4: ${formatRouteLog(route)}`);
      obs.logEvent('run_start', {
        pr: pr.number,
        prTitle: String(pr.title || '').slice(0, PR_TITLE_LOG_MAX),
        headSha: headSha?.slice(0, 7),
        reviewMode,
        dryRun,
        files: changedPaths.length,
        diffBytes: diff.length,
      });
      obs.logEvent('router', { route: route.route, reason: route.reason, files: route.files?.map((f) => f.class) });

      if (route.route === 'skip' || route.route === 'light') {
        const md = buildRouterSummary(cfg, { route, pr, headSha, changedPaths });
        finalObs = { exitCode: 0, route: route.route, wouldBlock: false };
        await persistAndCapture(buildReviewJson({
          pr, headSha, reviewMode, route, wouldBlock: false,
          counts: { critical: 0, major: 0, minor: 0, nit: 0 },
          total: 0, findings: [], blockers: [], reviews: [],
          changedPaths,
        }));
        if (dryRun) {
          log(`=== DRY-RUN: router ${route.route} — LLM пропущен ===`);
          log('\n--- SUMMARY MARKDOWN ---\n' + md);
          finalObs = { exitCode: 0, route: route.route, wouldBlock: false, degraded: true };
          return detailedResult(0, { wouldBlock: false, degraded: true, reason: `router ${route.route}` });
        }
        if (postCreds) {
          await github.deleteStaleInline().catch(() => {});
          await github.upsertSticky(md);
          log(`router ${route.route}: sticky обновлён, LLM не вызывался`);
        }
        finalObs = { exitCode: 0, route: route.route, wouldBlock: false, degraded: true };
        return detailedResult(0, { wouldBlock: false, degraded: true, reason: `router ${route.route}` });
      }

      const prepT0 = Date.now();
      // Добавленные файлы контекстом не прикладываем: их дифф И ЕСТЬ полный текст, второй
      // экземпляр оплачивается в каждом вызове каждой роли. На PR из преимущественно новых
      // файлов это половина всего входа. Символы для blast-radius не теряются:
      // collectBlastSymbols разбирает changedText, куда строки нового файла уже попали.
      const addedPaths = limits.skipAddedFileContext ? collectAddedPaths(diff) : new Set();
      const deletedPaths = collectDeletedPaths(diff);
      if (addedPaths.size) log(`контекст: ${addedPaths.size} добавленных файл(ов) — дифф уже содержит их целиком, полный текст не прикладываем`);
      // Сначала читаем файлы: collectBlastSymbols добирает exports из полного текста,
      // а не только из hunk-строк (иначе правка тела функции → 0 symbols → пустой blast).
      const [contentMap, rawPriorFindings, prescanResult] = await Promise.all([
        readChangedFiles(files, limits, log, { addedPaths }),
        (github.readPriorStickyData
          ? github.readPriorStickyData()
          : github.readPriorFindings().then((findings) => ({ findings, runs: [], sha: '' }))
        ).catch((e) => {
          // Молчаливый провал стоил бы сходимости: без prior findings прогон начинает с нуля.
          log(`prior sticky недоступен (${e.message}) — прогон без carry-over`);
          return { findings: [], runs: [], sha: '' };
        }),
        // Удалённым файлам линтер не нужен: их нет в рабочем дереве (RFC 17).
        prescan.run(deletedPaths.size ? changedPaths.filter((p) => !deletedPaths.has(p)) : changedPaths),
      ]);
      const symbols = collectBlastSymbols(changedText, contentMap, limits.maxSymbols);
      log(`файлов: ${files.size} · символов под blast-radius: ${symbols.length}`, symbols);
      // Окна контекста для advisors и жюри (RFC 07): в промпт едут окрестности изменённых
      // строк, а не файлы целиком. Место в потоке — само решение, а не деталь:
      //   ПОСЛЕ collectBlastSymbols — он добирает экспорты из ПОЛНОГО текста, и
      //     `export function` в начале файла не попадает ни в одно окно вокруг хунков в
      //     конце; окно раньше — тихо пустой blast-radius;
      //   ДО fileInputHashes — RFC 05 определяет единицу сравнения как вход модели, и хэш
      //     от окна сам инвалидируется при смене margin (правка configFingerprint,
      //     сбрасывающая состояние целиком, не нужна);
      //   verifier получает по-прежнему ПОЛНУЮ contentMap: он режет сам, вокруг строки
      //     кандидата, и двойная вырезка оставила бы находку вне хунков без контекста
      //     ровно там, где решается блокировать ли merge.
      const ctxWindows = windowChunkFileContents(contentMap, files, {
        margin: limits.contextMargin, minChars: limits.contextWindowMinChars,
      });
      if (ctxWindows.windowed.size) {
        log(`контекст: окна ±${limits.contextMargin} вокруг изменённых строк в ${ctxWindows.windowed.size} файл(ах) из ${contentMap.size} — ${ctxWindows.before}→${ctxWindows.after} симв.`);
      }
      // Freshness измеряем даже если diff не содержит экспортов: иначе `graph_fresh`
      // был бы неизвестен именно в тех review-plans, где blast-radius пустой.
      const blastResult = await graphify.buildBlastRadius(symbols);
      const { blocks: blastBlocks, metrics: blastMetrics, graphIndex } = normalizeBlastRadius(blastResult, symbols);
      log(`prep parallel: ${Date.now() - prepT0}мс (graphify+files+prior+prescan)`);
      // REVIEW_BLAST_REQUIRED: без code graph гейт не видит callers вне диффа → fail-closed
      // ДО LLM (тратить токены на ревью, которое всё равно заблокируется, незачем).
      const blastFailure = gate.blastRequired ? blastUnavailableReason(blastMetrics) : null;
      if (blastFailure) {
        log(`blast-radius недоступен (${blastFailure}) — fail-closed (REVIEW_BLAST_REQUIRED)`);
        obs.logEvent('blast_failed', { reason: blastFailure, required: true, blast_radius: blastMetrics });
        await persistAndCapture(buildReviewJson({
          pr, headSha, reviewMode, route, wouldBlock: true,
          counts: { critical: 0, major: 0, minor: 0, nit: 0 },
          total: 0, findings: [], blockers: [], reviews: [],
          blastMetrics, changedPaths,
        }));
        finalObs = { exitCode: 1, wouldBlock: true, blastFailed: blastFailure, degraded: true };
        if (!dryRun && postCreds) {
          try { await github.upsertSticky(buildBlastUnavailableSummary(blastFailure)); } catch (e) {
            log('заметку не запостили:', String(e?.message || e).slice(0, 200));
          }
        }
        return detailedResult(1, { wouldBlock: true, degraded: true, reason: `blast-radius unavailable: ${blastFailure}` });
      }
      // Fingerprint-инвалидация carry-over ДО LLM: устаревшие prior findings не подмешиваем.
      // Нормализуем форму: клиент github инжектируется (DI-тесты, кастомные провайдеры) и
      // может отдать массив по старому контракту или неполный объект. Ни то, ни другое не
      // должно ронять прогон на первом же обращении к .findings.
      const priorSticky = {
        findings: Array.isArray(rawPriorFindings?.findings) ? rawPriorFindings.findings : [],
        runs: Array.isArray(rawPriorFindings?.runs) ? rawPriorFindings.runs : [],
        sha: typeof rawPriorFindings?.sha === 'string' ? rawPriorFindings.sha : '',
      };
      // Находки прошлого прогона по путям, которые модели теперь не видят, в промпт не идут:
      // жюри без файла не может ни подтвердить, ни снять их — только повторить вслепую.
      const priorFindings = filterStalePriorFindings(priorSticky.findings, files, log)
        .filter((f) => !reviewSkipped.all.has(f?.file));
      if (priorFindings.length !== priorSticky.findings.length) {
        obs.logEvent('prior_findings', { loaded: priorSticky.findings.length, kept: priorFindings.length });
      }

      // Контекст доказуемости для маршрутизации advisory: что уже было в прошлом прогоне,
      // что подтвердил prescan, какого класса файл. Строится ОДИН раз — его читают и
      // verifier-стадия (кого дополнительно проверить), и classify (кого поднять в строку).
      const chunkClasses = (chunk) => [...new Set((chunk?.paths || []).map(fileClass))].sort();
      const actionabilityCtx = {
        priorKeys: new Set(priorFindings.map(findingLocationKey)),
        prescanKeys: new Set(prescanResult.findings.map(findingLocationKey)),
        fileClass,
      };
      const changedFilesSection = buildChangedFilesSection(changedPaths);

      // Инкрементальный прогон (RFC 05): файлы, чей вход для модели не изменился с прошлого
      // раза, заново не ревьюим — их находки переносим. Карта PR остаётся ПОЛНОЙ: чанки
      // увидят остальные файлы как [ANOTHER PART], и запрет absence-claims сработает как
      // обычно. Планируем до buildChunks, чтобы нарезать только то, что действительно идёт в LLM.
      const diffSections = extractFileDiffSections(annotated);
      // Карточки символов (RFC 14, шаг 2): объявления того, что вызывают добавленные строки,
      // если модель их не видит. Место в потоке:
      //   ПОСЛЕ blast-radius — оттуда индекс уже прочитанного графа;
      //   ДО fileInputHashes — карточки входят в хэш входа файла (без номеров строк, см.
      //     symbolCardsHashInput), иначе смена сигнатуры вызываемой функции в другом PR
      //     не заставила бы пере-ревьюить вызывающий файл;
      //   ДО buildChunks — чанк собирает их из своих файлов.
      const symbolCardsOn = Boolean(limits.symbolCards);
      const symbolCards = symbolCardsOn
        ? buildSymbolCards(changedPaths, {
          graphIndex,
          sections: diffSections,
          windows: ctxWindows.map,
          readLines,
          limits: { bodyMaxLines: limits.symbolCardsBodyMaxLines },
          appDir: cfg.profile?.layout?.appDir || '',
        })
        : null;
      if (symbolCards) {
        log(`карточки символов: ${symbolCards.metrics.cards} из ${symbolCards.metrics.candidates} вызванных имён`, symbolCards.metrics.skip_reasons);
      }
      const priorIncremental = cfg.incremental?.enabled
        ? await readIncrementalState(cfg.incremental.stateFile)
        : null;
      // Вход карточек в хэш. Граф недоступен (graphify упал в CI — шаг continue-on-error), а
      // фича включена: карточек нет не потому, что код изменился, — берём вход карточек из
      // прошлого прогона, иначе каждый такой сбой заново ревьюил бы все JS/TS-файлы PR
      // (аудит RFC 14, п. 1.9). Перенесённые находки получены С карточками — это не хуже.
      let cardsInput = symbolCards ? new Map([...symbolCards.byPath].map(([p, cards]) => [p, symbolCardsHashInput(cards)])) : null;
      let cardsTargets = symbolCards
        ? Object.fromEntries([...symbolCards.byPath].map(([p, cards]) => [p, [...new Set(cards.map((c) => c.path))]]))
        : null;
      let cardsInputReused = 0;
      // Переиспользование сужено, чтобы не перенести находку мимо реального изменения:
      //   - вызывающий файл, чья карточка указывает на файл ЭТОГО PR, не переиспользуется —
      //     сигнатура могла измениться в этом же пуше, а без графа мы этого не увидим;
      //   - не больше одного прогона подряд: второй сбой графа подряд пересчитывает честно,
      //     иначе на время сбоя вход карточек застыл бы на неопределённый срок.
      if (symbolCards && !graphIndex && priorIncremental?.cardsInput && !priorIncremental.cardsInputReused) {
        const inPr = new Set(changedPaths);
        const targets = priorIncremental.cardsTargets || {};
        cardsInput = new Map(Object.entries(priorIncremental.cardsInput).filter(([p]) => inPr.has(p)
          && Array.isArray(targets[p]) && !targets[p].some((t) => inPr.has(t))));
        cardsTargets = Object.fromEntries([...cardsInput.keys()].map((p) => [p, targets[p]]));
        cardsInputReused = cardsInput.size;
        if (cardsInputReused) log(`карточки: граф недоступен — вход карточек ${cardsInputReused} файл(ов) взят из прошлого прогона (их карточки не указывают на файлы PR)`);
      }
      const inputHashes = fileInputHashes(changedPaths, diffSections, ctxWindows.map, cardsInput);
      const configFp = configFingerprint({ ...cfg, version: cfg.profile?.version });
      const incrementalPlan = cfg.incremental?.enabled
        ? planIncremental({
          state: priorIncremental,
          hashes: inputHashes,
          changedPaths,
          configFp,
          fullEvery: cfg.incremental.fullEvery,
        })
        : { mode: 'full', reviewPaths: new Set(changedPaths), carried: [], unchanged: [], reason: 'выключено' };
      if (cfg.incremental?.enabled) {
        log(`инкрементальность: ${incrementalPlan.mode} — ${incrementalPlan.reason}`);
        obs.logEvent('incremental', {
          mode: incrementalPlan.mode,
          reason: incrementalPlan.reason,
          reviewed: incrementalPlan.reviewPaths.size,
          unchanged: incrementalPlan.unchanged.length,
          carried: incrementalPlan.carried.length,
          // сколько хэшей посчитано с входом карточек из прошлого прогона (граф недоступен)
          symbolCardsReused: cardsInputReused,
        });
      }
      // Дифф для LLM сужаем до ревьюируемых файлов; `files`/`changedPaths` остаются полными —
      // на них держатся карта PR, отпечатки, гейт и Δ-блок. Сужают двое: инкрементальный план
      // (неизменившиеся файлы) и skip-пути (lockfile/generated/reviewSkip).
      const toLlm = (p) => !reviewSkipped.all.has(p)
        && (incrementalPlan.mode !== 'incremental' || incrementalPlan.reviewPaths.has(p));
      const changedSet = new Set(changedPaths);
      const wantNarrow = incrementalPlan.mode === 'incremental' || reviewSkipped.all.size > 0;
      if (wantNarrow && unmatchedPaths.length) {
        log(`инкрементальность: у ${unmatchedPaths.length} файл(ов) PR нет своей секции в диффе — моделям уходит полный дифф`, unmatchedPaths);
      }
      const narrowLlm = wantNarrow && !unmatchedPaths.length;
      // Секция, чей путь не из changedPaths (преамбула, rename-only, бинарь), не выбрасывается:
      // сужение убирает только то, что точно известно как skip или неизменившееся.
      const annotatedForLlm = narrowLlm
        ? diffByPath.filter((s) => !s.path || !changedSet.has(s.path) || toLlm(s.path)).map((s) => s.text).join('\n')
        : annotated;
      const contextForLlm = narrowLlm
        ? new Map([...ctxWindows.map.entries()].filter(([p]) => toLlm(p)))
        : ctxWindows.map;
      const llmPathCount = changedPaths.filter(toLlm).length;
      if (reviewSkipped.all.size) {
        log(`skip-пути: ${reviewSkipped.all.size} файл(ов) не уходят в LLM (generated/lockfile ${reviewSkipped.builtIn.length}, reviewSkip профиля ${reviewSkipped.byProfile.length})`, [...reviewSkipped.all]);
      }
      // Перенос по skip-пути — находка, сделанная до того, как путь объявили skip:
      // показать её значит снова блокировать merge тем, что модели больше не читают.
      const carriedFindings = incrementalPlan.mode === 'incremental'
        ? markCarried(incrementalPlan.carried, priorIncremental?.headSha || '').filter((f) => !reviewSkipped.all.has(f?.file))
        : [];
      if (carriedFindings.length) log(`перенесено находок по неизменившимся файлам: ${carriedFindings.length}`);

      const chunks = buildChunks(annotatedForLlm, contextForLlm, limits);
      // windowedPaths дописываем ПОСЛЕ нарезки, а не заводим полем в buildChunks: окна
      // ставит pipeline, он же и знает список — Rust-порт нарезки трогать не нужно, и
      // поверхности для молчаливого рассинхрона сторон не появляется. Читатель ровно один —
      // карта PR в промпте: путь с урезанным контекстом метится [THIS PART, CONTEXT WINDOWED]
      // и включает запрет absence-claims даже на PR из одного чанка (RFC 07, решение 3).
      if (ctxWindows.windowed.size) {
        for (const c of chunks) c.windowedPaths = (c.paths || []).filter((p) => ctxWindows.windowed.has(p));
      }
      // Карточки — тем же способом, что windowedPaths: поле чанка, его читает buildMessages.
      let symbolCardsDropped = 0;
      if (symbolCards?.byPath.size) {
        for (const c of chunks) {
          const sel = selectChunkCards(symbolCards.byPath, c.paths, {
            max: limits.symbolCardsMax, maxChars: limits.symbolCardsMaxChars,
            visibleText: `${c.annotated || ''}\n${c.fileContents || ''}`,
          });
          if (sel.cards.length) { c.symbolCards = sel.cards; c.symbolCardNames = sel.names; }
          c.symbolCardsChars = sel.chars;
          c.symbolCardsDropped = sel.dropped;
          symbolCardsDropped += sel.dropped;
        }
      }
      const note0 = [
        chunks.length > 1
          ? `PR большой — разбит на ${chunks.length} частей для полного охвата (каждая ревьюится отдельно).`
          : '',
        formatIncrementalNote(incrementalPlan),
        formatReviewSkippedNote(reviewSkipped),
      ].filter(Boolean).join(' ');
      // Дедлайн LLM-фазы: после него новые чанки пропускаем и постим частичный отчёт —
      // процесс завершается штатно (sticky/export/observability) до убийства job раннером.
      const deadline = limits.timeBudgetMs > 0 ? Date.now() + limits.timeBudgetMs : 0;
      log(`blast-radius блоков: ${blastBlocks.length} · symbols ${blastMetrics.symbols_resolved ?? '—'}/${blastMetrics.symbols_total} · affected ${blastMetrics.affected_nonempty} · graph ${blastMetrics.graph_status} · файлов с контекстом: ${contentMap.size} · чанков: ${chunks.length}`);
      const blastFindings = emitBlastRadiusFindings(blastBlocks, changedPaths, {
        symbols,
        model: BLAST_FINDING_SOURCE,
        dropAmbiguous: cfg.graphify?.dropAmbiguous !== false,
      });
      if (blastFindings.length) log(`blast-radius: +${blastFindings.length} детерминированных cross-layer/community caller(s)`);
      obs.logEvent('plan', {
        chunks: chunks.length,
        // Пути PR, не отданные моделям: lockfile/generated и layout.reviewSkip профиля.
        reviewSkipped: { builtIn: reviewSkipped.builtIn.length, profile: reviewSkipped.byProfile.length },
        symbols: symbols.length,
        blastBlocks: blastBlocks.length,
        models: models.list,
        llmConcurrency: limits.concurrency,
        chunkBytes: limits.chunkBytes,
        blast_radius: blastMetrics,
        // Окна контекста (RFC 07): без этих величин экономия неотличима от того, что файлов
        // в PR просто стало меньше — а решение имеет смысл ровно пока она видна отдельно.
        // Карточки символов (RFC 14): candidates и чанки, упёршиеся в лимит, решают, нужен ли
        // ранкер (шаг 3); null — фича выключена.
        symbolCards: symbolCards ? {
          ...symbolCards.metrics,
          dropped: symbolCardsDropped,
          // граф недоступен: вход карточек для хэша взят из прошлого прогона (аудит, п. 1.9)
          hash_input_reused: cardsInputReused,
          chunks_over_limit: chunks.filter((c) => c.symbolCardsDropped > 0).length,
        } : null,
        contextWindows: {
          margin: limits.contextMargin,
          minChars: limits.contextWindowMinChars,
          files: ctxWindows.windowed.size,
          before: ctxWindows.before,
          after: ctxWindows.after,
        },
        // Состав каждого чанка: без него «прогон стоил N токенов» не разбирается на
        // слагаемые — не видно, что вход раздул контекст файлов, а не дифф, и что
        // часть чанков вообще не содержит кода.
        chunkPlan: chunks.map((c, i) => ({
          i,
          diff_chars: c.annotated?.length ?? 0,
          context_chars: c.fileContents?.length ?? 0,
          windowed_paths: c.windowedPaths?.length ?? 0,
          symbol_cards: c.symbolCards?.length ?? 0,
          symbol_cards_chars: c.symbolCardsChars ?? 0,
          symbol_card_names: c.symbolCardNames ?? [],
          paths: c.paths?.length ?? 0,
          classes: chunkClasses(c),
        })),
      });

      if (priorFindings.length) log(`прошлых находок подгружено: ${priorFindings.length} (сходимость)`);
      log(`prescan R6: ${formatPrescanLog(prescanResult)}`);
      obs.logEvent('prescan', {
        findings: prescanResult.findings.length,
        scanned: prescanResult.scanned,
        skipped: prescanResult.skipped,
        truncated: prescanResult.truncated || false,
        lintableTotal: prescanResult.lintableTotal,
        byTool: prescanResult.byTool,
        error: prescanResult.error,
        eslintError: prescanResult.eslint?.error,
        astgrepError: prescanResult.astgrep?.error,
        sample: slimFindings(prescanResult.findings),
      });

      log(`режим: ${reviewMode}${reviewMode === 'moa'
        ? ` · ${specialists.enabled ? `specialists: ${specialists.assignments.map((a) => `${a.role}=${a.model}`).join(', ')}` : `advisors: ${moa.references.join(', ')}`} · aggregator: ${moa.aggregator}` : ''} · модели: ${models.list.join(', ')} · порог: ${gate.consensusMin}`);

      // Инкрементальный прогон, в котором не изменился ни один файл: ревьюить нечего, весь
      // отчёт собирается из перенесённых находок. Это не аутедж LLM — ветку fail-closed
      // ниже он не поднимает, иначе «ничего не поменялось» блокировало бы merge.
      // То же, когда все оставшиеся пути — skip (lockfile/generated/reviewSkip): при выключенном
      // роутере PR из одних таких путей доходит сюда, а модели в нём читать нечего.
      const nothingToReview = (incrementalPlan.mode === 'incremental' && incrementalPlan.reviewPaths.size === 0)
        || (reviewSkipped.all.size > 0 && llmPathCount === 0);
      if (nothingToReview) {
        log(incrementalPlan.mode === 'incremental' && incrementalPlan.reviewPaths.size === 0
          ? 'инкрементальность: изменившихся файлов нет — LLM не вызывается'
          : 'все изменившиеся файлы — skip-пути (generated/lockfile/reviewSkip) — LLM не вызывается');
      }
      const reviews = nothingToReview ? [] : await engine.reviewAllModels(chunks, {
        // changedPaths, а не готовая секция: карту PR («что в этом чанке, что ревьюится
        // отдельно») движок рисует под каждый чанк сам. changedFilesSection остаётся для
        // verifier-стадии — у неё контекст всего PR.
        blastBlocks, pr, priorFindings, changedPaths, prescanSection: prescanResult.promptSection,
        deadline,
        // Кому из чанков нужен полный состав специалистов (RFC 04): класс пути и
        // чувствительные пути знает эта сторона — профиль и мемоизация живут здесь.
        chunkRouting: { fileClass, sensitive: cfg.profile?.layout?.sensitive || null },
      });
      // max, а не сумма: в consensus-режиме один пропущенный чанк считается у каждой модели.
      const skippedChunks = reviews.reduce((n, r) => Math.max(n, r.skippedChunks || 0), 0);
      if (skippedChunks) {
        log(`⏱ тайм-бюджет: ${skippedChunks} из ${chunks.length} чанков пропущены — отчёт частичный`);
        obs.logEvent('time_budget', { skippedChunks, totalChunks: chunks.length, budgetMs: limits.timeBudgetMs });
      }
      const note = [note0, skippedChunks
        ? `⏱ Тайм-бюджет исчерпан: ${skippedChunks} из ${chunks.length} частей не ревьюились — охват частичный, перезапустите ревью.`
        : ''].filter(Boolean).join(' ');
      const ok = reviews.filter((r) => !r.error);
      // Покрытие по чанкам (RFC 19). Чанк покрыт, если его отревьюила хоть одна модель.
      // Агрегат собирается из успешных чанков, поэтому без этого счёта «7 из 10 частей»
      // выглядело как полный прогон, а файлы упавших частей попадали в инкрементальный state
      // как просмотренные — и не ревьюились больше никогда, пока не изменятся.
      const chunkCovered = (ci) => ok.some((r) => !(r.failedChunks || []).includes(ci)
        && !(r.skippedChunkIndexes || []).includes(ci));
      const uncoveredChunks = ok.length ? chunks.map((_, ci) => ci).filter((ci) => !chunkCovered(ci)) : [];
      // Сбой, а не тайм-бюджет: хоть одна модель упала на этом чанке. Перезапуск лечит —
      // поэтому это degraded по REVIEW_LLM_REQUIRED; пропуск по тайм-бюджету — заметка (см. note).
      const failedUncovered = uncoveredChunks.filter((ci) => ok.some((r) => (r.failedChunks || []).includes(ci)));
      const uncoveredPaths = new Set(uncoveredChunks.flatMap((ci) => chunks[ci]?.paths || []));
      if (failedUncovered.length) {
        log(`покрытие: ${failedUncovered.length} из ${chunks.length} частей не отревьюены из-за сбоя модели`, [...new Set(failedUncovered.flatMap((ci) => chunks[ci]?.paths || []))]);
        obs.logEvent('coverage_gap', {
          failed: failedUncovered.length,
          total: chunks.length,
          paths: [...new Set(failedUncovered.flatMap((ci) => chunks[ci]?.paths || []))].slice(0, 20),
        });
      }
      // Находка модели о skip-пути сделана вслепую (файл в чанк не попадал, модель видела
      // только его имя в карте PR) — в отчёт не идёт.
      const llmFindingsAll = ok.flatMap((r) => r.findings);
      const llmFindings = reviewSkipped.all.size
        ? llmFindingsAll.filter((f) => !reviewSkipped.all.has(f?.file))
        : llmFindingsAll;
      if (llmFindings.length !== llmFindingsAll.length) {
        log(`skip-пути: снято ${llmFindingsAll.length - llmFindings.length} находок модели о файлах, которых она не видела`);
      }
      let allFindings = [
        ...prescanResult.findings,
        ...blastFindings,
        ...dedupeAgainstPrescan(llmFindings, prescanResult.findings),
      ];
      logFindingsStage(obs, 'after_llm_merge', allFindings);
      // Дерево репозитория для FP-семейства 7 («файла нет» о файле вне PR). null — дерево
      // не читается или чужое (см. buildRepoPathIndex): фильтр тогда — только паттерны.
      const repoIndex = allFindings.length
        ? buildRepoPathIndex(await Promise.resolve().then(repoFiles).catch(() => null), changedPaths)
        : null;
      // Выключенный фильтр не должен быть невидимым: иначе «защита не работала» не отличить
      // от «ложных находок не было».
      if (allFindings.length && !repoIndex) log('FP-семейство 7 выключено: дерево репозитория не прочитано или чужое');

      if (!ok.length && !nothingToReview && (gate.llmRequired || !allFindings.length)) {
        const why = reviews.map((r) => `${r.model}: ${r.error}`).join('; ');
        const whyPublic = reviews.map((r) => `${r.model}: ${publicErrorLabel(r.error)}`).join('; ');
        log('все модели недоступны:', why, gate.llmRequired ? '— fail-closed (REVIEW_LLM_REQUIRED)' : '— fail-open');
        // Prescan-находки (eslint/ast-grep) не зависят от LLM — не теряем их. Прогоняем
        // LLM-независимые стадии (FP-фильтр, resolveBlockers с prescan.blockEnabled/blockRules);
        // refutation/verifier — LLM-стадии, при полном аутедже недоступны по определению.
        const gateResolution = await runGateResolution({
          findings: allFindings, changedPaths, gate, prescanCfg: cfg.prescan,
          patternsFile: cfg.feedback?.patternsFile, repoIndex, log,
        });
        allFindings = gateResolution.findings;
        // Отпечатки до экспорта: артефакт этой ветки (LLM-аутедж, находки только от prescan)
        // тоже уходит потребителю и тоже должен дедуплицироваться по коду, а не по заголовку.
        attachFingerprints(allFindings, files);
        const prescanBlockers = gateResolution.blockers;
        const fpRemoved = gateResolution.fpRemoved;
        if (fpRemoved.length) {
          log(`ложных находок отфильтровано: ${fpRemoved.length}`);
        }
        obs.logEvent('fp_filter', {
          before: gateResolution.beforeFpCount,
          after: gateResolution.afterFpCount,
          removed: fpRemoved.length,
        });
        logFindingsStage(obs, 'after_fp_filter', allFindings);
        const counts = countBySeverity(allFindings);
        const gateBlocked = gate.llmRequired || prescanBlockers.length > 0;
        obs.logEvent('llm_failed', { prescanOnly: true, required: gate.llmRequired });
        obs.logEvent('gate', {
          wouldBlock: gateBlocked,
          blockers: prescanBlockers.length,
          sample: slimFindings(prescanBlockers),
        });
        logFindingsStage(obs, 'after_gate', allFindings);
        await persistAndCapture(buildReviewJson({
          pr, headSha, reviewMode, route,
          wouldBlock: gateBlocked,
          counts, total: allFindings.length, findings: allFindings,
          blockers: prescanBlockers, reviews: [],
          llmFailed: true,
          llmErrors: reviews.map((r) => ({ model: r.model, error: String(r.error || '').slice(0, 200) })),
          blastMetrics, changedPaths,
        }));
        if (dryRun) {
          finalObs = {
            exitCode: gateBlocked ? 1 : 0,
            llmFailed: true,
            dryRun: true,
            wouldBlock: gateBlocked,
            blockers: prescanBlockers.length,
            total: allFindings.length,
            counts,
            degraded: true,
          };
          return detailedResult(finalObs.exitCode, {
            wouldBlock: gateBlocked,
            degraded: true,
            reason: 'llm unavailable',
          });
        }
        const prescanBlockerSet = new Set(prescanBlockers);
        const prescanAdvisoryFindings = allFindings.filter((f) => f.prescan && !prescanBlockerSet.has(f));
        const body = buildLlmUnavailableSummary({
          llmRequired: gate.llmRequired,
          whyPublic,
          models: models.list,
          prescanBlockers,
          prescanAdvisoryFindings,
        });
        const { inline } = findingsFmt.classify(prescanBlockers, files);
        try {
          const pub = await publishInline(github, {
            inline, headSha, maxInline: limits.maxInline, cleanupWithoutPost: true,
          }, log);
          log(`fail-closed: запощено ${pub.posted}/${pub.total} prescan inline, очищено старых ${pub.cleared}`);
        } catch (e) {
          log('fail-closed: постинг/чистка inline упали (не критично):', String(e?.message || e).slice(0, 200));
        }
        try { await github.upsertSticky(body); } catch (e2) {
          log('и заметку не запостили:', String(e2?.message || e2).slice(0, 200));
        }
        // При неблокирующем fail-open не пингуем дежурного: sticky сохраняет диагноз без спама.
        if (gateBlocked) {
          await telegramNotifier(cfg, log, {
            pr, counts, total: allFindings.length, htmlUrl: pr.html_url,
            reason: 'fail-closed: LLM outage',
          });
        }
        finalObs = {
          exitCode: gateBlocked ? 1 : 0,
          llmFailed: true,
          wouldBlock: gateBlocked,
          blockers: prescanBlockers.length,
          total: allFindings.length,
          counts,
          degraded: true,
        };
        return detailedResult(finalObs.exitCode, {
          wouldBlock: gateBlocked,
          degraded: true,
          reason: 'llm unavailable',
        });
      }
      if (!ok.length && allFindings.length) {
        log('LLM недоступен — продолжаем с prescan-only findings (R6), REVIEW_LLM_REQUIRED=0');
        obs.logEvent('llm_failed', { prescanOnly: true, required: gate.llmRequired });
      }

      // FP-фильтр идёт ДО LLM-стадий (refutation/verifier): иначе verifier тратит токены
      // на известные ложные находки, а refutation может понизить severity уже отфильтрованного.
      const gateResolution = await runGateResolution({
        findings: allFindings, changedPaths, gate, prescanCfg: cfg.prescan,
        patternsFile: cfg.feedback?.patternsFile, repoIndex, log,
        beforeResolve: async (fpFilteredFindings) => {
          let currentFindings = fpFilteredFindings;

          // Refutation/verifier — LLM-стадии; при полном LLM-аутедже (ok.length === 0) пропускаем,
          // чтобы fail-closed и fall-through пути вели себя симметрично.
          let refutationStats = null;
          if (specialists.enabled && ok.length) {
            const applied = applyRefutation(currentFindings, log, {
              strict: true, blockOn: gate.blockOn, skipBlast: gate.blastVerify !== '0',
            });
            currentFindings = applied.findings;
            refutationStats = applied.stats;
            log(`refutation R5: ${formatRefutationLog(refutationStats)}`);
            obs.logEvent('refutation', refutationStats);
            logFindingsStage(obs, 'after_refutation', currentFindings);
          }

          const verifierStage = ok.length
            ? await runVerifierStage(currentFindings, {
                engine, gate, verifier, deadline, contentMap, annotated, blastBlocks, pr, changedFilesSection,
                actionability: cfg.actionability,
                actionabilityCtx,
                files,
                symbolCardsByPath: symbolCards?.byPath ?? null,
                limits,
                readOutside,
              }, log, obs)
            : { findings: currentFindings, stats: null, unavailable: null };
          currentFindings = verifierStage.findings;
          const verifierStats = verifierStage.stats;
          const verifierUnavailable = verifierStage.unavailable;

          // Перенесённые находки подмешиваем ПОСЛЕ refutation и verifier — намеренно.
          // Обе стадии они уже прошли в том прогоне, где были найдены, и принесли свои
          // вердикты с собой. Пропусти их здесь ещё раз — и strict-refutation понизила бы
          // подтверждённый блокер до minor только потому, что в ЭТОМ прогоне жюри не
          // высказывалось: файл-то не менялся и никому не показывался.
          if (carriedFindings.length) {
            currentFindings = [...currentFindings, ...carriedFindings];
            logFindingsStage(obs, 'after_carry_over', currentFindings);
          }

          return { findings: currentFindings, refutationStats, verifierStats, verifierUnavailable };
        },
      });
      allFindings = gateResolution.findings;
      // Один расчёт отпечатков на прогон — до того, как набор разойдётся по футеру sticky,
      // Δ-блоку и артефакту экспорта: все трое обязаны показывать одну величину.
      attachFingerprints(allFindings, files);
      const blockerClusters = gateResolution.clusters.filter((g) => g.some((f) => f.consensusBlock));
      // RFC 21, shadow: что blast-находки заблокировали бы — материал для замера перед дефолтом.
      const blastWouldBlock = allFindings.filter((f) => f.wouldBlockBlast);
      if (blastWouldBlock.length) {
        log(`blast-verify shadow: ${blastWouldBlock.length} blast-находок заблокировали бы merge (REVIEW_BLAST_VERIFY=shadow)`);
        for (const f of blastWouldBlock) {
          obs.logEvent('blast_would_block', {
            file: f.file, line: f.line, title: String(f.title || '').slice(0, 200),
            verifierReason: String(f.verifierReason || '').slice(0, 300),
          });
        }
      }
      const blockers = gateResolution.blockers;
      const fpRemoved = gateResolution.fpRemoved;
      const refutationStats = gateResolution.refutationStats;
      const verifierStats = gateResolution.verifierStats;
      // Verifier не дал вердикта по находкам уровня блока из-за сбоя (RFC 18). Сами находки
      // уже понижены и блокерами не считаются; блокирует ли прогон — как при аутедже LLM:
      // REVIEW_LLM_REQUIRED=1 (дефолт) — degraded-блок до перезапуска, 0 — fail-open.
      const verifierUnavailable = gateResolution.verifierUnavailable
        ? { ...gateResolution.verifierUnavailable, blocked: Boolean(gate.llmRequired) }
        : null;
      // Части PR, не отревьюенные из-за сбоя модели (RFC 19), — тот же рычаг.
      const coverageGap = failedUncovered.length
        ? { failed: failedUncovered.length, total: chunks.length, blocked: Boolean(gate.llmRequired) }
        : null;
      const degradedBlock = Boolean(verifierUnavailable?.blocked || coverageGap?.blocked);
      const degradedWhy = [
        verifierUnavailable ? `verifier недоступен (${verifierUnavailable.reason}), не проверено ${verifierUnavailable.unverified}` : '',
        coverageGap ? `не отревьюено ${coverageGap.failed} из ${coverageGap.total} частей PR (сбой модели)` : '',
      ].filter(Boolean).join('; ');
      const wouldBlock = blockers.length > 0 || degradedBlock;
      if (fpRemoved.length) {
        log(`ложных находок отфильтровано: ${fpRemoved.length}`);
      }
      obs.logEvent('fp_filter', { before: gateResolution.beforeFpCount, after: gateResolution.afterFpCount, removed: fpRemoved.length });
      logFindingsStage(obs, 'after_fp_filter', allFindings);
      obs.logEvent('gate', {
        wouldBlock,
        blockers: blockers.length,
        ...(verifierUnavailable ? { verifierUnavailable } : {}),
        ...(coverageGap ? { coverageGap } : {}),
        sample: slimFindings(blockers),
      });
      logFindingsStage(obs, 'after_gate', allFindings);

      const { inline, outside } = findingsFmt.classify(allFindings, files, actionabilityCtx);
      // Карточки → находки (аудит RFC 14, п. 1.5) и датчик «символ не определён» по телам —
      // пишется и при выключенных карточках: это база для сравнения.
      obs.logEvent('symbol_cards_outcome', symbolCards
        ? { enabled: true, ...symbolCardsOutcome({ chunks, llmFindings, finalFindings: allFindings, inline }) }
        : { enabled: false, llm: { findings: llmFindings.length, absenceClaims: llmFindings.filter(isAbsenceClaim).length } });
      logActionabilityDistribution(obs, {
        findings: allFindings,
        inlineCount: inline.length,
        outside,
        threshold: cfg.actionability?.threshold,
      });
      const { md, counts, total, delta, verdict } = buildSummary(cfg, {
        reviews, ok, allFindings, outside, blastBlocks, blastMetrics, files, pr, headSha, note, blockerClusters,
        verifierStats, refutationStats, prescanResult, fpRemoved, verifierUnavailable, coverageGap,
        // Δ считаем против ПОЛНОГО набора прошлого прогона, а не против отфильтрованного
        // carry-over: «закрыто» — это то, что ушло из отчёта, включая снятое инвалидацией.
        priorFindings: priorSticky.findings, priorRuns: priorSticky.runs, priorSha: priorSticky.sha,
        costSnapshot: costTracker.snapshot(),
      });
      // Дисперсия набора находок — измеримая величина, а не впечатление от чтения sticky:
      // в JSONL уходит Jaccard по точкам кода и его слагаемые, по ним недельный отчёт строит
      // тренд (бэклог P1 #8). Размеры списков Δ-блока — в `delta.findings`, они считаются по
      // другому ключу (находка = точка + формулировка) и метрику не задают.
      obs.logEvent('stability', delta);
      // Вердикт со sha — отдельным событием: `gate` не знает коммита, а без него нельзя отличить
      // «на разных диффах решения разные» (норма) от «на одном диффе решения разные» (гейт
      // недетерминирован, замер #533: блокер в 4 прогонах из 6 при неизменном диффе).
      obs.logEvent('verdict', verdict);

      await persistAndCapture(buildReviewJson({
        pr, headSha, reviewMode, route, wouldBlock, verdict,
        counts, total, findings: allFindings, blockers, reviews,
        verifierStats, refutationStats, prescanResult,
        blastMetrics, changedPaths,
      }));

      // Состояние для следующего прогона. Пишем итоговый набор — тот, что виден человеку
      // (после FP-фильтра, refutation, verifier и гейта), потому что именно его придётся
      // переносить. Prescan и blast-radius в перенос не идут: они детерминированы, дёшевы
      // и пересчитываются на каждом прогоне заново.
      if (cfg.incremental?.enabled) {
        // Находка, до которой verifier не дошёл (сбой — RFC 18, кап — RFC 17), не переносится,
        // а её файл не получает хэш: перенос считает находку прошедшей verifier и заново её не
        // проверяет. Иначе «перезапустите ревью» после degraded-блока переносил бы непроверенный
        // critical как minor без LLM и без verifier — и merge проходил бы.
        // То же для файлов частей PR, которые не отревьюила ни одна модель (сбой или тайм-бюджет,
        // RFC 19): без хэша следующий прогон ревьюит их, а не считает просмотренными.
        const unverifiedPaths = new Set([
          ...allFindings
            .filter((f) => f.verifierStatus === 'unavailable' || f.verifierStatus === 'overflow')
            .map((f) => f.file),
          ...uncoveredPaths,
        ]);
        const carryable = allFindings.filter((f) => !f.prescan && f.source !== BLAST_FINDING_SOURCE
          && !unverifiedPaths.has(f.file));
        if (unverifiedPaths.size) log(`инкрементальность: ${unverifiedPaths.size} файл(ов) с непроверенными verifier'ом находками — в следующем прогоне ревьюятся заново`);
        await writeIncrementalState(cfg.incremental.stateFile, {
          pr: pr.number,
          headSha,
          configFp,
          // Счётчик прогонов подряд без полного: страховка от накопления дрейфа.
          sinceFull: incrementalPlan.mode === 'incremental' ? Number(priorIncremental?.sinceFull || 0) + 1 : 0,
          hashes: unverifiedPaths.size
            ? Object.fromEntries(Object.entries(inputHashes).filter(([p]) => !unverifiedPaths.has(p)))
            : inputHashes,
          // Вход карточек по файлам — чтобы сбой графа в следующем прогоне не менял хэши.
          ...(cardsInput?.size ? { cardsInput: Object.fromEntries(cardsInput), cardsTargets } : {}),
          ...(cardsInputReused ? { cardsInputReused: true } : {}),
          findings: carryable,
        }, log);
      }

      if (dryRun) {
        log('=== DRY-RUN: вывод не постится ===');
        log(`модели-ок: ${ok.map((r) => r.model).join(', ')} · findings=${total} · inline=${inline.length} · outside=${outside.length}`);
        log(blockers.length
          ? `⛔ WOULD BLOCK merge: ${blockers.length} кластер(ов) × ${gate.blockOn.join('/')}`
          : degradedBlock
            ? `⛔ WOULD BLOCK merge (degraded): ${degradedWhy}`
            : `✅ не блокирует (${reviewMode === 'moa' ? 'MoA-aggregator' : `консенсус ≥${gate.consensusMin}`})`);
        log('\n--- INLINE COMMENTS ---');
        for (const c of inline) log(`${c.path}:${c.line}\n  ${c.body.split('\n').slice(0, 2).join(' / ')}`);
        log('\n--- SUMMARY MARKDOWN ---\n' + md);
        const exitCode = wouldBlock ? 1 : 0;
        finalObs = {
          exitCode, dryRun: true, wouldBlock, blockers: blockers.length, total, counts, inline: inline.length,
          ...(degradedBlock ? { degraded: true } : {}),
        };
        return detailedResult(exitCode, { wouldBlock, ...(degradedBlock ? { degraded: true, reason: degradedWhy } : {}) });
      }

      const pub = await publishReview(github, { inline, md, headSha, maxInline: limits.maxInline }, log);
      log(`review запощен (${pub.posted}/${pub.total} inline, очищено старых ${pub.cleared}) · summary ${pub.op}`);

      if (cfg.triage?.enabled) {
        try {
          const triageCtx = buildTriageContext({
            allFindings,
            blockers,
            changedPaths,
            headSha,
            blockOn: gate.blockOn,
            patternsFile: cfg.feedback?.patternsFile,
            trustedAuthors: parseTrustedAuthors(cfg.feedback?.trustedAuthors),
          });
          const triageStats = await runAutoTriage(github, triageCtx, log);
          obs?.logEvent('auto_triage', triageStats);
          if (triageStats.resolved) log(`auto-triage: resolved ${triageStats.resolved} thread(s)`);
        } catch (e) {
          log('auto-triage failed (fail-open):', e.message);
          obs?.logEvent('auto_triage_error', { error: String(e.message || e).slice(0, 200) });
        }
      }

      await telegramNotifier(cfg, log, { pr, counts, total, htmlUrl: pr.html_url });

      if (blockers.length) {
        log(`⛔ merge ЗАБЛОКИРОВАН: ${blockers.length} кластер(ов) уровня ${gate.blockOn.join('/')}`);
        finalObs = { exitCode: 1, wouldBlock: true, blockers: blockers.length, total, counts, inline: inline.length };
        return detailedResult(1, { wouldBlock: true });
      }
      if (degradedBlock) {
        log(`⛔ merge ЗАБЛОКИРОВАН (degraded): ${degradedWhy} — перезапустите ревью`);
        finalObs = { exitCode: 1, wouldBlock: true, degraded: true, blockers: 0, total, counts, inline: inline.length };
        return detailedResult(1, { wouldBlock: true, degraded: true, reason: degradedWhy });
      }
      log(`готово — блокеров нет (${reviewMode}).`);
      finalObs = { exitCode: 0, wouldBlock: false, blockers: 0, total, counts, inline: inline.length };
      return detailedResult(0, { wouldBlock: false });
    } catch (e) {
      if (e?.name === 'PipelineSoftFail') {
        finalObs = {
          exitCode: 0,
          degraded: true,
          error: String(e.message || e).slice(0, 200),
        };
        return detailedResult(0, { wouldBlock: false, degraded: true, reason: e.message });
      }
      finalObs = {
        exitCode: 0,
        crashed: true,
        error: String(e?.message || e).slice(0, 200),
      };
      throw e;
    } finally {
      await finishObs(obs, finalObs);
    }
  }

  return { run, runDetailed };
}

/** CLI: загрузка конфига и запуск. */
export async function runReview(env = process.env, argv = process.argv.slice(2)) {
  const cfg = loadReviewConfig(env, parseCli(argv));
  const pipeline = createReviewPipeline(cfg);
  try {
    process.exit(await pipeline.run());
  } catch (e) {
    console.error('[pr-review]', e.stack || e.message);
    process.exit(0);
  }
}
