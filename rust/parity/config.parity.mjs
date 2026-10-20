/**
 * Parity-прогон Фазы 1, шага 6: Rust config ↔ чистый JS fallback.
 *
 * Запуск: node rust/parity/config.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртки lib/config.mjs и lib/profile.mjs:
 * resolveConfigBin() читает AI_REVIEWER_CONFIG_FORCE_JS на каждый вызов, поэтому
 * js-сторона прогоняется с FORCE_JS=1, rust-сторона — без него (диспетчеризация
 * и frozen-сборка assembleReviewConfig проверяются тоже). Профиль process-wide:
 * resetProfile() перед каждым вызовом. Результаты канонизируются JSON round-trip
 * (`j()`): profile/markers с RegExp/Map/getter'ами выпадают одинаково с обеих сторон.
 *
 * Корпус:
 *   1. loadReviewConfig: матрица env × {default, orakul, strict-кастом, битый}
 *      профиль × cli — приоритеты `||`/`??`, дефолты, флаги, клэмпы, списки;
 *   2. normalizeProfile: raw-профили (в т.ч. битые regex, не-строковые поля) —
 *      сравнение сериализуемой редукции финальных профилей;
 *   3. createMarkers: namespace-края.
 */

import { existsSync, readFileSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');
const BIN = resolve(REPO, 'rust/target/debug/config');

if (!existsSync(BIN)) {
  console.error(`нет бинаря ${BIN} — сначала cargo build`);
  process.exit(1);
}

process.env.AI_REVIEWER_CONFIG_FORCE_JS = '1';
// Сырой ответ Rust: дополнение недостающих ключей из JS скрыло бы отставание порта.
process.env.AI_REVIEWER_CONFIG_NO_BACKFILL = '1';
const { loadReviewConfig } = await import('../../lib/config.mjs');
const P = await import('../../lib/profile.mjs');

const withForceJs = (fn) => {
  process.env.AI_REVIEWER_CONFIG_FORCE_JS = '1';
  try {
    return fn();
  } finally {
    delete process.env.AI_REVIEWER_CONFIG_FORCE_JS;
  }
};
const withRust = (fn) => {
  delete process.env.AI_REVIEWER_CONFIG_FORCE_JS;
  try {
    return fn();
  } finally {
    process.env.AI_REVIEWER_CONFIG_FORCE_JS = '1';
  }
};

/** Канонизация: undefined/функции/RegExp/Map выпадают (JSON их не несёт). */
const j = (x) => JSON.parse(JSON.stringify(x));

let compared = 0;
const failures = [];

/**
 * Ключи, которые с 0.6.2 есть только в Rust (RFC 20, § 6): deprecated JS-копия конфига их не
 * считает и не получит. Срезаются с rust-стороны перед сравнением — остальное сверяется как раньше.
 */
const RUST_OWNED_OLLAMA_KEYS = ['modelSettings', 'modelSettingsErrors'];
const RUST_OWNED_VERIFIER_KEYS = ['fallbackModel'];
function stripRustOwned(x) {
  if (!x || typeof x !== 'object') return x;
  const out = { ...x };
  if (x.ollama && typeof x.ollama === 'object') {
    out.ollama = { ...x.ollama };
    for (const k of RUST_OWNED_OLLAMA_KEYS) delete out.ollama[k];
  }
  if (x.verifier && typeof x.verifier === 'object') {
    out.verifier = { ...x.verifier };
    for (const k of RUST_OWNED_VERIFIER_KEYS) delete out.verifier[k];
  }
  return out;
}

function compare(label, jsResult, rustResult) {
  compared += 1;
  try {
    assert.deepEqual(stripRustOwned(rustResult), jsResult);
  } catch (e) {
    failures.push({
      label,
      js: JSON.stringify(jsResult)?.slice(0, 600),
      rust: JSON.stringify(rustResult)?.slice(0, 600),
    });
  }
}

// ---------------------------------------------------------------------------
// Профили для матрицы
// ---------------------------------------------------------------------------

const ORAKUL_PROFILE = resolve(REPO, 'profiles/orakul.json');

const tmp = mkdtempSync(resolve(tmpdir(), 'ai-reviewer-config-parity-'));
const STRICT_PROFILE = resolve(tmp, 'strict-profile.json');
writeFileSync(STRICT_PROFILE, JSON.stringify({
  name: 'strictp',
  strict: true,
  markerNamespace: 'sp-review',
  layout: { appDir: 'app2', layerDirs: ['x', 'y'] },
  paths: {
    feedbackLog: 'p/fb.jsonl',
    feedbackCandidatesDir: 'p/cand',
    fpPatterns: 'p/patterns.jsonl',
    fpMarkdown: 'p/fp.md',
    observabilityDir: 'p/obs',
    exportDir: 'p/export',
    structureExportDir: 'p/struct',
    coverageLcov: 'p/lcov.info',
    astgrepConfig: 'p/sg.yml',
  },
}), 'utf8');
const BROKEN_PROFILE = resolve(tmp, 'broken.json');
writeFileSync(BROKEN_PROFILE, '{ this is not json', 'utf8');

const CLI_A = { dryRun: true, repo: '', prNumber: 0 };
const CLI_B = { dryRun: false, diffFile: '/tmp/x.patch', repo: 'o/r', prNumber: 17 };

// ---------------------------------------------------------------------------
// Корпус 1: loadReviewConfig — матрица env × профиль × cli
// ---------------------------------------------------------------------------

const envCases = [
  ['empty → default profile', {}, CLI_A],
  ['orakul profile only', { REVIEW_PROFILE: ORAKUL_PROFILE }, CLI_A],
  ['orakul + cli B', { REVIEW_PROFILE: ORAKUL_PROFILE }, CLI_B],
  ['strict profile', { REVIEW_PROFILE: STRICT_PROFILE }, CLI_A],
  ['broken profile → fail-open default', { REVIEW_PROFILE: BROKEN_PROFILE }, CLI_A],
  ['missing profile file → fail-open default', { REVIEW_PROFILE: resolve(tmp, 'nope.json') }, CLI_A],

  // LLM creds: LLM_* бьёт OLLAMA_*; trailing slash; пустые → дефолты.
  ['llm creds both', {
    LLM_API_KEY: ' llm-k ', OLLAMA_API_KEY: 'ollama-k',
    LLM_BASE_URL: 'https://ghe.corp/api/v3/', OLLAMA_BASE_URL: 'https://ollama.local/',
  }, CLI_A],
  ['llm creds ollama only', { OLLAMA_API_KEY: 'ok', OLLAMA_BASE_URL: 'http://x/' }, CLI_A],
  ['llm creds empty base → default', { OLLAMA_API_KEY: 'ok', LLM_BASE_URL: '', OLLAMA_BASE_URL: '' }, CLI_A],
  ['llm creds none', {}, CLI_B],

  // REVIEW_MODE / модели.
  ['mode consensus + models dedupe', {
    REVIEW_MODE: 'consensus', OLLAMA_MODELS: 'm1, m2, m1,', OLLAMA_MODEL: 'single',
  }, CLI_A],
  ['mode MoA uppercase + spaces', { REVIEW_MODE: ' MoA ', OLLAMA_MODELS: 'm9' }, CLI_A],
  ['mode unknown → not moa', { REVIEW_MODE: 'foo', OLLAMA_MODELS: 'm1,m2' }, CLI_A],
  ['mode consensus fallback model', { REVIEW_MODE: 'consensus' }, CLI_A],
  ['ollama model override', { OLLAMA_MODEL: 'custom-model' }, CLI_A],
  ['models fallback env', { OLLAMA_MODELS_FALLBACK: 'f1,f2,f1' }, CLI_A],

  // MoA: references/aggregator/температуры/токены.
  ['moa refs custom', { MOA_REFERENCE_MODELS: 'a,b,a, c' }, CLI_A],
  ['moa refs empty → default', { MOA_REFERENCE_MODELS: '' }, CLI_A],
  ['moa aggregator custom', { MOA_AGGREGATOR: ' agg-x ' }, CLI_A],
  ['moa temps valid', { MOA_REFERENCE_TEMPERATURE: '0.9', MOA_AGGREGATOR_TEMPERATURE: '0' }, CLI_A],
  ['moa temps empty → 0 (Number пусто)', { MOA_REFERENCE_TEMPERATURE: '', MOA_AGGREGATOR_TEMPERATURE: 'abc' }, CLI_A],
  ['moa temps negative → default', { MOA_REFERENCE_TEMPERATURE: '-1', MOA_AGGREGATOR_TEMPERATURE: '-0.5' }, CLI_A],
  ['moa max tokens 0 → default', { MOA_REFERENCE_MAX_TOKENS: '0' }, CLI_A],

  // Specialists.
  ['specialists bulk partial', { MOA_SPECIALIST_MODELS: 'security:s1' }, CLI_A],
  ['specialists per-role override', {
    MOA_SPECIALIST_MODELS: 'security:s1,correctness:c1,architecture:a1',
    MOA_SPECIALIST_SECURITY_MODEL: ' s2 ', MOA_SPECIALIST_ARCHITECTURE_MODEL: '',
  }, CLI_A],
  ['specialists off in moa', { REVIEW_SPECIALISTS: '0', MOA_REFERENCE_MODELS: 'a,b' }, CLI_A],
  ['specialists env in consensus mode ignored', {
    REVIEW_MODE: 'consensus', OLLAMA_MODELS: 'm1,m2', MOA_SPECIALIST_SECURITY_MODEL: 'zz',
  }, CLI_A],

  // Gate.
  ['gate block_on custom', { REVIEW_BLOCK_ON: 'critical, minor ,,nit' }, CLI_A],
  ['gate block_on empty → []', { REVIEW_BLOCK_ON: '' }, CLI_A],
  ['gate inline_on custom', { REVIEW_INLINE_ON: 'critical' }, CLI_A],
  ['gate block_single intersect', { REVIEW_BLOCK_ON: 'critical,minor', REVIEW_BLOCK_SINGLE: 'critical,major,foo' }, CLI_A],
  ['gate block_single empty → []', { REVIEW_BLOCK_SINGLE: '' }, CLI_A],
  ['gate consensus 5 in moa → 1', { REVIEW_CONSENSUS: '5' }, CLI_A],
  ['gate consensus 0/abc → default', { REVIEW_MODE: 'consensus', OLLAMA_MODELS: 'm1,m2,m3', REVIEW_CONSENSUS: 'abc' }, CLI_A],
  ['gate consensus 9 clamp to models', { REVIEW_MODE: 'consensus', OLLAMA_MODELS: 'm1,m2,m3', REVIEW_CONSENSUS: '9' }, CLI_A],
  ['gate consensus 2.5', { REVIEW_MODE: 'consensus', OLLAMA_MODELS: 'm1,m2,m3', REVIEW_CONSENSUS: '2.5' }, CLI_A],
  ['gate confidence valid', { REVIEW_CONFIDENCE_MIN: '0.5' }, CLI_A],
  ['gate confidence out of range', { REVIEW_CONFIDENCE_MIN: '1.2' }, CLI_A],
  ['gate confidence empty → 0 (Number пусто)', { REVIEW_CONFIDENCE_MIN: '' }, CLI_A],

  // Strict / requireQuote.
  ['strict env on', { REVIEW_STRICT: '1' }, CLI_A],
  ['strict env junk → default off', { REVIEW_STRICT: 'junk' }, CLI_A],
  ['strict profile + requireQuote off', { REVIEW_PROFILE: STRICT_PROFILE, REVIEW_REQUIRE_QUOTE: '0' }, CLI_A],
  ['strict profile + requireQuote junk → inherits strict', { REVIEW_PROFILE: STRICT_PROFILE, REVIEW_REQUIRE_QUOTE: 'junk' }, CLI_A],

  // Числа: numEnv/ratioEnv края.
  ['numbers overrides', {
    REVIEW_HTTP_RETRIES: '2.5', REVIEW_LLM_TIMEOUT_MS: '123000', REVIEW_PROVIDER_CONCURRENCY: '3',
    REVIEW_PROVIDER_PER_MODEL: '1', REVIEW_TIME_BUDGET_MIN: '7', REVIEW_CONCURRENCY: '4',
    REVIEW_MAX_DIFF_BYTES: '100', REVIEW_CHUNK_BYTES: '500', REVIEW_MAX_CHUNKS: '2',
  }, CLI_A],
  // Окна контекста (RFC 07): 0 — ЗНАЧАЩЕЕ значение (окна выключены), а не повод взять дефолт.
  ['context window custom', { REVIEW_CONTEXT_MARGIN: '20', REVIEW_CONTEXT_WINDOW_MIN_CHARS: '3000' }, CLI_A],
  ['context window off (0)', { REVIEW_CONTEXT_MARGIN: '0', REVIEW_CONTEXT_WINDOW_MIN_CHARS: '0' }, CLI_A],
  ['context window empty → defaults', { REVIEW_CONTEXT_MARGIN: '', REVIEW_CONTEXT_WINDOW_MIN_CHARS: ' ' }, CLI_A],
  ['context window junk/negative → defaults', { REVIEW_CONTEXT_MARGIN: 'abc', REVIEW_CONTEXT_WINDOW_MIN_CHARS: '-5' }, CLI_A],
  ['context window non-string scalars', { REVIEW_CONTEXT_MARGIN: 0, REVIEW_CONTEXT_WINDOW_MIN_CHARS: 12.5 }, CLI_A],
  ['numbers zero/abc → defaults', {
    REVIEW_HTTP_RETRIES: '0', REVIEW_LLM_TIMEOUT_MS: 'abc', REVIEW_PROVIDER_CONCURRENCY: '-1',
  }, CLI_A],
  ['env non-string scalars', {
    REVIEW_HTTP_RETRIES: 5, REVIEW_TEMPERATURE: 0.5, REVIEW_SPECIALISTS: true, REVIEW_VERIFIER: false,
  }, CLI_A],

  // Prescan / ast-grep.
  ['prescan off', { REVIEW_PRESCAN: '0' }, CLI_A],
  ['prescan block off', { REVIEW_PRESCAN_BLOCK: 'no' }, CLI_A],
  ['prescan block rules empty → []', { REVIEW_PRESCAN_BLOCK_RULES: '' }, CLI_A],
  ['prescan block rules custom', { REVIEW_PRESCAN_BLOCK_RULES: 'a, b,,c' }, CLI_A],
  ['prescan appDir over orakul', { REVIEW_PROFILE: ORAKUL_PROFILE, REVIEW_APP_DIR: 'packages/api' }, CLI_A],
  ['prescan appDir empty kept (??)', { REVIEW_PROFILE: ORAKUL_PROFILE, REVIEW_APP_DIR: '' }, CLI_A],
  ['prescan max files / eslint bin', { REVIEW_PRESCAN_MAX_FILES: '7', REVIEW_ESLINT_BIN: 'eslint' }, CLI_A],
  ['astgrep off', { REVIEW_PRESCAN_ASTGREP: '0' }, CLI_A],
  ['astgrep config empty → profile', { REVIEW_PROFILE: ORAKUL_PROFILE, REVIEW_ASTGREP_CONFIG: '' }, CLI_A],
  ['astgrep config custom', { REVIEW_ASTGREP_CONFIG: 'custom/sg.yml', REVIEW_ASTGREP_BIN: 'sg' }, CLI_A],
  ['astgrep block rules empty → []', { REVIEW_PRESCAN_ASTGREP_BLOCK_RULES: '' }, CLI_A],

  // Structure.
  ['structure GRAPH override', { GRAPH: 'out/g/graph.json' }, CLI_A],
  ['structure graph relative', { REVIEW_STRUCTURE_GRAPH: 'graph.json' }, CLI_A],
  ['structure graph nested', { REVIEW_STRUCTURE_GRAPH: 'out/custom/graph.json' }, CLI_A],
  ['structure analysis explicit', { REVIEW_STRUCTURE_GRAPH_ANALYSIS: 'explicit.json' }, CLI_A],
  ['structure report override', { REVIEW_STRUCTURE_GRAPH_REPORT: 'R.md', GRAPHIFY_REPORT: 'G.md' }, CLI_A],
  ['structure lcov over orakul', { REVIEW_PROFILE: ORAKUL_PROFILE, REVIEW_STRUCTURE_LCOV: 'x/lcov.info' }, CLI_A],
  ['structure trust tier + scope', { REVIEW_STRUCTURE_TRUST_TIER: 'untrusted', REVIEW_STRUCTURE_SCOPE: 'app/server' }, CLI_A],
  ['structure thresholds', {
    REVIEW_STRUCTURE_HIGH_FAN_IN: '9', REVIEW_STRUCTURE_MAX_UNRESOLVED_RATE: '0.5',
    REVIEW_STRUCTURE_MAX_ENTITIES: '10', REVIEW_STRUCTURE_MAX_EDGES: '20',
  }, CLI_A],
  ['structure unresolved rate out of range', { REVIEW_STRUCTURE_MAX_UNRESOLVED_RATE: '2' }, CLI_A],
  ['structure include ambiguous on', { REVIEW_STRUCTURE_INCLUDE_AMBIGUOUS: 'yes' }, CLI_A],

  // Feedback / export / observability поверх orakul (env бьёт пути профиля).
  ['feedback dirs over orakul', {
    REVIEW_PROFILE: ORAKUL_PROFILE,
    REVIEW_FEEDBACK_LABEL: 'lbl', REVIEW_FEEDBACK_LOG: 'x/fb.jsonl',
    REVIEW_FEEDBACK_CANDIDATES_DIR: 'x/cand', REVIEW_FEEDBACK_MD: 'x/fp.md',
    REVIEW_FEEDBACK_TRUSTED_AUTHORS: 'bot[bot],me', REVIEW_FP_PATTERNS: 'x/p.jsonl',
  }, CLI_A],
  ['feedback flags off', {
    REVIEW_FEEDBACK_MD_APPEND: '1', REVIEW_FEEDBACK_CANDIDATES: '0', REVIEW_FEEDBACK_WRITE: 'off',
    REVIEW_FEEDBACK_CONCURRENCY: '2', REVIEW_FP_PROMOTE_MIN: '5',
  }, CLI_A],
  ['export dirs over orakul', {
    REVIEW_PROFILE: ORAKUL_PROFILE,
    REVIEW_EXPORT_DIR: 'out/review', REVIEW_OBSERVABILITY_DIR: 'out/obs',
    REVIEW_OBSERVABILITY_FILE: 'runs.jsonl',
  }, CLI_A],
  ['export flags', {
    REVIEW_EXPORT_JSON: '0', REVIEW_EXPORT_SARIF: 'no', REVIEW_EXPORT_SARIF_ALL: '1',
    REVIEW_EXPORT_DRY_RUN: 'true', REVIEW_REASONING_STICKY: 'off',
  }, CLI_A],
  ['observability off', { REVIEW_OBSERVABILITY: '0' }, CLI_A],
  ['export off', { REVIEW_EXPORT: '0' }, CLI_A],
  ['feedback off', { REVIEW_FEEDBACK: '0' }, CLI_A],
  ['triage off', { REVIEW_AUTO_TRIAGE: '0' }, CLI_A],
  // Доверенные авторы: новое имя главнее алиаса; пустая строка — откат «доверять всем».
  ['trusted authors new name', { REVIEW_TRUSTED_AUTHORS: ' a[bot] , b ,', REVIEW_FEEDBACK_TRUSTED_AUTHORS: 'old[bot]' }, CLI_A],
  ['trusted authors empty rollback', { REVIEW_TRUSTED_AUTHORS: '' }, CLI_A],
  ['trusted authors alias empty', { REVIEW_FEEDBACK_TRUSTED_AUTHORS: '' }, CLI_A],

  // Price map.
  ['price json valid', { REVIEW_PRICE_JSON: '{"m1":{"in":1.5,"out":2}}' }, CLI_A],
  ['price json broken', { REVIEW_PRICE_JSON: '{broken' }, CLI_A],
  ['price json array', { REVIEW_PRICE_JSON: '[]' }, CLI_A],
  ['price json scalar → null', { REVIEW_PRICE_JSON: '5' }, CLI_A],
  ['price json null → null', { REVIEW_PRICE_JSON: 'null' }, CLI_A],

  // GitHub / graphify / telegram / прочее.
  ['github api trailing slash + token', { GITHUB_API_URL: 'https://ghe/api/v3/', GITHUB_TOKEN: 't' }, CLI_A],
  ['graphify bin + depth high', { GRAPHIFY_BIN: 'gf2', REVIEW_GRAPHIFY_DEPTH: '99' }, CLI_A],
  ['graphify depth 0 → default 2', { REVIEW_GRAPHIFY_DEPTH: '0' }, CLI_A],
  ['graphify drop ambiguous off', { REVIEW_GRAPHIFY_DROP_AMBIGUOUS: 'no' }, CLI_A],
  ['telegram', { TELEGRAM_BOT_TOKEN: 'tt', TELEGRAM_CHAT_ID: 'cc' }, CLI_A],
  ['render: iconBaseUrl', { REVIEW_ICON_BASE_URL: 'https://icons.example/v1/icons' }, CLI_A],
  ['seed valid / effort', { REVIEW_SEED: '42', REVIEW_REASONING_EFFORT: 'none' }, CLI_A],
  ['seed non-integer → 7', { REVIEW_SEED: '3.5' }, CLI_A],
  ['review temperature empty → 0', { REVIEW_TEMPERATURE: '' }, CLI_A],
  ['review temperature negative → default', { REVIEW_TEMPERATURE: '-2' }, CLI_A],
  ['json mode off', { REVIEW_JSON_MODE: '0' }, CLI_A],
  ['verifier custom', {
    REVIEW_VERIFIER_MODEL: ' v-x ', REVIEW_VERIFIER_TEMPERATURE: '0.1', REVIEW_VERIFIER_MAX: '3',
  }, CLI_A],
  ['verifier off', { REVIEW_VERIFIER: '0' }, CLI_A],
  ['verifier strict off', { REVIEW_VERIFIER_STRICT: '0' }, CLI_A],
  ['router off', { REVIEW_ROUTER: '0' }, CLI_A],
  ['llm required off', { REVIEW_LLM_REQUIRED: '0' }, CLI_A],
  ['actionability custom', {
    REVIEW_ACTIONABILITY_MIN: '6', REVIEW_MAX_ADVISORY_INLINE: '2', REVIEW_VERIFY_ADVISORY: '1',
  }, CLI_A],
];

// Флаговая матрица: каждый default-on/off флаг × значения envFlag.
const DEFAULT_ON_FLAGS = [
  'REVIEW_VERIFIER', 'REVIEW_ROUTER', 'REVIEW_PRESCAN', 'REVIEW_PRESCAN_BLOCK',
  'REVIEW_PRESCAN_ASTGREP', 'REVIEW_OBSERVABILITY', 'REVIEW_FEEDBACK', 'REVIEW_EXPORT',
  'REVIEW_AUTO_TRIAGE', 'REVIEW_LLM_REQUIRED', 'REVIEW_VERIFIER_STRICT', 'REVIEW_STRUCTURE',
  'REVIEW_JSON_MODE', 'REVIEW_GRAPHIFY_DROP_AMBIGUOUS',
];
const DEFAULT_OFF_FLAGS = [
  'REVIEW_STRICT', 'REVIEW_REQUIRE_QUOTE', 'REVIEW_EXPORT_SARIF_ALL', 'REVIEW_EXPORT_DRY_RUN',
  'REVIEW_FEEDBACK_MD_APPEND', 'REVIEW_STRUCTURE_INCLUDE_AMBIGUOUS',
];
const FLAG_VALUES = ['0', 'false', 'no', 'off', '1', 'true', 'yes', 'on', 'junk', ''];
for (const flag of [...DEFAULT_ON_FLAGS, ...DEFAULT_OFF_FLAGS]) {
  for (const value of FLAG_VALUES) {
    envCases.push([`flag ${flag}=${JSON.stringify(value)}`, { [flag]: value, OLLAMA_API_KEY: 'x' }, CLI_A]);
  }
}

for (const [label, env, cli] of envCases) {
  const js = withForceJs(() => {
    P.resetProfile();
    try {
      return j(loadReviewConfig(env, cli));
    } finally {
      P.resetProfile();
    }
  });
  const rust = withRust(() => {
    P.resetProfile();
    try {
      return j(loadReviewConfig(env, cli));
    } finally {
      P.resetProfile();
    }
  });
  compare(`loadReviewConfig ${label}`, js, rust);
}

// ---------------------------------------------------------------------------
// Корпус 2: normalizeProfile — сериализуемая редукция финальных профилей
// ---------------------------------------------------------------------------

const reduceProfile = (pr) => ({
  name: pr.name,
  source: pr.source,
  baseDir: pr.baseDir,
  markerNamespace: pr.markerNamespace,
  toolName: pr.toolName,
  sarifRulePrefix: pr.sarifRulePrefix,
  repositoryUri: pr.repositoryUri,
  prompts: pr.prompts,
  markers: {
    ns: pr.markers.ns,
    review: pr.markers.review,
    inline: pr.markers.inline,
    dataKey: pr.markers.dataKey,
  },
  layout: {
    appDir: pr.layout.appDir,
    layerDirs: [...pr.layout.layerDirs],
    appPrefix: pr.layout.appPrefix ? pr.layout.appPrefix.source : null,
    lintable: pr.layout.lintable ? pr.layout.lintable.source : null,
    sensitive: pr.layout.sensitive ? pr.layout.sensitive.source : null,
    e2eSpec: pr.layout.e2eSpec ? pr.layout.e2eSpec.source : null,
    logic: pr.layout.logic ? pr.layout.logic.source : null,
  },
  astgrep: {
    scopes: Object.fromEntries([...pr.astgrep.scopes.entries()].map(([k, re]) => [k, re.source])),
    allow: Object.fromEntries([...pr.astgrep.allow.entries()].map(([k, s]) => [k, [...s]])),
  },
  strict: pr.strict,
  paths: pr.paths,
});

const orakulRaw = JSON.parse(readFileSync(ORAKUL_PROFILE, 'utf8'));
const rawProfiles = [
  ['orakul', orakulRaw],
  ['empty', {}],
  ['acme', {
    name: 'acme',
    markerNamespace: 'acme-review',
    layout: { appDir: 'app/', layerDirs: ['server'], lintable: '([unclosed' },
    astgrep: { scopes: { r1: '^app/server/' }, allow: { r1: 'app/server/ok.ts' } },
  }],
  ['weird strings', {
    name: 'w',
    markerNamespace: '  ',
    toolName: '',
    sarifRulePrefix: ' ',
    repositoryUri: 'https://x/y',
    layout: { appDir: '././app//', layerDirs: 'not-an-array', sensitive: '(?:^|/)ops/' },
    prompts: { advisorExtra: '  A  ', verifierExtra: 5, aggregatorExtra: '' },
  }],
  ['astgrep mixed', {
    astgrep: {
      scopes: { r1: '([unclosed', r2: '^app/', r3: '' },
      allow: { r1: ['a.ts', 5], r3: null, r4: 'x.ts' },
    },
  }],
  ['strict + paths', {
    strict: true,
    paths: { exportDir: 'x/out', astgrepConfig: 'x/sg.yml' },
  }],
  ['custom valid regexes', {
    layout: { lintable: '^app/.*\\.ts$', e2eSpec: '^e2e/', logic: '\\.ts$', sensitive: 'money' },
  }],
];

for (const [label, raw] of rawProfiles) {
  const opts = { baseDir: tmp, source: `${tmp}/${label}.json` };
  const js = withForceJs(() => j(reduceProfile(P.normalizeProfile(raw, opts))));
  const rust = withRust(() => j(reduceProfile(P.normalizeProfile(raw, opts))));
  compare(`normalizeProfile ${label}`, js, rust);
}

// ---------------------------------------------------------------------------
// Корпус 3: createMarkers
// ---------------------------------------------------------------------------

for (const namespace of ['', '   ', 'acme-review', 'ns x ', undefined]) {
  const js = withForceJs(() => {
    const m = P.createMarkers(namespace);
    return { ns: m.ns, review: m.review, inline: m.inline, dataKey: m.dataKey, sample: m.dataRe.exec('<!-- x-data:{"a":1} -->')?.[1] ?? null };
  });
  const rust = withRust(() => {
    const m = P.createMarkers(namespace);
    return { ns: m.ns, review: m.review, inline: m.inline, dataKey: m.dataKey, sample: m.dataRe.exec('<!-- x-data:{"a":1} -->')?.[1] ?? null };
  });
  compare(`createMarkers ${JSON.stringify(namespace)}`, js, rust);
}

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

rmSync(tmp, { recursive: true, force: true });
console.log(`config parity cases compared: ${compared}`);
if (failures.length) {
  console.log(`FAILURES: ${failures.length}`);
  for (const f of failures.slice(0, 20)) {
    console.log('---', f.label);
    console.log('  js  :', f.js);
    console.log('  rust:', f.rust);
  }
  process.exit(1);
}
console.log('PARITY OK');
