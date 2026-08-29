/**
 * Детерминированные находки по blast-radius: caller вне диффа и, вероятно, другой слой/сервис.
 * Дополняет LLM-prescan до merge кластеров.
 */

import { getProfile } from './profile.mjs';

/** Парсит пути зависимостей из текстового блока Graphify blast-radius. */
export function parseBlastDependentPaths(blocks) {
  const paths = new Set();
  for (const block of blocks || []) {
    const text = String(block || '');
    for (const m of text.matchAll(/([^\s,`]+?\.(?:ts|tsx|js|jsx|mjs|cjs|vue|svelte|go|py|rs)):\d+/gi)) {
      paths.add(m[1].replace(/^[`'"]+|[`'"]+$/g, ''));
    }
  }
  return paths;
}

/** Первый сегмент пути до `/` — грубый proxy слоя/сервиса. */
function pathLayerPrefix(filePath) {
  const p = String(filePath || '').replace(/^\.\/+/, '');
  const slash = p.indexOf('/');
  return slash >= 0 ? p.slice(0, slash) : p;
}

function isDifferentLayer(changedFile, dependentPath, layout) {
  const dep = String(dependentPath || '');
  if (!dep) return false;
  const changedPrefix = pathLayerPrefix(changedFile);
  const depPrefix = pathLayerPrefix(dep);
  if (changedPrefix && depPrefix && changedPrefix !== depPrefix) return true;
  const layerDirs = layout?.layerDirs || [];
  if (layerDirs.length) {
    const inLayer = (p) => layerDirs.some((d) => p.startsWith(`${d}/`) || p === d);
    if (inLayer(dep) && changedFile && !inLayer(changedFile)) return true;
    if (inLayer(changedFile) && !inLayer(dep)) return true;
  }
  return false;
}

/**
 * Эмитит major-находки для callers вне changed-files из другого слоя.
 * @param {string[]} blastBlocks — текстовые блоки Graphify
 * @param {Set<string>|string[]} changedPaths — пути из диффа
 * @param {object} [opts] — symbols, model label
 */
export function emitBlastRadiusFindings(blastBlocks, changedPaths, opts = {}) {
  const changed = new Set(changedPaths || []);
  if (!blastBlocks?.length || !changed.size) return [];

  const layout = getProfile().layout;
  const dependents = parseBlastDependentPaths(blastBlocks);
  const outside = [...dependents].filter((p) => !changed.has(p));
  if (!outside.length) return [];

  const findings = [];
  const symbols = opts.symbols || [];
  const changedList = [...changed];

  for (const dep of outside) {
    const crossLayer = changedList.some((cf) => isDifferentLayer(cf, dep, layout));
    if (!crossLayer) continue;
    const symbol = symbols[findings.length] || symbols[0] || 'changed-symbol';
    findings.push({
      file: dep,
      line: 1,
      severity: 'major',
      confidence: 0.85,
      title: 'Breaking change: caller вне диффа не обновлён (blast-radius)',
      body: `Graphify blast-radius: зависимый файл \`${dep}\` не входит в изменённые файлы PR, `
        + `но относится к другому слою/сервису относительно изменений. `
        + `Проверьте совместимость контракта символа \`${symbol}\` с этим caller.`,
      model: opts.model || 'blast-radius',
      prescan: false,
      source: 'blast-radius',
      blastCaller: true,
    });
    if (findings.length >= 8) break;
  }
  return findings;
}
