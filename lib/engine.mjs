/**
 * Фаза 3, шаг 2: lib/engine.mjs — оркестрация MoA/consensus поверх Rust-бинаря
 * rust/target/debug/engine (порт чистых частей: скоуп prior findings по чанку,
 * демоут находок про невидимые чанку файлы, агрегация ответов модели по чанкам
 * с verdict-ранком и дедупом внутри модели).
 *
 * Почему оркестрация остаётся в JS (вариант (b) из постановки):
 *   - DI-моки (test/pr-review.engine.test.mjs, pipeline deps) подменяют ollama
 *     и prompt — они должны остаться внутри процесса Node; Rust-ядро получает
 *     только JSON-результаты вызовов;
 *   - адаптивные семафоры/троттлинг 429 живут в lib/llm.mjs (JS) — каждый
 *     LLM-вызов и так идёт через Rust-бинарь llm (Фаза 3, шаг 1);
 *   - дедлайн бюджета — Date.now() оркестратора: skip-метки TIME_SKIP ставит
 *     JS до вызова модели (после дедлайна LLM не дёргается вовсе);
 *   - fail-open/fail-closed семантика (runGateResolution в pipeline) завязана
 *     на то, что ошибки моделей возвращаются результатами, а не бросаются.
 *
 * Rust получает три батч-операции (один spawn на прогон частей, а не на чанк):
 *   planChunks  {priorFindings, chunks}            → scoped priors на чанк
 *   demote      {items: [{res, chunkIndex}], chunks} → демоут findings чанка
 *   aggregate   {done, models}                     → aggregateModel на модель
 *                                                    (+ dropped для лога дедупа)
 *
 * Резолвинг бинаря (по аналогии с lib/findings.mjs):
 *   AI_REVIEWER_ENGINE_BIN — override; по умолчанию <repo>/rust/target/debug/engine;
 *   AI_REVIEWER_ENGINE_FORCE_JS=1 — принудительно чистый JS (паритет/отладка).
 * Бинаря нет (CI до cargo build) — эталонный JS fallback ниже; бинарь битый —
 * ошибка пробрасывается (молчаливая подмена скрывала бы регрессии).
 * Паритет проверяется rust/parity/engine.parity.mjs.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { mapLimit, redactSecretsForLog } from './util.mjs';
import { VERDICT_RANK } from './constants.mjs';
import { mergeSpecialistReferences } from './specialists.mjs';
import { normalizeFinding, dedupeSameModelFindings } from './findings.mjs';
import { createVerifierRunner } from './verifier.mjs';
import { chunkNeedsSpecialists, formatChunkRoutingLog } from './chunk-routing.mjs';
import { shouldMergeSpecialists, mergedTextToReferences } from './specialist-merge.mjs';
import { sharedPrefixAdvisor, sharedPrefixAggregator, sharedPrefixChars } from './prompt-layout.mjs';
import { buildMergedSpecialistSystem } from './prompt.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_ENGINE_BIN = resolve(__dir, '../rust/target/debug/engine');

function resolveEngineBin() {
  if (process.env.AI_REVIEWER_ENGINE_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_ENGINE_BIN || '').trim() || DEFAULT_ENGINE_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; undefined — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveEngineBin();
  if (!bin) return undefined;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

/** Маркер пропуска чанка по тайм-бюджету (не путать с недоступностью модели — fallback не нужен). */
const TIME_SKIP = 'тайм-бюджет прогона исчерпан';
const deadlineHit = (ctx) => Boolean(ctx?.deadline) && Date.now() > ctx.deadline;

/**
 * Размер промпта в символах — детерминированный прокси токенов, доступный ДО вызова
 * и независимый от того, вернул ли провайдер usage. Нужен, чтобы отвечать на вопрос
 * «какая роль и какой чанк съели вход», а не только «сколько всего потрачено».
 */
const messagesChars = (messages) => (Array.isArray(messages) ? messages : [])
  .reduce((n, m) => n + String(m?.content ?? '').length, 0);

/**
 * Метрики вызова для observability: сколько входа и какая его доля лежит в общем префиксе.
 * `cache_prefix_chars` отвечает на вопрос «сколько провайдер МОГ БЫ отдать из кэша» —
 * без него нельзя отличить «раскладка включена» от «раскладка приносит пользу».
 */
