/**
 * Фаза 1, шаг 6: lib/profile.mjs — обёртка над Rust-бинарём rust/target/debug/config.
 *
 * Граница порта (см. rust/src/config.rs): в Rust ушла ЧИСТАЯ нормализация профиля
 * (createMarkers, выбор raw-vs-fallback для regex-источников, merge paths, appDir,
 * toolName/sarifRulePrefix, astgrep allow/scopes как строки). В JS остаётся то, что
 * не круглится через JSON или живёт на границе процесса:
 *   - resolveProfile/loadProfileFile — fs + process-wide singleton (active/explicit);
 *   - readRepoRules — чтение repoRulesFile/AGENTS.md из диска;
 *   - компиляция RegExp (валидация диалектом JS — regex-крейт Rust не 1:1 с JS:
 *     lookahead'и и пр.), Map/Set, getter `dataRe` (свежий RegExp на чтение).
 *
 * Резолвинг бинаря (по аналогии с lib/diff.mjs):
 *   AI_REVIEWER_CONFIG_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/config;
 *   AI_REVIEWER_CONFIG_FORCE_JS=1 — принудительно чистый JS (паритетные прогоны/отладка).
 *
 * Если бинарь отсутствует (чистый клон до первого `cargo build`), обёртка деградирует
 * на чистый JS fallback — эталонную копию исходной реализации. Битый бинарь (non-zero
 * exit) fallback НЕ подменяет: молчаливая подмена скрывала бы регрессии Rust-кода.
 */

import { readFileSync } from 'node:fs';
import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, isAbsolute, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { pkgPath } from './package-root.mjs';

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

/** Имя файла профиля, который движок ищет в корне репозитория-потребителя. */
export const PROFILE_FILENAMES = ['.ai-reviewer.json', 'ai-reviewer.config.json'];

/**
 * Нейтральные дефолты. Ни одного project-specific пути: репозиторий без профиля
 * получает ревью, где prescan видит любой TS/JS-код в корне, а комментарии помечены `ai-review`.
 */
export const DEFAULT_PROFILE = Object.freeze({
  name: 'default',
  markerNamespace: 'ai-review',
  toolName: 'ai-review',
  sarifRulePrefix: 'ai-review',
  repositoryUri: '',
  repoRules: '',
  prompts: Object.freeze({
    advisorExtra: '',
    specialistExtra: '',
    aggregatorExtra: '',
    verifierExtra: '',
  }),
  layout: Object.freeze({
    appDir: '',
    layerDirs: Object.freeze([]),
    lintable: '\\.(?:ts|tsx|mts|cts|js|jsx|mjs|cjs)$',
    e2eSpec: '(?:^|/)(?:e2e|tests?)/.+\\.(?:spec|test)\\.[cm]?[jt]sx?$',
    logic: '\\.(?:ts|tsx|js|jsx|mjs|cjs|vue|svelte)$',
    sensitive: '',
  }),
  astgrep: Object.freeze({ scopes: Object.freeze({}), allow: Object.freeze({}) }),
  paths: Object.freeze({
    feedbackLog: '.ai-review/feedback/fp-feedback.jsonl',
    feedbackCandidatesDir: '.ai-review/feedback/candidates',
    fpPatterns: '',                 // '' → стартовый набор из пакета (feedback/fp-patterns.jsonl)
    fpMarkdown: 'docs/ai-review-false-positives.md',
    observabilityDir: '.ai-review/observability',
    exportDir: '.ai-review/export',
    structureExportDir: '.ai-review/structure',
    coverageLcov: 'coverage/lcov.info',
    astgrepConfig: '',              // '' → ruleset из пакета (ast-grep/sgconfig.yml)
  }),
});

