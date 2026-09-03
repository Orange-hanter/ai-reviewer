/**
 * Фаза 1, шаг 6: lib/config.mjs — обёртка над Rust-бинарём rust/target/debug/config.
 *
 * Портирована ВСЯ логика приоритетов (env/CLI → профиль → дефолты движка) — см.
 * rust/src/config.rs (`loadReviewConfig`): Rust получает снимок env + биты resolved
 * профиля (strict/appDir/paths) и возвращает «plain parts» конфигурации. В JS остаются:
 *   - resolveProfile — fs + process-wide singleton (lib/profile.mjs);
 *   - parseCli — читает process.argv;
 *   - сборка замороженного конфига (assembleReviewConfig) — общая для обеих веток,
 *     поэтому frozen-семантика (что именно Object.freeze) не может разойтись.
 *
 * Резолвинг бинаря: AI_REVIEWER_CONFIG_BIN (override), по умолчанию
 * <repo>/rust/target/debug/config; AI_REVIEWER_CONFIG_FORCE_JS=1 — чистый JS.
 * Бинаря нет (чистый клон до cargo build) → чистый JS fallback; бинарь битый
 * (non-zero exit) → ошибка пробрасывается, fallback не подменяет его.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { numEnv, parseModelList, envFlag, ratioEnv } from './util.mjs';
import { resolveProfile } from './profile.mjs';
import {
  DEFAULT_MOA_REFERENCES, DEFAULT_MOA_AGGREGATOR, DEFAULT_REVIEW_VERIFIER_MODEL,
  DEFAULT_OLLAMA_MODELS_FALLBACK,
} from './constants.mjs';
import { parseSpecialistAssignments } from './specialists.mjs';
import { loadPriceMap } from './cost.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_CONFIG_BIN = resolve(__dir, '../rust/target/debug/config');

function resolveConfigBin() {
  if (process.env.AI_REVIEWER_CONFIG_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_CONFIG_BIN || '').trim() || DEFAULT_CONFIG_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveConfigBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

/** Дефолтный trust tier structure-evidence — контракт доверия движка, см. docs/configuration.md. */
const DEFAULT_STRUCTURE_TRUST_TIER = 'trusted_local';

/** CLI-флаги (--dry-run, --diff, --pr, --repo). Не портировано: читает process.argv. */
export function parseCli(argv = process.argv.slice(2)) {
  const flag = (name) => {
    const i = argv.indexOf(name);
    return i >= 0 ? (argv[i + 1] ?? '') : undefined;
  };
  return {
    dryRun: argv.includes('--dry-run'),
    diffFile: flag('--diff'),
    repo: flag('--repo') || process.env.GITHUB_REPOSITORY || '',
    prNumber: Number(flag('--pr') || process.env.PR_NUMBER || 0),
  };
}

/**
 * Единый конфиг прогона из профиля проекта + env + CLI; передаётся по пайплайну вместо глобалов.
 *
 * Приоритет: env/CLI (прогон) → профиль проекта (репозиторий) → дефолты движка. Профиль
 * подбирается по `REVIEW_PROFILE`, иначе `.ai-reviewer.json` в корне — см. lib/profile.mjs.
 */
export function loadReviewConfig(env = process.env, cli = parseCli()) {
  const profile = resolveProfile(env);
  const parts = runRust('loadReviewConfig', {
    env,
    profile: {
      strict: profile.strict === true,
      appDir: profile.layout?.appDir ?? '',
      paths: profile.paths || {},
    },
  }) ?? computeConfigPartsJs(env, profile);
  return assembleReviewConfig(cli, profile, parts);
}

