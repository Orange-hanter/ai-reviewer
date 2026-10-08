/**
 * MCP полного прогона (RFC 08 + RFC 09 + RFC 10 + RFC 12): превью для агента, не merge-гейт.
 * GitHub не трогаем. Structure не импортируем.
 *
 * Пин: headSha обязателен на живом диффе; пути — только внутри REVIEW_MCP_ROOT.
 * review_run — единственный LLM и единственный мьютекс.
 * wait:false — stub + jobId до MoA; sidecar ключ = jobId.
 */

import { execFile } from 'node:child_process';
import { mkdir, readFile, readdir, stat, unlink, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { createReviewPipeline, PipelineSoftFail } from '../pipeline.mjs';
import { scoreActionability } from './actionability.mjs';
import { loadReviewConfig } from './config.mjs';
import { parseDiff, collectBlastSymbols } from './diff.mjs';
import { findingFingerprint, filterStalePriorFindings } from './findings.mjs';
import { matchFpPatterns, getFpPatterns } from './fp-patterns.mjs';
import { createGraphifyClient } from './graphify.mjs';
import { parseJsonl, aggregateMetrics } from './metrics.mjs';
import { createPrescanRunner } from './prescan-runner.mjs';
import { routeDiff } from './router.mjs';
import { suggestedScopeFiles } from './swarm.mjs';
import { resolveContainedPath, sha256, canonicalJson } from './util.mjs';
import { handleReviewMcpHandshake } from './review-mcp-handshake.mjs';
import {
  REVIEW_RUN_TOOL,
  REVIEW_FINDINGS_TOOL,
  REVIEW_ROUTE_TOOL,
  REVIEW_PRESCAN_TOOL,
  FP_CHECK_TOOL,
  FINGERPRINT_CHECK_TOOL,
  REVIEW_ARTIFACT_TOOL,
  BLAST_SCOPE_TOOL,
  ACTIONABILITY_EXPLAIN_TOOL,
  REVIEW_METRICS_TOOL,
  extraEnabled,
} from './review-mcp-tools.mjs';

export const REVIEW_MCP_PROTOCOL_VERSION = 1;
export {
  REVIEW_RUN_TOOL,
  REVIEW_FINDINGS_TOOL,
  REVIEW_ROUTE_TOOL,
  REVIEW_PRESCAN_TOOL,
  FP_CHECK_TOOL,
  FINGERPRINT_CHECK_TOOL,
  REVIEW_ARTIFACT_TOOL,
  BLAST_SCOPE_TOOL,
  ACTIONABILITY_EXPLAIN_TOOL,
  REVIEW_METRICS_TOOL,
};

const execFileP = promisify(execFile);
const HEAD_SHA_RE = /^[0-9a-fA-F]{7,64}$/;
const FINGERPRINT_RE = /^[0-9a-f]{64}$/;
const SEV_RANK = Object.freeze({ critical: 0, major: 1, minor: 2, nit: 3 });
const STORE_CAP = 8;
const DISK_CAP = 32;
const TEXT_MAX = 8192;
const ARTIFACT_MAX_BYTES = 8 * 1024 * 1024;
const BLAST_MAX_FILES = 50;
const EXTRA_PATHS_MAX = 20;
const DEFAULT_TOKEN_BUDGET = 4000;
const TOKEN_BUDGET_SCHEMA = Object.freeze({ minimum: 256, maximum: 32000 });
const SIDECAR_DIR = '.ai-review/mcp/runs';

const FIND_PROPS = REVIEW_FINDINGS_TOOL.inputSchema.properties;

const store = new Map();
const storeOrder = [];
let reviewRunning = false;
let currentJobId = '';

export function resetReviewMcpStore() {
  store.clear();
  storeOrder.length = 0;
  reviewRunning = false;
  currentJobId = '';
}

export function computeReviewJobId({ headSha, uncommitted, diffPath, diffHash }) {
  return sha256(canonicalJson({
    diffHash,
    diffPath,
    headSha,
    uncommitted: Boolean(uncommitted),
  }));
}

function mcpRoot(extra) {
  return extra.root || extra.env?.REVIEW_MCP_ROOT || process.env.REVIEW_MCP_ROOT || process.cwd();
}

function boundedInt(value, schema, fallback) {
  const number = Number(value);
  if (!Number.isInteger(number)) return fallback;
  return Math.min(Math.max(number, schema.minimum), schema.maximum);
}

function estimateTokens(value) {
  return Math.ceil(JSON.stringify(value).length / 4);
}

function slimFinding(f) {
  return {
    file: f.file || '',
    line: Number(f.line) || 0,
    severity: f.severity || '',
    title: String(f.title || '').slice(0, 200),
    fp: f.fp || undefined,
    verifierStatus: f.verifierStatus || undefined,
    actionabilityScore: Number.isFinite(f.actionabilityScore) ? f.actionabilityScore : undefined,
    prescan: f.prescan || undefined,
    consensusBlock: f.consensusBlock || undefined,
  };
}

function sortFindings(list) {
  return [...list].sort((a, b) => {
    const sa = SEV_RANK[a.severity] ?? 9;
    const sb = SEV_RANK[b.severity] ?? 9;
    if (sa !== sb) return sa - sb;
    const aa = Number(a.actionabilityScore) || 0;
    const bb = Number(b.actionabilityScore) || 0;
    if (bb !== aa) return bb - aa;
    return String(a.fp || '').localeCompare(String(b.fp || ''));
  });
}

export function headShaPrefixMatch(a, b) {
  const x = String(a || '').toLowerCase();
  const y = String(b || '').toLowerCase();
  if (!x || !y) return false;
  const shorter = x.length <= y.length ? x : y;
  const longer = x.length <= y.length ? y : x;
  return shorter.length >= 7 && longer.startsWith(shorter);
}

function requireHeadSha(args) {
  if (!args?.headSha) throw new Error('headSha is required');
  // Пин передан, но не hex — «required» тут уводит агента искать пропавший аргумент.
  if (!HEAD_SHA_RE.test(args.headSha)) throw new Error('headSha must be 7–64 hex chars');
}

function createDefaultGit(root) {
  const opts = { timeout: 5_000, maxBuffer: 1024 * 1024 };
  return {
    async isInsideWorkTree() {
      try {
        const { stdout } = await execFileP('git', ['-C', root, 'rev-parse', '--is-inside-work-tree'], opts);
        return String(stdout).trim() === 'true';
      } catch {
        return false;
      }
    },
    async revParseHead() {
      const { stdout } = await execFileP('git', ['-C', root, 'rev-parse', 'HEAD'], { ...opts, maxBuffer: 1024 });
      return String(stdout).trim();
    },
    async statusPorcelain() {
      const { stdout } = await execFileP('git', ['-C', root, 'status', '--porcelain'], opts);
      return String(stdout);
    },
  };
}

async function assertLiveDiffPin(args, extra) {
  if (!args?.diffPath) throw new Error('diffPath is required');
  requireHeadSha(args);
  if (args.uncommitted === true) return;
  const git = extra.git || createDefaultGit(mcpRoot(extra));
  if (!(await git.isInsideWorkTree())) return;
  const head = await git.revParseHead();
  if (!headShaPrefixMatch(args.headSha, head)) {
    throw new Error('headSha does not match HEAD');
  }
  const dirty = String(await git.statusPorcelain() || '').trim();
  if (dirty) throw new Error('working tree dirty; pass uncommitted: true');
}

async function readDiffText(diffPath, extra) {
  const root = mcpRoot(extra);
  const absolute = await resolveContainedPath(diffPath, { rootDir: root, mustExist: true });
  const text = await readFile(absolute, 'utf8');
  const parsed = parseDiff(text);
  return { absolute, text, files: parsed.files, changedText: parsed.changedText };
}

function sidecarDir(root) {
  return join(root, SIDECAR_DIR);
}

function sidecarPath(root, id) {
  return join(sidecarDir(root), `${id}.json`);
}

function entryStatus(entry) {
  return entry?.status || entry?.preview?.status || '';
}

function runningStub({ jobId, headSha, startedAt }) {
  return {
    kind: 'review_preview',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    status: 'running',
    jobId,
    runFingerprint: '',
    headSha,
    previewWouldBlock: false,
    blockers: [],
    findingsPreview: [],
    startedAt,
  };
}

function requireRunLookupId(args) {
  const jobOk = args?.jobId && FINGERPRINT_RE.test(args.jobId);
  const fpOk = args?.runFingerprint && FINGERPRINT_RE.test(args.runFingerprint);
  if (jobOk && fpOk) throw new Error('provide jobId or runFingerprint, not both');
  if (!jobOk && !fpOk) throw new Error('jobId or runFingerprint is required');
  return jobOk ? args.jobId : args.runFingerprint;
}

async function evictSidecars(root) {
  const dir = sidecarDir(root);
  let names;
  try {
    names = await readdir(dir);
  } catch {
    return;
  }
  const json = names.filter((n) => n.endsWith('.json'));
  if (json.length <= DISK_CAP) return;
  const ranked = [];
  for (const name of json) {
    const path = join(dir, name);
    try {
      const info = await stat(path);
      ranked.push({ path, mtime: info.mtimeMs });
    } catch {
      ranked.push({ path, mtime: 0 });
    }
  }
  ranked.sort((a, b) => a.mtime - b.mtime);
  const excess = ranked.length - DISK_CAP;
  for (const row of ranked.slice(0, excess)) {
    await unlink(row.path).catch(() => {});
  }
}

function hydrateSidecar(payload) {
  if (!payload || typeof payload !== 'object') return null;
  return {
    jobId: payload.jobId || '',
    runFingerprint: payload.runFingerprint || '',
    headSha: payload.headSha,
    artifactPath: payload.artifactPath || '',
    artifact: payload.artifact || {},
    preview: payload.preview,
    status: payload.status || payload.preview?.status || '',
    startedAt: payload.startedAt || payload.preview?.startedAt,
  };
}

async function persistRun(entry, extra) {
  const root = mcpRoot(extra);
  const id = entry.jobId || entry.runFingerprint;
  if (!id) throw new Error('jobId is required to persist sidecar');
  const dir = sidecarDir(root);
  await mkdir(dir, { recursive: true });
  const path = sidecarPath(root, id);
  const body = {
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    jobId: entry.jobId || '',
    runFingerprint: entry.runFingerprint || '',
    headSha: entry.headSha,
    status: entryStatus(entry),
    artifactPath: entry.artifactPath || '',
    savedAt: new Date().toISOString(),
    startedAt: entry.startedAt || entry.preview?.startedAt,
    preview: entry.preview || undefined,
    artifact: entry.artifact || {},
  };
  await writeFile(path, `${JSON.stringify(body)}\n`, 'utf8');
  await evictSidecars(root);
}

async function readSidecarFile(path, root, id) {
  let absolute;
  try {
    absolute = await resolveContainedPath(path, { rootDir: root, mustExist: true });
  } catch {
    return null;
  }
  let payload;
  try {
    payload = JSON.parse(await readFile(absolute, 'utf8'));
  } catch {
    return null;
  }
  const entry = hydrateSidecar(payload);
  if (!entry) return null;
  if (id && entry.jobId !== id && entry.runFingerprint !== id) return null;
  return entry;
}

async function loadSidecar(id, extra) {
  const root = mcpRoot(extra);
  const direct = await readSidecarFile(sidecarPath(root, id), root, id);
  if (direct) return direct;
  const dir = sidecarDir(root);
  let names;
  try {
    names = await readdir(dir);
  } catch {
    return null;
  }
  for (const name of names.filter((n) => n.endsWith('.json'))) {
    const entry = await readSidecarFile(join(dir, name), root, id);
    if (entry) return entry;
  }
  return null;
}

async function lookupEntry(id, extra) {
  return store.get(id) || await loadSidecar(id, extra);
}

function forgetJob(jobKey) {
  const entry = store.get(jobKey);
  store.delete(jobKey);
  if (entry?.jobId) store.delete(entry.jobId);
  if (entry?.runFingerprint) store.delete(entry.runFingerprint);
}

function rememberRun(entry) {
  const key = entry.jobId || entry.runFingerprint;
  if (!key) return;
  if (entry.jobId) {
    const prev = store.get(entry.jobId);
    if (prev?.runFingerprint && prev.runFingerprint !== entry.runFingerprint) {
      store.delete(prev.runFingerprint);
    }
    store.set(entry.jobId, entry);
  }
  if (entry.runFingerprint) store.set(entry.runFingerprint, entry);
  const idx = storeOrder.indexOf(key);
  if (idx >= 0) storeOrder.splice(idx, 1);
  storeOrder.push(key);
  while (storeOrder.length > STORE_CAP) forgetJob(storeOrder.shift());
}

function createSilentGithub(prior, calls) {
  return {
    readPriorStickyData: async () => prior || { findings: [], runs: [], sha: '' },
    readPriorFindings: async () => prior?.findings || [],
    loadDiff: async () => { calls.loadDiff++; return null; },
    deleteStaleInline: async () => { calls.deleteStaleInline++; return 0; },
    postReview: async () => { calls.postReview++; return null; },
    upsertSticky: async () => { calls.upsertSticky++; return 'skipped'; },
  };
}

async function loadPriorExport(path, root) {
  const absolute = await resolveContainedPath(path, { rootDir: root, mustExist: true });
  const payload = JSON.parse(await readFile(absolute, 'utf8'));
  return {
    findings: Array.isArray(payload.findings) ? payload.findings : [],
    runs: Array.isArray(payload.verdict?.runs) ? payload.verdict.runs : [],
    sha: typeof payload.headSha === 'string' ? payload.headSha : '',
  };
}

function fitPreview({ artifact, result, headSha, tokenBudget, kind = 'review_preview', jobId = '' }) {
  const swarm = artifact?.swarm || {};
  const blockers = sortFindings((artifact?.blockers || []).map(slimFinding));
  const findings = sortFindings((artifact?.findings || []).map(slimFinding));
  const base = {
    kind,
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    status: result?.degraded ? 'degraded' : 'completed',
    reason: result?.reason || undefined,
    headSha,
    jobId,
    runFingerprint: '',
    artifactPath: result?.exportResult?.written?.find((p) => p.endsWith('.json') && !p.endsWith('-swarm.json')) || '',
    previewWouldBlock: Boolean(artifact?.wouldBlock ?? result?.wouldBlock),
    strong: Boolean(swarm.strong),
    route: artifact?.router?.route || undefined,
    counts: artifact?.counts || {},
    blockers: [],
    findingsPreview: [],
    truncated: false,
  };

  const fingerprintSource = artifact || { headSha, reason: result?.reason || 'degraded' };
  base.runFingerprint = sha256(canonicalJson(fingerprintSource));

  const fill = (key, items) => {
    const out = [];
    for (const item of items) {
      const trial = { ...base, [key]: [...out, item], truncated: true };
      if (estimateTokens(trial) > tokenBudget) {
        base.truncated = true;
        break;
      }
      out.push(item);
    }
    base[key] = out;
  };

  fill('blockers', blockers);
  const blockerKeys = new Set(base.blockers.map((f) => `${f.file}|${f.line}|${f.fp || ''}`));
  fill('findingsPreview', findings.filter((f) => !blockerKeys.has(`${f.file}|${f.line}|${f.fp || ''}`)));
  if (base.blockers.length < blockers.length || base.findingsPreview.length < findings.filter((f) => !blockerKeys.has(`${f.file}|${f.line}|${f.fp || ''}`)).length) {
    base.truncated = true;
  }
  if (estimateTokens(base) > tokenBudget) {
    base.findingsPreview = [];
    base.truncated = true;
    while (base.blockers.length && estimateTokens(base) > tokenBudget) {
      base.blockers.pop();
      base.truncated = true;
    }
  }
  return base;
}

async function finishDegraded(jobId, headSha, startedAt, error, extra, tokenBudget) {
  const reason = error?.message || String(error);
  const preview = {
    kind: 'review_preview',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    status: 'degraded',
    reason,
    headSha,
    jobId,
    runFingerprint: sha256(canonicalJson({ headSha, reason })),
    artifactPath: '',
    previewWouldBlock: false,
    blockers: [],
    findingsPreview: [],
    startedAt,
    truncated: false,
  };
  if (tokenBudget && estimateTokens(preview) > tokenBudget) {
    preview.truncated = true;
  }
  const entry = {
    jobId,
    runFingerprint: preview.runFingerprint,
    headSha,
    status: 'degraded',
    startedAt,
    artifactPath: '',
    artifact: {},
    preview,
  };
  rememberRun(entry);
  await persistRun(entry, extra);
  return preview;
}

async function executeReviewRun(args, extra, jobId, startedAt) {
  const root = mcpRoot(extra);
  const tokenBudget = boundedInt(args.tokenBudget, TOKEN_BUDGET_SCHEMA, DEFAULT_TOKEN_BUDGET);
  const diffPath = await resolveContainedPath(args.diffPath, { rootDir: root, mustExist: true });

  let prior = { findings: [], runs: [], sha: '' };
  if (args.priorExportPath) {
    prior = await loadPriorExport(args.priorExportPath, root);
  }

  const env = extra.env ?? process.env;
  const cfg = loadReviewConfig({
    ...env,
    REVIEW_EXPORT_DRY_RUN: extra.env?.REVIEW_EXPORT_DRY_RUN ?? env.REVIEW_EXPORT_DRY_RUN ?? '1',
  }, {
    dryRun: true,
    diffFile: diffPath,
    headSha: args.headSha,
    repo: '',
    prNumber: 0,
  });

  const githubCalls = extra.githubCalls || { postReview: 0, upsertSticky: 0, deleteStaleInline: 0, loadDiff: 0 };
  extra.githubCalls = githubCalls;
  const github = extra.github || createSilentGithub(prior, githubCalls);

  const notify = extra.notify;
  const log = extra.log || ((...a) => {
    notify?.('notifications/message', { level: 'info', data: a.map(String).join(' ') });
  });

  const pipelineDeps = {
    ...(extra.pipelineDeps || {}),
    log,
    die: (msg) => { throw new PipelineSoftFail(msg); },
    github,
    notifyTelegram: async () => {},
    headSha: args.headSha,
  };

  const createPipeline = extra.createPipeline || createReviewPipeline;
  const pipeline = createPipeline(cfg, pipelineDeps);
  if (typeof pipeline.runDetailed !== 'function') {
    throw new Error('pipeline.runDetailed is required');
  }

  try {
    const result = await pipeline.runDetailed();
    const artifact = result.artifact;
    if (artifact?.headSha && !headShaPrefixMatch(args.headSha, artifact.headSha)) {
      throw new Error(`pinned headSha ${args.headSha} does not match artifact ${artifact.headSha}`);
    }
    const preview = fitPreview({
      artifact, result, headSha: args.headSha, tokenBudget, jobId,
    });
    if (startedAt) preview.startedAt = startedAt;
    const entry = {
      jobId,
      runFingerprint: preview.runFingerprint,
      headSha: args.headSha,
      status: preview.status,
      startedAt,
      artifactPath: preview.artifactPath || '',
      artifact,
      preview,
    };
    rememberRun(entry);
    await persistRun(entry, extra);
    return preview;
  } catch (error) {
    await finishDegraded(jobId, args.headSha, startedAt, error, extra, tokenBudget);
    throw error;
  } finally {
    if (currentJobId === jobId) {
      reviewRunning = false;
      currentJobId = '';
    }
  }
}

async function callReviewRun(args, extra = {}) {
  if (!args?.diffPath) throw new Error('diffPath is required');
  requireHeadSha(args);
  await assertLiveDiffPin(args, extra);

  const { text } = await readDiffText(args.diffPath, extra);
  const jobId = computeReviewJobId({
    headSha: args.headSha,
    uncommitted: args.uncommitted,
    diffPath: args.diffPath,
    diffHash: sha256(text),
  });
  const wait = args.wait !== false;
  const existing = await lookupEntry(jobId, extra);

  if (existing && (entryStatus(existing) === 'completed' || entryStatus(existing) === 'degraded')) {
    const tokenBudget = boundedInt(args.tokenBudget, TOKEN_BUDGET_SCHEMA, DEFAULT_TOKEN_BUDGET);
    if (existing.artifact && Object.keys(existing.artifact).length) {
      return fitPreview({
        artifact: existing.artifact,
        result: {
          degraded: entryStatus(existing) === 'degraded',
          reason: existing.preview?.reason,
          wouldBlock: existing.artifact?.wouldBlock,
          exportResult: { written: existing.artifactPath ? [existing.artifactPath] : [] },
        },
        headSha: args.headSha,
        tokenBudget,
        jobId,
      });
    }
    if (existing.preview) return existing.preview;
  }

  if (existing && entryStatus(existing) === 'running') {
    return existing.preview || runningStub({
      jobId, headSha: args.headSha, startedAt: existing.startedAt,
    });
  }

  if (reviewRunning) {
    if (currentJobId === jobId) {
      const live = store.get(jobId);
      return live?.preview || runningStub({ jobId, headSha: args.headSha, startedAt: live?.startedAt });
    }
    throw new Error('review already running');
  }

  const startedAt = new Date().toISOString();
  const stub = runningStub({ jobId, headSha: args.headSha, startedAt });
  const stubEntry = {
    jobId,
    runFingerprint: '',
    headSha: args.headSha,
    status: 'running',
    startedAt,
    artifactPath: '',
    artifact: {},
    preview: stub,
  };
  reviewRunning = true;
  currentJobId = jobId;
  rememberRun(stubEntry);
  try {
    await persistRun(stubEntry, extra);
  } catch (error) {
    reviewRunning = false;
    currentJobId = '';
    throw error;
  }

  const runPromise = executeReviewRun(args, extra, jobId, startedAt);
  extra.captureRun?.(runPromise);
  if (!wait) {
    runPromise.catch((error) => {
      extra.notify?.('notifications/message', {
        level: 'error',
        data: error?.message || String(error),
      });
    });
    return stub;
  }
  return await runPromise;
}

async function callReviewFindings(args, extra = {}) {
  const id = requireRunLookupId(args);
  if (args.headSha && !HEAD_SHA_RE.test(args.headSha)) {
    throw new Error(`invalid headSha: ${args.headSha}`);
  }
  const entry = await lookupEntry(id, extra);
  if (!entry) throw new Error('unknown runFingerprint');
  if (entry.jobId || entry.runFingerprint) rememberRun(entry);
  if (args.headSha && !headShaPrefixMatch(args.headSha, entry.headSha)) {
    throw new Error(`pinned headSha ${args.headSha} does not match run ${entry.headSha}`);
  }
  const offset = boundedInt(args.offset, FIND_PROPS.offset, 0);
  const limit = boundedInt(args.limit, FIND_PROPS.limit, 20);
  const status = entryStatus(entry) || 'completed';
  if (status === 'running') {
    return {
      kind: 'review_findings',
      protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
      status: 'running',
      jobId: entry.jobId || '',
      runFingerprint: '',
      headSha: entry.headSha,
      offset,
      limit,
      total: 0,
      findings: [],
    };
  }
  let findings = sortFindings((entry.artifact?.findings || []).map(slimFinding));
  if (args.file) findings = findings.filter((f) => f.file === args.file);
  if (args.severity) findings = findings.filter((f) => f.severity === args.severity);
  const slice = findings.slice(offset, offset + limit);
  return {
    kind: 'review_findings',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    status,
    jobId: entry.jobId || '',
    runFingerprint: entry.runFingerprint,
    headSha: entry.headSha,
    offset,
    limit,
    total: findings.length,
    findings: slice,
  };
}

async function callReviewRoute(args, extra = {}) {
  await assertLiveDiffPin(args, extra);
  const { text, files } = await readDiffText(args.diffPath, extra);
  const changedPaths = [...files.keys()];
  const routed = routeDiff(changedPaths, text);
  return {
    kind: 'review_route',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    headSha: args.headSha,
    route: routed.route,
    reason: routed.reason,
    files: routed.files || [],
  };
}

function fitPrescanFindings(base, findings, tokenBudget) {
  const slim = sortFindings(findings.map(slimFinding));
  const out = [];
  let truncated = Boolean(base.truncated);
  for (const item of slim) {
    const trial = { ...base, findings: [...out, item], truncated: true };
    if (estimateTokens(trial) > tokenBudget) {
      truncated = true;
      break;
    }
    out.push(item);
  }
  if (out.length < slim.length) truncated = true;
  return { findings: out, truncated };
}

async function callReviewPrescan(args, extra = {}) {
  await assertLiveDiffPin(args, extra);
  const tokenBudget = boundedInt(args.tokenBudget, TOKEN_BUDGET_SCHEMA, DEFAULT_TOKEN_BUDGET);
  const { absolute, files } = await readDiffText(args.diffPath, extra);
  const changedPaths = [...files.keys()];
  const env = extra.env ?? process.env;
  const cfg = loadReviewConfig(env, {
    dryRun: true,
    diffFile: absolute,
    headSha: args.headSha,
    repo: '',
    prNumber: 0,
  });
  const notify = extra.notify;
  const log = extra.log || ((...a) => {
    notify?.('notifications/message', { level: 'info', data: a.map(String).join(' ') });
  });
  const runnerOpts = { log };
  if (extra.prescanExec) runnerOpts.exec = extra.prescanExec;
  const runner = createPrescanRunner(cfg, runnerOpts);
  const result = await runner.run(changedPaths);
  const base = {
    kind: 'review_prescan',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    status: result.error ? 'degraded' : 'completed',
    headSha: args.headSha,
    scanned: result.scanned ?? 0,
    lintableTotal: result.lintableTotal ?? 0,
    truncated: Boolean(result.truncated),
    skipped: result.skipped ?? 0,
    byTool: result.byTool || undefined,
    error: result.error || undefined,
    findings: [],
  };
  const fitted = fitPrescanFindings(base, result.findings || [], tokenBudget);
  return { ...base, findings: fitted.findings, truncated: fitted.truncated };
}

function callFpCheck(args) {
  const file = String(args?.file || '').trim();
  if (!file) throw new Error('file is required');
  const title = String(args?.title ?? '');
  const body = String(args?.body ?? '');
  if (title.length + body.length > TEXT_MAX) throw new Error('text too long');
  const extraPaths = Array.isArray(args.extraPaths) ? args.extraPaths.slice(0, EXTRA_PATHS_MAX).map(String) : [];
  const text = `${title} ${body}`;
  const paths = [file, ...extraPaths];
  const patternId = matchFpPatterns(text, paths, getFpPatterns());
  return {
    kind: 'fp_check',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    match: Boolean(patternId),
    patternId: patternId || null,
  };
}

async function callFingerprintCheck(args, extra = {}) {
  const file = String(args?.file || '').trim();
  if (!file) throw new Error('file is required');
  const priorFp = String(args?.priorFp || '').trim();
  if (!priorFp) throw new Error('priorFp is required');
  if (!args?.diffPath) throw new Error('diffPath is required');
  const line = Number(args.line);
  if (!Number.isInteger(line) || line < 1) throw new Error('line is required');
  if (args.headSha) requireHeadSha(args);
  const { files } = await readDiffText(args.diffPath, extra);
  const rec = files.get(file);
  if (!rec) {
    return {
      kind: 'fingerprint_check',
      protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
      status: 'stale',
      currentFp: '',
      line: null,
    };
  }
  const currentFp = findingFingerprint(rec, line) || '';
  const kept = filterStalePriorFindings([{ file, line, fp: priorFp }], files);
  if (kept.length) {
    const keptLine = Number(kept[0].line) || line;
    const keptRec = files.get(file) || rec;
    return {
      kind: 'fingerprint_check',
      protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
      status: 'still_present',
      currentFp: findingFingerprint(keptRec, keptLine) || currentFp || priorFp,
      line: keptLine,
    };
  }
  if (!currentFp) {
    return {
      kind: 'fingerprint_check',
      protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
      status: 'unknown_line',
      currentFp: '',
      line,
    };
  }
  return {
    kind: 'fingerprint_check',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    status: 'stale',
    currentFp,
    line: null,
  };
}

function unwrapArtifactPayload(payload) {
  if (payload && typeof payload === 'object' && payload.artifact && typeof payload.artifact === 'object'
    && (payload.runFingerprint || payload.protocolVersion)) {
    return payload.artifact;
  }
  return payload;
}

async function callReviewArtifact(args, extra = {}) {
  const hasPath = Boolean(args?.path);
  const jobOk = args?.jobId && FINGERPRINT_RE.test(args.jobId);
  const fpOk = args?.runFingerprint && FINGERPRINT_RE.test(args.runFingerprint);
  const n = [hasPath, jobOk, fpOk].filter(Boolean).length;
  if (n !== 1) throw new Error('provide exactly one of path, jobId, runFingerprint');
  if (args.headSha && !HEAD_SHA_RE.test(args.headSha)) {
    throw new Error(`invalid headSha: ${args.headSha}`);
  }
  const tokenBudget = boundedInt(args.tokenBudget, TOKEN_BUDGET_SCHEMA, DEFAULT_TOKEN_BUDGET);

  if (jobOk || fpOk) {
    const id = jobOk ? args.jobId : args.runFingerprint;
    const entry = await lookupEntry(id, extra);
    if (!entry) throw new Error('unknown runFingerprint');
    if (args.headSha && !headShaPrefixMatch(args.headSha, entry.headSha)) {
      throw new Error(`pinned headSha ${args.headSha} does not match run ${entry.headSha}`);
    }
    const status = entryStatus(entry) || 'completed';
    if (status === 'running') {
      return {
        kind: 'review_artifact',
        protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
        status: 'running',
        jobId: entry.jobId || '',
        runFingerprint: '',
        headSha: args.headSha || entry.headSha,
        previewWouldBlock: false,
        blockers: [],
        findingsPreview: [],
        truncated: false,
      };
    }
    const preview = fitPreview({
      artifact: entry.artifact || {},
      result: {
        degraded: status === 'degraded',
        wouldBlock: entry.artifact?.wouldBlock,
        exportResult: { written: entry.artifactPath ? [entry.artifactPath] : [] },
      },
      headSha: args.headSha || entry.headSha,
      tokenBudget,
      kind: 'review_artifact',
      jobId: entry.jobId || '',
    });
    return {
      kind: 'review_artifact',
      protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
      status,
      jobId: entry.jobId || '',
      headSha: args.headSha || entry.headSha,
      previewWouldBlock: preview.previewWouldBlock,
      strong: preview.strong,
      route: preview.route,
      counts: preview.counts,
      blockers: preview.blockers,
      findingsPreview: preview.findingsPreview,
      truncated: preview.truncated,
    };
  }

  const root = mcpRoot(extra);
  const absolute = await resolveContainedPath(args.path, { rootDir: root, mustExist: true });
  const info = await stat(absolute);
  if (info.size > ARTIFACT_MAX_BYTES) throw new Error('artifact too large');
  const payload = JSON.parse(await readFile(absolute, 'utf8'));
  const artifact = unwrapArtifactPayload(payload);
  const artifactSha = artifact?.headSha || payload?.headSha || '';
  if (args.headSha && artifactSha && !headShaPrefixMatch(args.headSha, artifactSha)) {
    throw new Error(`pinned headSha ${args.headSha} does not match artifact ${artifactSha}`);
  }
  const preview = fitPreview({
    artifact,
    result: { degraded: false, wouldBlock: artifact?.wouldBlock, exportResult: { written: [absolute] } },
    headSha: args.headSha || artifactSha,
    tokenBudget,
    kind: 'review_artifact',
    jobId: payload?.jobId || '',
  });
  return {
    kind: 'review_artifact',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    headSha: args.headSha || artifactSha,
    previewWouldBlock: preview.previewWouldBlock,
    strong: preview.strong,
    route: preview.route,
    counts: preview.counts,
    blockers: preview.blockers,
    findingsPreview: preview.findingsPreview,
    truncated: preview.truncated,
  };
}

async function callBlastScope(args, extra = {}) {
  await assertLiveDiffPin(args, extra);
  const { text, files, changedText } = await readDiffText(args.diffPath, extra);
  const changedPaths = [...files.keys()];
  const env = extra.env ?? process.env;
  const cfg = loadReviewConfig(env, {
    dryRun: true,
    diffFile: args.diffPath,
    headSha: args.headSha,
    repo: '',
    prNumber: 0,
  });
  const symbols = collectBlastSymbols(changedText || text, new Map());
  let skipReason;
  let affected = [];
  try {
    const graphify = extra.graphify || createGraphifyClient(cfg);
    const blast = await graphify.buildBlastRadius(symbols);
    const metrics = blast?.metrics || {};
    affected = Array.isArray(metrics.affected_files) ? metrics.affected_files : [];
    if (metrics.skip_reason && metrics.skip_reason !== 'none') {
      skipReason = metrics.skip_reason === 'graph_missing' ? 'graphify_unavailable' : metrics.skip_reason;
    }
  } catch (error) {
    const reason = error?.code === 'ENOENT' ? 'graphify_unavailable' : 'graphify_unavailable';
    skipReason = reason;
    affected = [];
  }
  const filesOut = suggestedScopeFiles(affected, changedPaths).slice(0, BLAST_MAX_FILES);
  const out = {
    kind: 'blast_scope',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    headSha: args.headSha,
    files: filesOut,
  };
  if (skipReason) out.skipReason = skipReason;
  return out;
}

function callActionabilityExplain(args) {
  const finding = args?.finding;
  if (!finding || typeof finding !== 'object') throw new Error('finding is required');
  const { score, reasons } = scoreActionability({ ...finding });
  return {
    kind: 'actionability_explain',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    score,
    reasons,
  };
}

async function callReviewMetrics(args, extra = {}) {
  if (!args?.path) throw new Error('path is required');
  const root = mcpRoot(extra);
  const absolute = await resolveContainedPath(args.path, { rootDir: root, mustExist: true });
  const info = await stat(absolute);
  if (info.size > ARTIFACT_MAX_BYTES) throw new Error('artifact too large');
  const text = await readFile(absolute, 'utf8');
  const records = parseJsonl(text);
  const agg = aggregateMetrics(records);
  return {
    kind: 'review_metrics',
    protocolVersion: REVIEW_MCP_PROTOCOL_VERSION,
    ...agg,
  };
}

function rpcResult(id, payload) {
  return {
    jsonrpc: '2.0',
    id,
    result: {
      content: [{ type: 'text', text: JSON.stringify(payload, null, 2) }],
      structuredContent: payload,
    },
  };
}

function rpcError(id, message) {
  return {
    jsonrpc: '2.0',
    id,
    error: {
      code: message.startsWith('method not found') ? -32601 : -32602,
      message,
    },
  };
}

export async function handleReviewMcpRequest(request, extra = {}) {
  const id = request?.id ?? null;
  try {
    const handshake = handleReviewMcpHandshake(request, extra);
    if (handshake !== undefined) return handshake;
    if (request?.method === 'tools/call') {
      const name = request.params?.name;
      const args = request.params?.arguments || {};
      if (name === REVIEW_RUN_TOOL.name) return rpcResult(id, await callReviewRun(args, extra));
      if (name === REVIEW_FINDINGS_TOOL.name) return rpcResult(id, await callReviewFindings(args, extra));
      if (name === REVIEW_ROUTE_TOOL.name) return rpcResult(id, await callReviewRoute(args, extra));
      if (name === REVIEW_PRESCAN_TOOL.name) return rpcResult(id, await callReviewPrescan(args, extra));
      if (name === FP_CHECK_TOOL.name) return rpcResult(id, callFpCheck(args));
      if (name === FINGERPRINT_CHECK_TOOL.name) return rpcResult(id, await callFingerprintCheck(args, extra));
      if (name === REVIEW_ARTIFACT_TOOL.name) return rpcResult(id, await callReviewArtifact(args, extra));
      if (name === BLAST_SCOPE_TOOL.name
        || name === ACTIONABILITY_EXPLAIN_TOOL.name
        || name === REVIEW_METRICS_TOOL.name) {
        if (!extraEnabled(extra)) throw new Error(`unknown tool: ${name}`);
        if (name === BLAST_SCOPE_TOOL.name) return rpcResult(id, await callBlastScope(args, extra));
        if (name === ACTIONABILITY_EXPLAIN_TOOL.name) return rpcResult(id, callActionabilityExplain(args));
        return rpcResult(id, await callReviewMetrics(args, extra));
      }
      throw new Error(`unknown tool: ${name}`);
    }
    throw new Error(`method not found: ${request?.method}`);
  } catch (error) {
    return rpcError(id, error.message);
  }
}