/** Маркеры комментариев из namespace: sticky, inline и ключ машинного футера. */
export function createMarkers(namespace = DEFAULT_PROFILE.markerNamespace) {
  const r = runRust('createMarkers', { namespace });
  const base = r ?? createMarkersJs(namespace);
  return Object.freeze({
    ns: base.ns,
    review: base.review,
    inline: base.inline,
    dataKey: base.dataKey,
    get dataRe() {
      // Свежий объект на каждое чтение: у RegExp есть состояние (lastIndex), и общий
      // экземпляр между вызовами exec на разных телах комментариев вёл бы себя непредсказуемо.
      return new RegExp(`<!--\\s*${base.dataKey}([\\s\\S]*?)-->`);
    },
  });
}

function compileRegex(source, fallback) {
  const raw = typeof source === 'string' && source.trim() ? source : fallback;
  if (!raw) return null;
  try {
    return new RegExp(raw, 'i');
  } catch {
    return fallback ? new RegExp(fallback, 'i') : null;
  }
}

/** Компиляция кандидата из Rust-фактов: валидация диалектом JS, при битом — fallback. */
function compileCandidate(candidate, fallback) {
  if (candidate == null) return null;
  try {
    return new RegExp(candidate, 'i');
  } catch {
    return fallback ? new RegExp(fallback, 'i') : null;
  }
}

function readRepoRules(raw, baseDir) {
  if (typeof raw.repoRules === 'string' && raw.repoRules.trim()) return raw.repoRules;
  if (typeof raw.repoRulesFile === 'string' && raw.repoRulesFile.trim()) {
    const file = isAbsolute(raw.repoRulesFile) ? raw.repoRulesFile : resolve(baseDir, raw.repoRulesFile);
    try {
      return readFileSync(file, 'utf8').trim();
    } catch {
      return '';
    }
  }
  // De facto стандарт (Cursor / PR-Agent / OpenAI): AGENTS.md в корне репо-потребителя.
  // Только когда профиль не задал явных правил — иначе чужой AGENTS.md не перетирает профиль.
  for (const name of ['AGENTS.md', 'agents.md']) {
    try {
      const text = readFileSync(resolve(baseDir, name), 'utf8').trim();
      if (text) return text;
    } catch { /* нет файла */ }
  }
  // baseDir профиля может быть ops/ai-review/ — тогда AGENTS.md ищем ещё в cwd прогона.
  if (resolve(baseDir) !== resolve(process.cwd())) {
    for (const name of ['AGENTS.md', 'agents.md']) {
      try {
        const text = readFileSync(resolve(process.cwd(), name), 'utf8').trim();
        if (text) return text;
      } catch { /* нет файла */ }
    }
  }
  return DEFAULT_PROFILE.repoRules;
}

/** Опциональные вставки в системные промпты — без правки кода движка. */
function normalizePrompts(raw = {}) {
  const str = (v) => (typeof v === 'string' && v.trim() ? v.trim() : '');
  return Object.freeze({
    advisorExtra: str(raw.advisorExtra),
    specialistExtra: str(raw.specialistExtra),
    aggregatorExtra: str(raw.aggregatorExtra),
    verifierExtra: str(raw.verifierExtra),
  });
}

/**
 * Сырой JSON → нормализованный профиль (скомпилированные регексы, маркеры, пути).
 * Сериализуемая нормализация — в Rust; здесь только компиляция RegExp/Map/Set,
 * readRepoRules (fs) и заморозка.
 */
