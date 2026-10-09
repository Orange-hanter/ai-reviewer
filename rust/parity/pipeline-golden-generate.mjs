#!/usr/bin/env node
/**
 * Генератор golden-фикстур пайплайна (RFC 16, решение 7 — golden-harness).
 *
 * Зачем: `pipeline.mjs` будет удалён после cutover, а `rust/src/pipeline.rs` (T26) обязан
 * повторить его поведение байт в байт. Эталон нельзя держать «в голове» и нельзя сверять
 * прогоном Node в рантайме теста — поэтому вход и выход JS-прогона фиксируются на диске
 * ДО удаления, а Rust-тест (rust/tests/pipeline_golden.rs) читает готовые файлы.
 *
 * Что именно фиксируется: полный `run()` с подменёнными зависимостями (github/engine/
 * prescan/graphify/notifyTelegram) — тот же DI, что в test/pr-review.pipeline.test.mjs.
 * Сеть и LLM не задействованы.
 *
 * Детерминизм:
 *   - часы заморожены (`Date.now()` и `new Date()` → FIXED_NOW) — иначе `generatedAt`
 *     в export.json и дедлайн тайм-бюджета плавают между прогонами;
 *   - observability выключена (её runId = Date.now + randomBytes — единственный
 *     оставшийся источник случайности в пайплайне);
 *   - все модули принудительно идут по JS-ветке (AI_REVIEWER_*_FORCE_JS=1): эталон —
 *     это поведение JS, а не уже портированного Rust-модуля;
 *   - cwd = корень репозитория (от него резолвятся профиль, fp-patterns и readChangedFiles).
 *
 * Запуск:  node rust/parity/pipeline-golden-generate.mjs
 * Формат:  rust/parity/README-golden.md
 */

import { mkdir, rm, writeFile, readFile, readdir, copyFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '../..');
const OUT_ROOT = join(HERE, 'golden-fixtures');
const FIXTURES_ROOT = join(REPO_ROOT, 'fixtures');

// ---------------------------------------------------------------------------
// Заморозка часов и JS-ветки модулей — до первого import движка
// ---------------------------------------------------------------------------

const FIXED_NOW_ISO = '2026-01-01T00:00:00.000Z';
const FIXED_NOW = Date.parse(FIXED_NOW_ISO);

const RealDate = Date;
class FrozenDate extends RealDate {
  constructor(...args) {
    if (args.length === 0) super(FIXED_NOW);
    else super(...args);
  }

  static now() { return FIXED_NOW; }
}
globalThis.Date = FrozenDate;

// Список синхронизирован с `grep -rn FORCE_JS lib/`: каждый модуль-мост умеет уходить
// в Rust-бинарь, если тот собран. Golden обязан быть снимком JS-эталона.
const FORCE_JS_FLAGS = [
  'ACTIONABILITY', 'CONFIG', 'DIFF', 'ENGINE', 'EXPORT', 'FINDINGS', 'GITHUB',
  'JSON_REPAIR', 'LLM', 'METRICS', 'PROMPT', 'REPORT', 'ROUTER', 'SPECIALISTS',
  'VERIFIER',
];
for (const name of FORCE_JS_FLAGS) process.env[`AI_REVIEWER_${name}_FORCE_JS`] = '1';

process.chdir(REPO_ROOT);

const { createReviewPipeline, loadReviewConfig } = await import(
  new URL('../../index.mjs', import.meta.url).href
);

// ---------------------------------------------------------------------------
// Общие константы прогона
// ---------------------------------------------------------------------------

/** Фиксированный head sha: он утекает в sticky, Δ-блок и export.json. */
const HEAD_SHA = '0123456789abcdef0123456789abcdef01234567';

const PR = { number: 1, title: 'golden PR', html_url: 'https://github.com/o/r/pull/1' };

/** Плейсхолдер в env.json: реальный каталог экспорта — временный, его путь недетерминирован. */
const EXPORT_DIR_PLACEHOLDER = '${EXPORT_DIR}';