/** Сборка замороженного конфига — единая точка для Rust- и JS-ветки (frozen-семантика 1:1). */
function assembleReviewConfig(cli, profile, p) {
  return Object.freeze({
    cli,
    profile,
    markers: profile.markers,
    reviewMode: p.reviewMode,
    httpRetries: p.httpRetries,
    github: Object.freeze({
      api: p.github.api,
      token: p.github.token,
      repo: cli.repo,
      prNumber: cli.prNumber,
    }),
    llm: Object.freeze({ key: p.llm.key, base: p.llm.base }),
    ollama: Object.freeze({ ...p.ollama }),
    moa: Object.freeze({ ...p.moa }),
    models: Object.freeze({
      list: Object.freeze([...p.models.list]),
      fallback: p.models.fallback,
    }),
    gate: Object.freeze({ ...p.gate }),
    cost: Object.freeze({ priceMap: p.cost.priceMap }),
    inlineOn: Object.freeze([...p.inlineOn]),
    actionability: Object.freeze({ ...p.actionability }),
    verifier: Object.freeze({ ...p.verifier }),
    router: Object.freeze({ ...p.router }),
    specialists: Object.freeze({
      enabled: p.specialists.enabled,
      assignments: Object.freeze(p.specialists.assignments.map((a) => Object.freeze({ ...a }))),
    }),
    prescan: Object.freeze({
      enabled: p.prescan.enabled,
      blockEnabled: p.prescan.blockEnabled,
      appDir: p.prescan.appDir,
      eslintBin: p.prescan.eslintBin,
      maxFiles: p.prescan.maxFiles,
      blockRules: Object.freeze([...p.prescan.blockRules]),
      astgrep: Object.freeze({
        enabled: p.prescan.astgrep.enabled,
        configPath: p.prescan.astgrep.configPath,
        bin: p.prescan.astgrep.bin,
        blockRules: Object.freeze([...p.prescan.astgrep.blockRules]),
      }),
    }),
    observability: Object.freeze({ ...p.observability }),
    export: Object.freeze({ ...p.export }),
    triage: Object.freeze({ ...p.triage }),
    feedback: Object.freeze({ ...p.feedback }),
    limits: Object.freeze({ ...p.limits }),
    graphify: Object.freeze({ ...p.graphify }),
    structure: Object.freeze({
      ...p.structure,
      thresholds: Object.freeze({ ...p.structure.thresholds }),
    }),
    telegram: Object.freeze({ ...p.telegram }),
  });
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/config.mjs до Фазы 1).
// Возвращает те же «plain parts», что и op `loadReviewConfig` бинаря (без cli/profile/markers
// и без github.repo/prNumber — они приходят из cli при сборке).
// ---------------------------------------------------------------------------