export function normalizeProfile(raw = {}, { baseDir = process.cwd(), source = '' } = {}) {
  const facts = runRust('normalizeProfile', { raw, baseDir, source })
    ?? normalizeProfileFactsJs(raw, baseDir, source);
  const rawObj = raw && typeof raw === 'object' && !Array.isArray(raw) ? raw : {};
  return Object.freeze({
    name: facts.name,
    source,
    baseDir,
    markerNamespace: facts.markerNamespace,
    toolName: facts.toolName,
    sarifRulePrefix: facts.sarifRulePrefix,
    repositoryUri: facts.repositoryUri,
    repoRules: readRepoRules(rawObj, baseDir),
    prompts: Object.freeze({ ...facts.prompts }),
    markers: createMarkers(facts.markerNamespace),
    layout: Object.freeze({
      appDir: facts.layout.appDir,
      layerDirs: Object.freeze([...facts.layout.layerDirs]),
      appPrefix: facts.layout.appPrefixSource ? new RegExp(facts.layout.appPrefixSource) : null,
      lintable: compileCandidate(facts.layout.lintable, DEFAULT_PROFILE.layout.lintable),
      sensitive: compileCandidate(facts.layout.sensitive, ''),
      e2eSpec: compileCandidate(facts.layout.e2eSpec, DEFAULT_PROFILE.layout.e2eSpec),
      logic: compileCandidate(facts.layout.logic, DEFAULT_PROFILE.layout.logic),
      baseDir,
    }),
    astgrep: Object.freeze({
      scopes: new Map(
        Object.entries(facts.astgrep.scopes || {})
          .map(([ruleId, src]) => [ruleId, compileCandidate(src, '')])
          .filter(([, re]) => re !== null),
      ),
      allow: new Map(
        Object.entries(facts.astgrep.allow || {}).map(([ruleId, paths]) => [ruleId, new Set(paths)]),
      ),
    }),
    strict: facts.strict === true,
    paths: Object.freeze({ ...facts.paths }),
  });
}

/** Профиль из JSON-файла. Бросает — вызывающий решает, fail-open это или нет. */
export function loadProfileFile(file) {
  const abs = isAbsolute(file) ? file : resolve(process.cwd(), file);
  const raw = JSON.parse(readFileSync(abs, 'utf8'));
  return normalizeProfile(raw, { baseDir: dirname(abs), source: abs });
}

const BUILT_IN_DEFAULT = normalizeProfile({}, { baseDir: pkgPath('.'), source: '(default)' });

let active = BUILT_IN_DEFAULT;
let explicit = false;

/** Активный профиль прогона. */
export function getProfile() {
  return active;
}

/** Явно назначить профиль (CLI bootstrap, тесты). Такой профиль не перетирается автопоиском. */
export function setProfile(profile) {
  active = profile ? (profile.markers ? profile : normalizeProfile(profile)) : BUILT_IN_DEFAULT;
  explicit = true;
  return active;
}

export function resetProfile() {
  active = BUILT_IN_DEFAULT;
  explicit = false;
  return active;
}

/**
 * Профиль для прогона: `REVIEW_PROFILE` → уже назначенный явно → автопоиск в cwd → дефолт.
 * Профиль, который не читается, не должен ронять ревью — падаем в дефолтный и пишем в лог.
 * Не портировано в Rust намеренно: fs + process-wide singleton.
 */
export function resolveProfile(env = process.env, { cwd = process.cwd(), log } = {}) {
  const named = String(env.REVIEW_PROFILE || '').trim();
  if (named) {
    try {
      return setProfile(loadProfileFile(isAbsolute(named) ? named : resolve(cwd, named)));
    } catch (e) {
      log?.(`profile: не удалось прочитать ${named} (${e.message}) — работаем на дефолтном профиле`);
      return resetProfile();
    }
  }
  if (explicit) return active;
  for (const name of PROFILE_FILENAMES) {
    try {
      return setProfile(loadProfileFile(resolve(cwd, name)));
    } catch { /* нет файла — пробуем следующее имя */ }
  }
  return active;
}

/** Путь из профиля/env → абсолютный (относительные — от корня репозитория-потребителя). */
export function profilePath(value, { cwd = process.cwd() } = {}) {
  if (!value) return '';
  return isAbsolute(value) ? value : resolve(cwd, value);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/profile.mjs до Фазы 1).
// Используется только когда Rust-бинарь отсутствует; при битом бинаре НЕ подменяет его.
// ---------------------------------------------------------------------------

function createMarkersJs(namespace = DEFAULT_PROFILE.markerNamespace) {
  const ns = String(namespace || DEFAULT_PROFILE.markerNamespace).trim() || DEFAULT_PROFILE.markerNamespace;
  const dataKey = `${ns}-data:`;
  return { ns, review: `<!-- ${ns} -->`, inline: `<!-- ${ns}-inline -->`, dataKey };
}

