// DI-тесты пайплайна pr-review: createReviewPipeline с подменёнными github/engine/prescan —
// весь run() end-to-end без сети и LLM. Запуск: node --test ops/pr-review.pipeline.test.mjs
import './helpers/reference-profile.mjs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { writeFile, readFile, rm, mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const { createReviewPipeline, loadReviewConfig, runGateResolution, MARKERS } = await import('../index.mjs');

const ENV = {
  OLLAMA_API_KEY: 'k',
  GITHUB_TOKEN: 't',
  REVIEW_MODE: 'consensus',
  OLLAMA_MODELS: 'm1,m2',
  REVIEW_OBSERVABILITY: '0',
  REVIEW_EXPORT: '0',
  REVIEW_AUTO_TRIAGE: '0',
  REVIEW_VERIFIER: '0',
};

const CODE_DIFF = `diff --git a/foo.ts b/foo.ts
--- a/foo.ts
+++ b/foo.ts
@@ -1,2 +1,3 @@
 const a = 1;
-const b = 2;
+const b = 3;
+export function foo() { return a; }
`;

const LOCK_DIFF = `diff --git a/package-lock.json b/package-lock.json
--- a/package-lock.json
+++ b/package-lock.json
@@ -1,1 +1,1 @@
-old
+new
`;

function mkDeps({ reviews, graphify = { buildBlastRadius: async () => [] }, verifyFindings, prescanFindings = [], notifyTelegram }) {
  const calls = { order: [], postReview: [], upsertSticky: [], deleteStaleInline: [], telegram: [], engineRuns: 0 };
  const deps = {
    log: () => {},
    die: (m) => { throw new Error(`die: ${m}`); },
    // Дерево репозитория не читаем: иначе тест зависел бы от `git ls-files` каталога запуска.
    repoFiles: async () => null,
    graphify,
    prescan: {
      run: async () => ({
        findings: prescanFindings, scanned: 0, skipped: 0, truncated: false,
        lintableTotal: 0, byTool: null, error: null, promptSection: '',
      }),
    },
    engine: {
      reviewAllModels: async () => { calls.engineRuns++; return reviews; },
      verifyFindings: verifyFindings || (async () => []),
    },
    github: {
      // На ПЕРВОМ прогоне PR sticky ещё нет: клиент обязан вернуть форму объекта, иначе
      // пайплайн падал на `priorSticky.findings.length` (critical из ревью #534).
      readPriorStickyData: async () => ({ findings: [], runs: [], sha: '' }),
      readPriorFindings: async () => [],
      deleteStaleInline: async (opts) => { calls.order.push('delete'); calls.deleteStaleInline.push(opts); return 0; },
      postReview: async (p) => { calls.order.push('post'); calls.postReview.push(p); return 555; },
      upsertSticky: async (body) => { calls.order.push('sticky'); calls.upsertSticky.push(body); return 'created'; },
    },
  };
  if (notifyTelegram) {
    deps.notifyTelegram = async (...args) => {
      calls.order.push('telegram');
      calls.telegram.push(args[2]);
      return notifyTelegram(...args);
    };
  }
  return { deps, calls };
}

async function runPipeline({
  diff, reviews, graphify, observabilityDir = '', env = {}, verifyFindings, prescanFindings, notifyTelegram,
  mutateDeps,
}) {
  const tmp = join(tmpdir(), `pr-review-pipe-${process.pid}-${Math.random().toString(36).slice(2)}.patch`);
  await writeFile(tmp, diff);
  try {
    const cfg = loadReviewConfig({
      ...ENV,
      ...(observabilityDir ? { REVIEW_OBSERVABILITY: '1', REVIEW_OBSERVABILITY_DIR: observabilityDir } : {}),
      ...env,
    }, { dryRun: false, diffFile: tmp, repo: 'o/r', prNumber: 1 });
    const { deps, calls } = mkDeps({ reviews, graphify, verifyFindings, prescanFindings, notifyTelegram });
    mutateDeps?.(deps, calls);
    const code = await createReviewPipeline(cfg, deps).run();
    return { code, calls };
  } finally {
    await rm(tmp, { force: true });
  }
}

const criticalReview = {
  model: 'm1', summary: 'нашёл баг', verdict: 'changes_requested',
  findings: [{ file: 'foo.ts', line: 3, severity: 'critical', title: 'реальный баг', body: 'null pointer', confidence: 0.9, model: 'm1' }],
};

test('pipeline: critical с confidence≥0.8 → блок (exit 1), inline + sticky запощены', async () => {
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews: [criticalReview] });
  assert.equal(code, 1, 'блокирующий finding → exit 1');
  assert.equal(calls.engineRuns, 1);
  assert.equal(calls.postReview.length, 1);
  assert.equal(calls.postReview[0].inline[0].path, 'foo.ts');
  assert.equal(calls.postReview[0].inline[0].line, 3);
  assert.ok(calls.upsertSticky[0].includes('Merge заблокирован'));
});

test('pipeline: «модуля нет» о файле из дерева репозитория вне PR → снят как FP, merge не блокирует', async () => {
  const absent = {
    ...criticalReview,
    findings: [{ ...criticalReview.findings[0], title: 'импорт несуществующего модуля', body: "Cannot find module './bar'" }],
  };
  const withTree = await runPipeline({
    diff: CODE_DIFF, reviews: [absent],
    mutateDeps: (deps) => { deps.repoFiles = async () => ['foo.ts', 'bar.ts']; },
  });
  assert.equal(withTree.code, 0, 'bar.ts есть в дереве — блокер ложный');
  assert.ok(withTree.calls.upsertSticky[0].includes('Отфильтровано как известные FP (1)'));

  const noTree = await runPipeline({ diff: CODE_DIFF, reviews: [absent] });
  assert.equal(noTree.code, 1, 'дерево не прочитано — фильтр выключен, гейт как раньше');

  const broken = await runPipeline({
    diff: CODE_DIFF, reviews: [absent],
    mutateDeps: (deps) => { deps.repoFiles = async () => { throw new Error('git сломан'); }; },
  });
  assert.equal(broken.code, 1, 'сбой чтения дерева — fail-open: фильтр выключен, прогон не падает');
});

test('pipeline: post-then-delete — старые инлайны чистятся ПОСЛЕ постинга, новые исключены', async () => {
  const { calls } = await runPipeline({ diff: CODE_DIFF, reviews: [criticalReview] });
  assert.deepEqual(calls.order.slice(0, 2), ['post', 'delete'], 'порядок: postReview → deleteStaleInline');
  assert.equal(calls.deleteStaleInline[0].exceptReviewId, 555, 'свежий review исключён из чистки');
});