const promptMeta = (messages, layout) => (layout === 'shared-prefix'
  ? { prompt_chars: messagesChars(messages), cache_prefix_chars: sharedPrefixChars(messages) }
  : { prompt_chars: messagesChars(messages) });

/**
 * Состав чанка известен, только если buildChunks посчитал его целиком: пустой список или
 * секция с неопознанным путём (`unknownPaths`) означают «состав неполный» — тогда ни скоуп
 * priors, ни понижение находок применять нельзя, иначе видимый файл сочтут невидимым.
 */
const chunkScope = (chunk) => (chunk?.paths?.length && !chunk.unknownPaths ? new Set(chunk.paths) : null);

/**
 * Prior findings — только по файлам ЭТОГО чанка. Иначе промпт требует «проверь по текущему коду:
 * исправлено → не повторяй» для файла, которого в чанке нет: проверить нечем, и безопасный для
 * модели ход — повторить находку. Так закрытая находка возвращается прогон за прогоном.
 */
function scopePriorsJs(priorFindings, chunk) {
  const scope = chunkScope(chunk);
  if (!scope || !priorFindings?.length) return priorFindings;
  return priorFindings.filter((f) => !f.file || scope.has(f.file));
}

/** scopePriors для всех чанков одним вызовом бинаря (null в ответе — «без изменений»). */
function planAllPriors(chunks, priorFindings) {
  const r = runRust('planChunks', { priorFindings: priorFindings ?? null, chunks });
  if (r === undefined) return null;
  return r.scoped.map((s) => (s === null ? priorFindings : s));
}

/**
 * Детерминированная страховка поверх промпта: находка про файл, которого чанк не видит, не может
 * быть доказана — понижаем до nit и метим. Промпт просит, эта функция гарантирует: слепой
 * блокер merge невозможен. Файл, который есть в чанке хотя бы частично, не трогаем.
 */
function demoteOutOfChunkJs(res, chunk, chunkIndex) {
  const scope = chunkScope(chunk);
  if (!scope || !res?.findings?.length) return res;
  let demoted = 0;
  const findings = res.findings.map((f) => {
    if (!f.file || scope.has(f.file) || f.severity === 'nit') return f;
    demoted++;
    return { ...f, severity: 'nit', outOfChunk: true, confidence: Math.min(f.confidence ?? 0.5, 0.5) };
  });
  return demoted ? { ...res, findings, demotedOutOfChunk: (res.demotedOutOfChunk || 0) + demoted, chunkIndex } : res;
}

/** demoteOutOfChunk для всех результатов одним вызовом бинаря. */
function demoteAll(done, chunks) {
  const r = runRust('demote', {
    items: done.map((d) => ({ res: d.res, chunkIndex: d.ci })),
    chunks,
  });
  if (r === undefined) {
    return done.map((d) => ({ model: d.model, res: demoteOutOfChunkJs(d.res, chunks[d.ci], d.ci) }));
  }
  return done.map((d, i) => ({ model: d.model, res: r.items[i] }));
}

/** aggregateModel для списка моделей одним вызовом бинаря (+ лог дедупа, как у findings). */
function aggregateAll(models, done) {
  const r = runRust('aggregate', { done, models });
  if (r === undefined) return models.map((m) => aggregateModelJs(m, done));
  r.dropped.forEach((d) => {
    if (d > 0) logRef(`дедуп повторов внутри модели: ${d} (одна тема из нескольких чанков)`);
  });
  return r.results;
}

/** Эталонная копия aggregateModel (fallback при отсутствии бинаря). */
function aggregateModelJs(model, done) {
  const parts = done.filter((d) => d.model === model).map((d) => d.res);
  const okParts = parts.filter((p) => !p.error);
  if (!okParts.length) {
    const allSkipped = parts.length > 0 && parts.every((p) => p.skipped);
    return { model, error: parts[0]?.error || 'нет ответа', ...(allSkipped ? { skipped: true } : {}) };
  }
  const verdict = okParts.map((p) => p.verdict)
    .sort((a, b) => (VERDICT_RANK[b] ?? 0) - (VERDICT_RANK[a] ?? 0))[0];
  return {
    model,
    // На многочанковых PR склейка summary всех чанков — мусор; chunk 0 видит
    // changed-files секцию и даёт обзорное summary — берём первое непустое.
    summary: okParts.map((p) => p.summary).filter(Boolean)[0] || '',
    verdict,
    // Дедуп ВНУТРИ одной модели: одна и та же находка, пришедшая из разных чанков, —
    // это дубль, а не подтверждение. Между моделями не дедупим: там совпадение и есть
    // сигнал консенсуса (clusterFindings считает модели по кластеру).
    findings: dedupeSameModelFindings(okParts.flatMap((p) => p.findings), logRef),
  };
}

