/**
 * Детерминированные находки по blast-radius: caller вне диффа и другой слой/community.
 * Дополняет LLM-prescan до merge кластеров.
 */

import {
  BLAST_PATH_EXTS,
  BLAST_CONF_UNKNOWN, BLAST_CONF_INFERRED, BLAST_CONF_EXTRACTED, BLAST_CONF_AMBIGUOUS,
  BLAST_FINDINGS_MAX, BLAST_FINDING_SOURCE,
} from './constants.mjs';
import { getProfile } from './profile.mjs';

export {
  BLAST_CONF_UNKNOWN, BLAST_CONF_INFERRED, BLAST_CONF_EXTRACTED, BLAST_CONF_AMBIGUOUS,
  BLAST_FINDINGS_MAX, BLAST_FINDING_SOURCE,
};

function escapeRegex(s) {
  return String(s).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

const BLAST_EXT_ALT = BLAST_PATH_EXTS.map(escapeRegex).join('|');

/**
 * Запись в blast-блоке:
 *   `caller` @ file:42 [calls][EXTRACTED] cross-community
 *   file:L42 [calls]
 */
const DEP_ENTRY_RE = new RegExp(
  `(?:\`([^\`]+)\`\\s*@\\s*)?([^\\s,\`]+?\\.(?:${BLAST_EXT_ALT})):(?:L)?(\\d+)(?:\\s*\\[([\\wˍ]+)\\])?(?:\\[(EXTRACTED|INFERRED|AMBIGUOUS)\\])?(?:\\s*(cross-community))?`,
  'gi',
);

/**
 * Обратное к обратимой части `escapeMdFragment` (lib/graphify.mjs): блок пишется
 * для Markdown, и `_` в нём — `ˍ` (U+02CD), `*` — `∗` (U+2217). Без обратной замены
 * путь `cod_doc/x.py` из блока не совпадал с путём диффа, и каждый caller в файле с
 * подчёркиванием считался «вне диффа» — на Python-репозитории это все файлы.
 * Скобки и backtick экранируются необратимо (`[`→`(`) и тут не трогаются.
 */
function unescapeMdFragment(s) {
  return String(s || '').replace(/ˍ/g, '_').replace(/∗/g, '*');
}

/** Снимает layout.appDir с пути: `app/server/x.ts` → `server/x.ts`. */
function stripAppDir(filePath, appDir) {
  const p = String(filePath || '').replace(/^\.\/+/, '');
  const dir = String(appDir || '').replace(/^\.\/+|\/+$/g, '');
  if (!dir) return p;
  const prefix = `${dir}/`;
  return p.startsWith(prefix) ? p.slice(prefix.length) : p;
}

/**
 * Парсит зависимости из текстового блока Graphify blast-radius.
 * @returns {{ file: string, line: number, symbol: string, caller?: string, relation?: string, confidence?: string, crossCommunity?: boolean }[]}
 */
export function parseBlastDependents(blocks) {
  const items = [];
  for (const block of blocks || []) {
    const text = String(block || '');
    const symM = text.match(/^- `([^`]+)`/m);
    const symbol = unescapeMdFragment(symM?.[1] || '');
    // matchAll + global: сбрасываем lastIndex на случай повторных вызовов.
    DEP_ENTRY_RE.lastIndex = 0;
    for (const m of text.matchAll(DEP_ENTRY_RE)) {
      items.push({
        caller: unescapeMdFragment(m[1] || ''),
        file: unescapeMdFragment(m[2].replace(/^[`'"]+|[`'"]+$/g, '')),
        line: Number(m[3]) || 1,
        relation: unescapeMdFragment(m[4] || ''),
        confidence: m[5] ? String(m[5]).toUpperCase() : '',
        crossCommunity: Boolean(m[6]),
        symbol,
      });
    }
  }
  return items;
}

/** Парсит пути зависимостей из текстового блока Graphify blast-radius. */
export function parseBlastDependentPaths(blocks) {
  return new Set(parseBlastDependents(blocks).map((d) => d.file));
}

/**
 * Слой/сервис пути: сначала совпадение с layout.layerDirs (после strip appDir),
 * иначе первый сегмент пути.
 */
function pathLayerPrefix(filePath, layout) {
  const p = stripAppDir(filePath, layout?.appDir);
  const layers = [...(layout?.layerDirs || [])].sort((a, b) => b.length - a.length);
  for (const d of layers) {
    if (p === d || p.startsWith(`${d}/`)) return d;
  }
  const slash = p.indexOf('/');
  return slash >= 0 ? p.slice(0, slash) : p;
}

function isDifferentLayer(changedFile, dependentPath, layout) {
  const depRaw = String(dependentPath || '');
  if (!depRaw) return false;
  const changedLayer = pathLayerPrefix(changedFile, layout);
  const depLayer = pathLayerPrefix(depRaw, layout);
  if (changedLayer && depLayer && changedLayer !== depLayer) return true;

  const layerDirs = layout?.layerDirs || [];
  if (layerDirs.length) {
    const inLayer = (raw) => {
      const p = stripAppDir(raw, layout?.appDir);
      return layerDirs.some((d) => p === d || p.startsWith(`${d}/`));
    };
    if (inLayer(depRaw) && changedFile && !inLayer(changedFile)) return true;
    if (inLayer(changedFile) && !inLayer(depRaw)) return true;
  }
  return false;
}