test('pipeline: только minor → exit 0, merge не блокируется', async () => {
  const minor = { ...criticalReview, findings: [{ ...criticalReview.findings[0], severity: 'minor' }] };
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews: [minor] });
  assert.equal(code, 0);
  assert.ok(calls.upsertSticky[0].includes('есть замечания'));
  assert.ok(!calls.upsertSticky[0].includes('Merge заблокирован'));
});

test('pipeline: все модели недоступны → fail-closed (exit 1), sticky с блоком merge', async () => {
  const reviews = [
    { model: 'm1', error: 'Ollama 500: boom internal detail' },
    { model: 'm2', error: 'Ollama 429: limit' },
  ];
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews });
  assert.equal(code, 1, 'REVIEW_LLM_REQUIRED=1 (default): без ревью merge заблокирован');
  assert.equal(calls.postReview.length, 0, 'inline не постится');
  assert.deepEqual(calls.order, ['delete', 'sticky'], 'без prescan inline чистка предшествует sticky');
  assert.ok(calls.upsertSticky[0].includes('недоступно'));
  assert.ok(calls.upsertSticky[0].includes('заблокирован'));
  assert.ok(calls.upsertSticky[0].includes('m1'), 'модель указана');
  assert.ok(calls.upsertSticky[0].includes('HTTP 500'), 'HTTP-статус санитизирован');
  assert.ok(calls.upsertSticky[0].includes('m2'), 'вторая модель указана');
  assert.ok(calls.upsertSticky[0].includes('HTTP 429'), 'ошибка второй модели видна');
  assert.ok(!calls.upsertSticky[0].includes('boom internal detail'));
});

test('pipeline: partial outage (одна модель упала) → результат доступной модели, exit 0', async () => {
  const reviews = [
    { model: 'm1', error: 'Ollama 500: boom internal detail' },
    { model: 'm2', summary: '', verdict: 'comment', findings: [{ file: 'foo.ts', line: 3, severity: 'minor', title: 'style', body: 'x', confidence: 0.9, model: 'm2' }] },
  ];
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews });
  assert.equal(code, 0, 'partial outage: доступная модель не нашла блокеров');
  assert.ok(calls.upsertSticky[0].includes('minor'));
});

test('pipeline: все модели недоступны + REVIEW_LLM_REQUIRED=0 → мягкий пропуск (exit 0)', async () => {
  const reviews = [
    { model: 'm1', error: 'Ollama 500: boom internal detail' },
    { model: 'm2', error: 'Ollama 429: limit' },
  ];
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews, env: { REVIEW_LLM_REQUIRED: '0' } });
  assert.equal(code, 0, 'fail-open: инфра-сбой merge не блокирует');
  assert.ok(calls.upsertSticky[0].includes('временно недоступно'));
  assert.ok(calls.upsertSticky[0].includes('не блокирует merge'));
});

test('pipeline: REVIEW_LLM_REQUIRED=0 + prescan block → prescan блокирует (exit 1)', async () => {
  const reviews = [
    { model: 'm1', error: 'Ollama 500: boom internal detail' },
    { model: 'm2', error: 'Ollama 429: limit' },
  ];
  const prescanFindings = [
    { file: 'foo.ts', line: 3, severity: 'critical', title: 'no-restricted-globals', body: 'x',
      model: 'eslint', confidence: 0.95, prescan: true, prescanBlock: true },
  ];
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews, prescanFindings, env: { REVIEW_LLM_REQUIRED: '0' } });
  assert.equal(code, 1, 'fail-open для LLM, но prescan-блокеры всё ещё блокируют');
  assert.ok(calls.upsertSticky[0].includes('no-restricted-globals'), 'prescan-блокер виден в sticky');
});

test('pipeline: prescanBlock=false с severity critical → не блокирует (флаг prescanBlock обязателен)', async () => {
  const reviews = [
    { model: 'm1', error: 'Ollama 500: boom internal detail' },
    { model: 'm2', error: 'Ollama 429: limit' },
  ];
  const prescanFindings = [
    { file: 'foo.ts', line: 3, severity: 'critical', title: 'style', body: 'x',
      model: 'eslint', confidence: 0.95, prescan: true, prescanBlock: false },
  ];
  const { code } = await runPipeline({ diff: CODE_DIFF, reviews, prescanFindings, env: { REVIEW_LLM_REQUIRED: '0' } });
  assert.equal(code, 0, 'critical prescan без prescanBlock не блокирует');
});

test('pipeline: prescanBlock=true с severity minor → не блокирует (severity не в blockOn)', async () => {
  const reviews = [
    { model: 'm1', error: 'Ollama 500: boom internal detail' },
    { model: 'm2', error: 'Ollama 429: limit' },
  ];
  const prescanFindings = [
    { file: 'foo.ts', line: 3, severity: 'minor', title: 'style', body: 'x',
      model: 'eslint', confidence: 0.95, prescan: true, prescanBlock: true },
  ];
  const { code } = await runPipeline({ diff: CODE_DIFF, reviews, prescanFindings, env: { REVIEW_LLM_REQUIRED: '0' } });
  assert.equal(code, 0, 'minor prescan не блокирует, т.к. severity не в REVIEW_BLOCK_ON');
});

test('pipeline: LLM недоступен, но prescan нашёл critical → находки не теряются в sticky и экспорте', async () => {
  const reviews = [
    { model: 'm1', error: 'Ollama 429: limit' },
    { model: 'm2', error: 'Ollama 500: boom' },
  ];
  const prescanFindings = [
    { file: 'foo.ts', line: 3, severity: 'critical', title: 'no-restricted-globals', body: 'x',
      model: 'eslint', confidence: 0.95, prescan: true, prescanBlock: true },
  ];
  const exportDir = await mkdtemp(join(tmpdir(), 'pr-review-export-'));
  try {
    const { code, calls } = await runPipeline({
      diff: CODE_DIFF, reviews, prescanFindings,
      env: { REVIEW_EXPORT: '1', REVIEW_EXPORT_DIR: exportDir },
    });
    assert.equal(code, 1, 'fail-closed: exit 1');
    assert.equal(calls.postReview.length, 1, 'prescan-блокер публикуется inline даже при LLM-аутедже');
    assert.equal(calls.postReview[0].inline[0].path, 'foo.ts');
    assert.ok(calls.upsertSticky[0].includes('Prescan-находки'), 'prescan-секция в sticky');
    assert.ok(calls.upsertSticky[0].includes('no-restricted-globals'), 'конкретная находка видна');
    const json = await readFile(join(exportDir, 'orakul-ai-review.json'), 'utf8');
    const payload = JSON.parse(json);
    assert.equal(payload.blockers.length, 1, 'prescan-блокер попал в экспорт');
    assert.equal(payload.llmFailed, true, 'флаг LLM-аутеджа в экспорте');
  } finally {
    await rm(exportDir, { recursive: true, force: true });
  }
});

