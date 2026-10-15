// Тесты engine (MoA/consensus оркестрация, fallback, deadline) и ollama-клиента
// (SSE-парсинг, retry-бюджет) — с подменёнными ollama/prompt/fetch.
// Запуск: node --test ops/pr-review.engine.test.mjs
import './helpers/reference-profile.mjs';
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { createReviewEngine, createOllamaClient, createAdaptiveSemaphore } = await import('../index.mjs');

// --- engine ---

const ENGINE_CFG = {
  reviewMode: 'consensus',
  models: { list: ['m1'], fallback: [] },
  moa: { aggregator: 'agg', label: 'moa', references: [], refMaxTokens: 100, refTemp: 0.6, aggTemp: 0.4 },
  limits: { concurrency: 2 },
  specialists: { enabled: false, assignments: [] },
  ollama: { temperature: 0.4 },
};
const PROMPT = { buildMessages: () => [{ role: 'user', content: 'x' }] };
const CHUNKS = [{ annotated: 'd1', fileContents: '' }, { annotated: 'd2', fileContents: '' }];

const mkEngine = (ollama, cfg = ENGINE_CFG, prompt = PROMPT) =>
  createReviewEngine(cfg, { ollama, prompt, log: () => {}, observability: null });

test('engine: дедлайн истёк → чанки пропускаются, LLM не вызывается, fallback не запускается', async () => {
  let llmCalls = 0;
  const ollama = {
    chatJson: async () => { llmCalls++; throw new Error('must not be called'); },
  };
  const engine = mkEngine(ollama);
  const res = await engine.reviewAllModels(CHUNKS, { pr: {}, deadline: Date.now() - 1000 });
  assert.equal(llmCalls, 0, 'ни одного LLM-вызова после дедлайна');
  assert.equal(res[0].skippedChunks, 2);
  assert.ok(res[0].skipped, 'aggregate помечен skipped (не «модель недоступна»)');
  assert.ok(/тайм-бюджет/.test(res[0].error));
});

test('engine: primary недоступна → fallback-модель, fallbackFor выставлен', async () => {
  const cfg = { ...ENGINE_CFG, models: { list: ['m1'], fallback: ['m2'] } };
  const ollama = {
    chatJson: async (_msgs, model) => {
      if (model === 'm1') throw new Error('модель лежит');
      return { summary: 'ok', verdict: 'approve', findings: [] };
    },
  };
  const engine = mkEngine(ollama, cfg);
  const res = await engine.reviewAllModels([CHUNKS[0]], { pr: {} });
  assert.equal(res[0].model, 'm2');
  assert.equal(res[0].fallbackFor, 'm1');
  assert.ok(!res[0].error);
});

test('engine: findings нормализуются и агрегируются по чанкам', async () => {
  const ollama = {
    chatJson: async (msgs) => ({
      summary: 's', verdict: 'comment',
      // Разные находки по чанкам: агрегация проверяется на РАЗНЫХ находках, иначе тест
      // не отличить от проверки дедупа повторов (см. соседний тест).
      findings: [{ file: `${msgs[0].content}.ts`, line: 1, severity: 'minor', title: 't', confidence: 0.5 }],
    }),
  };
  const engine = mkEngine(ollama, ENGINE_CFG, {
    buildMessages: ({ annotated }) => [{ role: 'user', content: annotated }],
  });
  const res = await engine.reviewAllModels(CHUNKS, { pr: {} });
  assert.equal(res[0].findings.length, 2, 'по finding с каждого из двух чанков');
  assert.equal(res[0].findings[0].model, 'm1');
});

// Регрессия PR #520: одна тема, увиденная из нескольких чанков, попадала в sticky N раз —
// на 11 прогонов подряд это давало «18 находок» из ~6 тем. Между МОДЕЛЯМИ дедупа быть не
// должно: там повтор — сигнал консенсуса для clusterFindings.
test('engine: одинаковая находка из разных чанков схлопывается внутри модели', async () => {
  const ollama = {
    chatJson: async () => ({
      summary: 's', verdict: 'comment',
      findings: [{ file: 'a.ts', line: 1, severity: 'minor', title: 'Один и тот же дефект', confidence: 0.5 }],
    }),
  };
  const one = await mkEngine(ollama).reviewAllModels(CHUNKS, { pr: {} });
  assert.equal(one[0].findings.length, 1, 'дубль из второго чанка схлопнут');
  assert.equal(one[0].findings[0].dupCount, 2, 'число повторов сохранено');

  const twoModels = { ...ENGINE_CFG, models: { list: ['m1', 'm2'], fallback: [] } };
  const both = await mkEngine(ollama, twoModels).reviewAllModels(CHUNKS, { pr: {} });
  assert.deepEqual(both.map((r) => r.findings.length), [1, 1], 'у каждой модели своя находка осталась');
  // Атрибуция, а не только счёт: кластеризация считает консенсус по НАБОРУ моделей, и если
  // дедуп подменит model, две модели схлопнутся в одну и блокер потеряется.
  assert.deepEqual(both.map((r) => r.findings[0].model), ['m1', 'm2']);
});

