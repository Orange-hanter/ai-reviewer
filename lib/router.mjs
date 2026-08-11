/** R4: классификация diff и роутинг (skip / light / full). */

import { parseDiffGitPath } from './diff.mjs';

const LOCKFILE = /(?:^|\/)?(?:package-lock\.json|yarn\.lock|pnpm-lock\.yaml|composer\.lock|Cargo\.lock|poetry\.lock|Gemfile\.lock)$/i;
const GENERATED = /(?:^|\/)(?:dist|graphify-out|node_modules|\.next|coverage|build)\/|\.(?:min\.js|map|d\.ts\.map)$|[-.]generated\.|\/__generated__\//i;
const DOCS = /\.(?:md|mdx|rst|txt)$|(?:^|\/)docs\//i;
const LOGIC = /^(?:app\/)?(?:server|shared|client)\/.+\.(?:ts|tsx|js|jsx|mjs|cjs)$/i;
const CODE = /\.(?:ts|tsx|js|jsx|mjs|cjs|vue|svelte)$/i;

/** Класс одного изменённого файла. */
export function classifyFilePath(path) {
  if (LOCKFILE.test(path) || path.endsWith('.lock')) return 'skip';
  if (GENERATED.test(path)) return 'skip';
  if (DOCS.test(path) || /^README/i.test(path.split('/').pop() || '')) return 'docs';
  if (LOGIC.test(path)) return 'logic';
  if (CODE.test(path)) return 'logic';
  if (/\.(?:yml|yaml|json|toml|sh|sql|env\.example)$/i.test(path)) return 'config';
  return 'other';
}

/** Вырезает unified-diff секции по пути `+++ b/path` (учитывает quoted-пути с пробелами). */
export function extractFileDiffSections(diff) {
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

/** Rename-only или нулевой контентный diff (только метаданные). */
export function isRenameOnlySection(section) {
  if (!section) return false;
  if (/^rename from /m.test(section)) return true;
  if (/^similarity index 100(?:\.0*)?%/m.test(section)) return true;
  for (const line of section.split('\n')) {
    if (line.startsWith('+') && !line.startsWith('+++')) return false;
    if (line.startsWith('-') && !line.startsWith('---')) return false;
  }
  return true;
}

/** Класс файла с учётом rename-only секции diff. */
export function classifyChangedFile(path, section) {
  if (section && isRenameOnlySection(section)) return 'skip';
  return classifyFilePath(path);
}

/**
 * Маршрут PR: skip | light | full.
 * - skip: lockfile / generated / rename-only — без LLM
 * - light: docs-only (+ skip-класс) — без LLM, merge не блокируется по коду
 * - full: server/shared/client logic или смешанный код — полный pipeline
 */
export function routeDiff(changedPaths, diff, { enabled = true } = {}) {
  if (!enabled) {
    return { route: 'full', reason: 'router выключен (REVIEW_ROUTER=0)', files: [] };
  }
  if (!changedPaths.length) {
    return { route: 'full', reason: 'нет файлов в diff', files: [] };
  }

  const sections = extractFileDiffSections(diff || '');
  const files = changedPaths.map((path) => ({
    path,
    class: classifyChangedFile(path, sections.get(path)),
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

export function formatRouteLog(route) {
  const breakdown = route.files.map((f) => `${f.path}→${f.class}`).join(', ');
  return `${route.route} (${route.reason})${breakdown ? `: ${breakdown}` : ''}`;
}
