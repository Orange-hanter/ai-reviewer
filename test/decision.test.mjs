// Оркестрация shadow decision-моделей (lib/decision.mjs). Вопросы, таблица семейств и
// построители входа — в Rust (rust/src/decision.rs, свои тесты); здесь — клиент и shadow,
// shadow ходит в настоящий бинарь `decision` (нет бинаря — тесты shadow пропускаются).
import test from 'node:test';
import assert from 'node:assert/strict';
import { createDecisionClient, createDecisionShadow, resolveDecisionOpsBin, runDecisionOp } from '../lib/decision.mjs';

const CFG = Object.freeze({
  decision: Object.freeze({
    mode: 'shadow', base: 'https://x.test/api/alpha', key: 'k', model: 'm/d',
    permutations: 2, timeoutMs: 1000, maxCalls: 6, concurrency: 2, questions: ['fp', 'chunk'],
  }),
});
const NO_OPS = !resolveDecisionOpsBin() && 'нет Rust-бинаря decision (cargo build)';
const Q = { name: 'fp_family', instructions: 'i', choices: [{ value: 'real', description: 'd' }, { value: 'fp', description: 'd' }] };

function memObs() {
  const events = [];
  return { events, logEvent: (type, data) => events.push({ type, ...data }) };
}

const okAnswer = (argmax = 'real') => ({
  answer: { name: 'q', probabilities: { [argmax]: 0.9, other_fp: 0.1 }, argmax, flip: false, tvMax: 0.01, confidence: 0.8, refusal: null },
  usage: { prompt_tokens: 100, completion_tokens: 1, total_tokens: 101, cost: 0.00001 },
  latencyMs: 300, calls: 2, servedModel: 'm/d-1', truncated: false,
});

test('decision client: бюджет считает перестановки, квота выключает клиент, цена — в свой трекер', async () => {
  const inputs = [];
  const recorded = [];
  let quotaNext = false;
  const runBin = async (_bin, input) => {
    inputs.push(JSON.parse(input));
    if (quotaNext) return { __error: { error: 'decision 402 key=sk-secret-123456789012345', status: 402, quota: true, usage: null } };
    return okAnswer();
  };
  const client = createDecisionClient(CFG, { runBin, bin: '/x/llm', costTracker: { record: (...a) => recorded.push(a) } });
  const r1 = await client.decide('S', Q);
  assert.equal(r1.answer.argmax, 'real');
  assert.equal(inputs[0].decide, true);
  assert.equal(inputs[0].url, 'https://x.test/api/alpha/decisions');
  assert.equal(inputs[0].permutations, 2);
  assert.deepEqual(recorded[0].slice(1), ['m/d', 'decision']);

  quotaNext = true;
  const r2 = await client.decide('S', Q);
  assert.equal(r2.quota, true);
  assert.doesNotMatch(r2.error, /sk-secret-123456789012345/, 'ключ в ошибке маскируется');
  assert.equal((await client.decide('S', Q)).error, 'quota-off', 'после квоты — отказ без вызова бинаря');
  assert.equal(inputs.length, 2);
  assert.deepEqual([client.stats().calls, client.stats().quotaHit, client.stats().errors], [4, true, 1]);
});

test('decision client: бюджет вызовов и отсутствие бинаря/ключа — отказ без вызова', async () => {
  let calls = 0;
  const runBin = async () => { calls++; return okAnswer(); };
  const client = createDecisionClient(CFG, { runBin, bin: '/x/llm' });
  for (let i = 0; i < 3; i++) assert.ok((await client.decide('S', Q)).answer);
  assert.equal((await client.decide('S', Q)).error, 'budget', '6 вызовов = 3 × 2 перестановки');
  assert.equal(client.stats().budgetHit, true);
  assert.equal((await createDecisionClient(CFG, { runBin, bin: null }).decide('S', Q)).error, 'no-binary');
  const noKey = { decision: { ...CFG.decision, key: '' } };
  assert.equal((await createDecisionClient(noKey, { runBin, bin: '/x/llm' }).decide('S', Q)).error, 'no-key');
  assert.equal(calls, 3);
});

test('decision client: исключение бинаря — ошибка, а не throw', async () => {
  const client = createDecisionClient(CFG, { runBin: async () => { throw new Error('llm binary timeout'); }, bin: '/x/llm' });
  assert.match((await client.decide('S', Q)).error, /timeout/);
});

test('decision shadow: нет бинаря decision — одно decision_error, наружу не протекает', async () => {
  const obs = memObs();
  const decider = { model: 'm', permutations: 1, decide: async () => okAnswer(), stats: () => ({ calls: 0, costUsd: 0, tokens: 0 }) };
  const ops = () => { throw Object.assign(new Error('no-binary'), { code: 'no-binary' }); };
  const shadow = createDecisionShadow({ cfg: CFG, decider, obs, ops });
  shadow.onFindings([{ file: 'a', line: 1, title: 't', body: 'b' }]);
  await shadow.drain(100);
  assert.deepEqual(obs.events.filter((e) => e.type === 'decision_error').map((e) => e.reason), ['no-binary']);
});