const BASE_ENV = {
  OLLAMA_API_KEY: 'k',
  GITHUB_TOKEN: 't',
  REVIEW_PROFILE: 'profiles/orakul.json',
  REVIEW_FP_PATTERNS: 'feedback/fp-patterns.jsonl',
  REVIEW_MODE: 'consensus',
  OLLAMA_MODELS: 'm1,m2',
  REVIEW_OBSERVABILITY: '0',
  REVIEW_AUTO_TRIAGE: '0',
  REVIEW_VERIFIER: '0',
  REVIEW_EXPORT: '1',
  REVIEW_EXPORT_DIR: EXPORT_DIR_PLACEHOLDER,
};

const BASE_CLI = { dryRun: false, diffFile: undefined, repo: 'o/r', prNumber: 1 };

const GITHUB_RESULTS = { postReviewId: 555, deleteStaleInline: 0, upsertSticky: 'created' };

const EMPTY_PRESCAN = {
  findings: [], scanned: 0, skipped: 0, truncated: false,
  lintableTotal: 0, byTool: null, error: null, promptSection: '',
};

const EMPTY_BLAST = {
  blocks: [],
  metrics: {
    symbols_total: 0,
    symbols_resolved: 0,
    affected_nonempty: 0,
    graph_fresh: true,
    graph_status: 'fresh',
    skip_reason: '',
    skip_reasons: {},
  },
};

const EMPTY_PRIOR = { findings: [], runs: [], sha: '' };

// ---------------------------------------------------------------------------
// Диффы спецкейсов
// ---------------------------------------------------------------------------

const DIFF_CODE = `diff --git a/app/server/api.ts b/app/server/api.ts
--- a/app/server/api.ts
+++ b/app/server/api.ts
@@ -1,4 +1,6 @@
 import { db } from './db';
-const LIMIT = 10;
+const LIMIT = 25;
+export function getUser(id: string) { return db.user.findFirst({ where: { id } }); }
+export function dropUser(id: string) { return db.user.delete({ where: { id } }); }
 export const ready = true;
`;

const DIFF_LOCKFILE = `diff --git a/package-lock.json b/package-lock.json
--- a/package-lock.json
+++ b/package-lock.json
@@ -1,1 +1,1 @@
-  "version": "1.0.0"
+  "version": "1.0.1"
`;

const DIFF_DOCS = `diff --git a/docs/architecture.md b/docs/architecture.md
--- a/docs/architecture.md
+++ b/docs/architecture.md
@@ -1,2 +1,3 @@
 # Архитектура
-Старый абзац.
+Новый абзац.
+Ещё строка.
`;

// ---------------------------------------------------------------------------
// Утилиты
// ---------------------------------------------------------------------------

const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

/** JSON с рекурсивно отсортированными ключами — иначе повторный прогон даёт другой байт-поток. */
function stableStringify(value) {
  const sort = (v) => {
    if (Array.isArray(v)) return v.map(sort);
    if (v && typeof v === 'object') {
      const out = {};
      for (const k of Object.keys(v).sort()) out[k] = sort(v[k]);
      return out;
    }
    return v;
  };
  return `${JSON.stringify(sort(value), null, 2)}\n`;
}

function critical(file, line, title, extra = {}) {
  return {
    file, line, severity: 'critical', title, body: 'подробности находки',
    confidence: 0.9, model: 'm1', ...extra,
  };
}

// ---------------------------------------------------------------------------
// Фейковые зависимости — тот же контракт, что в test/pr-review.pipeline.test.mjs
// ---------------------------------------------------------------------------