/** Confidence неизвестного ребра ≤ INFERRED (не «обгоняет» явную пониженную метку). */
function findingConfidence(dep) {
  const c = String(dep.confidence || '').toUpperCase();
  if (c === 'EXTRACTED') return BLAST_CONF_EXTRACTED;
  if (c === 'INFERRED') return BLAST_CONF_INFERRED;
  if (c === 'AMBIGUOUS') return BLAST_CONF_AMBIGUOUS;
  return BLAST_CONF_UNKNOWN;
}

function escapeMdTicks(s) {
  // Убираем backticks (не заменяем на '), чтобы не искажать идентификаторы в отчёте.
  return String(s || '').replace(/`+/g, '');
}

/** Глагол ребра Graphify для текста находки; неизвестное ребро — нейтральное «использует». */
const RELATION_VERB = {
  calls: 'вызывает',
  imports: 'импортирует',
  imports_from: 'импортирует',
  uses: 'использует',
  references: 'ссылается на',
  inherits: 'наследует',
  extends: 'расширяет',
  implements: 'реализует',
};

/** Надёжность ребра словами: метка `[EXTRACTED]` читателю PR ничего не говорит. */
const EDGE_CONFIDENCE_TEXT = {
  EXTRACTED: 'связь прямая, из AST',
  INFERRED: 'связь выведена эвристикой',
  AMBIGUOUS: 'связь неоднозначна',
};

/**
 * Caller-узел Graphify уровня файла называется именем файла (`AccessPanel.tsx`,
 * `StockTab/index.tsx`) — рядом с путём находки он ничего не добавляет.
 */
function isFileLabel(caller, file) {
  return Boolean(caller) && (file === caller || file.endsWith(`/${caller}`));
}

/**
 * Эмитит major-находки для callers вне changed-files из другого слоя/community.
 * Блокировка merge — только после verifier (см. clusterFindings); сами по себе soft.
 * @param {string[]} blastBlocks — текстовые блоки Graphify
 * @param {Set<string>|string[]} changedPaths — пути из диффа
 * @param {object} [opts] — symbols, model, dropAmbiguous (default true), layout (иначе getProfile().layout)
 */
export function emitBlastRadiusFindings(blastBlocks, changedPaths, opts = {}) {
  const changed = new Set(changedPaths || []);
  if (!blastBlocks?.length || !changed.size) return [];

  const dropAmbiguous = opts.dropAmbiguous !== false;
  const layout = opts.layout || getProfile().layout;
  const dependents = parseBlastDependents(blastBlocks);
  const outside = dependents.filter((d) => !changed.has(d.file));
  if (!outside.length) return [];

  const findings = [];
  const seen = new Set();
  const changedList = [...changed];
  const fallbackSymbol = opts.symbols?.[0] || 'changed-symbol';

  for (const dep of outside) {
    // Ключ — файл + символ (RFC 21): файл, зависящий от двух изменённых символов, давал одну
    // находку — по первому символу, и verifier судил не тот контракт (gold orakul-d128046c:
    // server.js остался с queuePriceAlerts, сломанный вызов getTgToken в нём потерялся).
    const key = `${dep.file}\0${dep.symbol || fallbackSymbol}`;
    if (seen.has(key)) continue;
    if (dropAmbiguous && String(dep.confidence || '').toUpperCase() === 'AMBIGUOUS') continue;

    const crossLayer = changedList.some((cf) => isDifferentLayer(cf, dep.file, layout));
    const crossCommunity = Boolean(dep.crossCommunity);
    if (!crossLayer && !crossCommunity) continue;

    seen.add(key);
    const file = escapeMdTicks(dep.file);
    const symbol = escapeMdTicks(dep.symbol || fallbackSymbol);
    const callerRaw = escapeMdTicks(dep.caller || '');
    const where = callerRaw && !isFileLabel(callerRaw, file) ? ` в \`${callerRaw}\`` : '';
    const verb = RELATION_VERB[String(dep.relation || '').toLowerCase()] || 'использует';
    const why = [
      crossLayer ? 'другой слой приложения' : null,
      crossCommunity ? 'другой кластер графа зависимостей' : null,
      EDGE_CONFIDENCE_TEXT[String(dep.confidence || '').toUpperCase()] || null,
    ].filter(Boolean).join('; ');

    findings.push({
      file: dep.file,
      line: dep.line || 1,
      severity: 'major',
      confidence: findingConfidence(dep),
      title: `Код вне PR зависит от \`${symbol}\` — проверьте совместимость`,
      body: `Этот файл не изменён в PR, но ${verb} \`${symbol}\`${where}, а \`${symbol}\` затронут `
        + `изменениями (Graphify blast-radius: ${why}). Если сигнатура, возвращаемое значение `
        + `или поведение \`${symbol}\` поменялись — обновите этот вызов; если нет — замечание можно закрыть.`,
      model: opts.model || BLAST_FINDING_SOURCE,
      prescan: false,
      source: BLAST_FINDING_SOURCE,
      blastCaller: true,
      // RFC 21: имя изменённого символа — по нему verifier получает хунк диффа, где меняется
      // контракт (файл находки вне PR, по нему секцию диффа не найти).
      blastSymbol: dep.symbol || fallbackSymbol,
      blastCrossCommunity: crossCommunity,
      blastEdgeConfidence: dep.confidence || '',
    });
  }
  findings.sort((a, b) => (b.confidence - a.confidence)
    || (a.file || '').localeCompare(b.file || '')
    || ((a.line || 0) - (b.line || 0))
    || String(a.blastEdgeConfidence || '').localeCompare(String(b.blastEdgeConfidence || '')));
  return findings.slice(0, BLAST_FINDINGS_MAX);
}
