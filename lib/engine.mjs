import { mapLimit } from './util.mjs';
import { VERDICT_RANK } from './constants.mjs';
import { mergeSpecialistReferences } from './specialists.mjs';
import { normalizeFinding, dedupeSameModelFindings } from './findings.mjs';
import { createVerifierRunner } from './verifier.mjs';

/** Маркер пропуска чанка по тайм-бюджету (не путать с недоступностью модели — fallback не нужен). */
const TIME_SKIP = 'тайм-бюджет прогона исчерпан';
const deadlineHit = (ctx) => Boolean(ctx?.deadline) && Date.now() > ctx.deadline;

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
function scopePriors(priorFindings, chunk) {
  const scope = chunkScope(chunk);
  if (!scope || !priorFindings?.length) return priorFindings;
  return priorFindings.filter((f) => !f.file || scope.has(f.file));
}

/**
 * Детерминированная страховка поверх промпта: находка про файл, которого чанк не видит, не может
 * быть доказана — понижаем до nit и метим. Промпт просит, эта функция гарантирует: слепой
 * блокер merge невозможен. Файл, который есть в чанке хотя бы частично, не трогаем.
 */
function demoteOutOfChunk(res, chunk, chunkIndex) {
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

/** MoA + consensus: прогон моделей по чанкам, fallback при сбое primary. */
export function createReviewEngine(cfg, { ollama, prompt, log, observability }) {
  const obs = observability?.enabled ? observability : null;

  async function trackLlm(callType, meta, fn) {
    const t0 = Date.now();
    try {
      const result = await fn();
      const err = result?.error;
      obs?.logEvent('llm_call', {
        callType,
        ...meta,
        duration_ms: Date.now() - t0,
        ok: !err,
        findings: result?.findings?.length ?? 0,
        error: err ? String(err).slice(0, 200) : undefined,
      });
      return result;
    } catch (e) {
      obs?.logEvent('llm_call', {
        callType,
        ...meta,
        duration_ms: Date.now() - t0,
        ok: false,
        error: e.message?.slice(0, 200),
      });
      throw e;
    }
  }

  async function trackText(callType, meta, fn) {
    const t0 = Date.now();
    try {
      const text = await fn();
      obs?.logEvent('llm_call', {
        callType,
        ...meta,
        duration_ms: Date.now() - t0,
        ok: true,
        text_chars: text?.length ?? 0,
      });
      return text;
    } catch (e) {
      obs?.logEvent('llm_call', {
        callType,
        ...meta,
        duration_ms: Date.now() - t0,
        ok: false,
        error: e.message?.slice(0, 200),
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

  async function reviewWithModel(model, messages, { temperature = cfg.ollama.temperature, chunkIndex, callType = 'review_json' } = {}) {
    return trackLlm(callType, { model, chunkIndex, reviewMode }, async () => {
      try {
        const result = await ollama.chatJson(messages, model, { temperature });
        if (result.__repaired) {
          obs?.logEvent('json_repair', { kind: result.__repaired, model, chunkIndex });
        }
        const findings = (Array.isArray(result.findings) ? result.findings : [])
          .map((f) => normalizeFinding(f, model))
          .filter(Boolean);
        return { model, summary: String(result.summary || ''), verdict: result.verdict, findings };
      } catch (e) {
        log(`модель ${model} недоступна: ${e.message}`);
        return { model, error: e.message };
      }
    });
  }

  function aggregateModel(model, done) {
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
      findings: dedupeSameModelFindings(okParts.flatMap((p) => p.findings), log),
    };
  }

  async function runModelsOverChunks(modelList, chunks, ctx) {
    const tasks = [];
    for (const model of modelList) {
      chunks.forEach((chunk, ci) => tasks.push({ model, chunk, first: ci === 0, ci }));
    }
    return mapLimit(tasks, limits.concurrency, async ({ model, chunk, first, ci }) => {
      if (deadlineHit(ctx)) return { model, res: { model, error: TIME_SKIP, skipped: true } };
      const messages = buildMessages({
        annotated: chunk.annotated, blastBlocks: ctx.blastBlocks, pr: ctx.pr,
        fileContents: chunk.fileContents, priorFindings: scopePriors(ctx.priorFindings, chunk),
        changedPaths: ctx.changedPaths, chunk,
        prescanSection: first ? ctx.prescanSection : '',
      });
      const res = await reviewWithModel(model, messages, { chunkIndex: ci });
      return { model, res: demoteOutOfChunk(res, chunk, ci) };
    });
  }

  async function reviewSpecialist(role, model, baseMessages, chunkIndex) {
    try {
      const text = await trackText('specialist', { model, role, chunkIndex, reviewMode }, () =>
        ollama.chatText(buildSpecialistMessages(role, baseMessages), model));
      return { role, model, text };
    } catch (e) {
      log(`specialist ${role}/${model} недоступен: ${e.message}`);
      return { role, model, error: e.message };
    }
  }

  async function reviewReferenceModel(model, baseMessages, chunkIndex) {
    try {
      const text = await trackText('advisor', { model, chunkIndex, reviewMode }, () =>
        ollama.chatText(buildReferenceMessages(baseMessages), model));
      return { model, text };
    } catch (e) {
      log(`advisor ${model} недоступен: ${e.message}`);
      return { model, error: e.message };
    }
  }

  async function reviewChunkMoA(baseMessages, chunkIndex, ctx) {
    const references = useSpecialists
      ? await Promise.all(
        specialists.assignments.map(({ role, model }) => reviewSpecialist(role, model, baseMessages, chunkIndex)),
      )
      : await Promise.all(
        moa.references.map((m) => reviewReferenceModel(m, baseMessages, chunkIndex).then((r) => ({ ...r, role: 'advisor' }))),
      );
    if (!references.some((r) => r.text && !r.error)) {
      return {
        model: moa.aggregator,
        error: references.map((r) => `${r.role}/${r.model}: ${r.error || 'пусто'}`).join('; '),
      };
    }
    if (deadlineHit(ctx)) return { model: moa.aggregator, error: TIME_SKIP, skipped: true, references };
    const aggMessages = useSpecialists
      ? buildSpecialistAggregatorMessages(baseMessages, references)
      : buildAggregatorMessages(baseMessages, references);
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
    return mapLimit(chunks.map((chunk, ci) => ({ chunk, first: ci === 0, ci })), limits.concurrency, async ({ chunk, first, ci }) => {
      if (deadlineHit(ctx)) return { model: moa.aggregator, res: { model: moa.aggregator, error: TIME_SKIP, skipped: true } };
      const messages = buildMessages({
        annotated: chunk.annotated, blastBlocks: ctx.blastBlocks, pr: ctx.pr,
        fileContents: chunk.fileContents, priorFindings: scopePriors(ctx.priorFindings, chunk),
        changedPaths: ctx.changedPaths, chunk,
        prescanSection: first ? ctx.prescanSection : '',
      });
      const res = await reviewChunkMoA(messages, ci, ctx);
      return { model: moa.aggregator, res: demoteOutOfChunk(res, chunk, ci) };
    });
  }

  async function reviewAllMoA(chunks, ctx) {
    const done = await runMoAOverChunks(chunks, ctx);
    const agg = aggregateModel(moa.aggregator, done);
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
      const fbRes = aggregateModel(fb, plain);
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
    const results = models.list.map((model) => {
      const agg = aggregateModel(model, done);
      agg.skippedChunks = done.filter((d) => d.model === model && d.res?.skipped).length;
      return agg;
    });
    return applyFallback(results, chunks, ctx, { labelAs: 'primary' });
  }

  return { reviewAllModels, ...createVerifierRunner(cfg, { ollama, prompt, log, observability: obs }) };
}
