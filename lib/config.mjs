import { numEnv, parseModelList } from './util.mjs';
import {
  DEFAULT_MOA_REFERENCES, DEFAULT_MOA_AGGREGATOR, DEFAULT_REVIEW_VERIFIER_MODEL,
  DEFAULT_OLLAMA_MODELS_FALLBACK,
} from './constants.mjs';
import { parseSpecialistAssignments } from './specialists.mjs';
import { pkgPath } from './package-root.mjs';

/** CLI-флаги (--dry-run, --diff, --pr, --repo). */
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

/** Единый конфиг из env + CLI; передаётся по пайплайну вместо глобалов. */
export function loadReviewConfig(env = process.env, cli = parseCli()) {
  const ollamaModel = env.OLLAMA_MODEL || 'kimi-k2.7-code';
  const reviewMode = (env.REVIEW_MODE || 'moa').trim().toLowerCase();
  const moaReferences = parseModelList(env.MOA_REFERENCE_MODELS || DEFAULT_MOA_REFERENCES);
  const specialistsEnabled = reviewMode === 'moa' && env.REVIEW_SPECIALISTS !== '0';
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
    return Number.isFinite(n) && n >= 0 ? n : 0.6;
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
  const verifierEnabled = env.REVIEW_VERIFIER !== '0';
  const verifierModel = (env.REVIEW_VERIFIER_MODEL || DEFAULT_REVIEW_VERIFIER_MODEL).trim();
  const verifierTemp = (() => {
    const n = Number(env.REVIEW_VERIFIER_TEMPERATURE);
    return Number.isFinite(n) && n >= 0 ? n : 0.2;
  })();
  const routerEnabled = env.REVIEW_ROUTER !== '0';
  const prescanEnabled = env.REVIEW_PRESCAN !== '0';
  const prescanBlockRules = (env.REVIEW_PRESCAN_BLOCK_RULES ?? 'no-restricted-imports,no-restricted-globals')
    .split(',').map((s) => s.trim()).filter(Boolean);
  const astgrepEnabled = env.REVIEW_PRESCAN_ASTGREP !== '0';
  const astgrepBlockRules = (env.REVIEW_PRESCAN_ASTGREP_BLOCK_RULES ?? 'raw-x-forwarded-for')
    .split(',').map((s) => s.trim()).filter(Boolean);
  const observabilityEnabled = env.REVIEW_OBSERVABILITY !== '0';
  const feedbackEnabled = env.REVIEW_FEEDBACK !== '0';
  const exportEnabled = env.REVIEW_EXPORT !== '0';
  const triageEnabled = env.REVIEW_AUTO_TRIAGE !== '0';

  return Object.freeze({
    cli,
    reviewMode,
    httpRetries,
    github: Object.freeze({
      api: (env.GITHUB_API_URL || 'https://api.github.com').replace(/\/$/, ''),
      token: env.GITHUB_TOKEN || '',
      repo: cli.repo,
      prNumber: cli.prNumber,
    }),
    ollama: Object.freeze({
      key: env.OLLAMA_API_KEY || '',
      base: (env.OLLAMA_BASE_URL || 'https://ollama.com/v1').replace(/\/$/, ''),
      model: ollamaModel,
      temperature,
      jsonMode: env.REVIEW_JSON_MODE !== '0',
      seed,
      httpRetries,
    }),
    moa: Object.freeze({
      references: moaReferences,
      aggregator: moaAggregator,
      label: moaLabel,
      refMaxTokens: numEnv('MOA_REFERENCE_MAX_TOKENS', 12_000, env),
      refTemp: moaRefTemp,
      aggTemp: moaAggTemp,
    }),
    models: Object.freeze({
      list: models,
      fallback: parseModelList(env.OLLAMA_MODELS_FALLBACK || DEFAULT_OLLAMA_MODELS_FALLBACK, { exclude: models }),
    }),
    gate: Object.freeze({ blockOn, blockSingle, consensusMin, confidenceMin }),
    inlineOn: Object.freeze(inlineOn),
    verifier: Object.freeze({
      enabled: verifierEnabled,
      model: verifierModel,
      maxCandidates: numEnv('REVIEW_VERIFIER_MAX', 12, env),
      temperature: verifierTemp,
    }),
    router: Object.freeze({ enabled: routerEnabled }),
    specialists: Object.freeze({
      enabled: specialistsEnabled,
      assignments: Object.freeze(specialistAssignments.map((a) => Object.freeze({ ...a }))),
    }),
    prescan: Object.freeze({
      enabled: prescanEnabled,
      blockEnabled: env.REVIEW_PRESCAN_BLOCK !== '0',
      appDir: env.REVIEW_APP_DIR || 'app',
      eslintBin: env.REVIEW_ESLINT_BIN || 'npx',
      maxFiles: numEnv('REVIEW_PRESCAN_MAX_FILES', 30, env),
      blockRules: Object.freeze(prescanBlockRules),
      astgrep: Object.freeze({
        enabled: prescanEnabled && astgrepEnabled,
        configPath: env.REVIEW_ASTGREP_CONFIG || pkgPath('ast-grep/sgconfig.yml'),
        bin: env.REVIEW_ASTGREP_BIN || 'ast-grep',
        blockRules: Object.freeze(astgrepBlockRules),
      }),
    }),
    observability: Object.freeze({
      enabled: observabilityEnabled,
      dir: env.REVIEW_OBSERVABILITY_DIR || '.graphify/graphify-out/observability',
      file: env.REVIEW_OBSERVABILITY_FILE || 'pr-review-runs.jsonl',
    }),
    export: Object.freeze({
      enabled: exportEnabled,
      dir: env.REVIEW_EXPORT_DIR || '.graphify/graphify-out/review-export',
      json: env.REVIEW_EXPORT_JSON !== '0',
      sarif: env.REVIEW_EXPORT_SARIF !== '0',
      sarifAll: env.REVIEW_EXPORT_SARIF_ALL === '1',
      dryRun: env.REVIEW_EXPORT_DRY_RUN === '1',
      reasoningInSticky: env.REVIEW_REASONING_STICKY !== '0',
    }),
    triage: Object.freeze({
      enabled: triageEnabled,
    }),
    feedback: Object.freeze({
      enabled: feedbackEnabled,
      label: env.REVIEW_FEEDBACK_LABEL || 'ai-review-fp',
      logFile: env.REVIEW_FEEDBACK_LOG || pkgPath('feedback/fp-feedback.jsonl'),
      candidatesDir: env.REVIEW_FEEDBACK_CANDIDATES_DIR || pkgPath('feedback/candidates'),
      mdPath: env.REVIEW_FEEDBACK_MD || pkgPath('docs/ai-review-false-positives.md'),
      appendMarkdown: env.REVIEW_FEEDBACK_MD_APPEND === '1',
      writeCandidates: env.REVIEW_FEEDBACK_CANDIDATES !== '0',
      write: env.REVIEW_FEEDBACK_WRITE !== '0',
      trustedAuthors: env.REVIEW_FEEDBACK_TRUSTED_AUTHORS || 'github-actions[bot]',
      reactionConcurrency: numEnv('REVIEW_FEEDBACK_CONCURRENCY', 6, env),
      patternsFile: env.REVIEW_FP_PATTERNS || pkgPath('feedback/fp-patterns.jsonl'),
      promoteMinFpCount: numEnv('REVIEW_FP_PROMOTE_MIN', 2, env),
    }),
    limits: Object.freeze({
      maxDiffBytes: numEnv('REVIEW_MAX_DIFF_BYTES', 200_000, env),
      maxSymbols: 25,
      maxDepsPerSymbol: 20,
      godNodeThreshold: 20,
      maxFileLines: numEnv('REVIEW_MAX_FILE_LINES', 1500, env),
      maxContextBytes: numEnv('REVIEW_MAX_CONTEXT_BYTES', 180_000, env),
      chunkBytes: numEnv('REVIEW_CHUNK_BYTES', 56_000, env),
      maxChunks: numEnv('REVIEW_MAX_CHUNKS', 40, env),
      concurrency: numEnv('REVIEW_CONCURRENCY', 10, env),
      maxInline: numEnv('REVIEW_MAX_INLINE', 50, env),
    }),
    graphify: Object.freeze({
      bin: env.GRAPHIFY_BIN || '.graphify/gf',
      graph: env.GRAPH || '.graphify/graphify-out/graph.json',
    }),
    telegram: Object.freeze({
      token: env.TELEGRAM_BOT_TOKEN || '',
      chat: env.TELEGRAM_CHAT_ID || '',
    }),
  });
}
