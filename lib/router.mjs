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

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname, relative, isAbsolute } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseDiffGitPath } from './diff.mjs';
import { defaultRustBin } from './rust-bin.mjs';
import { PROFILE_FILENAMES } from './profile.mjs';

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
// (shell, SQL, Dockerfile, Makefile, .tf, .toml, .env*, ini, код…) — поведение или его
// конфигурация, и регекс из профиля PR его не прячет.
const INERT_DOCS = /\.(?:md|mdx|rst|txt)$/i;
const INERT_MAP = /\.map$/i;
const INERT_DATA = /\.(?:json|ya?ml)$/i;
// Данные, которые на деле — конфиг сборки/CI/инструментов: по имени файла…
const TOOLING_BASENAME = new RegExp(`^(?:${[
  'package\\.json', 'tsconfig.*\\.json', 'jsconfig.*\\.json', '.*\\.config\\.json',
  '\\.eslintrc.*', '\\.prettierrc.*', 'docker-compose.*\\.ya?ml', 'compose.*\\.ya?ml',
  'openapitools\\.json', 'renovate\\.json', 'vercel\\.json', 'netlify\\.(?:json|ya?ml)',
  '\\.gitlab-ci\\.ya?ml', 'azure-pipelines\\.ya?ml', 'bitbucket-pipelines\\.ya?ml',
  'action\\.ya?ml', 'chart\\.ya?ml', 'values.*\\.ya?ml',
].join('|')})$`, 'i');
// …и по каталогу: CI, git-хуки, манифесты кластера.
const TOOLING_DIR = /(?:^|\/)(?:\.github|\.circleci|\.husky|k8s|helm|charts)\//i;

/**
 * Инертный файл — тот, что skip вправе убрать из входа LLM: lockfile, source map,
 * документация (`md`/`mdx`/`rst`/`txt`), данные `json`/`yaml`/`yml`, кроме конфигов
 * сборки/CI/инструментов (TOOLING_BASENAME/TOOLING_DIR).
 */
export function isInertPath(path) {
  const p = String(path || '');
  const base = p.split('/').pop() || '';
  if (LOCKFILE.test(p) || p.endsWith('.lock')) return true;
  if (INERT_MAP.test(p)) return true;
  if (INERT_DOCS.test(p)) return true;
  if (INERT_DATA.test(p)) return !TOOLING_BASENAME.test(base) && !TOOLING_DIR.test(p);
  return false;
}

/**
 * Файлы, которые читает активный профиль, — путями от корня репозитория (cwd). Их правку
 * reviewSkip из того же профиля спрятать не может. `paths` — точные пути, `prefixes` —
 * каталоги (`dir/`), где лежат правила ast-grep: ruleDirs из sgconfig.yml читает сам
 * ast-grep, не движок, поэтому защищается весь каталог конфига.
 *   - файл профиля (REVIEW_PROFILE / автопоиск) и имена автопоиска в корне;
 *   - repoRulesFile (от каталога профиля, как readRepoRules), AGENTS.md/agents.md рядом
 *     с профилем и в cwd;
 *   - fpPatterns, fpMarkdown, feedbackLog, astgrepConfig — и из профиля, и итоговые из cfg
 *     (env перебивает профиль).
 * Пути вне cwd (пакет движка, /tmp) пропускаются: в дифф PR они не попадают.
 */
export function collectProfileProtected(cfg, { cwd = process.cwd() } = {}) {
  const profile = cfg?.profile || {};
  const paths = new Set(PROFILE_FILENAMES);
  const prefixes = [];
  const rel = (file, base = cwd) => {
    if (typeof file !== 'string' || !file.trim()) return null;
    const abs = isAbsolute(file) ? file : resolve(base, file);
    const r = relative(cwd, abs).split('\\').join('/');
    return r && !r.startsWith('..') && !isAbsolute(r) ? r : null;
  };
  const add = (file, base) => { const r = rel(file, base); if (r) paths.add(r); return r; };
  add(profile.source);
  const baseDir = profile.baseDir || cwd;
  add(profile.repoRulesFile, baseDir);
  for (const dir of new Set([baseDir, cwd])) {
    for (const name of ['AGENTS.md', 'agents.md']) add(name, dir);
  }
  const pp = profile.paths || {};
  for (const f of [pp.fpPatterns, pp.fpMarkdown, pp.feedbackLog,
    cfg?.feedback?.patternsFile, cfg?.feedback?.mdPath, cfg?.feedback?.logFile]) add(f);
  for (const f of [pp.astgrepConfig, cfg?.prescan?.astgrep?.configPath]) {
    const r = add(f);
    const dir = r ? dirname(r) : '.';
    if (dir !== '.' && !prefixes.includes(`${dir}/`)) prefixes.push(`${dir}/`);
  }
  return { paths, prefixes };
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

/** protectedPaths: Set путей или {paths: Set, prefixes: string[]} из collectProfileProtected. */
function isProfileProtected(path, prot) {
  if (!prot) return false;
  if (prot instanceof Set) return prot.has(path);
  return Boolean(prot.paths?.has(path) || prot.prefixes?.some((d) => path.startsWith(d)));
}

/**
 * Пути PR, которые не уходят в LLM-чанки:
 *   builtIn   — lockfile и generated по пути (classifyFilePath → skip), только инертные;
 *   byProfile — совпавшие с `layout.reviewSkip` профиля.
 * Оба списка проходят reviewSkipProtection: защищённый путь остаётся во входе LLM, а
 * совпадение уходит в `ignored` ({path, reason, by}) — pipeline пишет его в лог.
 * Rename-only сюда не входит: решается по секции диффа, а хунков у такой секции нет —
 * выкидывать из чанка нечего. Порядок — как в changedPaths.
 *
 * Чистый JS без Rust-op: регекс профиля скомпилирован диалектом JS (lib/profile.mjs);
 * класс пути и расширение кода — через переданные fileClass/isCode (в pipeline —
 * мемоизированные; паритет самих классификаторов — rust/parity/router.parity.mjs).
 */
export function collectReviewSkipped(changedPaths, {
  reviewSkip = null, fileClass = classifyFilePath, isCode = isCodePath, protectedPaths = null, sensitive = null,
} = {}) {
  const byProfile = [];
  const builtIn = [];
  const ignored = [];
  for (const path of changedPaths || []) {
    const by = reviewSkip && reviewSkip.test(path) ? 'reviewSkip' : (fileClass(path) === 'skip' ? 'builtIn' : null);
    if (!by) continue;
    const reason = reviewSkipProtection(path, { isCode, protectedPaths, sensitive });
    if (reason) ignored.push({ path, reason, by });
    else (by === 'reviewSkip' ? byProfile : builtIn).push(path);
  }
  return { byProfile, builtIn, ignored, all: new Set([...byProfile, ...builtIn]) };
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