// Корень несходящегося ревью на PR #520: карту PR («какие файлы в этом чанке, какие
// ревьюятся отдельно») получал только чанк 0, а prior findings — все чанки. Чанк без файла
// не мог проверить чужую находку и повторял её, а про невидимый файл заявлял, что его в PR нет.
const SCOPED_CHUNKS = [
  { annotated: 'd1', fileContents: '', paths: ['a.ts'], partialPaths: [] },
  { annotated: 'd2', fileContents: '', paths: ['b.ts'], partialPaths: [] },
];

test('engine: карта PR — в каждый чанк, prior findings — только по файлам чанка', async () => {
  const seen = [];
  const prompt = {
    buildMessages: (args) => {
      seen.push(args);
      return [{ role: 'user', content: 'x' }];
    },
  };
  const ollama = { chatJson: async () => ({ summary: 's', verdict: 'approve', findings: [] }) };
  const priorFindings = [
    { file: 'a.ts', line: 1, severity: 'major', title: 'по файлу первого чанка' },
    { file: 'b.ts', line: 2, severity: 'major', title: 'по файлу второго чанка' },
    { file: '', line: 0, severity: 'minor', title: 'без файла — видна всем' },
  ];
  await mkEngine(ollama, ENGINE_CFG, prompt)
    .reviewAllModels(SCOPED_CHUNKS, { pr: {}, changedPaths: ['a.ts', 'b.ts'], priorFindings });

  assert.equal(seen.length, 2, 'по вызову на чанк');
  for (const args of seen) {
    assert.deepEqual(args.changedPaths, ['a.ts', 'b.ts'], 'полный список файлов PR в каждом чанке');
    assert.ok(args.chunk?.paths?.length, 'чанк знает свой состав');
  }
  assert.deepEqual(seen[0].priorFindings.map((f) => f.title), ['по файлу первого чанка', 'без файла — видна всем']);
  assert.deepEqual(seen[1].priorFindings.map((f) => f.title), ['по файлу второго чанка', 'без файла — видна всем']);

  // Частично видимый файл — свой: prior по нему приходит, даже если его строка в другом куске.
  // Именно поэтому в карте обязательна пометка [THIS PART, SOME HUNKS ONLY].
  seen.length = 0;
  await mkEngine(ollama, ENGINE_CFG, prompt).reviewAllModels(
    [{ annotated: 'd', fileContents: '', paths: ['a.ts'], partialPaths: ['a.ts'] }],
    { pr: {}, changedPaths: ['a.ts', 'b.ts'], priorFindings },
  );
  assert.deepEqual(seen[0].priorFindings.map((f) => f.title), ['по файлу первого чанка', 'без файла — видна всем']);

  // Состав чанка неполный (секция без опознанного пути) — скоуп и понижение отключаются.
  seen.length = 0;
  await mkEngine(ollama, ENGINE_CFG, prompt).reviewAllModels(
    [{ annotated: 'd', fileContents: '', paths: ['a.ts'], partialPaths: [], unknownPaths: true }],
    { pr: {}, changedPaths: ['a.ts', 'b.ts'], priorFindings },
  );
  assert.equal(seen[0].priorFindings.length, 3, 'при неизвестном составе priors не режем');
});

test('engine: находка про файл вне чанка понижается до nit и метится', async () => {
  const ollama = {
    chatJson: async () => ({
      summary: 's', verdict: 'changes_requested',
      findings: [
        { file: 'b.ts', line: 3, severity: 'critical', title: 'слепой блокер', confidence: 0.9 },
        { file: 'a.ts', line: 1, severity: 'major', title: 'виден в этом чанке', confidence: 0.9 },
      ],
    }),
  };
  const res = await mkEngine(ollama)
    .reviewAllModels([SCOPED_CHUNKS[0]], { pr: {}, changedPaths: ['a.ts', 'b.ts'] });
  // find, а не Object.fromEntries: карта по file молча затёрла бы дубли и тест не отличил бы
  // «понижены обе находки по b.ts» от «понижена одна».
  const invisible = res[0].findings.filter((f) => f.file === 'b.ts');
  const visible = res[0].findings.filter((f) => f.file === 'a.ts');
  assert.equal(invisible.length, 1);
  assert.equal(invisible[0].severity, 'nit', 'блокер по невидимому файлу не блокирует merge');
  assert.equal(invisible[0].outOfChunk, true);
  assert.ok(invisible[0].confidence <= 0.5);
  assert.deepEqual(visible.map((f) => f.severity), ['major'], 'находка по видимому файлу не тронута');
});