function makeDeps(inputs) {
  const calls = {
    order: [],
    engineRuns: 0,
    verifierRuns: 0,
    loadDiff: 0,
    readPriorStickyData: 0,
    prescanRuns: 0,
    blastRuns: 0,
    postReview: [],
    deleteStaleInline: [],
    upsertSticky: [],
    telegram: [],
  };
  // Тело sticky не попадает в calls.json (там только ключи), но нужно целиком для sticky.md.
  const stickyBodies = [];

  const deps = {
    log: () => {},
    die: (msg) => { throw new Error(`die: ${msg}`); },
    repoFiles: () => clone(inputs.repoFiles),
    graphify: {
      buildBlastRadius: async () => { calls.blastRuns++; return clone(inputs.blastRadius); },
    },
    prescan: {
      run: async () => { calls.prescanRuns++; return clone(inputs.prescan); },
    },
    engine: {
      reviewAllModels: async () => { calls.engineRuns++; return clone(inputs.reviews); },
      verifyFindings: async (candidates) => {
        calls.verifierRuns++;
        const mode = inputs.verifier?.mode || 'none';
        if (mode === 'all-verified') {
          return candidates.map((_, i) => ({ candidateIndex: i, decision: 'verified' }));
        }
        if (mode === 'static') return clone(inputs.verifier.verdicts) || [];
        return [];
      },
    },
    github: {
      loadDiff: async () => {
        calls.loadDiff++;
        return {
          pr: clone(inputs.loadDiff.pr),
          headSha: inputs.loadDiff.headSha,
          draft: inputs.loadDiff.draft === true,
          diff: inputs.diff,
        };
      },
      readPriorStickyData: async () => {
        calls.readPriorStickyData++;
        return clone(inputs.priorSticky);
      },
      deleteStaleInline: async (opts) => {
        calls.order.push('delete');
        calls.deleteStaleInline.push(opts ? { exceptReviewId: opts.exceptReviewId ?? null } : {});
        return GITHUB_RESULTS.deleteStaleInline;
      },
      postReview: async (payload) => {
        calls.order.push('post');
        calls.postReview.push({
          headSha: payload.headSha ?? '',
          inline: (payload.inline || []).map((c) => ({ path: c.path, line: c.line })),
        });
        return GITHUB_RESULTS.postReviewId;
      },
      upsertSticky: async (body) => {
        calls.order.push('sticky');
        stickyBodies.push(body);
        calls.upsertSticky.push({ bytes: Buffer.byteLength(body, 'utf8') });
        return GITHUB_RESULTS.upsertSticky;
      },
    },
  };

  if (inputs.telegram?.enabled) {
    deps.notifyTelegram = async (_cfg, _log, payload) => {
      calls.order.push('telegram');
      calls.telegram.push({
        pr: payload.pr?.number ?? null,
        total: payload.total ?? null,
        counts: payload.counts ?? null,
        reason: payload.reason ?? null,
      });
    };
  }

  return { deps, calls, stickyBodies };
}

// ---------------------------------------------------------------------------
// Прогон одного кейса
// ---------------------------------------------------------------------------

