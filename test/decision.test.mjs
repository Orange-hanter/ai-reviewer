import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { pkgPath } from '../lib/package-root.mjs';
import {
  FP_QUESTION, CHUNK_QUESTION, FP_FAMILY_BY_PATTERN_ID, fpFamilyOfPattern, diffWindowFor,
  buildFpState, buildChunkState, createDecisionClient, createDecisionShadow,
} from '../lib/decision.mjs';

const CFG = Object.freeze({
  decision: Object.freeze({
    mode: 'shadow', base: 'https://x.test/api/alpha', key: 'k', model: 'm/d',
    permutations: 2, timeoutMs: 1000, maxCalls: 6, concurrency: 2, questions: ['fp', 'chunk'],
  }),
});

function memObs() {
  const events = [];
  return { events, logEvent: (type, data) => events.push({ type, ...data }) };
}

const okAnswer = (argmax = 'real') => ({
  answer: { name: 'q', probabilities: { [argmax]: 0.9, other_fp: 0.1 }, argmax, flip: false, tvMax: 0.01, confidence: 0.8, refusal: null },
  usage: { prompt_tokens: 100, completion_tokens: 1, total_tokens: 101, cost: 0.00001 },
  latencyMs: 300, calls: 2, servedModel: 'm/d-1', truncated: false,
});

test('decision: каждый id пакетного fp-patterns.jsonl отнесён к семейству из набора Q1', () => {
  const ids = readFileSync(pkgPath('feedback/fp-patterns.jsonl'), 'utf8')
    .split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l).id);
  const values = new Set(FP_QUESTION.choices.map((c) => c.value));
  const missing = ids.filter((id) => !(id in FP_FAMILY_BY_PATTERN_ID));
  assert.deepEqual(missing, [], 'новый паттерн без семейства — допишите FP_FAMILY_BY_PATTERN_ID');
  for (const [id, family] of Object.entries(FP_FAMILY_BY_PATTERN_ID)) {
    assert.ok(values.has(family), `${id} → ${family} вне набора вариантов`);
    assert.notEqual(family, 'real', `${id}: паттерн FP не может значить «настоящая находка»`);
  }
  assert.equal(fpFamilyOfPattern('чужой-паттерн-потребителя'), null);
  assert.equal(fpFamilyOfPattern(null), null);
});

test('decision: варианты вопросов уникальны, у Q2 первым идёт безопасный jury', () => {
  for (const q of [FP_QUESTION, CHUNK_QUESTION]) {
    const values = q.choices.map((c) => c.value);
    assert.equal(new Set(values).size, values.length);
    assert.ok(q.choices.every((c) => c.description.length > 10));
  }
  assert.equal(CHUNK_QUESTION.choices[0].value, 'jury');
});

test('decision: diffWindowFor — окно по новым номерам строк, удалённые строки внутри окна сохраняются', () => {
  const section = [
    'diff --git a/a.js b/a.js', '--- a/a.js', '+++ b/a.js', '@@ -1,40 +1,40 @@',
    ...Array.from({ length: 30 }, (_, i) => `   ${i + 1}:  line${i + 1}`),
    '     : -old20',
  ].join('\n');
  const w = diffWindowFor(section, 25, 2);
  assert.deepEqual(w.split('\n'), [
    'diff --git a/a.js b/a.js', '--- a/a.js', '+++ b/a.js',
    '   23:  line23', '   24:  line24', '   25:  line25', '   26:  line26', '   27:  line27',
  ]);
  const w30 = diffWindowFor(section, 30, 0);
  assert.ok(w30.endsWith('   30:  line30\n     : -old20'), 'удалённая строка после 30-й — в окне 30-й');
  assert.equal(diffWindowFor(section, 0), section, 'нет строки — вся секция');
  assert.equal(diffWindowFor('', 5), '');
});