// --- ollama client ---

const OLLAMA_CFG = {
  ollama: {
    base: 'http://ollama.test', key: 'k', seed: 1, jsonMode: false,
    temperature: 0.4, httpRetries: 3, requestTimeoutMs: 100,
  },
  moa: { refMaxTokens: 100, refTemp: 0.6 },
  httpRetries: 3,
};

const sse = (text) =>
  `data: ${JSON.stringify({ choices: [{ delta: { content: text } }] })}\n\ndata: [DONE]\n\n`;

async function withFetch(fn, run) {
  const orig = globalThis.fetch;
  globalThis.fetch = fn;
  try { await run(); } finally { globalThis.fetch = orig; }
}

test('ollama: SSE-стрим собирается в content', async () => {
  const client = createOllamaClient(OLLAMA_CFG, () => {});
  await withFetch(async () => new Response(sse('привет мир'), { status: 200 }), async () => {
    const text = await client.chatText([{ role: 'user', content: 'x' }], 'm1');
    assert.equal(text, 'привет мир');
  });
});

test('ollama: не-transient 400 без stream_options в теле — сразу стоп (без ложного retry)', async () => {
  let calls = 0;
  const bodies = [];
  const client = createOllamaClient(OLLAMA_CFG, () => {});
  await withFetch(async (_url, init) => {
    calls++;
    bodies.push(JSON.parse(init.body));
    return new Response('bad request: max_tokens', { status: 400 });
  }, async () => {
    await assert.rejects(() => client.chatText([], 'm1'), /LLM 400/);
    assert.equal(calls, 1, 'без stream_options/include_usage в теле — не ретраим');
    assert.ok(bodies[0].stream_options?.include_usage);
  });
});

test('ollama: 400 на stream_options → успех без include_usage', async () => {
  let calls = 0;
  const client = createOllamaClient(OLLAMA_CFG, () => {});
  await withFetch(async (_url, init) => {
    calls++;
    const body = JSON.parse(init.body);
    if (body.stream_options) return new Response('unknown field stream_options', { status: 400 });
    return new Response(sse('ok'), { status: 200 });
  }, async () => {
    const text = await client.chatText([], 'm1');
    assert.equal(text, 'ok');
    assert.equal(calls, 2);
  });
});

test('ollama: retry-бюджет — при исчерпанном таймауте transient 500 не ретраится', async () => {
  let calls = 0;
  // requestTimeoutMs=1мс, а ответ занимает 5мс → бюджет израсходован → повтора быть не должно.
  const cfg = { ...OLLAMA_CFG, ollama: { ...OLLAMA_CFG.ollama, requestTimeoutMs: 1 } };
  const client = createOllamaClient(cfg, () => {});
  await withFetch(async () => {
    calls++;
    await new Promise((r) => setTimeout(r, 5));
    return new Response('err', { status: 500 });
  }, async () => {
    await assert.rejects(() => client.chatText([], 'm1'), /LLM 500/);
    assert.equal(calls, 1, 'ретрай подавлен бюджетом таймаута');
  });
});

test('ollama: transient 500 → один ретрай с backoff, затем успех', async () => {
  let calls = 0;
  const client = createOllamaClient(OLLAMA_CFG, () => {});
  await withFetch(async () => {
    calls++;
    return calls === 1 ? new Response('err', { status: 500 }) : new Response(sse('ok'), { status: 200 });
  }, async () => {
    const text = await client.chatText([], 'm1');
    assert.equal(text, 'ok');
    assert.equal(calls, 2);
  });
});

// --- adaptive semaphore (rate-limit control) ---

test('semaphore: cap параллелизма соблюдается', async () => {
  const sem = createAdaptiveSemaphore(3);
  let running = 0, maxRunning = 0;
  await Promise.all(Array.from({ length: 10 }, async () => {
    await sem.acquire();
    running++;
    maxRunning = Math.max(maxRunning, running);
    await new Promise((r) => setTimeout(r, 10));
    running--;
    sem.release();
  }));
  assert.equal(maxRunning, 3, 'одновременно выполняется не больше лимита');
});

test('semaphore: throttle по 429 — ×0.7 floor 2, восстановление за серию успехов', () => {
  const sem = createAdaptiveSemaphore(10);
  sem.throttle();
  assert.equal(sem.stats().limit, 7);
  sem.throttle();
  assert.equal(sem.stats().limit, 4);
  for (let i = 0; i < 3; i++) sem.throttle();
  assert.equal(sem.stats().limit, 2, 'floor 2');
  for (let i = 0; i < 25; i++) sem.release();
  assert.equal(sem.stats().limit, 3, '+1 за 25 успешных release');
  for (let i = 0; i < 200; i++) sem.release();
  assert.equal(sem.stats().limit, 10, 'рост ограничен hardMax');
});

