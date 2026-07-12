/** R8 → R1: promote feedback candidate JSON to eval fixture directory. */

import { mkdir, readFile, writeFile, access, rm } from 'node:fs/promises';
import { constants } from 'node:fs';
import { dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { runFixtureCase } from '../lib/eval.mjs';
import { loadReviewConfig } from '../lib/config.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
export const DEFAULT_FIXTURES_DIR = resolve(__dir, '../fixtures');

/** Safe fixture directory name — no path traversal. */
export function sanitizeFixtureId(id) {
  const s = String(id || '').trim();
  if (!s || s.includes('..') || s.includes('/') || s.includes('\\')) return null;
  if (!/^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$/.test(s)) return null;
  return s;
}

/** Resolved fixture path must stay under fixturesDir. */
export function resolveFixtureDir(fixturesDir, id) {
  const safe = sanitizeFixtureId(id);
  if (!safe) return null;
  const root = resolve(fixturesDir);
  const target = resolve(root, safe);
  if (target !== root && !target.startsWith(`${root}${sep}`)) return null;
  return target;
}

export function matchPrFilePatch(files, file) {
  const variants = new Set([file, file.replace(/^app\//, ''), `app/${file.replace(/^app\//, '')}`]);
  return files.find((f) => variants.has(f.filename));
}

async function exists(path) {
  try {
    await access(path, constants.F_OK);
    return true;
  } catch {
    return false;
  }
}

/** Fetch unified diff for one file from PR via gh CLI or GitHub API. */
export async function fetchPrFileDiff({ pr, repo, file, token }) {
  const { execFile } = await import('node:child_process');
  const { promisify } = await import('node:util');
  const run = promisify(execFile);

  if (pr && file) {
    try {
      const { stdout } = await run('gh', ['pr', 'diff', String(pr), '--', file], {
        encoding: 'utf8',
        maxBuffer: 10 * 1024 * 1024,
      });
      if (stdout?.trim()) return stdout;
    } catch {
      /* API fallback */
    }
  }

  const [owner, name] = String(repo || '').split('/');
  if (!owner || !name || !pr || !file || !token) return '';

  const res = await fetch(`https://api.github.com/repos/${owner}/${name}/pulls/${pr}/files?per_page=100`, {
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: 'application/vnd.github+json',
      'X-GitHub-Api-Version': '2022-11-28',
    },
  });
  if (!res.ok) return '';
  const files = await res.json();
  const hit = matchPrFilePatch(files, file);
  return hit?.patch ? `${hit.patch}\n` : '';
}

/** @param {object} candidate from buildFixtureCandidate */
export async function promoteCandidateToFixture(candidate, {
  fixturesDir = DEFAULT_FIXTURES_DIR,
  pr,
  repo,
  token,
  fetchDiff = fetchPrFileDiff,
  log = () => {},
} = {}) {
  const rawId = candidate?.suggestedId || candidate?.meta?.id;
  const id = sanitizeFixtureId(rawId);
  const file = candidate?.findings?.[0]?.file;
  if (!id || !file) {
    return { ok: false, id: rawId || '?', error: 'invalid id or missing file' };
  }

  const target = resolveFixtureDir(fixturesDir, id);
  if (!target) {
    return { ok: false, id, error: 'invalid fixture path' };
  }
  if (await exists(join(target, 'meta.json'))) {
    return { ok: false, id, skipped: true, error: 'fixture exists' };
  }

  const prNum = pr || candidate.fromFeedback?.pr;
  let diff = '';
  if (prNum && repo) {
    diff = await fetchDiff({ pr: prNum, repo, file, token });
  }
  if (!diff.trim()) {
    diff = `diff --git a/${file} b/${file}\n--- a/${file}\n+++ b/${file}\n@@ -1 +1 @@\n+# placeholder — replace with gh pr diff ${prNum || 'N'} -- ${file}\n`;
  }

  const cfg = loadReviewConfig({ OLLAMA_API_KEY: 'eval' });
  const result = runFixtureCase(
    {
      id,
      diff,
      findings: candidate.findings,
      expected: candidate.meta?.expected || { shouldBlock: false, blockerCount: 0 },
      gateOverride: {},
      verifierVerdicts: null,
    },
    cfg.gate,
    cfg.prescan,
  );

  if (!result.ok) {
    return { ok: false, id, errors: result.errors, wouldBlock: result.wouldBlock };
  }

  try {
    await mkdir(target, { recursive: true });
    await writeFile(join(target, 'meta.json'), `${JSON.stringify(candidate.meta || { id }, null, 2)}\n`);
    await writeFile(join(target, 'findings.json'), `${JSON.stringify({ findings: candidate.findings }, null, 2)}\n`);
    await writeFile(join(target, 'diff.patch'), diff.endsWith('\n') ? diff : `${diff}\n`);
    log(`promoted fixture ${id} (${file})`);
  } catch (e) {
    await rm(target, { recursive: true, force: true }).catch(() => {});
    return { ok: false, id, error: e.message };
  }

  return { ok: true, id, errors: result.errors, wouldBlock: result.wouldBlock };
}

export async function promoteCandidatesFromDir(candidatesDir, opts = {}) {
  const { readdir } = await import('node:fs/promises');
  let entries = [];
  try {
    entries = await readdir(candidatesDir);
  } catch {
    return { promoted: [], skipped: [], failed: [] };
  }

  const promoted = [];
  const skipped = [];
  const failed = [];

  for (const name of entries.filter((n) => n.endsWith('.json'))) {
    let candidate;
    try {
      const raw = await readFile(join(candidatesDir, name), 'utf8');
      candidate = JSON.parse(raw);
    } catch (e) {
      failed.push({ id: name, ok: false, error: `parse: ${e.message}` });
      continue;
    }
    const r = await promoteCandidateToFixture(candidate, opts);
    if (r.skipped) skipped.push(r);
    else if (r.ok) promoted.push(r);
    else failed.push(r);
  }
  return { promoted, skipped, failed };
}
