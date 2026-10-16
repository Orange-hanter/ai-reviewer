#!/usr/bin/env node
// Пробник decision-моделей (RFC 21, шаг T1): форма запроса/ответа, детерминизм на повторе,
// чувствительность к порядку вариантов, латентность по длине входа, коды ошибок.
// Диагностика, а не поведение модуля llm: сырой fetch по образцу pr-review-probe-models.
//
// Запуск: REVIEW_DECISION_API_KEY=... node bin/pr-review-probe-decisions.mjs [model1,model2] [--json]
// Ключ: REVIEW_DECISION_API_KEY → OPENROUTER_API_KEY → LLM_API_KEY.
// База: REVIEW_DECISION_BASE_URL (по умолчанию https://openrouter.ai/api/alpha), путь /decisions.
const args = process.argv.slice(2);
const JSON_OUT = args.includes('--json');
const MODELS = (args.find((a) => !a.startsWith('--')) || 'microsoft/microsoft-decision-1,openai/gpt-6-luna-decisions').split(',');
const BASE = (process.env.REVIEW_DECISION_BASE_URL || 'https://openrouter.ai/api/alpha').replace(/\/$/, '');
const KEY = process.env.REVIEW_DECISION_API_KEY || process.env.OPENROUTER_API_KEY || process.env.LLM_API_KEY;
const REPEATS = Number(process.env.PROBE_REPEATS || 3);

if (!KEY) {
  console.error('нужен REVIEW_DECISION_API_KEY (или OPENROUTER_API_KEY / LLM_API_KEY)');
  process.exit(2);
}

// Варианты Q1 — семейства из docs/false-positives.md + other_fp + real.
const FP_CRITERIA = {
  f1_in_diff: 'Claims a file or test is missing from the PR, but it is present in the diff.',
  f2_convergence: 'Repeats an older finding that the current code already addressed.',
  f3_neighbor_commit: 'Ignores a fix made in a neighbouring commit of the same PR.',
  f4_hypothetical: 'Describes code that does not exist in the shown diff or context.',
  f5_outdated: 'Relies on outdated knowledge of a language, library or tool instead of the code.',
  f6_unseen: 'Asserts a critical problem about code the reviewer could not see.',
  f7_absent_path: 'Claims a file or module does not exist although it exists in the repository.',
  f8_retry_leak: 'Claims a resource leak inside a retry loop that is actually released.',
  other_fp: 'False positive of another kind (e.g. intentional project design).',
  real: 'A real defect that a careful reviewer would want fixed.',
};

const REAL_STATE = {
  finding: { file: 'lib/paginate.mjs', line: 12, severity: 'major',
    title: 'Off-by-one: последний элемент страницы теряется',
    body: 'slice(start, end - 1) отбрасывает последний элемент: end уже эксклюзивен.' },
  diff: [
    '@@ -8,6 +8,8 @@ export function page(items, n, size) {',
    '   const start = n * size;',
    '-  return items.slice(start, start + size);',
    '+  const end = start + size;',
    '+  return items.slice(start, end - 1);',
    ' }',
  ].join('\n'),
};

const FP_STATE = {
  finding: { file: 'test/paginate.test.mjs', line: 1, severity: 'major',
    title: 'Нет теста на page()',
    body: 'Изменение page() не покрыто тестом: файл test/paginate.test.mjs отсутствует в PR.' },
  diff: [
    'diff --git a/test/paginate.test.mjs b/test/paginate.test.mjs',
    'new file mode 100644',
    '@@ -0,0 +1,5 @@',
    "+import { page } from '../lib/paginate.mjs';",
    "+test('page keeps last item', () => assert.deepEqual(page([1,2,3], 0, 3), [1,2,3]));",
  ].join('\n'),
};

function q1(criteria = FP_CRITERIA) {
  return {
    fp_family: {
      type: 'choice',
      instructions: 'A code-review bot produced the finding below for the given diff. Classify it: a real defect, or which kind of false positive.',
      criteria,
    },
  };
}

function reversedCriteria(c) {
  return Object.fromEntries(Object.entries(c).reverse());
}

async function call(model, state, questions) {
  const t0 = Date.now();
  try {
    const res = await fetch(`${BASE}/decisions`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${KEY}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ model, state, questions }),
    });
    const ms = Date.now() - t0;
    const text = await res.text();
    let body = null;
    try { body = JSON.parse(text); } catch { /* не JSON */ }
    if (!res.ok) return { ok: false, status: res.status, ms, error: (body?.error?.message || text).slice(0, 200) };
    return { ok: true, status: res.status, ms, body };
  } catch (e) {
    return { ok: false, status: 0, ms: Date.now() - t0, error: String(e.message || e).slice(0, 200) };
  }
}

function probs(r, name = 'fp_family') {
  return r.ok ? r.body?.answers?.[name]?.probabilities || null : null;
}

function tv(a, b) {
  if (!a || !b) return null;
  const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
  let s = 0;
  for (const k of keys) s += Math.abs((a[k] || 0) - (b[k] || 0));
  return s / 2;
}