test('pipeline: fail-closed пишет llm/gate observability и пингует Telegram после sticky', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-fail-closed-'));
  const reviews = [
    { model: 'm1', error: 'Ollama 429: limit' },
    { model: 'm2', error: 'Ollama 500: boom' },
  ];
  try {
    const { code, calls } = await runPipeline({
      diff: CODE_DIFF,
      reviews,
      observabilityDir: dir,
      notifyTelegram: async () => {},
    });
    assert.equal(code, 1);
    assert.deepEqual(calls.order.slice(-2), ['sticky', 'telegram']);
    assert.equal(calls.telegram.length, 1, 'блокирующий LLM-аутедж уведомляет дежурного');
    assert.equal(calls.telegram[0].reason, 'fail-closed: LLM outage');
    const records = (await readFile(join(dir, 'pr-review-runs.jsonl'), 'utf8'))
      .trim().split('\n').map(JSON.parse);
    assert.ok(records.some((record) => record.type === 'llm_failed'));
    const gate = records.find((record) => record.type === 'gate');
    assert.equal(gate?.wouldBlock, true);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

// Дифф по РЕАЛЬНОМУ файлу репозитория: readChangedFiles читает рабочий каталог, а окна
// имеет смысл мерить только на файле, который длиннее порога и правится точечно.
// lib/config.mjs — 20+ КБ и 400+ строк, два хунка на противоположных концах.
const WINDOWED_DIFF = `diff --git a/lib/config.mjs b/lib/config.mjs
--- a/lib/config.mjs
+++ b/lib/config.mjs
@@ -30,1 +30,2 @@
 // контекст хунка
+const nearTheTop = 1;
@@ -400,1 +400,2 @@
 // контекст хунка
+const nearTheBottom = 2;
`;

/** Прогон с записью чанков и события plan: и то и другое читается только тут. */
async function runWindowed(env) {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-ctx-window-'));
  let chunks = null;
  try {
    await runPipeline({
      diff: WINDOWED_DIFF,
      reviews: [{ model: 'm1', summary: 'ок', findings: [] }],
      observabilityDir: dir,
      env,
      mutateDeps: (deps) => {
        const orig = deps.engine.reviewAllModels;
        deps.engine.reviewAllModels = async (cs, ctx) => { chunks = cs; return orig(cs, ctx); };
      },
    });
    const records = (await readFile(join(dir, 'pr-review-runs.jsonl'), 'utf8'))
      .trim().split('\n').map(JSON.parse);
    return { chunks, plan: records.find((r) => r.type === 'plan') };
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

test('pipeline: окна контекста режут вход чанков, blast-radius считается по полному файлу', async () => {
  const on = await runWindowed({});
  const off = await runWindowed({ REVIEW_CONTEXT_MARGIN: '0' });

  assert.equal(on.plan.contextWindows.margin, 40, 'дефолт RFC 07');
  assert.equal(on.plan.contextWindows.files, 1, 'lib/config.mjs длиннее порога и правится точечно');
  assert.ok(on.plan.contextWindows.after < on.plan.contextWindows.before,
    `контекст урезан: ${on.plan.contextWindows.before} → ${on.plan.contextWindows.after}`);
  assert.deepEqual(on.chunks[0].windowedPaths, ['lib/config.mjs']);
  assert.ok(on.plan.chunkPlan[0].context_chars < off.plan.chunkPlan[0].context_chars,
    'в чанк уехало меньше символов, чем без окон');

  // Порядок стадий (RFC 07, решение 4): символы blast-radius берутся из ПОЛНОГО текста.
  // Поставь окна раньше — и здесь молча станет меньше символов.
  assert.equal(on.plan.symbols, off.plan.symbols, 'blast-radius те же символы с окнами и без');
});

test('pipeline: REVIEW_CONTEXT_MARGIN=0 — контекст прежний, чанки без windowedPaths (откат)', async () => {
  const off = await runWindowed({ REVIEW_CONTEXT_MARGIN: '0' });
  assert.equal(off.plan.contextWindows.margin, 0, '0 — значащее значение, а не повод взять дефолт 40');
  assert.equal(off.plan.contextWindows.files, 0);
  assert.equal(off.plan.contextWindows.after, off.plan.contextWindows.before, 'ничего не урезано');
  assert.equal(off.chunks[0].windowedPaths, undefined, 'поля нет → карта PR про окна молчит');
  assert.equal(off.plan.chunkPlan[0].windowed_paths, 0);
});

test('export R9: fp/verifierStatus/actionabilityScore в артефакте, fp совпадает с футером sticky', async () => {
  // Регресс, ради которого тест написан: `fp` считался только внутри buildSummary — в список
  // для футера. В артефакт уходил тот же массив находок, но без отпечатка, и потребитель
  // (cod-doc ingest) откатывался на дедуп по заголовку: переформулированная находка на той же
  // строке считалась новой.
  const findings = [
    { file: 'foo.ts', line: 3, severity: 'critical', title: 'реальный баг', body: 'npe', confidence: 0.9 },
    { file: 'foo.ts', line: 2, severity: 'minor', title: 'магическое число', body: '', confidence: 0.9, suggestion: 'const B = 3' },
  ];
  const reviews = ['m1', 'm2'].map((model) => ({
    model, summary: 's', verdict: 'changes_requested',
    findings: findings.map((f) => ({ ...f, model })),
  }));
  const verifyFindings = async (candidates) => candidates.map((_, i) => ({ candidateIndex: i, decision: 'verified' }));
  const exportDir = await mkdtemp(join(tmpdir(), 'pr-review-export-fp-'));
  try {
    const { calls } = await runPipeline({
      diff: CODE_DIFF, reviews, verifyFindings,
      env: { REVIEW_EXPORT: '1', REVIEW_EXPORT_DIR: exportDir, REVIEW_VERIFIER: '1' },
    });
    const payload = JSON.parse(await readFile(join(exportDir, 'orakul-ai-review.json'), 'utf8'));
    assert.equal(payload.version, 2, 'версия артефакта поднята вместе с полями');

    const blocker = payload.findings.find((f) => f.severity === 'critical');
    assert.ok(blocker.fp, 'отпечаток кода в артефакте');
    assert.equal(blocker.verifierStatus, 'verified', 'вердикт verifier доезжает до артефакта');
    assert.ok(Number.isFinite(blocker.actionabilityScore), 'скор есть и у блокера, не только у поднятой advisory');

    const footerRaw = MARKERS.dataRe.exec(calls.upsertSticky[0]);
    assert.ok(footerRaw, 'машинный футер в sticky');
    const footerFp = new Map(
      JSON.parse(footerRaw[1].trim()).findings.map((f) => [`${f.file}:${f.line}`, f.fp]),
    );
    assert.ok(footerFp.size, 'футер несёт отпечатки');
    for (const f of payload.findings) {
      assert.equal(f.fp, footerFp.get(`${f.file}:${f.line}`),
        'артефакт и футер показывают ОДИН отпечаток: иначе carry-over и дедуп потребителя разъедутся');
    }
  } finally {
    await rm(exportDir, { recursive: true, force: true });
  }
});

test('runGateResolution: exported helper filters before resolving blockers', async () => {
  const finding = {
    file: 'foo.ts', line: 3, severity: 'critical', title: 'реальный баг', body: '',
    confidence: 0.9, model: 'm1',
  };
  const result = await runGateResolution({
    findings: [finding],
    changedPaths: ['foo.ts'],
    gate: { blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 2, confidenceMin: 0.8 },
    log: () => {},
  });
  assert.equal(result.findings[0], finding);
  assert.equal(result.blockers.length, 1);
});

test('pipeline: дедлайн-пропуски чанков → пометка «охват частичный» в sticky', async () => {
  const reviews = [{ model: 'm1', summary: '', verdict: 'comment', findings: [], skippedChunks: 2 }];
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews });
  assert.equal(code, 0);
  assert.ok(calls.upsertSticky[0].includes('охват частичный'));
});

test('pipeline: router skip (lockfile-only) → LLM не вызывается, sticky «пропуск»', async () => {
  const { code, calls } = await runPipeline({ diff: LOCK_DIFF, reviews: [criticalReview] });
  assert.equal(code, 0);
  assert.equal(calls.engineRuns, 0, 'engine не вызывался');
  assert.equal(calls.postReview.length, 0);
  assert.ok(calls.upsertSticky[0].includes('пропуск LLM'));
});

test('pipeline: пустой blast-radius сохраняет измеримые причины в plan JSONL', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-blast-'));
  const metrics = {
    symbols_total: 1,
    symbols_resolved: 0,
    affected_nonempty: 0,
    graph_fresh: false,
    skip_reason: 'graph_stale',
    skip_reasons: { graph_stale: 1, symbol_unresolved: 1 },
  };
  try {
    await runPipeline({
      diff: CODE_DIFF,
      reviews: [{ model: 'm1', summary: '', verdict: 'comment', findings: [] }],
      graphify: { buildBlastRadius: async () => ({ blocks: [], metrics }) },
      observabilityDir: dir,
    });
    const records = (await readFile(join(dir, 'pr-review-runs.jsonl'), 'utf8'))
      .trim().split('\n').map(JSON.parse);
    const plan = records.find((record) => record.type === 'plan');
    assert.deepEqual(plan.blast_radius, metrics);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('pipeline: REVIEW_BLAST_REQUIRED=1 + нет графа → fail-closed до LLM (exit 1), sticky', async () => {
  const graphify = { buildBlastRadius: async () => ({ blocks: [], metrics: {
    symbols_total: 1, symbols_resolved: 0, affected_nonempty: 0, graph_fresh: null,
    graph_status: 'missing', skip_reason: 'graph_missing', skip_reasons: { graph_missing: 1 },
  } }) };
  const reviews = [{ model: 'm1', summary: '', verdict: 'comment', findings: [] }];
  const required = await runPipeline({ diff: CODE_DIFF, reviews, graphify, env: { REVIEW_BLAST_REQUIRED: '1' } });
  assert.equal(required.code, 1, 'без графа merge заблокирован');
  assert.equal(required.calls.engineRuns, 0, 'LLM не вызывался');
  assert.equal(required.calls.upsertSticky.length, 1);
  assert.ok(required.calls.upsertSticky[0].includes('graph_missing'));
  assert.ok(required.calls.upsertSticky[0].includes('REVIEW_BLAST_REQUIRED=1'));

  const optional = await runPipeline({ diff: CODE_DIFF, reviews, graphify });
  assert.equal(optional.code, 0, 'дефолт 0: отсутствие графа — fail-open');
  assert.equal(optional.calls.engineRuns, 1);
});

test('pipeline: REVIEW_BLAST_REQUIRED=1 + граф есть, зависимых нет → обычный прогон', async () => {
  const graphify = { buildBlastRadius: async () => ({ blocks: [], metrics: {
    symbols_total: 1, symbols_resolved: 1, affected_nonempty: 0, graph_fresh: true,
    graph_status: 'fresh', skip_reason: 'no_dependents', skip_reasons: { no_dependents: 1 },
  } }) };
  const reviews = [{ model: 'm1', summary: '', verdict: 'comment', findings: [] }];
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews, graphify, env: { REVIEW_BLAST_REQUIRED: '1' } });
  assert.equal(code, 0);
  assert.equal(calls.engineRuns, 1);
});

const thirteenCriticals = () => Array.from({ length: 13 }, (_, i) => ({
  file: 'foo.ts', line: i + 1, severity: 'critical', title: `баг ${i + 1}`, body: '', confidence: 0.9, model: 'm1',
}));
const verifyAllBatch = async (candidates) => candidates.map((_, i) => ({ candidateIndex: i, decision: 'verified' }));

test('pipeline: verifier — 13-я находка уходит вторым батчем и блокирует наравне с первыми (RFC 17)', async () => {
  const reviews = [{ model: 'm1', summary: 's', verdict: 'changes_requested', findings: thirteenCriticals() }];
  const batchSizes = [];
  const verifyFindings = async (candidates) => { batchSizes.push(candidates.length); return verifyAllBatch(candidates); };
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews, verifyFindings, env: { REVIEW_VERIFIER: '1' } });
  assert.equal(code, 1);
  assert.deepEqual(batchSizes, [12, 1], 'кап одного вызова держится, хвост — отдельным батчем');
  const md = calls.upsertSticky[0];
  assert.ok(md.includes('🔴13 🟠0 🟡0'), 'все 13 critical проверены, ни одна не понижена как overflow');
});

