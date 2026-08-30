import { execFile } from 'node:child_process';
import { access, constants, readFile as readFileDefault } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { promisify } from 'node:util';
import { BLAST_PATH_EXTS } from './constants.mjs';
import { mapLimit } from './util.mjs';

const execFileP = promisify(execFile);

/** @deprecated импортируйте из `lib/constants.mjs` */
export { BLAST_PATH_EXTS };

/** Строка stdout `graphify affected`: `- label [relation] file:L42`. */
export const AFFECTED_LINE_RE = /^- (.+?) \[([\w_]+)\] (.+?):L?(\d+)\s*$/;

const CONF_RANK = { EXTRACTED: 3, INFERRED: 2, AMBIGUOUS: 1 };

function addReason(reasons, reason) {
  reasons[reason] = (reasons[reason] || 0) + 1;
}

function queryErrorReason(error) {
  if (error?.code === 'ENOENT') return 'graphify_unavailable';
  if (error?.code === 'ETIMEDOUT' || error?.killed) return 'graphify_timeout';
  return 'graphify_error';
}

function emptyMetrics(symbols, extras = {}) {
  const skipReasons = { ...(extras.skip_reasons || {}) };
  const reasons = Object.keys(skipReasons);
  return {
    blocks: [],
    metrics: {
      symbols_total: symbols.length,
      symbols_resolved: 0,
      affected_nonempty: 0,
      graph_fresh: extras.graph_fresh ?? null,
      graph_status: extras.graph_status ?? 'unknown',
      skip_reason: reasons.length === 0 ? 'none' : reasons.length === 1 ? reasons[0] : 'mixed',
      skip_reasons: skipReasons,
      affected_files: [],
      confidence: { EXTRACTED: 0, INFERRED: 0, AMBIGUOUS: 0, unknown: 0 },
      cross_community: 0,
    },
  };
}

/** Снимает все хвостовые `()` (graphify иногда отдаёт `foo()()`). */
export function bareLabel(label) {
  let s = String(label || '').trim();
  while (s.endsWith('()')) s = s.slice(0, -2);
  return s;
}

function confRank(c) {
  return CONF_RANK[String(c || '').toUpperCase()] || 0;
}

function pushLabel(map, key, node) {
  if (!key) return;
  if (!map.has(key)) map.set(key, []);
  map.get(key).push(node);
}