// Лог дедупа агрегата: closure createReviewEngine ещё не создана на момент
// определения helpers — реальный log подставляется в createReviewEngine.
let logRef = () => {};

/** MoA + consensus: прогон моделей по чанкам, fallback при сбое primary. */
export function createReviewEngine(cfg, { ollama, prompt, log, observability, costTracker } = {}) {
  // log для helpers модуля (батч-агрегат печатает дедуп той же строкой, что
  // dedupeSameModelFindings — см. aggregateAll выше).
  logRef = typeof log === 'function' ? log : () => {};
  // costTracker deprecated: учёт только в createLlmClient. Параметр оставлен + warn
  // (console.warn, если log не передали), чтобы silent no-op не маскировал устаревший вызов.
  if (costTracker != null) {
    const warn = typeof log === 'function' ? log : (...a) => console.warn(...a);
    warn('createReviewEngine: costTracker игнорируется (deprecated) — передайте в createLlmClient');
  }
  const obs = observability?.enabled ? observability : null;

  function usageFromResult(result) {
    return result?.__usage || null;
  }

  // Cost пишется в createLlmClient (успешный SSE); здесь только метрики в observability —
  // повторный recordUsage из __usage удваивал токены/$ в sticky.
  async function trackLlm(callType, meta, fn) {
    const t0 = Date.now();
    try {
      const result = await fn();
      const err = result?.error;
      const usage = usageFromResult(result);
      obs?.logEvent('llm_call', {
        callType,
        ...meta,
        duration_ms: Date.now() - t0,
        ok: !err,
        findings: result?.findings?.length ?? 0,
        error: err ? redactSecretsForLog(err, 200) : undefined,
        ...(usage ? {
          prompt_tokens: usage.prompt_tokens,
          completion_tokens: usage.completion_tokens,
          total_tokens: usage.total_tokens,
        } : {}),
      });
      return result;
    } catch (e) {
      obs?.logEvent('llm_call', {
        callType,
        ...meta,
        duration_ms: Date.now() - t0,
        ok: false,
        error: redactSecretsForLog(e?.message ?? e, 200),
      });
      throw e;
    }
  }

  async function trackText(callType, meta, fn) {
    const t0 = Date.now();
    let usage = null;
    try {
      // fn получает сборщик usage: prose-вызов возвращает строку (контракт chatText),
      // поэтому токены приходят боковым каналом. DI-мок, который его не зовёт, просто
      // оставляет usage пустым — событие пишется как раньше.
      const text = await fn((u) => { usage = u; });
      obs?.logEvent('llm_call', {
        callType,
        ...meta,
        duration_ms: Date.now() - t0,
        ok: true,
        text_chars: text?.length ?? 0,
        ...(usage ? {
          prompt_tokens: usage.prompt_tokens,
          completion_tokens: usage.completion_tokens,
          total_tokens: usage.total_tokens,
        } : {}),
      });
      return text;
    } catch (e) {
      obs?.logEvent('llm_call', {
        callType,
        ...meta,
        duration_ms: Date.now() - t0,
        ok: false,
        error: redactSecretsForLog(e?.message ?? e, 200),
      });
      throw e;
    }
  }

  const { reviewMode, models, moa, limits, specialists } = cfg;
  const {
    buildMessages, buildReferenceMessages, buildAggregatorMessages,
    buildSpecialistMessages, buildSpecialistAggregatorMessages,
  } = prompt;
  const useSpecialists = specialists?.enabled;
  const promptLayout = cfg.promptLayout || 'legacy';
  // Слияние ролей в один вызов решается один раз на прогон: назначения ролей за прогон не
  // меняются, а решение попадает в лог, чтобы «почему один specialist вместо трёх» читалось.
  const mergeSpecialists = useSpecialists
    && shouldMergeSpecialists(specialists.assignments, specialists.merge ?? 'auto');
  if (mergeSpecialists) {
    log(`specialist merge: роли ${specialists.assignments.map((a) => a.role).join('+')} на одной модели `
      + `${specialists.assignments[0].model} — один вызов вместо ${specialists.assignments.length} на чанк`);
  }

  async function reviewWithModel(model, messages, { temperature = cfg.ollama.temperature, chunkIndex, callType = 'review_json' } = {}) {
    return trackLlm(callType, { model, chunkIndex, reviewMode, ...promptMeta(messages, promptLayout) }, async () => {
      try {
        const result = await ollama.chatJson(messages, model, { temperature });
        if (result.__repaired) {
          obs?.logEvent('json_repair', { kind: result.__repaired, model, chunkIndex });
        }
        const normOpts = { requireQuote: cfg.gate?.requireQuote };
        const findings = (Array.isArray(result.findings) ? result.findings : [])
          .map((f) => normalizeFinding(f, model, normOpts))
          .filter(Boolean);
        return { model, summary: String(result.summary || ''), verdict: result.verdict, findings, __usage: result.__usage };
      } catch (e) {
        log(`модель ${model} недоступна: ${e.message}`);
        return { model, error: e.message };
      }
    });
  }

  async function runModelsOverChunks(modelList, chunks, ctx) {
    const tasks = [];
    for (const model of modelList) {
      chunks.forEach((chunk, ci) => tasks.push({ model, chunk, first: ci === 0, ci }));
    }
    // Скоуп priors — один батч-вызов на прогон (чистая функция от чанков).
    const scoped = planAllPriors(chunks, ctx.priorFindings)
      ?? chunks.map((chunk) => scopePriorsJs(ctx.priorFindings, chunk));
    const done = await mapLimit(tasks, limits.concurrency, async ({ model, chunk, first, ci }) => {
      if (deadlineHit(ctx)) return { model, res: { model, error: TIME_SKIP, skipped: true }, ci };
      const messages = buildMessages({
        annotated: chunk.annotated, blastBlocks: ctx.blastBlocks, pr: ctx.pr,
        fileContents: chunk.fileContents, priorFindings: scoped[ci],
        changedPaths: ctx.changedPaths, chunk,
        prescanSection: first ? ctx.prescanSection : '',
      });
      const res = await reviewWithModel(model, messages, { chunkIndex: ci });
      return { model, res, ci };
    });
    return demoteAll(done, chunks);
  }

  /**
   * Раскладка под префиксный кэш (RFC 04, решение 5). Не удалось разобрать форму сообщений —
   * остаёмся на прежней: собранный наугад промпт дороже несэкономленных токенов.
   */
  function applyLayout(messages, rebuild) {
    if (promptLayout !== 'shared-prefix') return messages;
    return rebuild() || messages;
  }

  async function reviewSpecialist(role, model, baseMessages, chunkIndex) {
    try {
      const built = buildSpecialistMessages(role, baseMessages);
      const messages = applyLayout(built, () => sharedPrefixAdvisor(built));
      const text = await trackText(
        'specialist',
        { model, role, chunkIndex, reviewMode, ...promptMeta(messages, promptLayout) },
        (onUsage) => ollama.chatText(messages, model, { onUsage }),
      );
      return { role, model, text };
    } catch (e) {
      log(`specialist ${role}/${model} недоступен: ${e.message}`);
      return { role, model, error: e.message };
    }
  }

  /**
   * Все роли на одной модели → один вызов вместо трёх (RFC 04, решение 4). Ответ режется
   * по секциям обратно в те же references: жюри, mergeSpecialistReferences и refutation
   * получают прежнюю форму и о слиянии не знают.
   */
  async function reviewSpecialistsMerged(assignments, baseMessages, chunkIndex) {
    const roles = assignments.map((a) => a.role);
    const model = assignments[0].model;
    const user = baseMessages.find((m) => m.role === 'user')?.content || '';
    const built = [
      { role: 'system', content: buildMergedSpecialistSystem(roles) },
      { role: 'user', content: user },
    ];
    const messages = applyLayout(built, () => sharedPrefixAdvisor(built));
    try {
      const text = await trackText(
        'specialist',
        {
          model, role: roles.join('+'), merged: true, chunkIndex, reviewMode,
          ...promptMeta(messages, promptLayout),
        },
        (onUsage) => ollama.chatText(messages, model, { onUsage }),
      );
      return mergedTextToReferences(text, assignments, {
        onFallback: () => log(`specialist merge: модель ${model} не выдержала формат секций на чанке ${chunkIndex} — жюри получит один советник вместо ${roles.length}`),
      });
    } catch (e) {
      log(`specialists ${roles.join('+')}/${model} недоступны: ${e.message}`);
      return assignments.map(({ role }) => ({ role, model, error: e.message }));
    }
  }

  async function reviewReferenceModel(model, baseMessages, chunkIndex) {
    try {
      const built = buildReferenceMessages(baseMessages);
      const messages = applyLayout(built, () => sharedPrefixAdvisor(built));
      const text = await trackText(
        'advisor',
        { model, chunkIndex, reviewMode, ...promptMeta(messages, promptLayout) },
        (onUsage) => ollama.chatText(messages, model, { onUsage }),
      );
      return { model, text };
    } catch (e) {
      log(`advisor ${model} недоступен: ${e.message}`);
      return { model, error: e.message };
    }
  }

  async function reviewChunkMoA(baseMessages, chunkIndex, ctx, { withSpecialists = true } = {}) {
    // Чанк без исполняемого кода (RFC 04, решение 2) идёт одним вызовом агрегатора.
    // Его находки останутся без advisorClaims и потому не смогут блокировать merge —
    // осознанный обмен: docs-only PR роутер и вовсе не отдаёт LLM.
    const references = !withSpecialists
      ? []
      : useSpecialists
        ? (mergeSpecialists
          ? await reviewSpecialistsMerged(specialists.assignments, baseMessages, chunkIndex)
          : await Promise.all(
            specialists.assignments.map(({ role, model }) => reviewSpecialist(role, model, baseMessages, chunkIndex)),
          ))
        : await Promise.all(
          moa.references.map((m) => reviewReferenceModel(m, baseMessages, chunkIndex).then((r) => ({ ...r, role: 'advisor' }))),
        );
    if (withSpecialists && !references.some((r) => r.text && !r.error)) {
      return {
        model: moa.aggregator,
        error: references.map((r) => `${r.role}/${r.model}: ${r.error || 'пусто'}`).join('; '),
      };
    }
    if (deadlineHit(ctx)) return { model: moa.aggregator, error: TIME_SKIP, skipped: true, references };
    // Без референсов агрегатору незачем секция «мнения советников» и схема advisorClaims:
    // она требует того, чего нет, и стоит входных токенов на каждом чанке.
    const aggBuilt = !withSpecialists
      ? baseMessages
      : useSpecialists
        ? buildSpecialistAggregatorMessages(baseMessages, references)
        : buildAggregatorMessages(baseMessages, references);
    // Агрегатор — четвёртый вызов по тому же чанку: его контекст обязан совпасть с тем,
    // что уже прочитали роли, иначе кэшируется префикс, которым никто не пользуется.
    const aggMessages = applyLayout(aggBuilt, () => sharedPrefixAggregator(baseMessages, aggBuilt));
    const agg = await reviewWithModel(
      moa.aggregator,
      aggMessages,
      { temperature: moa.aggTemp, chunkIndex, callType: 'moa_aggregate' },
    );
    if (agg.error) return agg;
    return {
      ...agg,
      model: moa.label,
      aggregator: moa.aggregator,
      references,
      findings: agg.findings.map((f) => ({ ...f, model: moa.label })),
    };
  }

  async function runMoAOverChunks(chunks, ctx) {
    const scoped = planAllPriors(chunks, ctx.priorFindings)
      ?? chunks.map((chunk) => scopePriorsJs(ctx.priorFindings, chunk));
    // Решение о составе ролей принимается один раз на прогон и логируется: иначе «почему у
    // этой находки нет advisorClaims» становится необъяснимым при чтении отчёта.
    // Контекст маршрутизации приходит из pipeline (там же живут профиль и мемоизация
    // классов). Не передали — маршрутизации нет: DI-прогон не должен молча терять роли.
    const routingCtx = limits.chunkRouting === false ? null : (ctx.chunkRouting || null);
    const needsSpecialists = chunks.map((chunk) => (routingCtx
      ? chunkNeedsSpecialists(chunk, routingCtx)
      : true));
    const routingLog = formatChunkRoutingLog(
      needsSpecialists,
      useSpecialists ? specialists.assignments.length : moa.references.length,
    );
    if (routingLog) {
      log(routingLog);
      obs?.logEvent('chunk_routing', {
        chunks: chunks.length,
        lean: needsSpecialists.filter((v) => !v).length,
        leanChunks: needsSpecialists.map((v, i) => (v ? null : i)).filter((i) => i !== null),
      });
    }
    const done = await mapLimit(chunks.map((chunk, ci) => ({ chunk, first: ci === 0, ci })), limits.concurrency, async ({ chunk, first, ci }) => {
      if (deadlineHit(ctx)) return { model: moa.aggregator, res: { model: moa.aggregator, error: TIME_SKIP, skipped: true }, ci };
      const messages = buildMessages({
        annotated: chunk.annotated, blastBlocks: ctx.blastBlocks, pr: ctx.pr,
        fileContents: chunk.fileContents, priorFindings: scoped[ci],
        changedPaths: ctx.changedPaths, chunk,
        prescanSection: first ? ctx.prescanSection : '',
      });
      const res = await reviewChunkMoA(messages, ci, ctx, { withSpecialists: needsSpecialists[ci] });
      return { model: moa.aggregator, res, ci };
    });
    return demoteAll(done, chunks);
  }

  async function reviewAllMoA(chunks, ctx) {
    const done = await runMoAOverChunks(chunks, ctx);
    const agg = aggregateAll([moa.aggregator], done)[0];
    agg.model = moa.label;
    agg.aggregator = moa.aggregator;
    agg.moaReferences = moa.references;
    agg.references = mergeSpecialistReferences(done.map((d) => d.res));
    agg.skippedChunks = done.filter((d) => d.res?.skipped).length;
    return [agg];
  }

  /** Единый fallback-цикл для MoA и consensus; decorate — специфика режима (label/juryBypassed). */
  async function applyFallback(results, chunks, ctx, { labelAs, decorate }) {
    const spare = [...models.fallback];
    for (let i = 0; i < results.length; i++) {
      if (!results[i].error || results[i].skipped) continue;
      if (deadlineHit(ctx)) { log('тайм-бюджет исчерпан — fallback не запускаем'); break; }
      const fb = spare.shift();
      if (!fb) break;
      const who = labelAs === 'MoA aggregator' ? labelAs : `primary ${results[i].model}`;
      log(`${who} недоступен (${results[i].error}) → резерв ${fb}`);
      if (reviewMode === 'moa' && useSpecialists) {
        log('MoA fallback: plain model без specialist jury — R5 refutation/advisorClaims недоступны');
      }
      const plain = await runModelsOverChunks([fb], chunks, ctx);
      const fbRes = aggregateAll([fb], plain)[0];
      fbRes.fallbackFor = labelAs === 'MoA aggregator' ? moa.aggregator : results[i].model;
      decorate?.(fbRes);
      results[i] = fbRes;
    }
    return results;
  }

  async function reviewAllModels(chunks, ctx) {
    if (reviewMode === 'moa') {
      const results = await reviewAllMoA(chunks, ctx);
      return applyFallback(results, chunks, ctx, {
        labelAs: 'MoA aggregator',
        decorate: (fbRes) => {
          if (useSpecialists) fbRes.juryBypassed = true;
          fbRes.model = moa.label;
          if (fbRes.findings?.length) {
            fbRes.findings = fbRes.findings.map((f) => ({ ...f, model: moa.label }));
          }
        },
      });
    }

    const done = await runModelsOverChunks(models.list, chunks, ctx);
    const aggs = aggregateAll(models.list, done);
    const results = models.list.map((model, i) => {
      const agg = aggs[i];
      agg.skippedChunks = done.filter((d) => d.model === model && d.res?.skipped).length;
      return agg;
    });
    return applyFallback(results, chunks, ctx, { labelAs: 'primary' });
  }

  return { reviewAllModels, ...createVerifierRunner(cfg, { ollama, prompt, log, observability: obs }) };
}