test('ollama: 429 вызывает throttle и ретрай (retry-after чтение не ломает)', async () => {
  let calls = 0;
  const logs = [];
  const client = createOllamaClient(OLLAMA_CFG, (...a) => logs.push(a.join(' ')));
  await withFetch(async () => {
    calls++;
    return calls === 1
      ? new Response('rate limited', { status: 429, headers: { 'retry-after': '0' } })
      : new Response(sse('ok'), { status: 200 });
  }, async () => {
    const text = await client.chatText([], 'm1');
    assert.equal(text, 'ok');
    assert.equal(calls, 2);
    assert.ok(logs.some((l) => l.includes('снижение параллелизма')), 'throttle залогирован');
  });
});

// --- MoA specialists → aggregator → advisorClaims ---

const MOA_CFG = {
  reviewMode: 'moa',
  models: { list: [], fallback: [] },
  moa: {
    aggregator: 'agg-model',
    label: 'moa',
    references: ['ref-a', 'ref-b'],
    refMaxTokens: 100,
    refTemp: 0.6,
    aggTemp: 0.4,
  },
  limits: { concurrency: 4 },
  specialists: {
    enabled: true,
    assignments: [
      { role: 'security', model: 'sec-m' },
      { role: 'correctness', model: 'corr-m' },
      { role: 'architecture', model: 'arch-m' },
    ],
  },
  ollama: { temperature: 0.4 },
  gate: {},
  verifier: { model: 'ver-m', temperature: 0.2 },
};

const MOA_PROMPT = {
  buildMessages: () => [{ role: 'user', content: 'base' }],
  buildSpecialistMessages: (role, base) => [...base, { role: 'system', content: `specialist:${role}` }],
  buildSpecialistAggregatorMessages: (base, refs) => [
    ...base,
    { role: 'user', content: `jury refs=${refs.map((r) => r.role).join(',')}` },
  ],
  buildReferenceMessages: (base) => base,
  buildAggregatorMessages: (base) => base,
  buildVerifierMessages: ({ candidates }) => [
    { role: 'user', content: `verify ${candidates.length}` },
  ],
};

const MOA_CHUNK = [{ annotated: 'd1', fileContents: 'code', paths: ['a.ts'], partialPaths: [] }];

