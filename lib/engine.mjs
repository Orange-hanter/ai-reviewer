import { mapLimit } from './util.mjs';
import { VERDICT_RANK } from './constants.mjs';
import { mergeSpecialistReferences } from './specialists.mjs';
import { normalizeFinding } from './findings.mjs';
import { createVerifierRunner } from './verifier.mjs';

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
    if (!okParts.length) return { model, error: parts[0]?.error || 'нет ответа' };
    const verdict = okParts.map((p) => p.verdict)
      .sort((a, b) => (VERDICT_RANK[b] ?? 0) - (VERDICT_RANK[a] ?? 0))[0];
    return {
      model,
      summary: okParts.map((p) => p.summary).filter(Boolean).join(' '),
      verdict,
      findings: okParts.flatMap((p) => p.findings),
    };
  }

  async function runModelsOverChunks(modelList, chunks, ctx) {
    const tasks = [];
    for (const model of modelList) {
      chunks.forEach((chunk, ci) => tasks.push({ model, chunk, first: ci === 0, ci }));
    }
    return mapLimit(tasks, limits.concurrency, async ({ model, chunk, first, ci }) => {
      const messages = buildMessages({
        annotated: chunk.annotated, blastBlocks: ctx.blastBlocks, pr: ctx.pr,
        fileContents: chunk.fileContents, priorFindings: ctx.priorFindings,
        changedFilesSection: first ? ctx.changedFilesSection : '',
        prescanSection: first ? ctx.prescanSection : '',
      });
      return { model, res: await reviewWithModel(model, messages, { chunkIndex: ci }) };
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

  async function reviewChunkMoA(baseMessages, chunkIndex) {
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
      const messages = buildMessages({
        annotated: chunk.annotated, blastBlocks: ctx.blastBlocks, pr: ctx.pr,
        fileContents: chunk.fileContents, priorFindings: ctx.priorFindings,
        changedFilesSection: first ? ctx.changedFilesSection : '',
        prescanSection: first ? ctx.prescanSection : '',
      });
      return { model: moa.aggregator, res: await reviewChunkMoA(messages, ci) };
    });
  }

  async function reviewAllMoA(chunks, ctx) {
    const done = await runMoAOverChunks(chunks, ctx);
    const agg = aggregateModel(moa.aggregator, done);
    agg.model = moa.label;
    agg.aggregator = moa.aggregator;
    agg.moaReferences = moa.references;
    agg.references = mergeSpecialistReferences(done.map((d) => d.res));
    return [agg];
  }

  async function applyFallback(results, chunks, ctx, { labelAs }) {
    const spare = [...models.fallback];
    for (let i = 0; i < results.length; i++) {
      if (!results[i].error) continue;
      const fb = spare.shift();
      if (!fb) break;
      log(`${labelAs} недоступен (${results[i].error}) → резерв ${fb}`);
      if (reviewMode === 'moa' && useSpecialists) {
        log('MoA fallback: plain model без specialist jury — R5 refutation/advisorClaims недоступны');
      }
      const plain = await runModelsOverChunks([fb], chunks, ctx);
      const fbRes = aggregateModel(fb, plain);
      fbRes.fallbackFor = labelAs === 'MoA aggregator' ? moa.aggregator : results[i].model;
      if (reviewMode === 'moa' && useSpecialists) fbRes.juryBypassed = true;
      if (reviewMode === 'moa') {
        fbRes.model = moa.label;
        if (fbRes.findings?.length) {
          fbRes.findings = fbRes.findings.map((f) => ({ ...f, model: moa.label }));
        }
      }
      results[i] = fbRes;
    }
    return results;
  }

  async function reviewAllModels(chunks, ctx) {
    if (reviewMode === 'moa') {
      const results = await reviewAllMoA(chunks, ctx);
      return applyFallback(results, chunks, ctx, { labelAs: 'MoA aggregator' });
    }

    const done = await runModelsOverChunks(models.list, chunks, ctx);
    const results = models.list.map((model) => aggregateModel(model, done));

    const spare = [...models.fallback];
    for (let i = 0; i < results.length; i++) {
      if (!results[i].error) continue;
      const fb = spare.shift();
      if (!fb) break;
      log(`primary ${results[i].model} недоступна (${results[i].error}) → резервная модель ${fb}`);
      const fbDone = await runModelsOverChunks([fb], chunks, ctx);
      const fbRes = aggregateModel(fb, fbDone);
      fbRes.fallbackFor = results[i].model;
      results[i] = fbRes;
    }
    return results;
  }

  return { reviewAllModels, ...createVerifierRunner(cfg, { ollama, prompt, log, observability: obs }) };
}