test('pipeline: verifier — вердикт второго батча ложится на свою находку, а не на находку первого', async () => {
  const reviews = [{ model: 'm1', summary: 's', verdict: 'changes_requested', findings: thirteenCriticals() }];
  let batch = 0;
  // второй батч отклоняет свой единственный кандидат (индекс 0 внутри батча)
  const verifyFindings = async (candidates) => (batch++ === 0
    ? verifyAllBatch(candidates)
    : [{ candidateIndex: 0, decision: 'rejected', reason: 'нет бага' }]);
  const { calls } = await runPipeline({ diff: CODE_DIFF, reviews, verifyFindings, env: { REVIEW_VERIFIER: '1' } });
  const md = calls.upsertSticky[0];
  // 🔴12, а не 🔴11: rejected с индексом 0 второго батча не лёг на индекс 0 первого
  assert.ok(md.includes('🔴12'), 'первые 12 подтверждены');
  assert.ok(!md.includes('баг 13'), '13-я отклонена своим вердиктом');
});

test('pipeline: verifier strict — сверх капа × батчей (REVIEW_VERIFIER_BATCHES=1) 13-я понижается до minor', async () => {
  const reviews = [{ model: 'm1', summary: 's', verdict: 'changes_requested', findings: thirteenCriticals() }];
  const { code, calls } = await runPipeline({
    diff: CODE_DIFF, reviews, verifyFindings: verifyAllBatch, env: { REVIEW_VERIFIER: '1', REVIEW_VERIFIER_BATCHES: '1' },
  });
  assert.equal(code, 1, '12 верифицированных critical блокируют');
  const md = calls.upsertSticky[0];
  assert.ok(md.includes('🔴12'), '12 critical после verifier');
  assert.ok(md.includes('🟡1'), '13-я находка (overflow капа) → minor');
});