function normalizeLayoutJs(raw = {}, baseDir) {
  const d = DEFAULT_PROFILE.layout;
  const appDir = String(raw.appDir ?? d.appDir).replace(/^\.\/+|\/+$/g, '');
  const layerDirs = Array.isArray(raw.layerDirs)
    ? raw.layerDirs.map((s) => String(s).trim()).filter(Boolean)
    : [...d.layerDirs];
  // Префикс appDir у пути из diff восстанавливаем только для перечисленных слоёв: иначе
  // в репозитории без appDir любой `server/x.ts` уехал бы в несуществующий `app/server/x.ts`.
  const appPrefix = appDir && layerDirs.length
    ? new RegExp(`^(?:${layerDirs.join('|')})/`)
    : null;
  return Object.freeze({
    appDir,
    layerDirs: Object.freeze(layerDirs),
    appPrefix,
    lintable: compileRegex(raw.lintable, d.lintable),
    // Зоны повышенной цены ошибки сверх встроенных (авторизация, деньги, миграции, workflows).
    sensitive: compileRegex(raw.sensitive, ''),
    e2eSpec: compileRegex(raw.e2eSpec, d.e2eSpec),
    logic: compileRegex(raw.logic, d.logic),
    baseDir,
  });
}

function normalizeAstgrepJs(raw = {}) {
  const scopes = new Map();
  for (const [ruleId, pattern] of Object.entries(raw.scopes || {})) {
    const re = compileRegex(pattern, '');
    if (re) scopes.set(ruleId, re);
  }
  const allow = new Map();
  for (const [ruleId, paths] of Object.entries(raw.allow || {})) {
    allow.set(ruleId, new Set((Array.isArray(paths) ? paths : [paths]).map(String)));
  }
  return Object.freeze({ scopes, allow });
}

/** Сериализуемая нормализация (факты) — JS-эталон op `normalizeProfile` бинаря config. */
function normalizeProfileFactsJs(raw = {}, baseDir, source) {
  const rawObj = raw && typeof raw === 'object' && !Array.isArray(raw) ? raw : {};
  const markerNamespace = String(rawObj.markerNamespace || DEFAULT_PROFILE.markerNamespace).trim()
    || DEFAULT_PROFILE.markerNamespace;
  const toolName = String(rawObj.toolName || markerNamespace).trim() || DEFAULT_PROFILE.toolName;
  const layout = normalizeLayoutJs(rawObj.layout || {}, baseDir);
  const astgrep = normalizeAstgrepJs(rawObj.astgrep || {});
  return {
    name: String(rawObj.name || DEFAULT_PROFILE.name),
    source,
    baseDir,
    markerNamespace,
    toolName,
    sarifRulePrefix: String(rawObj.sarifRulePrefix || toolName).trim() || DEFAULT_PROFILE.sarifRulePrefix,
    repositoryUri: String(rawObj.repositoryUri || DEFAULT_PROFILE.repositoryUri),
    prompts: { ...normalizePrompts(rawObj.prompts) },
    markers: createMarkersJs(markerNamespace),
    layout: {
      appDir: layout.appDir,
      layerDirs: [...layout.layerDirs],
      appPrefixSource: layout.appPrefix ? layout.appPrefix.source : null,
      lintable: layout.lintable ? layout.lintable.source : null,
      sensitive: layout.sensitive ? layout.sensitive.source : null,
      e2eSpec: layout.e2eSpec ? layout.e2eSpec.source : null,
      logic: layout.logic ? layout.logic.source : null,
    },
    astgrep: {
      scopes: Object.fromEntries([...astgrep.scopes.entries()].map(([k, re]) => [k, re.source])),
      allow: Object.fromEntries([...astgrep.allow.entries()].map(([k, s]) => [k, [...s]])),
    },
    strict: rawObj.strict === true,
    paths: { ...DEFAULT_PROFILE.paths, ...(rawObj.paths || {}) },
  };
}
