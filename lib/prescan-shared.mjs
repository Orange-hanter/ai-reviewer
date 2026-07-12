/** Shared path helpers + prompt для eslint/ast-grep prescan (без циклических импортов). */

import { spawn } from 'node:child_process';
import { relative } from 'node:path';

const LINTABLE = /^app\/(?:shared|server|client|integrations|ops|scripts|tests|e2e)\/.+\.(?:ts|tsx)$|^app\/[^/]+\.ts$/;
const APP_PREFIX = /^(?:server|shared|client|integrations|ops|scripts|tests|e2e)\//;

/** Путь как в git diff → `app/server/foo.ts` для eslint cwd. */
export function normalizeAppPath(path) {
  if (path.startsWith('app/')) return path;
  if (APP_PREFIX.test(path)) return `app/${path}`;
  return path;
}

export function isLintablePath(path) {
  return LINTABLE.test(normalizeAppPath(path));
}

/** Lintable файлы с сохранением ключа diff (`+++ b/...`). */
export function selectLintableEntries(changedPaths, max = 30) {
  const seen = new Set();
  const entries = [];
  for (const diffKey of changedPaths) {
    if (!isLintablePath(diffKey)) continue;
    const appPath = normalizeAppPath(diffKey);
    if (seen.has(appPath)) continue;
    seen.add(appPath);
    entries.push({ appPath, diffKey });
  }
  const truncated = entries.length > max;
  return { entries: entries.slice(0, max), total: entries.length, truncated };
}

export function buildPrescanPathMap(entries) {
  const map = new Map();
  for (const { appPath, diffKey } of entries) map.set(appPath, diffKey);
  return map;
}

/** Ключ finding.file = путь как в diff (для inline + dedup с LLM). */
export function resolvePrescanDiffKey(appPath, pathMap) {
  const norm = normalizeAppPath(appPath);
  if (pathMap?.has(norm)) return pathMap.get(norm);
  return norm;
}

/** eslint / ast-grep abs path → app/-relative path. */
export function toolPathToAppPath(absOrRel, cwd = process.cwd()) {
  const norm = String(absOrRel || '').replace(/\\/g, '/');
  if (norm.startsWith('app/')) return norm;
  const inApp = norm.indexOf('/app/');
  if (inApp >= 0) return norm.slice(inApp + 1);
  if (!norm.startsWith('/') && !/^[A-Za-z]:/.test(norm)) {
    return normalizeAppPath(norm);
  }
  const rel = relative(cwd, norm).replace(/\\/g, '/');
  if (rel.startsWith('app/')) return rel;
  if (rel && !rel.startsWith('..')) return normalizeAppPath(rel);
  return norm;
}

export function buildPrescanPromptSection(findings) {
  if (!findings.length) return '';
  const lines = findings.map(
    (f) => `- [${f.severity}] \`${f.file}:${f.line}\` — ${f.title}: ${f.body}`,
  );
  return `${lines.join('\n')}\n\nDo NOT repeat these deterministic pre-scan findings (eslint/ast-grep) — they are already reported.`;
}

export function mergePrescanSkipped(...parts) {
  const s = parts.filter(Boolean);
  return s.length ? s.join('+') : undefined;
}

export function logToolFailure(log, label, { code, stdout, stderr }) {
  if (code === 0) return;
  const msg = (stderr || stdout || '').trim().slice(0, 300);
  log(`prescan ${label} exit ${code}${msg ? `: ${msg}` : ''}`);
}

export function mergeLintableTotal(...values) {
  let max = 0;
  for (const v of values) {
    if (typeof v === 'number' && v > max) max = v;
  }
  return max > 0 ? max : undefined;
}

export function runCommand(cmd, args, { cwd, maxBuffer = 10_000_000 } = {}) {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(cmd, args, { cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    let settled = false;

    const finish = (fn, value) => {
      if (settled) return;
      settled = true;
      cleanup();
      fn(value);
    };

    const onStdout = (d) => {
      if (settled) return;
      const next = stdout + d;
      if (next.length > maxBuffer) {
        child.kill();
        finish(reject, new Error(`prescan stdout/stderr exceeded ${maxBuffer} bytes`));
        return;
      }
      stdout = next;
    };
    const onStderr = (d) => {
      if (settled) return;
      const next = stderr + d;
      if (next.length > maxBuffer) {
        child.kill();
        finish(reject, new Error(`prescan stdout/stderr exceeded ${maxBuffer} bytes`));
        return;
      }
      stderr = next;
    };
    const cleanup = () => {
      child.stdout.removeListener('data', onStdout);
      child.stderr.removeListener('data', onStderr);
      child.removeAllListeners('error');
      child.removeAllListeners('close');
    };
    child.stdout.on('data', onStdout);
    child.stderr.on('data', onStderr);
    child.on('error', (e) => finish(reject, e));
    child.on('close', (code) => {
      finish(resolvePromise, { code: code ?? 1, stdout, stderr });
    });
  });
}
