/**
 * Фаза 1, шаг 4: lib/router.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/router (порт классификации diff-путей и маршрутизации skip/light/full).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Все потребители
 * (pipeline.mjs, lib/eval.mjs, index.mjs, тесты) вызывают функции router СИНХРОННО,
 * поэтому вместо async execFile используется execFileSync: бинарь вызывается на каждую
 * операцию с JSON {"op", ...args} на stdin, ответ — JSON с результатом.
 *
 * Резолвинг бинаря (по аналогии с lib/diff.mjs, lib/json.mjs, lib/findings.mjs):
 *   AI_REVIEWER_ROUTER_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/router;
 *   AI_REVIEWER_ROUTER_FORCE_JS=1 — принудительно чистый JS (паритетные прогоны/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm test/eval
 * идут раньше cargo build, см. .github/workflows/ci.yml), обёртка деградирует на чистый
 * JS fallback ниже — эталонную копию исходной реализации. Если бинарь ЕСТЬ, но упал
 * (non-zero exit) — ошибка пробрасывается, fallback не используется: молчаливая
 * подмена реализации при битом Rust-коде скрывала бы регрессии.
 *
 * Нюансы паритета Rust-порта (проверены parity-прогоном rust/parity/router.parity.mjs):
 *   - путь секции диффа берётся через parse_diff_git_path из diff-модуля — тот же
 *     парсер quoted-путей, дублирования нет;
 *   - regex-паттерны классификации те же, `(?i)` в Rust ↔ флаг `i` в JS (обе стороны —
 *     Unicode simple case folding; на ASCII-путях идентичны);
 *   - `path.endsWith('.lock')` в JS регистрозависим — порт сохранил это (COMPOSE.LOCK
 *     не skip, а именованные lockfile матчатся регистронезависимым регексом);
 *   - `enabled` канонизируется в bool до вызова (в JS falsy-значения 0/'' отключают
 *     роутер через `if (!enabled)`).
 */

import { existsSync, readFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname, relative, isAbsolute } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseDiffGitPath } from './diff.mjs';
import { defaultRustBin } from './rust-bin.mjs';
import { PROFILE_FILENAMES } from './profile.mjs';
import { getInputSource } from './trusted-input.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_ROUTER_BIN = defaultRustBin('router');