async function runCase(kase) {
  const caseDir = join(OUT_ROOT, kase.id);
  const expectedDir = join(caseDir, 'expected');
  const exportDir = join(caseDir, '.export-tmp');
  await mkdir(expectedDir, { recursive: true });
  await mkdir(exportDir, { recursive: true });

  const env = { ...BASE_ENV, ...kase.env, REVIEW_EXPORT_DIR: exportDir };
  const cli = { ...BASE_CLI, ...kase.cli };
  const inputs = {
    diff: kase.diff,
    loadDiff: { pr: clone(PR), headSha: HEAD_SHA, draft: false, ...clone(kase.loadDiff) },
    priorSticky: clone(kase.priorSticky) ?? clone(EMPTY_PRIOR),
    prescan: clone(kase.prescan) ?? clone(EMPTY_PRESCAN),
    blastRadius: clone(kase.blastRadius) ?? clone(EMPTY_BLAST),
    reviews: clone(kase.reviews) ?? [],
    verifier: clone(kase.verifier) ?? { mode: 'none' },
    githubResults: { ...GITHUB_RESULTS },
    telegram: { enabled: kase.telegram === true },
    // Дерево репозитория для FP-семейства 7 (lib/absent-path.mjs). null — семейство
    // выключено: иначе эталон зависел бы от `git ls-files` этого репозитория.
    repoFiles: clone(kase.repoFiles) ?? null,
  };

  const cfg = loadReviewConfig(env, cli);
  const { deps, calls, stickyBodies } = makeDeps(inputs);
  const exitCode = await createReviewPipeline(cfg, deps).run();

  // --- вход
  await writeFile(join(caseDir, 'diff.patch'), kase.diff, 'utf8');
  await writeFile(join(caseDir, 'env.json'), stableStringify({
    description: kase.description,
    env: { ...env, REVIEW_EXPORT_DIR: EXPORT_DIR_PLACEHOLDER },
    cli: {
      dryRun: cli.dryRun === true,
      diffFile: cli.diffFile ?? null,
      repo: cli.repo,
      prNumber: cli.prNumber,
    },
    fixedNow: FIXED_NOW_ISO,
    forceJs: true,
  }), 'utf8');
  const inputsOnDisk = { ...inputs };
  delete inputsOnDisk.diff; // дифф лежит отдельным файлом — не дублируем
  await writeFile(join(caseDir, 'inputs.json'), stableStringify(inputsOnDisk), 'utf8');

  // --- выход
  await writeFile(join(expectedDir, 'exit-code.txt'), `${exitCode}\n`, 'utf8');
  await writeFile(join(caseDir, 'expected', 'calls.json'), stableStringify(calls), 'utf8');
  if (stickyBodies.length) {
    // Последний upsertSticky — тот, что увидит пользователь на PR.
    await writeFile(join(expectedDir, 'sticky.md'), stickyBodies[stickyBodies.length - 1], 'utf8');
  }

  // Артефакты экспорта копируются байт в байт: их формат — публичный контракт (EXPORT_VERSION).
  const exported = existsSync(exportDir) ? (await readdir(exportDir)).sort() : [];
  const artifacts = [];
  for (const name of exported) {
    const target = name.endsWith('.sarif.json')
      ? 'sarif.json'
      : name.endsWith('-swarm.json')
        ? 'swarm.json'
        : 'export.json';
    await copyFile(join(exportDir, name), join(expectedDir, target));
    artifacts.push(target);
  }
  await rm(exportDir, { recursive: true, force: true });

  return { id: kase.id, exitCode, artifacts: artifacts.sort(), sticky: stickyBodies.length > 0 };
}

// ---------------------------------------------------------------------------
// Кейсы из fixtures/*/ — те же входы, что гоняет eval harness, но через весь run()
// ---------------------------------------------------------------------------

async function fixtureCases() {
  const cases = [];
  for (const id of (await readdir(FIXTURES_ROOT)).sort()) {
    const dir = join(FIXTURES_ROOT, id);
    if (!existsSync(join(dir, 'meta.json'))) continue;
    const meta = JSON.parse(await readFile(join(dir, 'meta.json'), 'utf8'));
    const diff = await readFile(join(dir, 'diff.patch'), 'utf8');
    const all = JSON.parse(await readFile(join(dir, 'findings.json'), 'utf8')).findings || [];

    // Prescan-находки приходят из детерминированного контура, а не из «ответа модели»:
    // подмешивать их в reviews значило бы прогонять их через dedupeAgainstPrescan наоборот.
    const prescanFindings = all.filter((f) => f.prescan);
    const llmFindings = all.filter((f) => !f.prescan);
    const models = [...new Set(llmFindings.map((f) => f.model || 'm1'))];
    const reviews = (models.length ? models : ['m1']).map((model) => ({
      model,
      summary: `ревью ${model}`,
      verdict: 'changes_requested',
      findings: llmFindings.filter((f) => (f.model || 'm1') === model),
    }));

    // Дерево репозитория потребителя (по пути на строку) — как в loadFixtures (lib/eval.mjs).
    const repoFilesPath = join(dir, 'repo-files.txt');
    const repoFiles = existsSync(repoFilesPath)
      ? (await readFile(repoFilesPath, 'utf8')).split('\n').map((s) => s.trim()).filter(Boolean)
      : undefined;

    const hasClaims = llmFindings.some((f) => f.advisorClaims?.length);
    const verifierFile = join(dir, 'verifier.json');
    const hasVerifier = existsSync(verifierFile);
    const verifier = hasVerifier
      ? { mode: 'static', verdicts: JSON.parse(await readFile(verifierFile, 'utf8')).verdicts || [] }
      : { mode: 'none' };

    const env = {
      OLLAMA_MODELS: (models.length ? models : ['m1']).join(','),
      // Refutation — стадия MoA-специалистов: без REVIEW_MODE=moa она не включается.
      REVIEW_MODE: hasClaims ? 'moa' : 'consensus',
      REVIEW_VERIFIER: hasVerifier ? '1' : '0',
    };
    if (meta.gate?.consensusMin != null) env.REVIEW_CONSENSUS = String(meta.gate.consensusMin);

    cases.push({
      id: `fixture-${id}`,
      description: meta.description || id,
      diff,
      env,
      reviews,
      verifier,
      prescan: prescanFindings.length ? { ...EMPTY_PRESCAN, findings: prescanFindings } : undefined,
      repoFiles,
    });
  }
  return cases;
}

