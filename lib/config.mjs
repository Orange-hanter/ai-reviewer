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
import { numEnv, nonNegEnv, parseModelList, envFlag, envMode, ratioEnv, DEFAULT_TRUSTED_AUTHORS, parseTrustedAuthors } from './util.mjs';
import { resolveProfile } from './profile.mjs';
import {
  DEFAULT_MOA_REFERENCES, DEFAULT_MOA_AGGREGATOR, DEFAULT_REVIEW_VERIFIER_MODEL,
  DEFAULT_OLLAMA_MODELS_FALLBACK,
} from './constants.mjs';
import { parseSpecialistAssignments } from './specialists.mjs';
import { loadPriceMap } from './cost.mjs';
import { defaultRustBin } from './rust-bin.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_CONFIG_BIN = defaultRustBin('config');

// Вес сигнала `verified` из ACTIONABILITY_SIGNALS (lib/actionability.mjs). Дублируется
// числом намеренно: конфиг не должен зависеть от модуля маршрутизации (тот тянет профиль
// и Rust-бинарь), а Rust-сторона обязана держать ту же константу — см. rust/src/config.rs.
const VERIFIED_SIGNAL_WEIGHT = 3;

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
    headSha: flag('--head-sha') || '',
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
  const rust = runRust('loadReviewConfig', {
    env,
    profile: {
      strict: profile.strict === true,
      appDir: profile.layout?.appDir ?? '',
      paths: profile.paths || {},
    },
  });
  if (rust === null) return assembleReviewConfig(cli, profile, computeConfigPartsJs(env, profile));
  // Бинарь, собранный до появления ключа, его не отдаёт — и фича молча выключалась
  // (аудит RFC 14, п. 0.1: локальный rust/target старше коммита — карточек нет, CI не
  // ловит, там бинарь свежий). Недостающие ключи берём из JS-ветки и предупреждаем один
  // раз. Parity выключает дополнение (AI_REVIEWER_CONFIG_NO_BACKFILL=1): ему нужен сырой
  // ответ Rust, иначе отставание порта было бы скрыто.
  if (env.AI_REVIEWER_CONFIG_NO_BACKFILL !== '1' && process.env.AI_REVIEWER_CONFIG_NO_BACKFILL !== '1') {
    const missing = backfillMissing(rust, computeConfigPartsJs(env, profile));
    if (missing.length && !warnedStaleConfigBin) {
      warnedStaleConfigBin = true;
      process.emitWarning(`Rust-бинарь config устарел: нет ключей ${missing.join(', ')} — взяты из JS. Пересоберите: cd rust && cargo build`, 'AiReviewerStaleBinary');
    }
  }
  return assembleReviewConfig(cli, profile, rust);
}

let warnedStaleConfigBin = false;

/**
 * Дописывает в `target` ключи из `source`, которых в нём нет (рекурсивно по plain-объектам).
 * Существующие значения не трогает. @returns {string[]} пути дописанных ключей
 */
export function backfillMissing(target, source, prefix = '') {
  const added = [];
  if (!target || typeof target !== 'object' || !source || typeof source !== 'object') return added;
  for (const [k, v] of Object.entries(source)) {
    const path = prefix ? `${prefix}.${k}` : k;
    if (!(k in target) || target[k] === undefined) {
      target[k] = v;
      added.push(path);
    } else if (v && typeof v === 'object' && !Array.isArray(v) && target[k] && typeof target[k] === 'object' && !Array.isArray(target[k])) {
      added.push(...backfillMissing(target[k], v, path));
    }
  }
  return added;
}