test('pipeline: verifier — сбой хвостового батча не отменяет вердикты первого', async () => {
  const reviews = [{ model: 'm1', summary: 's', verdict: 'changes_requested', findings: thirteenCriticals() }];
  let batch = 0;
  const verifyFindings = async (candidates) => {
    if (batch++ > 0) throw new Error('rate limit');
    return candidates.map((_, i) => ({ candidateIndex: i, decision: i === 0 ? 'rejected' : 'verified' }));
  };
  const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews, verifyFindings, env: { REVIEW_VERIFIER: '1' } });
  assert.equal(code, 1);
  const md = calls.upsertSticky[0];
  assert.ok(md.includes('🔴11'), '11 подтверждены, 1 отклонён в первом батче');
  assert.ok(md.includes('🟡1'), 'непроверенный хвост → minor, как сверх капа');
});

test('pipeline: sticky throw после postReview — ошибка не молчит (inline уже ушёл)', async () => {
  let calls;
  await assert.rejects(
    () => runPipeline({
      diff: CODE_DIFF,
      reviews: [criticalReview],
      mutateDeps: (deps, c) => {
        calls = c;
        deps.github.upsertSticky = async (body) => {
          c.order.push('sticky');
          c.upsertSticky.push(body);
          throw new Error('GitHub 502: sticky upsert failed');
        };
      },
    }),
    /sticky upsert failed/,
  );
  assert.deepEqual(calls.order.slice(0, 2), ['post', 'delete'], 'inline успел уйти до sticky');
  assert.ok(calls.order.includes('sticky'), 'upsertSticky вызывался и упал');
});

// --- инкрементальный прогон (RFC 05) ---

const TWO_FILE_DIFF = `diff --git a/foo.ts b/foo.ts
--- a/foo.ts
+++ b/foo.ts
@@ -1,2 +1,3 @@
 const a = 1;
-const b = 2;
+const b = 3;
+export function foo() { return a; }
diff --git a/bar.ts b/bar.ts
--- a/bar.ts
+++ b/bar.ts
@@ -1,1 +1,2 @@
 const c = 1;
+export function bar() { return c; }
`;

// Дифф, в котором изменился ТОЛЬКО foo.ts: вход по bar.ts побайтово тот же.
const TWO_FILE_DIFF_V2 = TWO_FILE_DIFF.replace('const b = 3;', 'const b = 4;');

const barCritical = {
  model: 'm1', summary: 'нашёл баг в bar', verdict: 'changes_requested',
  findings: [{
    file: 'bar.ts', line: 2, severity: 'critical', title: 'блокер в bar', body: 'детали',
    confidence: 0.9, model: 'm1', quote: 'export function bar() { return c; }',
  }],
};

async function runIncremental(diff, stateFile, extraEnv = {}, reviews = [barCritical]) {
  return runPipeline({
    diff,
    reviews,
    env: { REVIEW_INCREMENTAL: '1', REVIEW_INCREMENTAL_STATE: stateFile, ...extraEnv },
  });
}