test('decision shadow: события по находкам (кроме prescan) и чанкам, находки не мутируются', { skip: NO_OPS }, async () => {
  const obs = memObs();
  const seen = [];
  const decider = {
    model: 'm/d', permutations: 2,
    decide: async (state, q) => { seen.push([q.name, state]); return okAnswer(q.name === 'chunk_route' ? 'single' : 'f1_in_diff'); },
    stats: () => ({ calls: 4, budgetHit: false, quotaHit: false, errors: 0, costUsd: 0.00002, tokens: 202 }),
  };
  const shadow = createDecisionShadow({ cfg: CFG, decider, obs });
  const findings = [
    Object.freeze({ file: 'e2e/a.spec.ts', line: 2, severity: 'major', title: 'нет e2e', body: 'e2e тест отсутствует в PR', model: 'x' }),
    Object.freeze({ file: 'a.js', line: 1, severity: 'major', title: 'eslint', body: 'b', prescan: true }),
  ];
  shadow.onFindings(findings, { changedPaths: ['e2e/a.spec.ts'], diffSections: new Map([['e2e/a.spec.ts', '    2: +x']]) });
  shadow.onChunks([{ paths: ['docs/a.md'], annotated: 'D' }], { fileClass: () => 'docs' });
  await shadow.drain(1000);

  assert.deepEqual(seen.map(([n]) => n).sort(), ['chunk_route', 'fp_family']);
  const fpState = seen.find(([n]) => n === 'fp_family')[1];
  assert.match(fpState, /^Finding: \[major\] e2e\/a\.spec\.ts:2/);
  assert.match(fpState, / {4}2: \+x/, 'окно диффа построено Rust-бинарём');
  const fp = obs.events.find((e) => e.type === 'decision_fp');
  assert.equal(fp.regexPatternId, 'e2e-in-diff', 'регекс-вердикт рядом с решением модели');
  assert.equal(fp.regexFamily, 'f1_in_diff');
  assert.equal(fp.decider.argmax, 'f1_in_diff');
  const ch = obs.events.find((e) => e.type === 'decision_chunk');
  assert.deepEqual([ch.deterministic, ch.decision, ch.agree], ['single', 'single', true]);
  assert.equal(obs.events.find((e) => e.type === 'decision_summary').asked, 2);
});

test('decision shadow: бросающий и зависший decider наружу не протекают; хвост — decision_error', { skip: NO_OPS }, async () => {
  const obs = memObs();
  let n = 0;
  const decider = {
    model: 'm', permutations: 1,
    decide: () => (n++ === 0 ? Promise.reject(new Error('boom')) : new Promise(() => {})),
    stats: () => ({ calls: 0, budgetHit: false, quotaHit: false, errors: 0, costUsd: 0, tokens: 0 }),
  };
  const shadow = createDecisionShadow({ cfg: CFG, decider, obs });
  shadow.onFindings([{ file: 'a', line: 1, title: 't', body: 'b' }, { file: 'b', line: 1, title: 't', body: 'b' }]);
  await shadow.drain(50);
  const errs = obs.events.filter((e) => e.type === 'decision_error').map((e) => e.reason);
  assert.ok(errs.includes('boom'));
  assert.ok(errs.includes('drain-timeout'));
  assert.ok(obs.events.some((e) => e.type === 'decision_summary'));
});

test('decision shadow: набор вопросов ограничивает, что спрашивается', async () => {
  let asked = 0;
  const decider = { model: 'm', permutations: 1, decide: async () => { asked++; return okAnswer(); }, stats: () => ({ calls: 0, costUsd: 0, tokens: 0 }) };
  const shadow = createDecisionShadow({ cfg: { decision: { ...CFG.decision, questions: ['chunk'] } }, decider, obs: memObs(), ops: () => { throw new Error('не должен вызываться'); } });
  shadow.onFindings([{ file: 'a', line: 1, title: 't', body: 'b' }]);
  await shadow.drain(100);
  assert.equal(asked, 0);
});

test('decision ops: вопросы из Rust-бинаря — Q2 начинается с безопасного jury', { skip: NO_OPS }, () => {
  const q = runDecisionOp('questions');
  assert.equal(q.chunk.choices[0].value, 'jury');
  assert.equal(q.fp.choices.at(-1).value, 'real');
  assert.equal(runDecisionOp('fpFamilyOfPattern', { id: 'gha-version-hallucination' }), 'f5_outdated');
});