/** Сборка замороженного конфига — единая точка для Rust- и JS-ветки (frozen-семантика 1:1). */
function assembleReviewConfig(cli, profile, p) {
  return Object.freeze({
    cli,
    profile,
    markers: profile.markers,
    reviewMode: p.reviewMode,
    promptLayout: p.promptLayout ?? 'legacy',
    httpRetries: p.httpRetries,
    github: Object.freeze({
      api: p.github.api,
      token: p.github.token,
      repo: cli.repo,
      prNumber: cli.prNumber,
      trustedAuthors: Object.freeze([...p.github.trustedAuthors]),
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
      merge: p.specialists.merge ?? 'auto',
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
    incremental: Object.freeze({ ...(p.incremental || { enabled: false, stateFile: '', fullEvery: 10 }) }),
    triage: Object.freeze({ ...p.triage }),
    feedback: Object.freeze({ ...p.feedback }),
    limits: Object.freeze({ ...p.limits }),
    graphify: Object.freeze({ ...p.graphify }),
    structure: Object.freeze({
      ...p.structure,
      thresholds: Object.freeze({ ...p.structure.thresholds }),
    }),
    render: Object.freeze({ ...(p.render || { iconBaseUrl: '' }) }),
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
  const actionabilityMin = numEnv('REVIEW_ACTIONABILITY_MIN', 4, env);
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
  // Кто считается автором бота (sticky, inline, auto-triage). Пустая строка — значащее
  // значение «доверять всем» (откат), поэтому проверяем undefined, а не falsy.
  const trustedAuthorsRaw = env.REVIEW_TRUSTED_AUTHORS !== undefined
    ? env.REVIEW_TRUSTED_AUTHORS
    : (env.REVIEW_FEEDBACK_TRUSTED_AUTHORS !== undefined ? env.REVIEW_FEEDBACK_TRUSTED_AUTHORS : DEFAULT_TRUSTED_AUTHORS);

  // единый источник путей graphify: structure.* наследует их, а не дублирует fallback-логику
  const graphifyGraph = env.GRAPH || '.graphify/graphify-out/graph.json';
  const graphifyReport = env.GRAPHIFY_REPORT || '.graphify/graphify-out/GRAPH_REPORT.md';
  const structureGraph = env.REVIEW_STRUCTURE_GRAPH || graphifyGraph;

  return {
    reviewMode,
    // Раскладка сообщений: legacy — роль в system (как было); shared-prefix — общая рамка
    // и контекст первыми, роль последней (RFC 04, решение 5). Дефолт консервативный:
    // выигрыш зависит от того, кэширует ли провайдер префикс, а порядок сообщений влияет
    // на находки — менять по умолчанию можно только после gold.
    promptLayout: ['legacy', 'shared-prefix'].includes((env.REVIEW_PROMPT_LAYOUT || '').trim().toLowerCase())
      ? env.REVIEW_PROMPT_LAYOUT.trim().toLowerCase()
      : 'legacy',
    httpRetries,
    github: {
      api: (env.GITHUB_API_URL || 'https://api.github.com').replace(/\/$/, ''),
      token: env.GITHUB_TOKEN || '',
      trustedAuthors: parseTrustedAuthors(trustedAuthorsRaw),
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
      // 240 с резали ровно посередине распределения: на замере (.ai-review/observability)
      // медиана prose-вызова kimi — 331 с, и по таймауту обрывалась половина её вызовов,
      // а также четверть вызовов агрегатора. Вход при этом оплачен, ответа нет — самый
      // дорогой вид потерь. 420 с покрывают p90 замера; сокращать промпт всё равно нужно,
      // но платить за выброшенные ответы не нужно уже сейчас.
      requestTimeoutMs: numEnv('REVIEW_LLM_TIMEOUT_MS', 420_000, env),
      // Reasoning effort для advisor (specialist/reference) вызовов: low (default, качество)
      // | none (быстрее в 2–3× по пробнику — качество под вопросом, см. docs/model-tuning.md).
      effort: (env.REVIEW_REASONING_EFFORT || 'low').trim(),
      // Reasoning effort для JSON-вызовов (агрегатор жюри, verifier) — первая попытка;
      // каскад ретраев chatJson по-прежнему сваливается в none и это не настраивается.
      // Отдельная ручка, потому что рассуждение здесь считается completion-токенами и
      // стоит денег: замер — 1903 completion-токена на находку у minimax-m3 против 474 у
      // sonnet, вся разница — reasoning. Эксперименту (бэклог A6, docs/backlog.md) нужен
      // переключатель, а не правка кода.
      aggregatorEffort: (env.REVIEW_AGGREGATOR_REASONING_EFFORT || 'low').trim(),
      // Rate-limit control: caps параллелизма провайдера (глобальный + per-model), AIMD по 429.
      providerConcurrency: numEnv('REVIEW_PROVIDER_CONCURRENCY', 12, env),
      providerPerModel: numEnv('REVIEW_PROVIDER_PER_MODEL', 6, env),
    },
    moa: {
      references: moaReferences,
      aggregator: moaAggregator,
      label: moaLabel,
      // Потолок выхода роли. 6000 — страховка под лимит «не более 8 пунктов» из промпта
      // (RFC 04, решение 3): с запасом хватает на восемь развёрнутых пунктов с цитатами,
      // но не оплачивает хвост из двадцати наблюдений, если модель проигнорировала просьбу.
      refMaxTokens: numEnv('MOA_REFERENCE_MAX_TOKENS', 6_000, env),
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
      // Fail-closed без code graph: blast-radius не посчитан (нет graph.json, граф не читается,
      // graphify CLI недоступен) → exit 1 до LLM. Дефолт 0: потребитель без Graphify не блокируется.
      blastRequired: envFlag('REVIEW_BLAST_REQUIRED', false, env),
      // RFC 21: blast-находка судится verifier'ом (refutation её не понижает, verifier видит
      // строку вызова вне PR). `shadow` — всё считается, но гейт её не блокирует.
      blastVerify: envMode('REVIEW_BLAST_VERIFY', '0', env),
      strict,
      requireQuote,
    },
    cost: { priceMap },
    inlineOn,
    // Маршрут advisory-находок (minor/nit) считается по доказуемости, а не по severity:
    // порог — сумма весов сигналов (см. lib/actionability.mjs), лимит держит PR читаемым,
    // verifyAdvisory — сколько лучших advisory отдать verifier'у на подтверждение.
    actionability: {
      threshold: actionabilityMin,
      maxAdvisoryInline: numEnv('REVIEW_MAX_ADVISORY_INLINE', 5, env),
      verifyAdvisory: numEnv('REVIEW_VERIFY_ADVISORY', 5, env),
      // Порог ДОПУСКА к verifier'у — отдельный от порога маршрута и намеренно ниже его.
      // Дефолт «порог минус вес сигнала `verified`» (ACTIONABILITY_SIGNALS в
      // lib/actionability.mjs, вес 3): кандидатом становится всё, что СТАНЕТ доказуемым,
      // если verifier подтвердит. Совпадение порогов давало курицу и яйцо — чтобы minor
      // дошёл до verifier'а за +3, он должен был уже набрать 4 без него, то есть уже
      // уходить в строку (docs/review-quality.md §2.2, P1).
      verifyMin: numEnv('REVIEW_VERIFY_ADVISORY_MIN', actionabilityMin - VERIFIED_SIGNAL_WEIGHT, env),
    },
    verifier: {
      enabled: verifierEnabled,
      model: verifierModel,
      maxCandidates: numEnv('REVIEW_VERIFIER_MAX', 12, env),
      // Блокеры сверх капа одного вызова — следующими батчами, а не молча в minor (RFC 17).
      maxBatches: numEnv('REVIEW_VERIFIER_BATCHES', 3, env),
      temperature: verifierTemp,
      // ±строк вокруг каждой строки-кандидата в контексте verifier'а. Целые файлы (до
      // limits.maxFileLines = 1500 строк) второй проход почти не использует: он проверяет
      // 1–2 строки на файл и делает это ДРУГОЙ моделью, то есть без шанса на кэш префикса
      // advisor'ов. См. lib/context-window.mjs.
      contextMargin: numEnv('REVIEW_VERIFIER_CONTEXT_MARGIN', 80, env),
      // ±строк вокруг строки вызова в файле вне PR для blast-кандидата (RFC 21).
      blastCallerMargin: numEnv('REVIEW_BLAST_VERIFY_MARGIN', 20, env),
      // Fail-closed для блоков: verifier отработал, но verdict'а по кандидату нет → minor.
      // Блок требует подтверждения второй независимой моделью (0 = fail-open, старое поведение).
      strictBlocks: envFlag('REVIEW_VERIFIER_STRICT', true, env),
      // Карточки символов verifier'у (RFC 14, решение 4): выключено до gold ×3 — это вход
      // судьи, чей вердикт решает блокировку.
      symbolCards: envFlag('REVIEW_VERIFIER_SYMBOL_CARDS', false, env),
    },
    router: { enabled: routerEnabled },
    specialists: {
      enabled: specialistsEnabled,
      assignments: specialistAssignments,
      // auto — слить роли в один вызов, когда модель у всех одна (RFC 04, решение 4);
      // always — сливать и при разных моделях; never — всегда опрашивать по отдельности.
      merge: ['auto', 'always', 'never'].includes((env.REVIEW_SPECIALISTS_MERGE || '').trim().toLowerCase())
        ? env.REVIEW_SPECIALISTS_MERGE.trim().toLowerCase()
        : 'auto',
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
    // Инкрементальный прогон: не ревьюить заново файлы, чей вход для модели не изменился
    // (RFC 05). fullEvery — каждый N-й прогон полный, страховка от накопления дрейфа.
    incremental: {
      enabled: envFlag('REVIEW_INCREMENTAL', false, env),
      stateFile: env.REVIEW_INCREMENTAL_STATE || p.incrementalState,
      fullEvery: numEnv('REVIEW_INCREMENTAL_FULL_EVERY', 10, env),
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
      trustedAuthors: trustedAuthorsRaw,
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
      // Добавленный файл целиком лежит в диффе — второй экземпляр в контексте оплачивается
      // каждой ролью на каждом чанке. Выключать имеет смысл только для отладки паритета.
      skipAddedFileContext: envFlag('REVIEW_SKIP_ADDED_FILE_CONTEXT', true, env),
      // Чанк без исполняемого кода обслуживает один агрегатор вместо жюри (RFC 04).
      chunkRouting: envFlag('REVIEW_CHUNK_ROUTING', true, env),
      // Окна контекста для advisors и жюри (RFC 07): в промпт едут окрестности изменённых
      // строк файла, а не файл целиком. НОЛЬ здесь — «окна выключены» (файлы едут целиком,
      // это и есть откат), а не «окно нулевой ширины»: отсюда nonNegEnv вместо numEnv.
      contextMargin: nonNegEnv('REVIEW_CONTEXT_MARGIN', 40, env),
      // Файл короче этого прикладывается целиком: там, где файл помещается в пару окон,
      // усечение — чистый риск без выигрыша (RFC 07, решение 2).
      contextWindowMinChars: nonNegEnv('REVIEW_CONTEXT_WINDOW_MIN_CHARS', 8000, env),
      // Карточки символов (RFC 14, шаг 2): строка объявления того, что вызывают добавленные
      // строки, если модель её не видит. Включено по gold A/B: вреда не найдено (RFC 14).
      // Без graph.json карточек нет — фича молча выключена.
      symbolCards: envFlag('REVIEW_SYMBOL_CARDS', true, env),
      symbolCardsMax: numEnv('REVIEW_SYMBOL_CARDS_MAX', 20, env),
      symbolCardsMaxChars: numEnv('REVIEW_SYMBOL_CARDS_MAX_CHARS', 3000, env),
      // Тело не длиннее N строк едет целиком; 0 — только объявление.
      symbolCardsBodyMaxLines: nonNegEnv('REVIEW_SYMBOL_CARDS_BODY_MAX_LINES', 0, env),
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
    // Пиктограммы вместо эмодзи в PR-комментариях (lib/icons.mjs); пусто — эмодзи.
    render: {
      iconBaseUrl: env.REVIEW_ICON_BASE_URL || '',
    },
    telegram: {
      token: env.TELEGRAM_BOT_TOKEN || '',
      chat: env.TELEGRAM_CHAT_ID || '',
    },
  };
}