test('инкрементально: неизменившийся файл не ревьюится заново, его блокер переносится', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-incr-'));
  const stateFile = join(dir, 'state.json');
  try {
    const first = await runIncremental(TWO_FILE_DIFF, stateFile);
    assert.equal(first.code, 1, 'первый прогон блокирует');
    assert.equal(first.calls.engineRuns, 1);
    const state = JSON.parse(await readFile(stateFile, 'utf8'));
    assert.equal(state.findings.length, 1, 'находка сохранена для переноса');
    assert.ok(state.hashes['bar.ts'] && state.hashes['foo.ts'], 'хэши входа по обоим файлам');

    const noFindings = { model: 'm1', summary: 'по foo чисто', verdict: 'comment', findings: [] };
    const second = await runIncremental(TWO_FILE_DIFF_V2, stateFile, {}, [noFindings]);
    assert.equal(second.code, 1, 'блокер по неизменившемуся файлу НЕ потерян');
    const sticky = second.calls.upsertSticky[0];
    assert.ok(sticky.includes('блокер в bar'), 'перенесённая находка в отчёте');
    assert.ok(sticky.includes('Инкрементальный прогон'), 'в отчёте сказано, что часть PR не ревьюилась заново');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('инкрементально: изменившийся файл теряет прошлую находку — её решает свежий прогон', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-incr2-'));
  const stateFile = join(dir, 'state.json');
  try {
    const fooCritical = {
      model: 'm1', summary: 'баг в foo', verdict: 'changes_requested',
      findings: [{
        file: 'foo.ts', line: 3, severity: 'critical', title: 'блокер в foo', body: 'детали',
        confidence: 0.9, model: 'm1',
      }],
    };
    await runIncremental(TWO_FILE_DIFF, stateFile, {}, [fooCritical]);
    const clean = { model: 'm1', summary: 'чисто', verdict: 'approve', findings: [] };
    const second = await runIncremental(TWO_FILE_DIFF_V2, stateFile, {}, [clean]);
    assert.equal(second.code, 0, 'находка по изменённому файлу не переносится — блокировки нет');
    assert.ok(!second.calls.upsertSticky[0].includes('блокер в foo'), 'старая находка не всплыла');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('инкрементально: смена конфигурации заставляет прогон быть полным', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-incr3-'));
  const stateFile = join(dir, 'state.json');
  try {
    await runIncremental(TWO_FILE_DIFF, stateFile);
    const noFindings = { model: 'm1', summary: 'чисто', verdict: 'comment', findings: [] };
    const second = await runIncremental(TWO_FILE_DIFF, stateFile, { OLLAMA_MODELS: 'm3,m4' }, [noFindings]);
    assert.equal(second.code, 0, 'перенос не выполнен: прогон полный, находок нет');
    assert.ok(!second.calls.upsertSticky[0].includes('Инкрементальный прогон'));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('инкрементально: без изменений LLM не вызывается, отчёт собран из переноса', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-incr4-'));
  const stateFile = join(dir, 'state.json');
  try {
    await runIncremental(TWO_FILE_DIFF, stateFile);
    const second = await runIncremental(TWO_FILE_DIFF, stateFile, {}, []);
    assert.equal(second.calls.engineRuns, 0, 'ни одного вызова движка');
    assert.equal(second.code, 1, 'перенесённый блокер по-прежнему блокирует');
    assert.ok(second.calls.upsertSticky[0].includes('блокер в bar'));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('инкрементально: выключено по умолчанию — состояние не пишется', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-incr5-'));
  const stateFile = join(dir, 'state.json');
  try {
    await runPipeline({
      diff: TWO_FILE_DIFF,
      reviews: [barCritical],
      env: { REVIEW_INCREMENTAL_STATE: stateFile },
    });
    await assert.rejects(() => readFile(stateFile, 'utf8'), 'файла состояния нет');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

// Существующие инкрементальные тесты гоняются в REVIEW_MODE=consensus / REVIEW_VERIFIER=0:
// applyRefutation и Pass B не вызываются, и перенос «до» них не падал бы. Этот кейс —
// ровно тот, из-за которого carry стоит после verifier: во втором прогоне жюри по bar
// молчит, strict без вердикта опустил бы подтверждённый блокер до minor.
test('инкрементально: перенесённый блокер переживает verifier strict второго прогона', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-incr-ver-'));
  const stateFile = join(dir, 'state.json');
  const verifyAll = async (candidates) => candidates.map((_, i) => ({ candidateIndex: i, decision: 'verified' }));
  try {
    const first = await runPipeline({
      diff: TWO_FILE_DIFF,
      reviews: [barCritical],
      verifyFindings: verifyAll,
      env: {
        REVIEW_INCREMENTAL: '1',
        REVIEW_INCREMENTAL_STATE: stateFile,
        REVIEW_VERIFIER: '1',
      },
    });
    assert.equal(first.code, 1, 'первый прогон: verifier подтвердил блокер');

    const second = await runPipeline({
      diff: TWO_FILE_DIFF_V2,
      reviews: [{ model: 'm1', summary: 'по foo чисто', verdict: 'comment', findings: [] }],
      verifyFindings: async () => [],
      env: {
        REVIEW_INCREMENTAL: '1',
        REVIEW_INCREMENTAL_STATE: stateFile,
        REVIEW_VERIFIER: '1',
      },
    });
    assert.equal(second.code, 1,
      'если carry был бы до Pass B, пустые вердикты + REVIEW_VERIFIER_STRICT опустили бы бар до minor');
    assert.ok(second.calls.upsertSticky[0].includes('блокер в bar'));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('инкрементально: находка без вердикта из-за сбоя verifier не переносится — файл ревьюится заново (RFC 18)', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-incr-vu-'));
  const stateFile = join(dir, 'state.json');
  const env = { REVIEW_INCREMENTAL: '1', REVIEW_INCREMENTAL_STATE: stateFile, REVIEW_VERIFIER: '1' };
  try {
    const first = await runPipeline({
      diff: TWO_FILE_DIFF, reviews: [structuredClone(barCritical)], env,
      verifyFindings: async () => { throw new Error('503'); },
    });
    assert.equal(first.code, 1, 'degraded-блок: verifier недоступен');

    const reviewedPaths = [];
    const second = await runPipeline({
      diff: TWO_FILE_DIFF_V2, reviews: [structuredClone(barCritical)], env,
      verifyFindings: async (c) => c.map((_, i) => ({ candidateIndex: i, decision: 'verified' })),
      mutateDeps: (deps) => {
        const inner = deps.engine.reviewAllModels;
        deps.engine.reviewAllModels = async (chunks, ctx) => {
          for (const c of chunks) reviewedPaths.push(...(c.paths || []));
          return inner(chunks, ctx);
        };
      },
    });
    assert.ok(reviewedPaths.includes('bar.ts'),
      'bar.ts не изменился, но его находка не прошла verifier — файл обязан уйти в LLM заново, а не в перенос');
    assert.equal(second.code, 1);
    assert.ok(second.calls.upsertSticky[0].includes('нужны правки'), 'теперь блокер подтверждён verifier\'ом');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// Порог допуска advisory к verifier'у (P1, docs/review-quality.md §2.2)
// ---------------------------------------------------------------------------

// minor в логическом файле с confidence ≥ 0.8 и предложенным фиксом: confidence(+1) +
// suggestion(+1) + logicFile(+1) = 3 балла при пороге маршрута 4. Ровно тот класс находок,
// который в замере R2 не доезжал ни до verifier'а, ни до строки.
//
// Фабрика, а не константа: пайплайн ходит по ТЕМ ЖЕ объектам находок (маршрут и verifier
// пишут вердикты прямо в них), поэтому общий литерал протёк бы вердиктом из теста в тест.
const advisoryMinorReview = () => ({
  model: 'm1', summary: 'мелочь', verdict: 'comment',
  findings: [{
    file: 'foo.ts', line: 2, severity: 'minor', title: 'магическое число', body: 'вынести в константу',
    confidence: 0.9, suggestion: 'const B = 3;', model: 'm1',
  }],
});

/** Прогон с перехватом того, что реально ушло в engine.verifyFindings. */
async function runWithVerifierSpy(env) {
  const seen = [];
  const { code, calls } = await runPipeline({
    diff: CODE_DIFF,
    reviews: [advisoryMinorReview()],
    verifyFindings: async (candidates) => {
      // Скор снимаем СРАЗУ: дальше по пайплайну маршрут пересчитает его в тех же объектах.
      seen.push(...candidates.map((c) => ({ line: c.line, score: c.actionabilityScore })));
      return candidates.map((_, i) => ({ candidateIndex: i, decision: 'verified' }));
    },
    env: { REVIEW_VERIFIER: '1', ...env },
  });
  return { code, calls, seen };
}

test('verifier: minor со скором 3 (ниже порога маршрута) допущен по пониженному порогу и после verified уходит в строку', async () => {
  const { code, calls, seen } = await runWithVerifierSpy({});
  assert.equal(code, 0, 'minor не блокирует merge даже подтверждённым');
  assert.equal(seen.length, 1, 'находка дошла до verifier — курица и яйцо разорваны');
  assert.deepEqual(seen[0], { line: 2, score: 3 }, 'до verifier у неё ровно 3 балла при пороге 4');
  assert.deepEqual(
    calls.postReview[0].inline.map((c) => [c.path, c.line]),
    [['foo.ts', 2]],
    '3 + вес(verified)=3 → 6 ≥ 4: находка занимает строку',
  );
});

test('verifier: REVIEW_VERIFY_ADVISORY_MIN=99 — та же находка до verifier не доезжает и в строку не идёт', async () => {
  const { code, calls, seen } = await runWithVerifierSpy({ REVIEW_VERIFY_ADVISORY_MIN: '99' });
  assert.equal(code, 0);
  assert.equal(seen.length, 0, 'порог допуска недостижим — кандидатов нет');
  assert.deepEqual(calls.postReview[0].inline, [], 'без подтверждения находка в строку не идёт');
  assert.ok(calls.upsertSticky[0].includes('магическое число'), 'но из отчёта не исчезает');
});

test('observability: событие actionability несёт гистограмму скоров и топ причин свода', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-actionability-'));
  try {
    await runPipeline({
      diff: CODE_DIFF,
      reviews: [advisoryMinorReview()],
      observabilityDir: dir,
    });
    const records = (await readFile(join(dir, 'pr-review-runs.jsonl'), 'utf8'))
      .trim().split('\n').map(JSON.parse);
    const ev = records.find((r) => r.type === 'actionability');
    assert.ok(ev, 'событие есть');
    assert.equal(ev.threshold, 4);
    assert.equal(ev.inline, 0);
    assert.equal(ev.outside, 1);
    assert.deepEqual(ev.histogram, { 3: 1 }, 'скор → число находок');
    assert.deepEqual(
      ev.topOutsideReasons.map((r) => r.key).sort(),
      ['confidence', 'logicFile', 'suggestion'],
      'причины отдаются КЛЮЧАМИ сигналов, а не формулировками из комментария',
    );
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('pipeline: runDetailed отдаёт артефакт с cli.headSha на file-diff, без GitHub', async () => {
  const tmp = join(tmpdir(), `pr-review-headsha-${process.pid}-${Math.random().toString(36).slice(2)}.patch`);
  await writeFile(tmp, CODE_DIFF);
  try {
    const cfg = loadReviewConfig({
      ...ENV,
      REVIEW_EXPORT: '0',
    }, { dryRun: true, diffFile: tmp, repo: '', prNumber: 0, headSha: 'abc1234deadbeef' });
    const { deps, calls } = mkDeps({ reviews: [criticalReview] });
    const detailed = await createReviewPipeline(cfg, deps).runDetailed();
    assert.equal(detailed.artifact.headSha, 'abc1234deadbeef');
    assert.equal(detailed.exitCode, 1);
    assert.equal(detailed.wouldBlock, true);
    assert.equal(calls.postReview.length, 0, 'dry-run не постит review');
    assert.equal(calls.upsertSticky.length, 0);
  } finally {
    await rm(tmp, { force: true });
  }
});

// RFC 18: сбой verifier'а — одно состояние «недоступен» вместо исхода, зависящего от вида сбоя.
// Раньше: исключение → блок по слову агрегатора без строки verifier в sticky; пустой ответ →
// critical молча понижен, merge проходит.
const VU_ENV = { REVIEW_VERIFIER: '1' };
// Пайплайн мутирует находки (verifier понижает severity) — каждому прогону свежая копия.
const freshCritical = () => structuredClone(criticalReview);
const verifierFailures = {
  'исключение (сеть/5xx)': async () => { throw new Error('503'); },
  'пустой ответ': async () => [],
  'вердикты с чужими индексами': async () => [{ candidateIndex: 7, decision: 'verified' }],
};
for (const [label, verifyFindings] of Object.entries(verifierFailures)) {
  test(`pipeline: verifier недоступен (${label}) → degraded-блок, critical не выдаётся за подтверждённый`, async () => {
    const { code, calls } = await runPipeline({ diff: CODE_DIFF, reviews: [freshCritical()], verifyFindings, env: VU_ENV });
    assert.equal(code, 1, 'REVIEW_LLM_REQUIRED=1 (дефолт): merge заблокирован до перезапуска');
    const md = calls.upsertSticky[0];
    assert.ok(md.includes('⛔ ревью не завершено'), md.slice(0, 300));
    assert.ok(md.includes('Merge заблокирован: verifier недоступен'));
    assert.ok(md.includes('🔴0'), 'непроверенный critical не считается блокером');
    assert.ok(!md.includes('нужны правки'));
  });
}

test('pipeline: verifier недоступен + REVIEW_LLM_REQUIRED=0 → fail-open с заметкой', async () => {
  const { code, calls } = await runPipeline({
    diff: CODE_DIFF, reviews: [freshCritical()], env: { ...VU_ENV, REVIEW_LLM_REQUIRED: '0' },
    verifyFindings: async () => { throw new Error('503'); },
  });
  assert.equal(code, 0);
  const md = calls.upsertSticky[0];
  assert.ok(md.includes('⚠️ **Verifier недоступен**'));
  assert.ok(md.includes('merge не блокируется'));
});

test('pipeline: тайм-бюджет перед verifier → то же состояние «недоступен»', async () => {
  const { code, calls } = await runPipeline({
    diff: CODE_DIFF, reviews: [freshCritical()], env: { ...VU_ENV, REVIEW_TIME_BUDGET_MIN: '1' },
    verifyFindings: async (c) => c.map((_, i) => ({ candidateIndex: i, decision: 'verified' })),
  });
  assert.equal(code, 1);
  assert.ok(calls.upsertSticky[0].includes('verifier недоступен** (time_budget)'));
});

test('pipeline: REVIEW_VERIFIER_STRICT=0 — сбой verifier ведёт себя как раньше (доверие агрегатору)', async () => {
  const { code, calls } = await runPipeline({
    diff: CODE_DIFF, reviews: [freshCritical()], env: { ...VU_ENV, REVIEW_VERIFIER_STRICT: '0' },
    verifyFindings: async () => { throw new Error('503'); },
  });
  assert.equal(code, 1, 'critical остаётся и блокирует');
  const md = calls.upsertSticky[0];
  assert.ok(md.includes('нужны правки'));
  assert.ok(!md.includes('verifier недоступен'));
});

test('pipeline: verifier без кандидатов уровня блока — его сбой гейт не трогает', async () => {
  const minorOnly = freshCritical();
  minorOnly.findings[0].severity = 'minor';
  const { code, calls } = await runPipeline({
    diff: CODE_DIFF, reviews: [minorOnly], env: VU_ENV,
    verifyFindings: async () => { throw new Error('503'); },
  });
  assert.equal(code, 0);
  assert.ok(!calls.upsertSticky[0].includes('verifier недоступен'));
});

// ---------------------------------------------------------------------------
// RFC 19: покрытие по чанкам — упавшая часть PR не выдаётся за отревьюенную
// ---------------------------------------------------------------------------

const okEmpty = (model) => ({ model, summary: 'ok', verdict: 'comment', findings: [] });

test('pipeline: чанк упал у всех моделей → degraded-блок «ревью неполное» (RFC 19)', async () => {
  const { code, calls } = await runPipeline({
    diff: CODE_DIFF,
    reviews: [{ ...okEmpty('m1'), failedChunks: [0] }, { ...okEmpty('m2'), failedChunks: [0] }],
  });
  assert.equal(code, 1, 'REVIEW_LLM_REQUIRED=1 (дефолт): часть PR без ревью блокирует до перезапуска');
  const md = calls.upsertSticky[0];
  assert.ok(md.includes('⛔ ревью не завершено'));
  assert.ok(md.includes('Merge заблокирован: ревью неполное** — 1 из 1 частей PR'));
});

test('pipeline: чанк упал у одной модели, другая его отревьюила → покрыт, не блок', async () => {
  const { code, calls } = await runPipeline({
    diff: CODE_DIFF,
    reviews: [{ ...okEmpty('m1'), failedChunks: [0] }, okEmpty('m2')],
  });
  assert.equal(code, 0);
  assert.ok(!calls.upsertSticky[0].includes('ревью неполное'));
});

test('pipeline: часть PR без ревью + REVIEW_LLM_REQUIRED=0 → fail-open с заметкой', async () => {
  const { code, calls } = await runPipeline({
    diff: CODE_DIFF,
    reviews: [{ ...okEmpty('m1'), failedChunks: [0] }, { ...okEmpty('m2'), failedChunks: [0] }],
    env: { REVIEW_LLM_REQUIRED: '0' },
  });
  assert.equal(code, 0);
  assert.ok(calls.upsertSticky[0].includes('⚠️ **Ревью неполное**'));
});

test('инкрементально: файл упавшей части PR ревьюится заново, а не считается просмотренным (RFC 19)', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'pr-review-incr-cov-'));
  const stateFile = join(dir, 'state.json');
  const env = { REVIEW_INCREMENTAL: '1', REVIEW_INCREMENTAL_STATE: stateFile, REVIEW_LLM_REQUIRED: '0' };
  try {
    const failBar = (chunks) => {
      const ci = chunks.findIndex((c) => (c.paths || []).includes('bar.ts'));
      return ci >= 0 ? [ci] : [];
    };
    await runPipeline({
      diff: TWO_FILE_DIFF, reviews: [], env,
      mutateDeps: (deps) => {
        deps.engine.reviewAllModels = async (chunks) => ['m1', 'm2'].map((m) => ({ ...okEmpty(m), failedChunks: failBar(chunks) }));
      },
    });
    const reviewedPaths = [];
    await runPipeline({
      diff: TWO_FILE_DIFF_V2, reviews: [], env,
      mutateDeps: (deps) => {
        deps.engine.reviewAllModels = async (chunks) => {
          for (const c of chunks) reviewedPaths.push(...(c.paths || []));
          return ['m1', 'm2'].map(okEmpty);
        };
      },
    });
    assert.ok(reviewedPaths.includes('bar.ts'),
      'bar.ts не изменился, но в первом прогоне его часть упала — он обязан уйти в LLM заново');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('engine: индексы чанков, упавших у модели, доходят до результата (RFC 19)', async () => {
  const { createReviewEngine } = await import('../lib/engine.mjs');
  const { createPromptBuilder } = await import('../lib/prompt.mjs');
  const cfg = loadReviewConfig({ ...ENV, OLLAMA_MODELS: 'm1' }, { dryRun: true, diffFile: '/dev/null', repo: 'o/r', prNumber: 1 });
  // Падает ровно вызов по чанку b.ts: заголовок его диффа есть только в его промпте.
  const ollama = {
    chatJson: async (messages) => {
      if (JSON.stringify(messages).includes('diff --git a/b.ts')) throw new Error('503');
      return { summary: 's', findings: [] };
    },
  };
  const engine = createReviewEngine(cfg, { ollama, prompt: createPromptBuilder(), log: () => {} });
  const chunk = (p) => ({ annotated: `diff --git a/${p} b/${p}\n`, fileContents: '', paths: [p] });
  const results = await engine.reviewAllModels([chunk('a.ts'), chunk('b.ts'), chunk('c.ts')], {
    blastBlocks: [], pr: { number: 1, title: 't', body: '' }, priorFindings: [], changedPaths: ['a.ts', 'b.ts', 'c.ts'],
    prescanSection: '', deadline: 0,
  });
  assert.equal(results.length, 1);
  assert.ok(!results[0].error, 'частичный сбой — не ошибка модели целиком');
  assert.deepEqual(results[0].failedChunks, [1]);
  assert.deepEqual(results[0].skippedChunkIndexes, []);
});
