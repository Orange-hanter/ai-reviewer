/**
 * Project profile — то, чем движок отличается от репозитория к репозиторию.
 *
 * Движок сам по себе ничего не знает про конкретный проект: имена маркеров в комментариях,
 * текст правил репозитория для промпта, раскладка каталогов для prescan, scope ast-grep правил
 * и пути для feedback/observability приходят ИЗ ПРОФИЛЯ. Дефолтный профиль — нейтральный,
 * подходит любому репозиторию; конкретный проект кладёт свой JSON и указывает
 * `REVIEW_PROFILE=path/to/profile.json` (или `.ai-reviewer.json` в корне — подхватится сам).
 *
 * Профиль — process-wide и read-only после загрузки: он описывает репозиторий, в котором
 * идёт прогон, а не отдельный вызов. Конфиг прогона (env/CLI) по-прежнему передаётся явно.
 */

import { readFileSync } from 'node:fs';
import { dirname, isAbsolute, resolve } from 'node:path';
import { pkgPath } from './package-root.mjs';

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
    astgrepConfig: '',              // '' → ruleset из пакета (ast-grep/sgconfig.yml)
  }),
});

/** Маркеры комментариев из namespace: sticky, inline и ключ машинного футера. */
export function createMarkers(namespace = DEFAULT_PROFILE.markerNamespace) {
  const ns = String(namespace || DEFAULT_PROFILE.markerNamespace).trim() || DEFAULT_PROFILE.markerNamespace;
  const dataKey = `${ns}-data:`;
  return Object.freeze({
    ns,
    review: `<!-- ${ns} -->`,
    inline: `<!-- ${ns}-inline -->`,
    dataKey,
    get dataRe() {
      // Свежий объект на каждое чтение: у RegExp есть состояние (lastIndex), и общий
      // экземпляр между вызовами exec на разных телах комментариев вёл бы себя непредсказуемо.
      return new RegExp(`<!--\\s*${dataKey}([\\s\\S]*?)-->`);
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

function normalizeLayout(raw = {}, baseDir) {
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

function normalizeAstgrep(raw = {}) {
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
  return DEFAULT_PROFILE.repoRules;
}

/** Сырой JSON → нормализованный профиль (скомпилированные регексы, маркеры, пути). */
export function normalizeProfile(raw = {}, { baseDir = process.cwd(), source = '' } = {}) {
  const markerNamespace = String(raw.markerNamespace || DEFAULT_PROFILE.markerNamespace).trim()
    || DEFAULT_PROFILE.markerNamespace;
  const toolName = String(raw.toolName || markerNamespace).trim() || DEFAULT_PROFILE.toolName;
  return Object.freeze({
    name: String(raw.name || DEFAULT_PROFILE.name),
    source,
    baseDir,
    markerNamespace,
    toolName,
    sarifRulePrefix: String(raw.sarifRulePrefix || toolName).trim() || DEFAULT_PROFILE.sarifRulePrefix,
    repositoryUri: String(raw.repositoryUri || DEFAULT_PROFILE.repositoryUri),
    repoRules: readRepoRules(raw, baseDir),
    markers: createMarkers(markerNamespace),
    layout: normalizeLayout(raw.layout, baseDir),
    astgrep: normalizeAstgrep(raw.astgrep),
    strict: raw.strict === true,
    paths: Object.freeze({ ...DEFAULT_PROFILE.paths, ...(raw.paths || {}) }),
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
