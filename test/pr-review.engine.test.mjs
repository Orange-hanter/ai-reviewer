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