test('engine MoA: specialists → aggregator; advisorClaims и model=moa label', async () => {
  const textCalls = [];
  const jsonCalls = [];
  const ollama = {
    chatText: async (msgs, model) => {
      textCalls.push({ model, tip: msgs.at(-1)?.content });
      return `prose from ${model}`;
    },
    chatJson: async (msgs, model) => {
      jsonCalls.push({ model, tip: msgs.at(-1)?.content });
      assert.equal(model, 'agg-model');
      return {
        summary: 'jury',
        verdict: 'changes_requested',
        findings: [{
          file: 'a.ts',
          line: 3,
          severity: 'critical',
          title: 'IDOR',
          body: 'open',
          confidence: 0.9,
          advisorClaims: [
            { role: 'security', status: 'verified', reason: 'real' },
            { role: 'correctness', status: 'rejected', reason: 'n/a' },
            { role: 'architecture', status: 'downgraded', severity: 'minor', reason: 'layer' },
          ],
        }],
      };
    },
  };
  const engine = createReviewEngine(MOA_CFG, {
    ollama, prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  const [res] = await engine.reviewAllModels(MOA_CHUNK, { pr: {} });

  assert.deepEqual(
    textCalls.map((c) => c.model).sort(),
    ['arch-m', 'corr-m', 'sec-m'],
    'три specialist prose-вызова',
  );
  assert.equal(jsonCalls.length, 1, 'один aggregator JSON');
  assert.equal(res.model, 'moa');
  assert.equal(res.aggregator, 'agg-model');
  assert.equal(res.findings.length, 1);
  assert.equal(res.findings[0].model, 'moa', 'атрибуция — label MoA, не agg-model');
  assert.equal(res.findings[0].advisorClaims.length, 3);
  assert.equal(res.findings[0].advisorClaims[0].status, 'verified');
  assert.ok(res.references?.length >= 3, 'specialist references смержены в результат');
  assert.ok(res.references.every((r) => r.text?.includes('prose from')));
});

test('engine MoA: все specialists недоступны → error, aggregator не зовётся', async () => {
  let jsonCalls = 0;
  const ollama = {
    chatText: async () => { throw new Error('down'); },
    chatJson: async () => { jsonCalls++; return { findings: [] }; },
  };
  const engine = createReviewEngine(MOA_CFG, {
    ollama, prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  const [res] = await engine.reviewAllModels(MOA_CHUNK, { pr: {} });
  assert.equal(jsonCalls, 0);
  assert.ok(res.error, 'ошибка без успешного jury');
  assert.match(res.error, /security\/sec-m/);
});

// --- маршрутизация ролей по содержимому чанка (RFC 04, решение 2) ---

const routingCtx = {
  pr: {},
  chunkRouting: { fileClass: (p) => (/\.(?:ts|mjs|js)$/.test(p) ? 'logic' : 'other'), sensitive: null },
};
const mkRoutingOllama = (calls) => ({
  chatText: async (_m, model) => { calls.text.push(model); return `prose from ${model}`; },
  chatJson: async (msgs, model) => {
    calls.json.push({ model, tip: msgs.at(-1)?.content });
    return { summary: 's', verdict: 'comment', findings: [] };
  },
});

test('chunk-routing: чанк из документации и данных обслуживает один агрегатор', async () => {
  const calls = { text: [], json: [] };
  const engine = createReviewEngine(MOA_CFG, {
    ollama: mkRoutingOllama(calls), prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  const chunks = [{ annotated: 'd', fileContents: '', paths: ['docs/architecture.md', 'fixtures/a.json'], partialPaths: [] }];
  await engine.reviewAllModels(chunks, routingCtx);
  assert.deepEqual(calls.text, [], 'специалисты не вызываются: исполняемого кода в чанке нет');
  assert.equal(calls.json.length, 1, 'ровно один вызов агрегатора');
  assert.equal(calls.json[0].tip, 'base', 'без референсов агрегатор получает базовый промпт, без секции жюри');
});

test('chunk-routing: код, CI-workflow и чувствительный путь жюри не теряют', async () => {
  const cases = [
    { name: 'код', paths: ['lib/engine.mjs'], ctx: routingCtx },
    { name: 'CI-workflow', paths: ['.github/workflows/ci.yml'], ctx: routingCtx },
    { name: 'shell', paths: ['scripts/deploy.sh'], ctx: routingCtx },
    {
      name: 'чувствительный путь профиля',
      paths: ['config/billing.json'],
      ctx: { pr: {}, chunkRouting: { fileClass: () => 'other', sensitive: /billing/i } },
    },
    { name: 'состав чанка неизвестен', paths: [], ctx: routingCtx },
  ];
  for (const c of cases) {
    const calls = { text: [], json: [] };
    const engine = createReviewEngine(MOA_CFG, {
      ollama: mkRoutingOllama(calls), prompt: MOA_PROMPT, log: () => {}, observability: null,
    });
    await engine.reviewAllModels([{ annotated: 'd', fileContents: '', paths: c.paths, partialPaths: [] }], c.ctx);
    assert.equal(calls.text.length, 3, `${c.name}: все три специалиста опрошены`);
  }
});

test('chunk-routing: выключенный флаг и отсутствие контекста возвращают полный MoA', async () => {
  for (const [name, cfg, ctx] of [
    ['REVIEW_CHUNK_ROUTING=0', { ...MOA_CFG, limits: { ...MOA_CFG.limits, chunkRouting: false } }, routingCtx],
    ['контекст не передан (DI-прогон)', MOA_CFG, { pr: {} }],
  ]) {
    const calls = { text: [], json: [] };
    const engine = createReviewEngine(cfg, {
      ollama: mkRoutingOllama(calls), prompt: MOA_PROMPT, log: () => {}, observability: null,
    });
    await engine.reviewAllModels([{ annotated: 'd', fileContents: '', paths: ['docs/a.md'], partialPaths: [] }], ctx);
    assert.equal(calls.text.length, 3, `${name}: состав ролей не урезается`);
  }
});

// --- слияние ролей на одной модели (RFC 04, решение 4) ---

const ONE_MODEL_CFG = {
  ...MOA_CFG,
  specialists: {
    enabled: true,
    merge: 'auto',
    assignments: [
      { role: 'security', model: 'solo' },
      { role: 'correctness', model: 'solo' },
      { role: 'architecture', model: 'solo' },
    ],
  },
};
const sectioned = (bodies) => [
  `### ROLE: security\n${bodies[0]}`,
  `### ROLE: correctness\n${bodies[1]}`,
  `### ROLE: architecture\n${bodies[2]}`,
].join('\n\n');

// --- summary агрегата: обзор PR берётся из чанка с кодом ---

// Промпт, доносящий до фейковой модели, какой это чанк: MOA_PROMPT отдаёт всем 'base'.
const SUMMARY_PROMPT = { ...MOA_PROMPT, buildMessages: ({ annotated }) => [{ role: 'user', content: annotated }] };
// Модель отвечает summary'ем своего чанка — так видно, чей обзор доехал до агрегата.
// Чанк опознаём по ПЕРВОМУ сообщению: у чанка с кодом жюри дописывает в хвост секцию
// мнений советников (buildSpecialistAggregatorMessages), у чанка без кода агрегатор
// получает базовый промпт как есть — msgs.at(-1) в этих двух случаях разное.
const SUMMARY_OLLAMA = {
  chatText: async (_m, model) => `prose from ${model}`,
  chatJson: async (msgs) => ({ summary: `обзор: ${msgs[0].content}`, findings: [] }),
};
const summaryChunk = (annotated, paths) => ({ annotated, fileContents: '', paths, partialPaths: [] });

test('summary агрегата: docs-чанк идёт первым, но обзор берётся у чанка с кодом', async () => {
  const engine = createReviewEngine(MOA_CFG, {
    ollama: SUMMARY_OLLAMA, prompt: SUMMARY_PROMPT, log: () => {}, observability: null,
  });
  const [res] = await engine.reviewAllModels([
    summaryChunk('docs', ['proposals/04-token-economy.md']),
    summaryChunk('code', ['lib/engine.mjs']),
  ], routingCtx);
  assert.equal(res.summary, 'обзор: code',
    'Walkthrough не должен открываться обзором plan-документа, когда в PR есть код');
});

test('summary агрегата: без единого кодового чанка — прежнее поведение (первое непустое)', async () => {
  const engine = createReviewEngine(MOA_CFG, {
    ollama: SUMMARY_OLLAMA, prompt: SUMMARY_PROMPT, log: () => {}, observability: null,
  });
  const [res] = await engine.reviewAllModels([
    summaryChunk('docs', ['docs/a.md']),
    summaryChunk('fixtures', ['fixtures/a.json']),
  ], routingCtx);
  assert.equal(res.summary, 'обзор: docs');
});

test('summary агрегата: маршрутизация выключена — выбор не меняется', async () => {
  const cfg = { ...MOA_CFG, limits: { ...MOA_CFG.limits, chunkRouting: false } };
  const engine = createReviewEngine(cfg, {
    ollama: SUMMARY_OLLAMA, prompt: SUMMARY_PROMPT, log: () => {}, observability: null,
  });
  const [res] = await engine.reviewAllModels([
    summaryChunk('docs', ['docs/a.md']),
    summaryChunk('code', ['lib/engine.mjs']),
  ], routingCtx);
  assert.equal(res.summary, 'обзор: docs', 'без маршрутизации все чанки равны — первое непустое');
});

test('summary агрегата: пустой обзор кодового чанка не съедает содержательный', async () => {
  const engine = createReviewEngine(MOA_CFG, {
    ollama: {
      ...SUMMARY_OLLAMA,
      chatJson: async (msgs) => ({
        summary: msgs[0].content === 'code' ? '' : `обзор: ${msgs[0].content}`,
        findings: [],
      }),
    },
    prompt: SUMMARY_PROMPT,
    log: () => {},
    observability: null,
  });
  const [res] = await engine.reviewAllModels([
    summaryChunk('docs', ['docs/a.md']),
    summaryChunk('code', ['lib/engine.mjs']),
  ], routingCtx);
  assert.equal(res.summary, 'обзор: docs');
});

test('агрегат не носит verdict модели: поле удалено из контракта', async () => {
  const engine = createReviewEngine(MOA_CFG, {
    ollama: {
      ...SUMMARY_OLLAMA,
      // модель всё равно может прислать лишнее поле — агрегат обязан его игнорировать
      chatJson: async () => ({ summary: 's', verdict: 'changes_requested', findings: [] }),
    },
    prompt: SUMMARY_PROMPT,
    log: () => {},
    observability: null,
  });
  const [res] = await engine.reviewAllModels([summaryChunk('code', ['lib/engine.mjs'])], routingCtx);
  assert.equal(res.verdict, undefined, 'verdict модели никто не читал — он не должен появляться в агрегате');
});

test('specialist merge: одна модель на все роли → один вызов, ответ разложен по ролям', async () => {
  const textCalls = [];
  const ollama = {
    chatText: async (msgs, model) => {
      textCalls.push({ model, system: msgs[0]?.content || '' });
      return sectioned(['- [major] `a.ts:1` — sec', '- [minor] `a.ts:2` — corr', 'в моей области серьёзных замечаний нет']);
    },
    chatJson: async () => ({ summary: 's', verdict: 'comment', findings: [] }),
  };
  const engine = createReviewEngine(ONE_MODEL_CFG, {
    ollama, prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  const [res] = await engine.reviewAllModels(MOA_CHUNK, { pr: {} });

  assert.equal(textCalls.length, 1, 'один prose-вызов вместо трёх');
  // Rust-текст (0.5+) и deprecated JS-копия пишут «panel of 3» в разном регистре.
  assert.match(textCalls[0].system, /panel of 3/i, 'модель предупреждена, что ролей три');
  const byRole = Object.fromEntries(res.references.map((r) => [r.role, r.text]));
  assert.match(byRole.security, /sec/);
  assert.match(byRole.correctness, /corr/);
  assert.match(byRole.architecture, /серьёзных замечаний нет/);
  assert.ok(!byRole.security.includes('corr'), 'секции не перетекают друг в друга');
});

test('specialist merge: сорванный формат не подделывает консенсус', async () => {
  const ollama = {
    chatText: async () => 'сплошной текст без заголовков секций',
    chatJson: async () => ({ summary: 's', verdict: 'comment', findings: [] }),
  };
  const engine = createReviewEngine(ONE_MODEL_CFG, {
    ollama, prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  const [res] = await engine.reviewAllModels(MOA_CHUNK, { pr: {} });
  assert.equal(res.references.length, 1, 'один советник, а не три копии одного текста');
  assert.equal(res.references[0].mergedFallback, true);
});

test('specialist merge: разные модели по ролям не сливаются', async () => {
  const models = [];
  const ollama = {
    chatText: async (_m, model) => { models.push(model); return 'prose'; },
    chatJson: async () => ({ summary: 's', verdict: 'comment', findings: [] }),
  };
  const engine = createReviewEngine(MOA_CFG, {
    ollama, prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  await engine.reviewAllModels(MOA_CHUNK, { pr: {} });
  assert.deepEqual(models, ['sec-m', 'corr-m', 'arch-m'], 'три модели — три независимых мнения, сливать нечего');
});

test('specialist merge: never опрашивает роли по отдельности даже на одной модели', async () => {
  const calls = [];
  const ollama = {
    chatText: async (_m, model) => { calls.push(model); return 'prose'; },
    chatJson: async () => ({ summary: 's', verdict: 'comment', findings: [] }),
  };
  const engine = createReviewEngine(
    { ...ONE_MODEL_CFG, specialists: { ...ONE_MODEL_CFG.specialists, merge: 'never' } },
    { ollama, prompt: MOA_PROMPT, log: () => {}, observability: null },
  );
  await engine.reviewAllModels(MOA_CHUNK, { pr: {} });
  assert.equal(calls.length, 3);
});

// --- раскладка под префиксный кэш (RFC 04, решение 5) ---

// Мок, повторяющий РЕАЛЬНУЮ форму lib/prompt.mjs: [system, user], а жюри дописывает свою
// секцию СПЕРЕДИ к последнему user. Именно на эту форму опирается разбор в prompt-layout.
const REAL_SHAPE_PROMPT = {
  buildMessages: () => [
    { role: 'system', content: 'AGGREGATOR RULES + JSON SCHEMA' },
    { role: 'user', content: 'КОНТЕКСТ ЧАНКА: дифф и файлы' },
  ],
  buildSpecialistMessages: (role, base) => [
    { role: 'system', content: `specialist rules: ${role}` },
    { role: 'user', content: base.at(-1).content },
  ],
  buildSpecialistAggregatorMessages: (base, refs) => base.map((m, i) => (
    i === base.length - 1
      ? { role: 'user', content: `jury refs=${refs.map((r) => r.role).join(',')}\n${m.content}` }
      : m
  )),
  buildReferenceMessages: (base) => base,
  buildAggregatorMessages: (base) => base,
  buildVerifierMessages: ({ candidates }) => [{ role: 'user', content: `verify ${candidates.length}` }],
};

test('prompt layout: shared-prefix даёт всем ролям чанка побайтово общий префикс', async () => {
  const seen = [];
  const ollama = {
    chatText: async (msgs) => { seen.push(msgs); return 'prose'; },
    chatJson: async (msgs) => { seen.push(msgs); return { summary: 's', verdict: 'comment', findings: [] }; },
  };
  const engine = createReviewEngine({ ...MOA_CFG, promptLayout: 'shared-prefix' }, {
    ollama, prompt: REAL_SHAPE_PROMPT, log: () => {}, observability: null,
  });
  await engine.reviewAllModels(MOA_CHUNK, { pr: {} });

  assert.equal(seen.length, 4, 'три роли и агрегатор');
  for (const msgs of seen) {
    assert.equal(msgs.length, 3, 'system + контекст + роль');
    assert.equal(msgs[0].role, 'system');
    assert.equal(msgs[2].role, 'user');
  }
  const prefix = (m) => `${m[0].content} ${m[1].content}`;
  const prefixes = new Set(seen.map(prefix));
  assert.equal(prefixes.size, 1, 'префикс совпадает у всех четырёх вызовов — иначе кэшировать нечего');
  const roleTails = seen.map((m) => m[2].content);
  assert.equal(new Set(roleTails).size, 4, 'инструкции ролей при этом различны');
  assert.match(roleTails[3], /jury refs=/, 'мнения советников уехали в хвост агрегатора');
  assert.match(roleTails[3], /AGGREGATOR RULES/, 'правила и схема ответа поехали туда же');
  assert.match(seen[0][1].content, /КОНТЕКСТ ЧАНКА/, 'в общем префиксе — именно контекст');
});

test('prompt layout: legacy по умолчанию — прежняя форма сообщений', async () => {
  const seen = [];
  const ollama = {
    chatText: async (msgs) => { seen.push(msgs); return 'prose'; },
    chatJson: async (msgs) => { seen.push(msgs); return { summary: 's', verdict: 'comment', findings: [] }; },
  };
  const engine = createReviewEngine(MOA_CFG, {
    ollama, prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  await engine.reviewAllModels(MOA_CHUNK, { pr: {} });
  assert.ok(seen.every((m) => m.length <= 2), 'без флага сообщения не перестраиваются');
});

test('prompt layout: неразбираемая форма промпта откатывается на legacy', async () => {
  const seen = [];
  const ollama = {
    chatText: async (msgs) => { seen.push(msgs); return 'prose'; },
    chatJson: async (msgs) => { seen.push(msgs); return { summary: 's', verdict: 'comment', findings: [] }; },
  };
  // Агрегатор дописывает мнения не спереди, а сзади — срез контекста стал бы неверным.
  const oddPrompt = {
    ...MOA_PROMPT,
    buildSpecialistAggregatorMessages: (base) => [...base, { role: 'user', content: 'хвостом' }],
  };
  const engine = createReviewEngine({ ...MOA_CFG, promptLayout: 'shared-prefix' }, {
    ollama, prompt: oddPrompt, log: () => {}, observability: null,
  });
  await engine.reviewAllModels(MOA_CHUNK, { pr: {} });
  const agg = seen.at(-1);
  assert.equal(agg.at(-1).content, 'хвостом', 'сообщения агрегатора остались как построены');
});

// --- verifier runner (createVerifierRunner через engine) ---

test('engine verifier: chatJson verdicts — order/decision доезжают (happy path)', async () => {
  const ollama = {
    chatJson: async (_msgs, model) => {
      assert.equal(model, 'ver-m');
      return {
        verdicts: [
          { candidateIndex: 0, decision: 'rejected', reason: 'fp' },
          { candidateIndex: 1, decision: 'verified', reason: 'ok' },
        ],
      };
    },
  };
  const engine = createReviewEngine(MOA_CFG, {
    ollama, prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  const verdicts = await engine.verifyFindings(
    [
      { file: 'a.ts', line: 1, severity: 'critical', title: 'x' },
      { file: 'b.ts', line: 2, severity: 'major', title: 'y' },
    ],
    { annotated: 'd', fileContents: '', pr: {} },
  );
  assert.equal(verdicts.length, 2);
  assert.equal(verdicts[0].candidateIndex, 0);
  assert.equal(verdicts[0].decision, 'rejected');
  assert.equal(verdicts[1].candidateIndex, 1);
  assert.equal(verdicts[1].decision, 'verified');
});

test('engine verifier: нет поля verdicts → пустой список (strict apply понизит снаружи)', async () => {
  const ollama = {
    chatJson: async () => ({ summary: 'oops no verdicts' }),
  };
  const engine = createReviewEngine(MOA_CFG, {
    ollama, prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  const verdicts = await engine.verifyFindings(
    [{ file: 'a.ts', line: 1, severity: 'critical', title: 'x' }],
    { annotated: '', fileContents: '', pr: {} },
  );
  assert.deepEqual(verdicts, []);
});

test('engine verifier: LLM throw пробрасывается (pipeline решает fail-closed)', async () => {
  const ollama = {
    chatJson: async () => { throw new Error('verifier timeout'); },
  };
  const engine = createReviewEngine(MOA_CFG, {
    ollama, prompt: MOA_PROMPT, log: () => {}, observability: null,
  });
  await assert.rejects(
    () => engine.verifyFindings(
      [{ file: 'a.ts', line: 1, severity: 'critical', title: 'x' }],
      { annotated: '', fileContents: '', pr: {} },
    ),
    /verifier timeout/,
  );
});