function argmax(p) {
  if (!p) return null;
  return Object.entries(p).sort((x, y) => y[1] - x[1])[0]?.[0] ?? null;
}

const pad = (s, n) => String(s).padEnd(n);
const report = [];

for (const model of MODELS) {
  const rec = { model, checks: {} };
  report.push(rec);

  // 1. Форма ответа.
  const shape = await call(model, REAL_STATE, q1());
  rec.checks.shape = shape.ok
    ? { ms: shape.ms, answer: shape.body.answers?.fp_family, usage: shape.body.usage,
      provider: shape.body.provider, servedModel: shape.body.model }
    : { status: shape.status, error: shape.error };
  if (!shape.ok) {
    if (!JSON_OUT) console.log(pad(model, 36), 'shape', `HTTP ${shape.status}: ${shape.error}`);
    continue;
  }

  // 2. Детерминизм: тот же вход REPEATS раз.
  const reps = [probs(shape)];
  for (let i = 1; i < REPEATS; i++) reps.push(probs(await call(model, REAL_STATE, q1())));
  const maxTvRepeat = Math.max(...reps.slice(1).map((p) => tv(reps[0], p) ?? 1));
  rec.checks.determinism = { repeats: REPEATS, argmax: reps.map(argmax), maxTv: maxTvRepeat };

  // 3. Порядок вариантов: обратный порядок criteria.
  const rev = probs(await call(model, REAL_STATE, q1(reversedCriteria(FP_CRITERIA))));
  rec.checks.order = { argmaxFwd: argmax(reps[0]), argmaxRev: argmax(rev), tv: tv(reps[0], rev) };

  // 4. FP-кейс: ожидаем f1_in_diff.
  const fp = await call(model, FP_STATE, q1());
  rec.checks.fpCase = fp.ok ? { argmax: argmax(probs(fp)), p: probs(fp) } : { status: fp.status, error: fp.error };

  // 5. Несколько вопросов в одном вызове + noul.
  const multi = await call(model, REAL_STATE, {
    ...q1(),
    is_real: { type: 'noul', instructions: 'Is the finding a real defect in the shown code?',
      criteria: { true: 'Real defect.', false: 'Not a defect.' } },
    route: { type: 'choice', instructions: 'Does reviewing this diff need a panel of specialist reviewers?',
      criteria: { jury: 'Executable logic with non-trivial behaviour.', single: 'Docs, data or trivial change.' } },
  });
  rec.checks.multi = multi.ok
    ? { keys: Object.keys(multi.body.answers || {}), noul: multi.body.answers?.is_real?.noul,
      route: multi.body.answers?.route?.choice, ms: multi.ms, usage: multi.body.usage }
    : { status: multi.status, error: multi.error };

  // 6. Латентность по длине входа.
  rec.checks.latency = {};
  for (const chars of [4000, 16000]) {
    const filler = '\n' + ' const x = 1; // context line\n'.repeat(Math.ceil(chars / 30)).slice(0, chars);
    const r = await call(model, { ...REAL_STATE, diff: REAL_STATE.diff + filler }, q1());
    rec.checks.latency[chars] = r.ok ? { ms: r.ms, inputTokens: r.body.usage?.input_tokens } : { status: r.status, error: r.error };
  }

  // 7. Ошибки формы запроса.
  const bad = await call(model, REAL_STATE, { x: { type: 'choice', instructions: 'no criteria' } });
  rec.checks.badRequest = { status: bad.status, error: bad.error || null };

  if (!JSON_OUT) {
    const c = rec.checks;
    console.log(`\n== ${model}  (served: ${c.shape.servedModel}, provider: ${c.shape.provider})`);
    console.log(' shape     ', c.shape.ms + 'ms', 'choice=' + c.shape.answer?.choice, 'conf=' + c.shape.answer?.confidence,
      'usage=' + JSON.stringify(c.shape.usage));
    console.log(' determin. ', 'argmax=' + c.determinism.argmax.join(','), 'maxTV=' + c.determinism.maxTv?.toFixed(4));
    console.log(' order     ', `fwd=${c.order.argmaxFwd} rev=${c.order.argmaxRev} tv=${c.order.tv?.toFixed(4)}`);
    console.log(' fp-case   ', c.fpCase.argmax ? 'argmax=' + c.fpCase.argmax : `HTTP ${c.fpCase.status}: ${c.fpCase.error}`);
    console.log(' multi     ', c.multi.keys ? `keys=${c.multi.keys} noul=${c.multi.noul} route=${c.multi.route} ${c.multi.ms}ms` : `HTTP ${c.multi.status}: ${c.multi.error}`);
    console.log(' latency   ', Object.entries(c.latency).map(([k, v]) => `${k}ch:${v.ms ?? 'ERR ' + v.status}ms/${v.inputTokens ?? '?'}tok`).join('  '));
    console.log(' bad req   ', `HTTP ${c.badRequest.status}: ${c.badRequest.error}`);
  }
}

if (JSON_OUT) console.log(JSON.stringify(report, null, 2));