function escapeMdFragment(s) {
  return String(s || '')
    .replace(/`/g, "'")
    .replace(/\[/g, '(')
    .replace(/\]/g, ')')
    .replace(/\*/g, '∗')
    .replace(/_/g, 'ˍ');
}

/**
 * Индекс graph.json.
 * @param {object} raw — node-link JSON (nodes + links|edges)
 * @param {{ gods?: Array<string|{label?: string}> }|null} [analysis] —
 *   опционально `.graphify_analysis.json` (поле `gods` для маркера god-node)
 */
export function buildGraphIndex(raw, analysis = null) {
  const nodes = Array.isArray(raw?.nodes) ? raw.nodes : [];
  const links = Array.isArray(raw?.links) ? raw.links
    : Array.isArray(raw?.edges) ? raw.edges : [];

  const byId = new Map();
  /** exact label (casefold, with `()`) → nodes */
  const byExactLabel = new Map();
  /** bare label (без `()`) → nodes — как graphify resolve_seed */
  const byBareLabel = new Map();
  for (const n of nodes) {
    if (!n || n.id == null) continue;
    const id = String(n.id);
    byId.set(id, n);
    const label = String(n.label || '');
    if (label) {
      pushLabel(byExactLabel, label.toLowerCase(), n);
      pushLabel(byBareLabel, bareLabel(label).toLowerCase(), n);
    }
  }

  /** targetId → incoming edges */
  const inbound = new Map();
  for (const e of links) {
    if (!e) continue;
    const t = String(e.target ?? '');
    if (!t) continue;
    if (!inbound.has(t)) inbound.set(t, []);
    inbound.get(t).push(e);
  }

  const gods = new Set();
  for (const g of analysis?.gods || []) {
    const label = typeof g === 'string' ? g : g?.label;
    if (label) gods.add(String(label).toLowerCase());
  }

  return { byId, byExactLabel, byBareLabel, inbound, gods };
}

/**
 * Уникальный узел по query — зеркало graphify `resolve_seed` (без path/contains).
 * Омонимы → null: лучше без enrichment, чем чужой seed.
 */
export function resolveUniqueNode(index, query) {
  if (!index || !query) return null;
  const q = String(query);
  if (index.byId.has(q)) return index.byId.get(q);

  const exact = index.byExactLabel.get(q.toLowerCase()) || [];
  if (exact.length === 1) return exact[0];
  if (exact.length > 1) return null;

  const bare = index.byBareLabel.get(bareLabel(q).toLowerCase()) || [];
  if (bare.length === 1) return bare[0];
  return null;
}

/** Caller: unique label, иначе label+file, иначе единственный узел файла. */
function resolveCallerNode(index, dep) {
  if (!index) return null;
  const file = String(dep.file || '');
  const exact = dep.label ? (index.byExactLabel.get(String(dep.label).toLowerCase()) || []) : [];
  if (exact.length === 1) return exact[0];
  if (exact.length > 1 && file) {
    const inFile = exact.filter((n) => String(n.source_file || '') === file);
    if (inFile.length === 1) return inFile[0];
  }

  const bare = dep.label ? (index.byBareLabel.get(bareLabel(dep.label).toLowerCase()) || []) : [];
  if (bare.length === 1) return bare[0];
  if (bare.length > 1 && file) {
    const inFile = bare.filter((n) => String(n.source_file || '') === file);
    if (inFile.length === 1) return inFile[0];
  }

  if (!file) return null;
  const byFile = [...index.byId.values()].filter((n) => String(n.source_file || '') === file);
  return byFile.length === 1 ? byFile[0] : null;
}

/**
 * Обогащает dep из affected confidence/community из graph.json.
 * Сопоставляет входящее ребро к seed по relation + (source_file | label).
 * Без уникального seed — не ставим community/confidence/god (омонимы).
 */
export function enrichAffectedDep(dep, seedQuery, index) {
  const out = { ...dep };
  if (!index) return out;

  const seed = resolveUniqueNode(index, seedQuery);
  if (!seed) return out;

  out.seedCommunity = seed.community ?? null;
  out.seedCommunityName = seed.community_name || seed.community_label || null;

  const caller = resolveCallerNode(index, dep);
  if (caller) {
    out.community = caller.community ?? null;
    out.communityName = caller.community_name || caller.community_label || null;
    if (out.seedCommunity != null && out.community != null && out.seedCommunity !== out.community) {
      out.crossCommunity = true;
    }
  }

  const edges = index.inbound.get(String(seed.id)) || [];
  const fileNorm = String(dep.file || '');
  const labelNorm = bareLabel(dep.label).toLowerCase();
  let best = null;
  for (const e of edges) {
    if (dep.relation && String(e.relation || '') !== dep.relation) continue;
    const src = index.byId.get(String(e.source ?? ''));
    const srcFile = String(e.source_file || src?.source_file || '');
    const srcLabel = bareLabel(src?.label).toLowerCase();
    const fileMatch = srcFile === fileNorm;
    const labelMatch = srcLabel && srcLabel === labelNorm;
    if (!fileMatch && !labelMatch) continue;
    if (!best || confRank(e.confidence) > confRank(best.confidence)) best = e;
  }
  if (best) {
    const conf = String(best.confidence || 'EXTRACTED').toUpperCase();
    out.confidence = conf;
    if (best.confidence_score != null) out.confidenceScore = Number(best.confidence_score);
  }

  // god-nodes: только exact label uniquely resolved seed (не bare — иначе create()
  // помечался бы от омонима create в gods).
  if (index.gods.has(String(seed.label || '').toLowerCase())) {
    out.seedIsGodNode = true;
  }
  return out;
}

/** Одна зависимость в тексте blast-блока (совместимо с parseBlastDependents). */
export function formatDepEntry(d) {
  const caller = d.label ? `\`${escapeMdFragment(d.label)}\` @ ` : '';
  const conf = d.confidence ? `[${escapeMdFragment(d.confidence)}]` : '';
  const cross = d.crossCommunity ? ' cross-community' : '';
  const file = escapeMdFragment(d.file);
  const relation = escapeMdFragment(d.relation || '');
  return `${caller}${file}:${d.line} [${relation}]${conf}${cross}`;
}

/**
 * @param {string} symbol
 * @param {object[]} deps
 * @param {{ godNodeThreshold: number, maxDepsPerSymbol: number }} limits
 * @param {{ seedIsGodNode?: boolean }} [opts]
 */
export function formatBlastBlock(symbol, deps, limits, opts = {}) {
  const seedGod = opts.seedIsGodNode === true;
  const god = (deps.length >= limits.godNodeThreshold || seedGod)
    ? ' ⚠️ god-node (высокий риск регрессий)' : '';
  const shown = deps.slice(0, limits.maxDepsPerSymbol).map(formatDepEntry).join(', ');
  const more = deps.length > limits.maxDepsPerSymbol
    ? ` … (+${deps.length - limits.maxDepsPerSymbol})` : '';
  return `- \`${escapeMdFragment(symbol)}\` → ${deps.length} зависимостей вне диффа${god}: ${shown}${more}`;
}

/** Dedup по file: предпочитаем EXTRACTED, затем более конкретный label. */
export function dedupeDepsByFile(deps, { dropAmbiguous = true } = {}) {
  const best = new Map();
  for (const d of deps) {
    const conf = String(d.confidence || '').toUpperCase();
    if (dropAmbiguous && conf === 'AMBIGUOUS') continue;
    const prev = best.get(d.file);
    if (!prev) { best.set(d.file, d); continue; }
    const betterConf = confRank(d.confidence) > confRank(prev.confidence);
    const sameConf = confRank(d.confidence) === confRank(prev.confidence);
    const betterLabel = sameConf && d.label && (!prev.label || d.label.length > prev.label.length);
    if (betterConf || betterLabel) best.set(d.file, d);
  }
  return [...best.values()];
}

/** Каталог для `git -C`: абсолютный путь без управляющих символов. */
function safeGitCwd(graphPath) {
  const raw = String(graphPath || '.').trim();
  if (!raw || /[\0\r\n]/.test(raw)) return process.cwd();
  try {
    return resolve(dirname(raw));
  } catch {
    return process.cwd();
  }
}

async function readGraphFreshness(g, { execFileP: exec, readFile }) {
  try {
    // git -C от каталога графа: CWD процесса может быть не корнем репо.
    const gitCwd = safeGitCwd(g.graph || g.report);
    const [report, head] = await Promise.all([
      readFile(g.report, 'utf8'),
      exec('git', ['-C', gitCwd, 'rev-parse', 'HEAD'], { timeout: 5_000, maxBuffer: 1024 }),
    ]);
    const built = String(report).match(/Built from commit:\s*`?([0-9a-f]{7,40})`?/i)?.[1];
    if (!built) return { fresh: null, status: 'unknown', reason: 'graph_build_commit_unknown' };
    const fresh = String(head.stdout).trim().startsWith(built);
    return fresh
      ? { fresh: true, status: 'fresh' }
      : { fresh: false, status: 'stale', reason: 'graph_stale' };
  } catch {
    return { fresh: null, status: 'unknown', reason: 'graph_freshness_unknown' };
  }
}

async function loadGraphEnrichment(g, { readFile }) {
  try {
    const raw = JSON.parse(await readFile(g.graph, 'utf8'));
    let analysis = null;
    try {
      const analysisPath = join(dirname(g.graph), '.graphify_analysis.json');
      analysis = JSON.parse(await readFile(analysisPath, 'utf8'));
    } catch { /* analysis optional */ }
    return buildGraphIndex(raw, analysis);
  } catch {
    return null;
  }
}

export function createGraphifyClient(cfg, deps = {}) {
  const { graphify: g, limits } = cfg;
  const exec = deps.execFileP ?? execFileP;
  const readFile = deps.readFile ?? readFileDefault;
  const accessFile = deps.access ?? access;
  const depth = Math.max(1, Number(g.depth) || 2);
  const dropAmbiguous = g.dropAmbiguous !== false;

  async function affected(symbol, index) {
    try {
      // Порядок фиксирован graphify CLI: `affected <query> [--graph] [--depth]`
      // (`query = sys.argv[2]`). execFile без shell — символ с `-` не флаг.
      const args = ['affected', String(symbol ?? ''), '--graph', g.graph, '--depth', String(depth)];
      const { stdout } = await exec(g.bin, args, {
        timeout: 20_000, maxBuffer: 4 * 1024 * 1024,
      });
      if (/No unique node match|No node matching/.test(stdout)) {
        return { symbol, status: 'unresolved', reason: 'symbol_unresolved', deps: [] };
      }
      if (/No affected nodes found/.test(stdout)) {
        return { symbol, status: 'resolved', reason: 'no_dependents', deps: [] };
      }
      const deps = [];
      for (const line of stdout.split('\n')) {
        const m = line.match(AFFECTED_LINE_RE);
        if (!m) continue;
        const raw = { label: m[1], relation: m[2], file: m[3], line: Number(m[4]) };
        deps.push(enrichAffectedDep(raw, symbol, index));
      }
      const uniqueDeps = dedupeDepsByFile(deps, { dropAmbiguous });
      if (!uniqueDeps.length) {
        const reason = deps.length ? 'ambiguous_only' : 'graphify_unparsed_output';
        return { symbol, status: deps.length ? 'resolved' : 'unknown', reason, deps: [] };
      }
      const seedIsGodNode = uniqueDeps.some((d) => d.seedIsGodNode);
      for (const d of uniqueDeps) delete d.seedIsGodNode;
      return { symbol, status: 'resolved', deps: uniqueDeps, seedIsGodNode };
    } catch (error) {
      return { symbol, status: 'error', reason: queryErrorReason(error), deps: [] };
    }
  }

  async function buildBlastRadius(symbols) {
    try {
      await accessFile(g.graph, constants.R_OK);
    } catch {
      return emptyMetrics(symbols, {
        graph_status: 'missing',
        skip_reasons: { graph_missing: 1 },
      });
    }

    const [freshness, index] = await Promise.all([
      readGraphFreshness(g, { execFileP: exec, readFile }),
      loadGraphEnrichment(g, { readFile }),
    ]);
    const affectedResults = await mapLimit(symbols, limits.concurrency, (s) => affected(s, index));

    const skipReasons = {};
    if (freshness.reason) addReason(skipReasons, freshness.reason);
    if (symbols.length === 0) addReason(skipReasons, 'no_symbols_in_diff');
    if (!index) addReason(skipReasons, 'graph_enrichment_unavailable');

    let symbolsResolved = 0;
    let affectedNonempty = 0;
    let crossCommunity = 0;
    const confidence = { EXTRACTED: 0, INFERRED: 0, AMBIGUOUS: 0, unknown: 0 };
    const affectedFiles = new Set();

    const blocks = affectedResults.map(({ symbol, status, reason, deps, seedIsGodNode }) => {
      if (status === 'resolved') symbolsResolved++;
      if (reason) addReason(skipReasons, reason);
      if (!deps.length) return null;
      affectedNonempty++;
      for (const d of deps) {
        affectedFiles.add(d.file);
        const c = String(d.confidence || 'unknown').toUpperCase();
        if (c in confidence) confidence[c]++;
        else confidence.unknown++;
        if (d.crossCommunity) crossCommunity++;
      }
      return formatBlastBlock(symbol, deps, limits, { seedIsGodNode });
    });
    const reasons = Object.keys(skipReasons);
    return {
      blocks: blocks.filter(Boolean),
      metrics: {
        symbols_total: symbols.length,
        symbols_resolved: symbolsResolved,
        affected_nonempty: affectedNonempty,
        graph_fresh: freshness.fresh,
        graph_status: freshness.status,
        skip_reason: reasons.length === 0 ? 'none' : reasons.length === 1 ? reasons[0] : 'mixed',
        skip_reasons: skipReasons,
        affected_files: [...affectedFiles].sort(),
        confidence,
        cross_community: crossCommunity,
      },
    };
  }

  return { buildBlastRadius };
}