function computeConfigPartsJs(env, profile) {
  const p = profile.paths;
  const ollamaModel = env.OLLAMA_MODEL || 'kimi-k2.7-code';
  const reviewMode = (env.REVIEW_MODE || 'moa').trim().toLowerCase();
  const moaReferences = parseModelList(env.MOA_REFERENCE_MODELS || DEFAULT_MOA_REFERENCES);
  const specialistsEnabled = reviewMode === 'moa' && envFlag('REVIEW_SPECIALISTS', true, env);
  const specialistAssignments = parseSpecialistAssignments(env, moaReferences);
  const moaAggregator = (env.MOA_AGGREGATOR || DEFAULT_MOA_AGGREGATOR).trim();
  const moaLabel = specialistsEnabled
    ? `moa-specialist(${moaAggregator}←${specialistAssignments.map((a) => a.role).join('+')})`
    : `moa(${moaAggregator}←${moaReferences.join('+')})`;
  const models = reviewMode === 'moa'
    ? [moaLabel]
    : parseModelList(env.OLLAMA_MODELS || ollamaModel);
  const blockOn = (env.REVIEW_BLOCK_ON ?? 'critical,major').split(',').map((s) => s.trim()).filter(Boolean);
  const inlineOn = (env.REVIEW_INLINE_ON ?? 'critical,major').split(',').map((s) => s.trim()).filter(Boolean);
  const blockSingle = (env.REVIEW_BLOCK_SINGLE ?? 'critical')
    .split(',').map((s) => s.trim()).filter(Boolean)
    .filter((s) => blockOn.includes(s));
  const consensusDefault = reviewMode === 'moa' ? 1 : 2;
  const consensusMin = Math.min(
    Math.max(1, Number(env.REVIEW_CONSENSUS || consensusDefault) || consensusDefault),
    reviewMode === 'moa' ? 1 : (models.length || 1),
  );
  const moaRefTemp = (() => {
    const n = Number(env.MOA_REFERENCE_TEMPERATURE);
    return Number.isFinite(n) && n >= 0 ? n : 0.3;
  })();
  const moaAggTemp = (() => {
    const n = Number(env.MOA_AGGREGATOR_TEMPERATURE);
    return Number.isFinite(n) && n >= 0 ? n : 0.4;
  })();
  const temperature = (() => {
    const n = Number(env.REVIEW_TEMPERATURE);
    return Number.isFinite(n) && n >= 0 ? n : 0.4;
  })();
  const seed = (() => {
    const n = Number(env.REVIEW_SEED);
    return Number.isInteger(n) && n >= 0 ? n : 7;
  })();
  const httpRetries = numEnv('REVIEW_HTTP_RETRIES', 3, env);
  const confidenceMin = (() => {
    const n = Number(env.REVIEW_CONFIDENCE_MIN);
    return Number.isFinite(n) && n >= 0 && n <= 1 ? n : 0.8;
  })();
  const verifierEnabled = envFlag('REVIEW_VERIFIER', true, env);
  const verifierModel = (env.REVIEW_VERIFIER_MODEL || DEFAULT_REVIEW_VERIFIER_MODEL).trim();
  const verifierTemp = (() => {
    const n = Number(env.REVIEW_VERIFIER_TEMPERATURE);
    return Number.isFinite(n) && n >= 0 ? n : 0.2;
  })();
  const routerEnabled = envFlag('REVIEW_ROUTER', true, env);
  const prescanEnabled = envFlag('REVIEW_PRESCAN', true, env);
  const prescanBlockRules = (env.REVIEW_PRESCAN_BLOCK_RULES ?? 'no-restricted-imports,no-restricted-globals')
    .split(',').map((s) => s.trim()).filter(Boolean);
  const astgrepEnabled = envFlag('REVIEW_PRESCAN_ASTGREP', true, env);
  const astgrepBlockRules = (env.REVIEW_PRESCAN_ASTGREP_BLOCK_RULES ?? 'raw-x-forwarded-for')
    .split(',').map((s) => s.trim()).filter(Boolean);
  const observabilityEnabled = envFlag('REVIEW_OBSERVABILITY', true, env);
  const feedbackEnabled = envFlag('REVIEW_FEEDBACK', true, env);
  const exportEnabled = envFlag('REVIEW_EXPORT', true, env);
  const triageEnabled = envFlag('REVIEW_AUTO_TRIAGE', true, env);
  const strict = envFlag('REVIEW_STRICT', false, env) || profile.strict === true;
  const requireQuote = envFlag('REVIEW_REQUIRE_QUOTE', strict, env);
  const llmKey = (env.LLM_API_KEY || env.OLLAMA_API_KEY || '').trim();
  const llmBase = (env.LLM_BASE_URL || env.OLLAMA_BASE_URL || 'https://ollama.com/v1').replace(/\/$/, '');
  const priceMap = loadPriceMap(env);

  // единый источник путей graphify: structure.* наследует их, а не дублирует fallback-логику
  const graphifyGraph = env.GRAPH || '.graphify/graphify-out/graph.json';
  const graphifyReport = env.GRAPHIFY_REPORT || '.graphify/graphify-out/GRAPH_REPORT.md';
  const structureGraph = env.REVIEW_STRUCTURE_GRAPH || graphifyGraph;

  return {
    reviewMode,
    httpRetries,
    github: {
      api: (env.GITHUB_API_URL || 'https://api.github.com').replace(/\/$/, ''),
      token: env.GITHUB_TOKEN || '',
    },
    llm: { key: llmKey, base: llmBase },
    ollama: {
      key: llmKey,
      base: llmBase,
      model: ollamaModel,
      temperature,
      jsonMode: envFlag('REVIEW_JSON_MODE', true, env),
      seed,
      httpRetries,
      requestTimeoutMs: numEnv('REVIEW_LLM_TIMEOUT_MS', 240_000, env),
      // Reasoning effort для advisor (specialist/reference) вызовов: low (default, качество)
      // | none (быстрее в 2–3× по пробнику — качество под вопросом, см. docs/model-tuning.md).
      effort: (env.REVIEW_REASONING_EFFORT || 'low').trim(),
      // Rate-limit control: caps параллелизма провайдера (глобальный + per-model), AIMD по 429.
      providerConcurrency: numEnv('REVIEW_PROVIDER_CONCURRENCY', 12, env),
      providerPerModel: numEnv('REVIEW_PROVIDER_PER_MODEL', 6, env),
    },
    moa: {
      references: moaReferences,
      aggregator: moaAggregator,
      label: moaLabel,
      refMaxTokens: numEnv('MOA_REFERENCE_MAX_TOKENS', 12_000, env),
      refTemp: moaRefTemp,
      aggTemp: moaAggTemp,
    },
    models: {
      list: models,
      fallback: parseModelList(env.OLLAMA_MODELS_FALLBACK || DEFAULT_OLLAMA_MODELS_FALLBACK, { exclude: models }),
    },
    gate: {
      blockOn, blockSingle, consensusMin, confidenceMin,
      // Fail-closed при полной недоступности LLM: без ревью гейт не может подтвердить
      // PR → exit 1 (0/false/no = fail-open, старое поведение «инфра-сбой не блокирует merge»).
      llmRequired: envFlag('REVIEW_LLM_REQUIRED', true, env),
      strict,
      requireQuote,
    },
    cost: { priceMap },
    inlineOn,
    // Маршрут advisory-находок (minor/nit) считается по доказуемости, а не по severity:
    // порог — сумма весов сигналов (см. lib/actionability.mjs), лимит держит PR читаемым,
    // verifyAdvisory — сколько лучших advisory отдать verifier'у на подтверждение.
    actionability: {
      threshold: numEnv('REVIEW_ACTIONABILITY_MIN', 4, env),
      maxAdvisoryInline: numEnv('REVIEW_MAX_ADVISORY_INLINE', 5, env),
      verifyAdvisory: numEnv('REVIEW_VERIFY_ADVISORY', 5, env),
    },
    verifier: {
      enabled: verifierEnabled,
      model: verifierModel,
      maxCandidates: numEnv('REVIEW_VERIFIER_MAX', 12, env),
      temperature: verifierTemp,
      // Fail-closed для блоков: verifier отработал, но verdict'а по кандидату нет → minor.
      // Блок требует подтверждения второй независимой моделью (0 = fail-open, старое поведение).
      strictBlocks: envFlag('REVIEW_VERIFIER_STRICT', true, env),
    },
    router: { enabled: routerEnabled },
    specialists: {
      enabled: specialistsEnabled,
      assignments: specialistAssignments,
    },
    prescan: {
      enabled: prescanEnabled,
      blockEnabled: envFlag('REVIEW_PRESCAN_BLOCK', true, env),
      appDir: env.REVIEW_APP_DIR ?? profile.layout.appDir,
      eslintBin: env.REVIEW_ESLINT_BIN || 'npx',
      maxFiles: numEnv('REVIEW_PRESCAN_MAX_FILES', 30, env),
      blockRules: prescanBlockRules,
      astgrep: {
        enabled: prescanEnabled && astgrepEnabled,
        configPath: env.REVIEW_ASTGREP_CONFIG || p.astgrepConfig || '',
        bin: env.REVIEW_ASTGREP_BIN || 'ast-grep',
        blockRules: astgrepBlockRules,
      },
    },
    observability: {
      enabled: observabilityEnabled,
      dir: env.REVIEW_OBSERVABILITY_DIR || p.observabilityDir,
      file: env.REVIEW_OBSERVABILITY_FILE || 'pr-review-runs.jsonl',
    },
    export: {
      enabled: exportEnabled,
      dir: env.REVIEW_EXPORT_DIR || p.exportDir,
      json: envFlag('REVIEW_EXPORT_JSON', true, env),
      sarif: envFlag('REVIEW_EXPORT_SARIF', true, env),
      sarifAll: envFlag('REVIEW_EXPORT_SARIF_ALL', false, env),
      dryRun: envFlag('REVIEW_EXPORT_DRY_RUN', false, env),
      reasoningInSticky: envFlag('REVIEW_REASONING_STICKY', true, env),
    },
    triage: { enabled: triageEnabled },
    feedback: {
      enabled: feedbackEnabled,
      label: env.REVIEW_FEEDBACK_LABEL || 'ai-review-fp',
      logFile: env.REVIEW_FEEDBACK_LOG || p.feedbackLog,
      candidatesDir: env.REVIEW_FEEDBACK_CANDIDATES_DIR || p.feedbackCandidatesDir,
      mdPath: env.REVIEW_FEEDBACK_MD || p.fpMarkdown,
      appendMarkdown: envFlag('REVIEW_FEEDBACK_MD_APPEND', false, env),
      writeCandidates: envFlag('REVIEW_FEEDBACK_CANDIDATES', true, env),
      write: envFlag('REVIEW_FEEDBACK_WRITE', true, env),
      trustedAuthors: env.REVIEW_FEEDBACK_TRUSTED_AUTHORS || 'github-actions[bot]',
      reactionConcurrency: numEnv('REVIEW_FEEDBACK_CONCURRENCY', 6, env),
      patternsFile: env.REVIEW_FP_PATTERNS || p.fpPatterns || '',
      promoteMinFpCount: numEnv('REVIEW_FP_PROMOTE_MIN', 2, env),
    },
    limits: {
      maxDiffBytes: numEnv('REVIEW_MAX_DIFF_BYTES', 200_000, env),
      maxSymbols: numEnv('REVIEW_MAX_SYMBOLS', 25, env),
      maxDepsPerSymbol: numEnv('REVIEW_MAX_DEPS_PER_SYMBOL', 20, env),
      godNodeThreshold: numEnv('REVIEW_GOD_NODE_THRESHOLD', 20, env),
      maxFileLines: numEnv('REVIEW_MAX_FILE_LINES', 1500, env),
      maxContextBytes: numEnv('REVIEW_MAX_CONTEXT_BYTES', 180_000, env),
      chunkBytes: numEnv('REVIEW_CHUNK_BYTES', 56_000, env),
      maxChunks: numEnv('REVIEW_MAX_CHUNKS', 40, env),
      concurrency: numEnv('REVIEW_CONCURRENCY', 10, env),
      maxInline: numEnv('REVIEW_MAX_INLINE', 50, env),
      // Дедлайн LLM-фазы прогона: после него чанки пропускаем и постим частичный отчёт,
      // чтобы процесс завершился штатно до убийства раннером (job timeout-minutes: 25).
      timeBudgetMs: numEnv('REVIEW_TIME_BUDGET_MIN', 20, env) * 60_000,
    },
    graphify: {
      bin: env.GRAPHIFY_BIN || '.graphify/gf',
      graph: graphifyGraph,
      report: graphifyReport,
      depth: Math.min(5, Math.max(1, numEnv('REVIEW_GRAPHIFY_DEPTH', 2, env))),
      dropAmbiguous: envFlag('REVIEW_GRAPHIFY_DROP_AMBIGUOUS', true, env),
    },
    structure: {
      enabled: envFlag('REVIEW_STRUCTURE', true, env),
      graph: structureGraph,
      graphReport: env.REVIEW_STRUCTURE_GRAPH_REPORT || graphifyReport,
      // sidecar живёт рядом с graph.json: следует за GRAPH/REVIEW_STRUCTURE_GRAPH override
      graphAnalysis: env.REVIEW_STRUCTURE_GRAPH_ANALYSIS
        || join(dirname(structureGraph), '.graphify_analysis.json'),
      scip: env.REVIEW_STRUCTURE_SCIP || '',
      lcov: env.REVIEW_STRUCTURE_LCOV || p.coverageLcov,
      obligations: env.REVIEW_STRUCTURE_OBLIGATIONS || '',
      mutationReport: env.REVIEW_STRUCTURE_MUTATION || '',
      propertyReport: env.REVIEW_STRUCTURE_PROPERTIES || '',
      exportDir: env.REVIEW_STRUCTURE_EXPORT_DIR || p.structureExportDir,
      scope: env.REVIEW_STRUCTURE_SCOPE || '',
      trustTier: env.REVIEW_STRUCTURE_TRUST_TIER || DEFAULT_STRUCTURE_TRUST_TIER,
      includeAmbiguous: envFlag('REVIEW_STRUCTURE_INCLUDE_AMBIGUOUS', false, env),
      maxEntities: numEnv('REVIEW_STRUCTURE_MAX_ENTITIES', 5000, env),
      maxEdges: numEnv('REVIEW_STRUCTURE_MAX_EDGES', 15000, env),
      thresholds: {
        highFanIn: numEnv('REVIEW_STRUCTURE_HIGH_FAN_IN', 5, env),
        unresolvedJoinRate: ratioEnv('REVIEW_STRUCTURE_MAX_UNRESOLVED_RATE', 0.1, env),
      },
    },
    telegram: {
      token: env.TELEGRAM_BOT_TOKEN || '',
      chat: env.TELEGRAM_CHAT_ID || '',
    },
  };
}
