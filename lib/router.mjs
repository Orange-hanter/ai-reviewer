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
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseDiffGitPath } from './diff.mjs';
import { defaultRustBin } from './rust-bin.mjs';

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

/**
 * Пути PR, которые не уходят в LLM-чанки: встроенный класс skip по пути (lockfile /
 * generated — classifyFilePath) и пути, совпавшие с `layout.reviewSkip` профиля.
 * Rename-only сюда не входит: решается по секции диффа, а хунков у такой секции нет —
 * выкидывать из чанка нечего. Порядок — как в changedPaths.
 *
 * Чистый JS без Rust-op: регекс профиля скомпилирован диалектом JS (lib/profile.mjs),
 * класс пути берётся через переданный fileClass (в pipeline — мемоизированный).
 */
export function collectReviewSkipped(changedPaths, { reviewSkip = null, fileClass = classifyFilePath } = {}) {
  const byProfile = [];
  const builtIn = [];
  for (const path of changedPaths || []) {
    if (reviewSkip && reviewSkip.test(path)) byProfile.push(path);
    else if (fileClass(path) === 'skip') builtIn.push(path);
  }
  return { byProfile, builtIn, all: new Set([...byProfile, ...builtIn]) };
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
