/**
 * Запасная модель verifier'а (REVIEW_VERIFIER_FALLBACK_MODEL, RFC 20 § 7): сбой основной не
 * превращается в «verifier недоступен» (RFC 18 — degraded-блок), пока жива запасная.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createVerifierRunner } from '../lib/verifier.mjs';

const prompt = { buildVerifierMessages: () => [{ role: 'user', content: 'x' }] };
const cand = [{ severity: 'critical', file: 'a.ts', line: 1, title: 't' }];
const ctx = { annotated: '', fileContents: '', pr: { number: 1 } };
const cfg = (fallbackModel) => ({ verifier: { model: 'free/model:free', fallbackModel, temperature: 0.2 } });

function harness(behaviour) {
  const calls = [];
  const events = [];
  const logs = [];
  const ollama = {
    chatJson: async (_m, model) => {
      calls.push(model);
      const r = behaviour[model];
      if (r instanceof Error) throw r;
      return r;
    },
  };
  const observability = { logEvent: (type, e) => events.push({ type, ...e }) };
  return { calls, events, logs, deps: { ollama, prompt, log: (l) => logs.push(l), observability } };
}

test('verifier: сбой основной модели → тот же батч у запасной', async () => {
  const h = harness({
    'free/model:free': Object.assign(new Error('LLM 429 (квота провайдера исчерпана): free-models-per-day'), { quota: true }),
    'deepseek/pro': { verdicts: [{ candidateIndex: 0, decision: 'verified', reason: 'ok' }] },
  });
  const verdicts = await createVerifierRunner(cfg('deepseek/pro'), h.deps).verifyFindings(cand, ctx);
  assert.deepEqual(h.calls, ['free/model:free', 'deepseek/pro']);
  assert.equal(verdicts[0].decision, 'verified');
  const [primary, fallback] = h.events.filter((e) => e.callType === 'verifier');
  assert.equal(primary.ok, false);
  assert.equal(fallback.ok, true);
  assert.equal(fallback.fallbackFrom, 'free/model:free', 'в метриках видно, что судила запасная');
  assert.ok(h.logs.some((l) => /запасная модель deepseek\/pro/.test(l)));
});

test('verifier: без запасной и при сбое обеих — ошибка доходит до pipeline (RFC 18)', async () => {
  const fail = new Error('down');
  let h = harness({ 'free/model:free': fail });
  await assert.rejects(createVerifierRunner(cfg(''), h.deps).verifyFindings(cand, ctx), /down/);
  assert.deepEqual(h.calls, ['free/model:free']);

  h = harness({ 'free/model:free': fail, 'deepseek/pro': new Error('also down') });
  await assert.rejects(createVerifierRunner(cfg('deepseek/pro'), h.deps).verifyFindings(cand, ctx), /also down/);
  assert.deepEqual(h.calls, ['free/model:free', 'deepseek/pro']);
});

test('verifier: основная ответила — запасная не вызывается', async () => {
  const h = harness({ 'free/model:free': { verdicts: [] }, 'deepseek/pro': { verdicts: [] } });
  await createVerifierRunner(cfg('deepseek/pro'), h.deps).verifyFindings(cand, ctx);
  assert.deepEqual(h.calls, ['free/model:free']);
});