test('decision: построители входа детерминированы и не зависят от лишних полей', () => {
  const f = { file: 'a.js', line: 3, severity: 'major', title: 'T', body: 'B', confidence: 0.9 };
  assert.equal(buildFpState(f, 'W'), buildFpState({ ...f, confidence: 0.1, model: 'x' }, 'W'));
  assert.match(buildFpState(f, ''), /no diff fragment/);
  const s = buildChunkState({ paths: ['a.js', 'b.md'], annotated: 'DIFF' }, { fileClass: (p) => (p.endsWith('.js') ? 'logic' : 'docs') });
  assert.match(s, /- a\.js \(logic\)\n- b\.md \(docs\)/);
  assert.ok(s.endsWith('DIFF'));
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
  const r1 = await client.decide('S', FP_QUESTION);
  assert.equal(r1.answer.argmax, 'real');
  assert.equal(inputs[0].decide, true);
  assert.equal(inputs[0].url, 'https://x.test/api/alpha/decisions');
  assert.equal(inputs[0].permutations, 2);
  assert.equal(inputs[0].question.name, 'fp_family');
  assert.deepEqual(recorded[0].slice(1), ['m/d', 'decision']);

  quotaNext = true;
  const r2 = await client.decide('S', FP_QUESTION);
  assert.equal(r2.quota, true);
  assert.doesNotMatch(r2.error, /sk-secret-123456789012345/, 'ключ в ошибке маскируется');
  const r3 = await client.decide('S', FP_QUESTION);
  assert.equal(r3.error, 'quota-off', 'после квоты — отказ без вызова бинаря');
  assert.equal(inputs.length, 2);

  const s = client.stats();
  assert.equal(s.calls, 4);
  assert.equal(s.quotaHit, true);
  assert.equal(s.errors, 1);
});

test('decision client: бюджет вызовов и отсутствие бинаря/ключа — отказ без вызова', async () => {
  let calls = 0;
  const runBin = async () => { calls++; return okAnswer(); };
  const client = createDecisionClient(CFG, { runBin, bin: '/x/llm' });
  for (let i = 0; i < 3; i++) assert.ok((await client.decide('S', FP_QUESTION)).answer);
  assert.equal((await client.decide('S', FP_QUESTION)).error, 'budget', '6 вызовов = 3 × 2 перестановки');
  assert.equal(client.stats().budgetHit, true);
  assert.equal(calls, 3);

  assert.equal((await createDecisionClient(CFG, { runBin, bin: null }).decide('S', FP_QUESTION)).error, 'no-binary');
  const noKey = { decision: { ...CFG.decision, key: '' } };
  assert.equal((await createDecisionClient(noKey, { runBin, bin: '/x/llm' }).decide('S', FP_QUESTION)).error, 'no-key');
  assert.equal(calls, 3);
});

test('decision client: исключение бинаря — ошибка, а не throw', async () => {
  const client = createDecisionClient(CFG, { runBin: async () => { throw new Error('llm binary timeout'); }, bin: '/x/llm' });
  const r = await client.decide('S', CHUNK_QUESTION);
  assert.match(r.error, /timeout/);
});

test('decision shadow: события по находкам (кроме prescan) и чанкам, находки не мутируются', async () => {
  const obs = memObs();
  const seen = [];
  const decider = {
    model: 'm/d', permutations: 2,
    decide: async (state, q) => { seen.push(q.name); return okAnswer(q.name === 'chunk_route' ? 'single' : 'f1_in_diff'); },
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

  assert.deepEqual(seen.sort(), ['chunk_route', 'fp_family']);
  const fp = obs.events.find((e) => e.type === 'decision_fp');
  assert.equal(fp.file, 'e2e/a.spec.ts');
  assert.equal(fp.regexPatternId, 'e2e-in-diff', 'регекс-вердикт рядом с решением модели');
  assert.equal(fp.regexFamily, 'f1_in_diff');
  assert.equal(fp.decider.argmax, 'f1_in_diff');
  assert.equal(fp.decider.model, 'm/d-1');
  const ch = obs.events.find((e) => e.type === 'decision_chunk');
  assert.deepEqual([ch.deterministic, ch.decision, ch.agree], ['single', 'single', true]);
  const sum = obs.events.find((e) => e.type === 'decision_summary');
  assert.equal(sum.asked, 2);
  assert.equal(sum.calls, 4);
});

test('decision shadow: бросающий и зависший decider наружу не протекают; хвост — decision_error', async () => {
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
  const obs = memObs();
  let asked = 0;
  const decider = { model: 'm', permutations: 1, decide: async () => { asked++; return okAnswer(); }, stats: () => ({ calls: 0, costUsd: 0, tokens: 0 }) };
  const cfg = { decision: { ...CFG.decision, questions: ['chunk'] } };
  const shadow = createDecisionShadow({ cfg, decider, obs });
  shadow.onFindings([{ file: 'a', line: 1, title: 't', body: 'b' }]);
  await shadow.drain(100);
  assert.equal(asked, 0);
});