// ---------------------------------------------------------------------------
// Спецкейсы — ветки run(), которых нет в eval-фикстурах
// ---------------------------------------------------------------------------

const OUTAGE_REVIEWS = [
  { model: 'm1', error: 'Ollama 500: boom internal detail' },
  { model: 'm2', error: 'Ollama 429: limit' },
];

const PRESCAN_BLOCKER = {
  ...EMPTY_PRESCAN,
  findings: [{
    file: 'app/server/api.ts', line: 3, severity: 'critical',
    title: 'no-restricted-globals', body: 'запрещённый глобал',
    model: 'eslint', confidence: 0.95, prescan: true, prescanBlock: true,
  }],
  scanned: 1,
  lintableTotal: 1,
  byTool: { eslint: 1 },
  promptSection: '',
};

const SPECIAL_CASES = [
  {
    id: 'router-skip-lockfile',
    description: 'router R4: lockfile-only → skip, LLM не вызывается',
    diff: DIFF_LOCKFILE,
    reviews: [{ model: 'm1', summary: '', verdict: 'comment', findings: [critical('x.ts', 1, 'не должно попасть')] }],
  },
  {
    id: 'router-light-docs',
    description: 'router R4: docs-only → light, merge не блокируется по коду',
    diff: DIFF_DOCS,
    reviews: [{ model: 'm1', summary: '', verdict: 'comment', findings: [] }],
  },
  {
    id: 'gate-block-critical',
    description: 'critical с confidence≥0.8 → блок (exit 1), inline + sticky',
    diff: DIFF_CODE,
    reviews: [{
      model: 'm1', summary: 'нашёл баг', verdict: 'changes_requested',
      findings: [critical('app/server/api.ts', 4, 'IDOR в dropUser')],
    }],
  },
  {
    id: 'llm-outage-fail-open',
    description: 'все модели недоступны + REVIEW_LLM_REQUIRED=0 → мягкий пропуск (exit 0)',
    diff: DIFF_CODE,
    env: { REVIEW_LLM_REQUIRED: '0' },
    reviews: OUTAGE_REVIEWS,
  },
  {
    id: 'llm-outage-fail-closed',
    description: 'все модели недоступны при REVIEW_LLM_REQUIRED=1 → fail-closed (exit 1) + telegram',
    diff: DIFF_CODE,
    env: { REVIEW_LLM_REQUIRED: '1' },
    reviews: OUTAGE_REVIEWS,
    telegram: true,
  },
  {
    id: 'llm-outage-prescan-block',
    description: 'fail-open для LLM, но prescan-блокер всё ещё блокирует (exit 1)',
    diff: DIFF_CODE,
    env: { REVIEW_LLM_REQUIRED: '0' },
    reviews: OUTAGE_REVIEWS,
    prescan: PRESCAN_BLOCKER,
  },
  {
    id: 'llm-outage-partial',
    description: 'partial outage: одна модель упала, вторая ответила → exit 0',
    diff: DIFF_CODE,
    reviews: [
      OUTAGE_REVIEWS[0],
      {
        model: 'm2', summary: '', verdict: 'comment',
        findings: [critical('app/server/api.ts', 4, 'магическое число', { severity: 'minor', model: 'm2' })],
      },
    ],
  },
  {
    id: 'verifier-strict-overflow',
    description: 'verifier strict: находки сверх капа REVIEW_VERIFIER_MAX понижаются до minor',
    diff: DIFF_CODE,
    env: { REVIEW_VERIFIER: '1', REVIEW_VERIFIER_STRICT: '1' },
    reviews: [{
      model: 'm1', summary: 's', verdict: 'changes_requested',
      findings: Array.from({ length: 13 }, (_, i) => critical('app/server/api.ts', i + 1, `баг ${i + 1}`)),
    }],
    verifier: { mode: 'all-verified' },
  },
  {
    id: 'telegram-notify',
    description: 'блокирующий прогон с включённым telegram: пинг после sticky',
    diff: DIFF_CODE,
    env: { TELEGRAM_BOT_TOKEN: 'tg', TELEGRAM_CHAT_ID: '42' },
    reviews: [{
      model: 'm1', summary: 'нашёл баг', verdict: 'changes_requested',
      findings: [critical('app/server/api.ts', 4, 'IDOR в dropUser')],
    }],
    telegram: true,
  },
  {
    id: 'prior-sticky-carryover',
    description: 'prior sticky: carry-over находки + история прогонов → Δ-блок в sticky',
    diff: DIFF_CODE,
    priorSticky: {
      findings: [
        { file: 'app/server/api.ts', line: 4, severity: 'critical', title: 'IDOR в dropUser', body: 'было в прошлом прогоне', confidence: 0.9 },
        { file: 'app/server/api.ts', line: 2, severity: 'minor', title: 'магическое число', body: '', confidence: 0.7 },
      ],
      runs: [
        { sha: HEAD_SHA.slice(0, 7), blocked: true, counts: { critical: 1, major: 0, minor: 1, nit: 0 } },
        { sha: 'deadbee', blocked: false, counts: { critical: 0, major: 0, minor: 1, nit: 0 } },
      ],
      sha: HEAD_SHA.slice(0, 7),
    },
    reviews: [{
      model: 'm1', summary: 'нашёл баг', verdict: 'changes_requested',
      findings: [critical('app/server/api.ts', 4, 'IDOR в dropUser')],
    }],
  },
  {
    id: 'time-budget-partial',
    description: 'дедлайн-пропуски чанков → пометка «охват частичный» в sticky',
    diff: DIFF_CODE,
    reviews: [{ model: 'm1', summary: '', verdict: 'comment', findings: [], skippedChunks: 2 }],
  },
  {
    id: 'blast-radius-cross-layer',
    description: 'непустой blast-radius: cross-layer caller вне диффа → детерминированная находка',
    diff: DIFF_CODE,
    blastRadius: {
      blocks: [
        '- `getUser` (app/server/api.ts)\n  - `renderUser` @ app/client/src/page.tsx:42 [calls][EXTRACTED] cross-community\n',
      ],
      metrics: {
        symbols_total: 2,
        symbols_resolved: 1,
        affected_nonempty: 1,
        graph_fresh: true,
        graph_status: 'fresh',
        skip_reason: '',
        skip_reasons: {},
        affected_files: ['app/client/src/page.tsx'],
      },
    },
    reviews: [{ model: 'm1', summary: '', verdict: 'comment', findings: [] }],
  },
];

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

async function main() {
  await rm(OUT_ROOT, { recursive: true, force: true });
  await mkdir(OUT_ROOT, { recursive: true });

  const cases = [...(await fixtureCases()), ...SPECIAL_CASES];
  const index = [];
  for (const kase of cases) {
    const result = await runCase(kase);
    index.push({ id: result.id, description: kase.description, exitCode: result.exitCode });
    console.log(`  ✓ ${result.id.padEnd(42)} exit=${result.exitCode} sticky=${result.sticky ? 'да' : 'нет'} artifacts=${result.artifacts.join(',') || '—'}`);
  }

  await writeFile(join(OUT_ROOT, 'index.json'), stableStringify({
    generator: 'rust/parity/pipeline-golden-generate.mjs',
    fixedNow: FIXED_NOW_ISO,
    headSha: HEAD_SHA,
    exportDirPlaceholder: EXPORT_DIR_PLACEHOLDER,
    cases: index,
  }), 'utf8');

  console.log(`\ngolden-фикстур: ${index.length} → ${OUT_ROOT}`);
}

await main();
