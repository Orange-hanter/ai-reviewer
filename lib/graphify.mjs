import { execFile } from 'node:child_process';
import { access, constants, readFile as readFileDefault } from 'node:fs/promises';
import { promisify } from 'node:util';
import { mapLimit } from './util.mjs';

const execFileP = promisify(execFile);

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
    },
  };
}

async function readGraphFreshness(g, { execFileP: exec, readFile }) {
  try {
    const [report, head] = await Promise.all([
      readFile(g.report, 'utf8'),
      exec('git', ['rev-parse', 'HEAD'], { timeout: 5_000, maxBuffer: 1024 }),
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

export function createGraphifyClient(cfg, deps = {}) {
  const { graphify: g, limits } = cfg;
  const exec = deps.execFileP ?? execFileP;
  const readFile = deps.readFile ?? readFileDefault;
  const accessFile = deps.access ?? access;

  async function affected(symbol) {
    try {
      const { stdout } = await exec(g.bin, ['affected', symbol, '--graph', g.graph], {
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
        const m = line.match(/^- (.+?) \[([\w_]+)\] (.+?):L(\d+)\s*$/);
        if (m) deps.push({ label: m[1], relation: m[2], file: m[3], line: Number(m[4]) });
      }
      const seen = new Set();
      const uniqueDeps = deps.filter((d) => (seen.has(d.file) ? false : seen.add(d.file)));
      if (!uniqueDeps.length) return { symbol, status: 'unknown', reason: 'graphify_unparsed_output', deps: [] };
      return { symbol, status: 'resolved', deps: uniqueDeps };
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

    const [freshness, results] = await Promise.all([
      readGraphFreshness(g, { execFileP: exec, readFile }),
      mapLimit(symbols, limits.concurrency, affected),
    ]);
    const skipReasons = {};
    if (freshness.reason) addReason(skipReasons, freshness.reason);
    if (symbols.length === 0) addReason(skipReasons, 'no_symbols_in_diff');
    let symbolsResolved = 0;
    let affectedNonempty = 0;
    const blocks = results.map(({ symbol, status, reason, deps }) => {
      if (status === 'resolved') symbolsResolved++;
      if (reason) addReason(skipReasons, reason);
      if (!deps.length) return null;
      affectedNonempty++;
      const god = deps.length >= limits.godNodeThreshold ? ' ⚠️ god-node (высокий риск регрессий)' : '';
      const shown = deps.slice(0, limits.maxDepsPerSymbol)
        .map((d) => `${d.file}:${d.line} [${d.relation}]`).join(', ');
      const more = deps.length > limits.maxDepsPerSymbol ? ` … (+${deps.length - limits.maxDepsPerSymbol})` : '';
      return `- \`${symbol}\` → ${deps.length} зависимостей вне диффа${god}: ${shown}${more}`;
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
      },
    };
  }

  return { buildBlastRadius };
}
