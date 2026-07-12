import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mapLimit } from './util.mjs';

const execFileP = promisify(execFile);

export function createGraphifyClient(cfg) {
  const { graphify: g, limits } = cfg;

  async function affected(symbol) {
    try {
      const { stdout } = await execFileP(g.bin, ['affected', symbol, '--graph', g.graph], {
        timeout: 20_000, maxBuffer: 4 * 1024 * 1024,
      });
      if (/No unique node match|No affected nodes found/.test(stdout)) return [];
      const deps = [];
      for (const line of stdout.split('\n')) {
        const m = line.match(/^- (.+?) \[([\w_]+)\] (.+?):L(\d+)\s*$/);
        if (m) deps.push({ label: m[1], relation: m[2], file: m[3], line: Number(m[4]) });
      }
      const seen = new Set();
      return deps.filter((d) => (seen.has(d.file) ? false : seen.add(d.file)));
    } catch {
      return [];
    }
  }

  async function buildBlastRadius(symbols) {
    const blocks = await mapLimit(symbols, limits.concurrency, async (sym) => {
      const deps = await affected(sym);
      if (!deps.length) return null;
      const god = deps.length >= limits.godNodeThreshold ? ' ⚠️ god-node (высокий риск регрессий)' : '';
      const shown = deps.slice(0, limits.maxDepsPerSymbol)
        .map((d) => `${d.file}:${d.line} [${d.relation}]`).join(', ');
      const more = deps.length > limits.maxDepsPerSymbol ? ` … (+${deps.length - limits.maxDepsPerSymbol})` : '';
      return `- \`${sym}\` → ${deps.length} зависимостей вне диффа${god}: ${shown}${more}`;
    });
    return blocks.filter(Boolean);
  }

  return { buildBlastRadius };
}