function resolveRouterBin() {
  if (process.env.AI_REVIEWER_ROUTER_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_ROUTER_BIN || '').trim() || DEFAULT_ROUTER_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveRouterBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

/** Класс одного изменённого файла: skip | docs | logic | config | other. */
export function classifyFilePath(path) {
  const r = runRust('classifyFilePath', { path });
  if (r !== null) return r;
  return classifyFilePathJs(path);
}

/**
 * Путь с расширением исходника (LOGIC или CODE) — по расширению, без учёта каталога и имени.
 * `x.generated.ts` и `dist/a.js` для маршрута — skip, но из входа LLM код по одному имени
 * не выпадает: имя выбирает автор PR.
 */
export function isCodePath(path) {
  const r = runRust('isCodePath', { path });
  if (r !== null) return r;
  return isCodePathJs(path);
}

/** Вырезает unified-diff секции по пути `+++ b/path` (учитывает quoted-пути с пробелами). */
export function extractFileDiffSections(diff) {
  const r = runRust('extractFileDiffSections', { diff: diff || '' });
  if (r !== null) return new Map(Object.entries(r));
  return extractFileDiffSectionsJs(diff);
}

/** Rename-only или нулевой контентный diff (только метаданные). */
export function isRenameOnlySection(section) {
  const r = runRust('isRenameOnlySection', { section: section ?? null });
  if (r !== null) return r;
  return isRenameOnlySectionJs(section);
}

/** Класс файла с учётом rename-only секции diff. */
export function classifyChangedFile(path, section) {
  const r = runRust('classifyChangedFile', { path, section: section ?? null });
  if (r !== null) return r;
  return classifyChangedFileJs(path, section);
}

/**
 * Маршрут PR: skip | light | full.
 * - skip: lockfile / generated / rename-only — без LLM
 * - light: docs-only (+ skip-класс) — без LLM, merge не блокируется по коду
 * - full: server/shared/client logic или смешанный код — полный pipeline
 */
export function routeDiff(changedPaths, diff, { enabled = true, skipPaths = [] } = {}) {
  // `!enabled` в JS трактует 0/''/null как «выключен» — канонизируем до bool до вызова.
  const skip = Array.isArray(skipPaths) ? skipPaths.map(String) : [];
  const r = runRust('routeDiff', {
    changedPaths: changedPaths || [],
    diff: diff || '',
    enabled: Boolean(enabled),
    ...(skip.length ? { skipPaths: skip } : {}),
  });
  if (r !== null) return r;
  return routeDiffJs(changedPaths, diff, { enabled, skipPaths: skip });
}

/** Файл пина движка у потребителя (`ops/ai-reviewer-ref`, examples/ci) — в любом каталоге. */
const ENGINE_PIN = /(?:^|\/)ai-reviewer-ref$/;
const GITHUB_DIR = /(?:^|\/)\.github\//;

// Allowlist инертных файлов: только их skip может убрать из входа LLM. Всё остальное
// (shell, SQL, Dockerfile, Makefile, .tf, .toml, .txt, .env*, ini, код…) — поведение или его
// конфигурация, и паттерн его не прячет. Действует и для доверенного профиля.
const INERT_DOCS = /\.(?:md|mdx|rst)$/i;
// Только настоящие source map: `app.js.map`, `x.css.map`. Голый `*.map` — это и nginx-карты,
// и данные/конфиги (`routes.map`), которые skip прятать не вправе.
const INERT_MAP = /\.(?:js|mjs|cjs|jsx|ts|tsx|css)\.map$/i;
const INERT_DATA = /\.(?:json|ya?ml)$/i;
// Общеизвестные lockfile — точно по basename (LOCKFILE роутера матчит и `xpackage-lock.json`).
const LOCKFILE_BASENAME = /^(?:package-lock\.json|yarn\.lock|pnpm-lock\.yaml|composer\.lock|Cargo\.lock|poetry\.lock|Gemfile\.lock)$/i;
// Встроенный skip — только lockfile, чьи добавленные `resolved`/`tarball` проверяет
// детерминированная проверка реестров (lib/lockfile-registry.mjs). Остальные lockfile без
// проверки остаются во входе LLM: убрать их значило бы не смотреть на смену источника вовсе.
const CHECKED_LOCKFILE_BASENAME = /^(?:package-lock\.json|yarn\.lock|pnpm-lock\.yaml)$/i;
// Данные, которые на деле — конфиг сборки/CI/инструментов: по имени файла…
const TOOLING_BASENAME = new RegExp(`^(?:${[
  'package\\.json', 'tsconfig.*\\.json', 'jsconfig.*\\.json', '.*\\.config\\.json',
  'docker-compose.*\\.ya?ml', 'compose.*\\.ya?ml',
  'openapitools\\.json', 'renovate\\.json', 'vercel\\.json', 'netlify\\.(?:json|ya?ml)',
  'azure-pipelines\\.ya?ml', 'bitbucket-pipelines\\.ya?ml',
  'action\\.ya?ml', 'chart\\.ya?ml', 'values.*\\.ya?ml',
].join('|')})$`, 'i');
// …по каталогу: CI, git-хуки, IDE/devcontainer, манифесты кластера и инфраструктуры…
const NEVER_INERT_DIR = /(?:^|\/)(?:\.github|\.gitlab|\.circleci|\.husky|\.devcontainer|\.vscode|\.idea|k8s|helm|charts|terraform|infra|deploy)\//i;
// …и инструкции агентам (их читают модели и IDE-агенты как правила).
const AGENT_INSTRUCTIONS = /^(?:agents|claude|gemini|copilot-instructions)\.md$/i;

/**
 * Инертный файл — тот, что skip вправе убрать из входа LLM: lockfile (basename), source map,
 * документация (`md`/`mdx`/`rst`), данные `json`/`yaml`/`yml`. Никогда не инертны: dotfile
 * (`.eslintrc`, `.gitlab-ci.yml`, `.cursorrules`…), всё под NEVER_INERT_DIR, инструкции
 * агентам и конфиги сборки/CI/инструментов (TOOLING_BASENAME).
 */
export function isInertPath(path) {
  const p = String(path || '');
  const base = p.split('/').pop() || '';
  if (NEVER_INERT_DIR.test(p) || base.startsWith('.') || AGENT_INSTRUCTIONS.test(base)) return false;
  if (LOCKFILE_BASENAME.test(base)) return true;
  if (INERT_MAP.test(p)) return true;
  if (INERT_DOCS.test(p)) return true;
  if (INERT_DATA.test(p)) return !TOOLING_BASENAME.test(base);
  return false;
}

/**
 * Встроенный skip (без профиля): npm/yarn/pnpm lockfile (по basename; их реестры проверяет
 * lib/lockfile-registry.mjs) и source map JS/TS/CSS. Прочий класс skip роутера (dist/,
 * build/, `*.generated.*`) решает лишь маршрут PR, вход LLM он не сужает.
 */
export function isBuiltInSkipPath(path) {
  const p = String(path || '');
  return CHECKED_LOCKFILE_BASENAME.test(p.split('/').pop() || '') || INERT_MAP.test(p);
}

/**
 * Каталоги из sgconfig.yml ast-grep: `ruleDirs`, `utilDirs` и `testConfigs[].testDir`/
 * `snapshotDir`. YAML-парсера в зависимостях движка нет, поэтому разбор — подмножество YAML,
 * и оно FAIL-CLOSED: null (вызывающий защищает каталог конфига и любой YAML PR) на любой
 * непустой строке вне комментария, которая не одно из:
 *   - верхнеуровневый ключ без кавычек в колонке 0 (`key:` / `key: scalar`);
 *   - элемент списка `- x` или инлайн `[a, b]` под ruleDirs/utilDirs;
 *   - элемент/поле записи под testConfigs;
 *   - вложенная строка под ключом, который каталогов не задаёт (DIRLESS_KEYS схемы ast-grep).
 * Значит null на `"ruleDirs":`, `? ruleDirs`, `{ruleDirs: …}`, `---`, якорях/алиасах,
 * merge-ключе `<<`, вложенных строках под незнакомым ключом и кавычках/флоу внутри значения:
 * там, где этот разбор и настоящий YAML-парсер могли бы разойтись.
 */
export function parseSgconfigDirs(text) {
  if (typeof text !== 'string') return null;
  const DIR_KEYS = new Set(['ruleDirs', 'utilDirs', 'testConfigs']);
  // Ключи sgconfig, под которыми каталогов правил нет: их вложенные строки пропускаем.
  const DIRLESS_KEYS = new Set(['languageGlobs', 'customLanguages', 'languageInjections']);
  // Значение-каталог: plain-скаляр или целиком закавыченная строка без кавычек/флоу внутри.
  const scalar = (raw) => {
    const v = raw.trim();
    const q = v.match(/^(['"])(.*)\1$/);
    const s = (q ? q[2] : v).trim();
    if (!s || /['"]/.test(s) || (!q && /^[[\]{}&*!|>%@`,?:-]/.test(s)) || /:\s|\s#/.test(s)) return null;
    return s;
  };
  const dirs = [];
  let cur = null;
  for (const rawLine of text.replace(/^﻿/, '').split('\n')) {
    const noCr = rawLine.replace(/\r$/, '');
    if (!noCr.trim() || /^\s*#/.test(noCr)) continue;
    // Хвостовой комментарий — только вне кавычек: `"a #b"` не обрезаем, scalar() его отвергнет.
    const line = /['"]/.test(noCr) ? noCr.replace(/\s+#[^'"]*$/, '') : noCr.replace(/\s+#.*$/, '');
    const top = line.match(/^([A-Za-z_][\w-]*)\s*:(?:\s+(.*))?$/);
    if (top) {
      const key = top[1];
      const rest = (top[2] || '').trim();
      cur = DIR_KEYS.has(key) ? key : (DIRLESS_KEYS.has(key) ? 'dirless' : 'other');
      if (DIR_KEYS.has(key) && rest) {
        const inline = rest.match(/^\[([^[\]{}]*)\]$/);
        if (!inline || key === 'testConfigs') return null;
        for (const v of inline[1].split(',')) {
          if (!v.trim()) continue;
          const s = scalar(v);
          if (!s) return null;
          dirs.push(s);
        }
        cur = 'done';
      } else if (rest && /^[&*!|>]/.test(rest)) {
        return null; // якорь/алиас/тег/блочный скаляр у ключа верхнего уровня
      } else if (rest) {
        cur = 'done'; // `key: scalar` — вложенных строк у него быть не может
      }
      continue;
    }
    if (!/^\s/.test(line)) return null; // в колонке 0 — не `key:` без кавычек
    if (cur === 'dirless') continue;
    if (cur !== 'ruleDirs' && cur !== 'utilDirs' && cur !== 'testConfigs') return null;
    const item = line.match(/^\s+-(?:\s+(.*))?$/);
    if (cur === 'testConfigs') {
      const body = item ? (item[1] || '') : line;
      const kv = body.match(/^\s*([A-Za-z_][\w-]*)\s*:(?:\s+(.*))?$/);
      if (item && !body.trim()) continue; // `-` с полями на следующих строках
      if (!kv) {
        // значение-список под полем записи (`includes:` → `- x`): каталогов не задаёт
        if (item && !/^[[{&*!|>]/.test(body.trim())) continue;
        return null;
      }
      if (kv[1] === 'testDir' || kv[1] === 'snapshotDir') {
        const s = kv[2] === undefined ? null : scalar(kv[2]);
        if (!s) return null;
        dirs.push(s);
      } else if (kv[2] && /^[&*!|>]/.test(kv[2].trim())) {
        return null;
      }
      continue;
    }
    if (!item || item[1] === undefined) return null;
    const s = scalar(item[1]);
    if (!s) return null;
    dirs.push(s);
  }
  return dirs;
}

/**
 * Всё, что читает движок, — путями от корня репозитория (cwd). Это не список «известных»
 * файлов, а инверсия: каждое строковое значение `profile.paths` и каждое путевое поле cfg.
 * `paths` — точные пути, `prefixes` — каталоги (`dir/`), `anyYaml` — защищать любой
 * `.yml`/`.yaml` (sgconfig не разобран — fail-closed).
 *   - файл профиля (REVIEW_PROFILE / автопоиск) и имена автопоиска в корне;
 *   - repoRulesFile (от каталога профиля, как readRepoRules), AGENTS.md/agents.md рядом
 *     с профилем и в cwd;
 *   - все значения profile.paths — и как файл, и как каталог;
 *   - cfg: incremental.stateFile, graphify.graph/report, structure.graph/graphReport/
 *     graphAnalysis/lcov/obligations/mutationReport/propertyReport, feedback.logFile/mdPath/
 *     patternsFile, candidatesDir/export.dir/observability.dir/structure.exportDir (каталоги);
 *   - astgrep config и каталоги из его ruleDirs/utilDirs/testConfigs.
 * Пути вне cwd (пакет движка, /tmp) пропускаются: в дифф PR они не попадают.
 */
export function collectProfileProtected(cfg, {
  cwd = process.cwd(),
  readText = (f) => readFileSync(f, 'utf8'),
  // Доверенный прогон: ast-grep получает sgconfig из base (lib/astgrep.mjs), PR правит копию
  // в рабочем дереве — защищаем каталоги ОБЕИХ версий; не разобралась любая — fail-closed.
  baseReadText = getInputSource().trusted ? getInputSource().readText : null,
} = {}) {
  const profile = cfg?.profile || {};
  const paths = new Set(PROFILE_FILENAMES);
  const prefixes = [];
  let anyYaml = false;
  const rel = (file, base = cwd) => {
    if (typeof file !== 'string' || !file.trim()) return null;
    const abs = isAbsolute(file) ? file : resolve(base, file);
    const r = relative(cwd, abs).split('\\').join('/');
    return r && !r.startsWith('..') && !isAbsolute(r) ? r : null;
  };
  const add = (file, base) => { const r = rel(file, base); if (r) paths.add(r); return r; };
  const addDir = (dir, base) => {
    const r = rel(dir, base);
    if (r && !prefixes.includes(`${r}/`)) prefixes.push(`${r}/`);
    return r;
  };
  add(profile.source);
  const baseDir = profile.baseDir || cwd;
  add(profile.repoRulesFile, baseDir);
  for (const dir of new Set([baseDir, cwd])) {
    for (const name of ['AGENTS.md', 'agents.md']) add(name, dir);
  }
  for (const v of Object.values(profile.paths || {})) {
    if (typeof v !== 'string') continue;
    add(v);
    addDir(v);
  }
  const s = cfg?.structure || {};
  for (const f of [cfg?.incremental?.stateFile, cfg?.graphify?.graph, cfg?.graphify?.report,
    s.graph, s.graphReport, s.graphAnalysis, s.lcov, s.obligations, s.mutationReport, s.propertyReport,
    cfg?.feedback?.patternsFile, cfg?.feedback?.mdPath, cfg?.feedback?.logFile]) add(f);
  for (const d of [cfg?.feedback?.candidatesDir, cfg?.export?.dir, cfg?.observability?.dir, s.exportDir]) addDir(d);
  // Разбираем тот sgconfig, который реально получит ast-grep: итоговый из cfg (env
  // REVIEW_ASTGREP_CONFIG перебивает профиль); значение профиля защищено выше как путь.
  const sgconfig = cfg?.prescan?.astgrep ? cfg.prescan.astgrep.configPath : profile.paths?.astgrepConfig;
  for (const f of [sgconfig]) {
    const r = add(f);
    if (!r) continue;
    const parseWith = (read) => { try { return parseSgconfigDirs(read(resolve(cwd, r))); } catch { return null; } };
    let dirs = parseWith(readText);
    if (dirs && baseReadText) {
      const baseDirs = parseWith(baseReadText);
      dirs = baseDirs ? [...dirs, ...baseDirs] : null;
    }
    const confDir = dirname(r);
    if (!dirs) {
      // fail-closed: каталог конфига и любой YAML в PR
      if (confDir !== '.') addDir(confDir);
      anyYaml = true;
      continue;
    }
    for (const d of dirs) addDir(d, resolve(cwd, confDir));
  }
  return { paths, prefixes, anyYaml };
}

/**
 * Почему путь нельзя исключать из входа LLM, или null. Профиль и имена файлов в
 * потребителе приходят из checkout'а PR — их контролирует автор PR, поэтому ни регекс
 * профиля, ни имя файла не убирают из ревью то, где живёт поведение и доверие:
 *   code       — исходник по расширению (isCodePath);
 *   .github    — workflows, actions, CODEOWNERS;
 *   engine-pin — пин движка (`ai-reviewer-ref`);
 *   profile    — файлы, которые читает профиль (collectProfileProtected);
 *   sensitive  — `layout.sensitive` профиля;
 *   not-inert  — не из allowlist инертных файлов (isInertPath).
 */
export function reviewSkipProtection(path, { isCode = isCodePath, protectedPaths = null, sensitive = null } = {}) {
  if (isCode(path)) return 'code';
  if (GITHUB_DIR.test(path)) return '.github';
  if (ENGINE_PIN.test(path)) return 'engine-pin';
  if (isProfileProtected(path, protectedPaths)) return 'profile';
  if (sensitive?.test(path)) return 'sensitive';
  if (!isInertPath(path)) return 'not-inert';
  return null;
}

/** protectedPaths: Set путей или {paths, prefixes, anyYaml} из collectProfileProtected. */
function isProfileProtected(path, prot) {
  if (!prot) return false;
  if (prot instanceof Set) return prot.has(path);
  return Boolean(prot.paths?.has(path) || prot.prefixes?.some((d) => path.startsWith(d))
    || (prot.anyYaml && /\.ya?ml$/i.test(path)));
}

/**
 * Пути PR, которые не уходят в LLM-чанки:
 *   builtIn   — общеизвестные lockfile и `*.map` (isBuiltInSkipPath), без профиля;
 *   byProfile — совпавшие с `layout.reviewSkip`, ТОЛЬКО при `trusted` (REVIEW_PROFILE_TRUSTED=1:
 *               профиль взят из base-ветки). Недоверенный профиль пишет автор PR — его
 *               reviewSkip не применяется вовсе (`reviewSkipIgnored: true`).
 * Оба списка проходят reviewSkipProtection: защищённый путь остаётся во входе LLM, а
 * совпадение уходит в `ignored` ({path, reason, by}) — pipeline пишет его в лог.
 * Порядок — как в changedPaths. Чистый JS: регекс профиля скомпилирован диалектом JS.
 */
export function collectReviewSkipped(changedPaths, {
  reviewSkip = null, trusted = false, isCode = isCodePath, protectedPaths = null, sensitive = null,
} = {}) {
  const byProfile = [];
  const builtIn = [];
  const ignored = [];
  const profileSkip = trusted ? reviewSkip : null;
  for (const path of changedPaths || []) {
    const by = profileSkip && profileSkip.test(path) ? 'reviewSkip' : (isBuiltInSkipPath(path) ? 'builtIn' : null);
    if (!by) continue;
    const reason = reviewSkipProtection(path, { isCode, protectedPaths, sensitive });
    if (reason) ignored.push({ path, reason, by });
    else (by === 'reviewSkip' ? byProfile : builtIn).push(path);
  }
  return {
    byProfile, builtIn, ignored,
    reviewSkipIgnored: Boolean(reviewSkip && !trusted),
    all: new Set([...byProfile, ...builtIn]),
  };
}

export function formatRouteLog(route) {
  const r = runRust('formatRouteLog', { route });
  if (r !== null) return r;
  return formatRouteLogJs(route);
}

/**
 * Строка заметки sticky о путях, не отданных моделям. Пусто — нечего сказать. Пути не
 * перечисляем все: сгенерированных в PR бывает десятки, хватает первых трёх и счёта.
 */
export function formatReviewSkippedNote(reviewSkipped) {
  const paths = [...(reviewSkipped?.all || [])];
  if (!paths.length) return '';
  const shown = paths.slice(0, 3).map((p) => `\`${p}\``).join(', ');
  const rest = paths.length > 3 ? ` и ещё ${paths.length - 3}` : '';
  return `Не ревьюились моделями (generated/lockfile/reviewSkip профиля): ${paths.length} файл(ов) — ${shown}${rest}.`;
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/router.mjs до Фазы 1).
// Используется только когда Rust-бинарь отсутствует; при битом бинаре НЕ подменяет его.
// ---------------------------------------------------------------------------

const LOCKFILE = /(?:^|\/)?(?:package-lock\.json|yarn\.lock|pnpm-lock\.yaml|composer\.lock|Cargo\.lock|poetry\.lock|Gemfile\.lock)$/i;
const GENERATED = /(?:^|\/)(?:dist|graphify-out|node_modules|\.next|coverage|build)\/|\.(?:min\.js|map|d\.ts\.map)$|[-.]generated\.|\/__generated__\//i;
const DOCS = /\.(?:md|mdx|rst|txt)$|(?:^|\/)docs\//i;
const LOGIC = /^(?:app\/)?(?:server|shared|client)\/.+\.(?:ts|tsx|js|jsx|mjs|cjs)$/i;
// Исходники любого языка — logic. Раньше список был только JS/TS-семейством, и PR,
// трогавший исключительно `.go`/`.py`/`.rb`, попадал в класс `other` → маршрут `light`:
// ревью не запускалось вообще, а sticky сообщал «docs/config-only». Для потребителя с
// не-JS репозиторием гейт молча не смотрел ничего.
const CODE = new RegExp(`\\.(?:${[
  'ts', 'tsx', 'js', 'jsx', 'mjs', 'cjs', 'cts', 'mts', 'vue', 'svelte', 'gjs', 'gts',
  'go', 'py', 'pyi', 'rb', 'rake', 'php', 'java', 'kt', 'kts', 'scala', 'groovy',
  'rs', 'c', 'h', 'cc', 'cpp', 'cxx', 'hpp', 'hxx', 'm', 'mm', 'cs', 'swift',
  'ex', 'exs', 'erl', 'hrl', 'clj', 'cljs', 'dart', 'lua', 'pl', 'pm', 'r',
].join('|')})$`, 'i');

function isCodePathJs(path) {
  return LOGIC.test(path) || CODE.test(path);
}

function classifyFilePathJs(path) {
  if (LOCKFILE.test(path) || path.endsWith('.lock')) return 'skip';
  if (GENERATED.test(path)) return 'skip';
  if (DOCS.test(path) || /^README/i.test(path.split('/').pop() || '')) return 'docs';
  if (LOGIC.test(path)) return 'logic';
  if (CODE.test(path)) return 'logic';
  if (/\.(?:yml|yaml|json|toml|sh|sql|env\.example)$/i.test(path)) return 'config';
  return 'other';
}

function extractFileDiffSectionsJs(diff) {
  const sections = new Map();
  let cur = null;
  let buf = [];
  for (const line of diff.split('\n')) {
    if (line.startsWith('diff --git')) {
      if (cur) sections.set(cur, buf.join('\n'));
      cur = parseDiffGitPath(line);
      buf = [line];
    } else if (cur) {
      buf.push(line);
    }
  }
  if (cur) sections.set(cur, buf.join('\n'));
  return sections;
}

function isRenameOnlySectionJs(section) {
  if (!section) return false;
  // `rename from` сам по себе rename-only НЕ делает: «переименовал и поменял логику»
  // приходит с хунками и обязан ревьюиться (RFC 17).
  // Строки — только по `\n` (без флага `/m`): `/m` считает границей ещё `\r` и U+2028/2029,
  // и `+код\u2028similarity index 100%` в строке автора PR выдавал правку за чистый rename —
  // файл уходил в skip, а PR из одного такого файла не ревьюился вовсе.
  const lines = section.split('\n');
  if (lines.some((line) => /^similarity index 100(?:\.0*)?%/.test(line))) return true;
  for (const line of lines) {
    if (line.startsWith('+') && !line.startsWith('+++')) return false;
    if (line.startsWith('-') && !line.startsWith('---')) return false;
  }
  return true;
}

function classifyChangedFileJs(path, section) {
  if (section && isRenameOnlySectionJs(section)) return 'skip';
  return classifyFilePathJs(path);
}

function routeDiffJs(changedPaths, diff, { enabled = true, skipPaths = [] } = {}) {
  if (!enabled) {
    return { route: 'full', reason: 'router выключен (REVIEW_ROUTER=0)', files: [] };
  }
  if (!changedPaths.length) {
    return { route: 'full', reason: 'нет файлов в diff', files: [] };
  }

  const sections = extractFileDiffSectionsJs(diff || '');
  const skip = new Set(skipPaths);
  const files = changedPaths.map((path) => ({
    path,
    class: skip.has(path) ? 'skip' : classifyChangedFileJs(path, sections.get(path)),
  }));

  const hasLogic = files.some((f) => f.class === 'logic');
  const classes = files.map((f) => f.class);

  if (classes.every((c) => c === 'skip')) {
    return { route: 'skip', reason: 'lockfile / generated / rename-only', files };
  }
  if (!hasLogic && classes.every((c) => c === 'docs' || c === 'skip')) {
    return { route: 'light', reason: 'docs-only', files };
  }
  if (hasLogic) {
    return { route: 'full', reason: 'server/shared/client или иной код', files };
  }
  if (classes.every((c) => ['skip', 'docs', 'config', 'other'].includes(c))) {
    return { route: 'light', reason: 'нет logic-путей (config/docs/skip)', files };
  }
  return { route: 'full', reason: 'смешанный diff', files };
}

function formatRouteLogJs(route) {
  const breakdown = route.files.map((f) => `${f.path}→${f.class}`).join(', ');
  return `${route.route} (${route.reason})${breakdown ? `: ${breakdown}` : ''}`;
}
