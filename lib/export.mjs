/** R9: machine-readable export — JSON artifact + SARIF for GitHub Code Scanning. */

import { createHash } from 'node:crypto';
import { mkdir, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { getProfile } from './profile.mjs';
import { buildSwarmVerdict } from './swarm.mjs';

// v2: slimFinding несёт fp/verifierStatus/actionabilityScore/actionabilityPromoted —
// дедупликация находок по коду и метрика стабильности набора на стороне потребителя
// (cod-doc ingest). `fp` — тот же отпечаток, что в футере sticky (attachFingerprints);
// `actionabilityScore` есть у каждой находки, `actionabilityPromoted` отмечает advisory,
// поднятую в строку. Изменение аддитивное: потребитель v1 читает v2 без поломки, поэтому
// сверять версию нужно на «известна ли», а не на равенство.
export const EXPORT_VERSION = 2;
export const SARIF_SCHEMA = 'https://json.schemastore.org/sarif-2.1.0.json';
export const SARIF_GUID_RE = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-5][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$/;
export const TOOL_VERSION = '1.0';

/** Имя инструмента в SARIF и в именах artifact-файлов — из профиля проекта. */
export function toolName() {
  return getProfile().toolName;
}

/** Stable RFC-4122-shaped GUID for SARIF automationDetails (Code Scanning requires UUID). */
export function sarifAutomationGuid(seed, tool = toolName()) {
  const h = createHash('sha256').update(`${tool}:${seed}`).digest('hex');
  const variant = ['8', '9', 'a', 'b'][parseInt(h.slice(16, 17), 16) % 4];
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-4${h.slice(13, 16)}-${variant}${h.slice(17, 20)}-${h.slice(20, 32)}`;
}

const BODY_EXPORT_MAX = 400;
const REASONING_EXPORT_MAX = 600;
const SARIF_MESSAGE_MAX = 500;
const TITLE_MAX = 200;

const SEV_TO_SARIF_LEVEL = {
  critical: 'error',
  major: 'warning',
  minor: 'note',
  nit: 'note',
};

const SEVERITIES = ['critical', 'major', 'minor', 'nit'];

/** SARIF ruleId: `<prefix>/<severity>`; префикс — из профиля (Code Scanning группирует по нему). */
function sarifRuleIds(prefix = getProfile().sarifRulePrefix) {
  return Object.fromEntries(SEVERITIES.map((sev) => [sev, `${prefix}/${sev}`]));
}

/** Redacted finding для artifact (body урезан — как R7 observability). */
function slimFinding(f) {
  const body = String(f.body || '');
  return {
    file: f.file || '',
    line: f.line || 0,
    severity: f.severity,
    title: String(f.title || '').slice(0, TITLE_MAX),
    body: body ? body.slice(0, BODY_EXPORT_MAX) : undefined,
    bodyTruncated: body.length > BODY_EXPORT_MAX,
    model: f.model || '',
    confidence: f.confidence,
    consensusBlock: f.consensusBlock || false,
    prescan: f.prescan || undefined,
    advisorClaims: f.advisorClaims?.map((c) => ({
      role: c.role,
      status: c.status,
      reason: c.reason ? String(c.reason).slice(0, 200) : '',
    })),
    refutationStatus: f.refutationStatus || undefined,
    fp: f.fp || undefined,
    verifierStatus: f.verifierStatus || undefined,
    actionabilityScore: Number.isFinite(f.actionabilityScore)
      ? f.actionabilityScore
      : undefined,
    actionabilityPromoted: f.actionabilityPromoted || undefined,
  };
}

/** Percent-encode path segments for SARIF artifactLocation.uri. */
export function sarifArtifactUri(file) {
  const raw = (file || '').trim();
  if (!raw) return 'unknown';
  return raw.split('/').map((seg) => encodeURIComponent(seg)).join('/');
}

/** Shared MoA advisor + consensus reasoning extraction (DRY for JSON + sticky footer). */
export function extractReasoningEntries(reviews, reviewMode, { excerptLen = 600, errorMax = 200 } = {}) {
  const out = [];
  for (const r of reviews || []) {
    if (r.references?.length) {
      for (const ref of r.references) {
        if (ref.error) {
          out.push({
            role: ref.role || 'advisor',
            model: ref.model,
            chunkCount: ref.chunkCount || 1,
            error: String(ref.error).slice(0, errorMax),
          });
        } else if (ref.text) {
          const text = String(ref.text);
          out.push({
            role: ref.role || 'advisor',
            model: ref.model,
            chunkCount: ref.chunkCount || 1,
            excerpt: text.slice(0, excerptLen),
            truncated: text.length > excerptLen,
          });
        }
      }
      continue;
    }
    if (reviewMode !== 'moa' && !r.error && r.summary) {
      const text = String(r.summary);
      out.push({
        role: 'model',
        model: r.model,
        excerpt: text.slice(0, excerptLen),
        truncated: text.length > excerptLen,
      });
    }
  }
  return out;
}

function slimReasoning(reviews, reviewMode) {
  return extractReasoningEntries(reviews, reviewMode, { excerptLen: REASONING_EXPORT_MAX });
}

/** Полный JSON payload для CI artifact / интеграций. */
export function buildReviewJson(input) {
  const {
    pr, headSha, reviewMode, route, wouldBlock, verdict, counts, total,
    findings, blockers, reviews, verifierStats, refutationStats, prescanResult,
    llmFailed, llmErrors, blastMetrics, changedPaths,
  } = input;

  const blast = blastMetrics && typeof blastMetrics === 'object'
    ? {
        symbols_total: blastMetrics.symbols_total ?? 0,
        symbols_resolved: blastMetrics.symbols_resolved ?? 0,
        affected_nonempty: blastMetrics.affected_nonempty ?? 0,
        graph_status: blastMetrics.graph_status || 'unknown',
        skip_reason: blastMetrics.skip_reason || undefined,
        affected_files: Array.isArray(blastMetrics.affected_files) ? blastMetrics.affected_files : [],
        confidence: blastMetrics.confidence || undefined,
        cross_community: blastMetrics.cross_community ?? 0,
      }
    : undefined;

  const swarm = buildSwarmVerdict({
    wouldBlock: Boolean(wouldBlock),
    blockers,
    blastMetrics: blast || blastMetrics,
    changedPaths,
    headSha,
    pr,
    verdict,
  });

  return {
    version: EXPORT_VERSION,
    generatedAt: new Date().toISOString(),
    pr: {
      number: pr?.number ?? 0,
      title: String(pr?.title || '').slice(0, 200),
      url: pr?.html_url || '',
    },
    headSha: String(headSha || '').slice(0, 40),
    reviewMode: reviewMode || 'consensus',
    router: route ? { route: route.route, reason: route.reason } : undefined,
    wouldBlock: Boolean(wouldBlock),
    // Воспроизводимость вердикта на этом коммите: `wouldBlock` говорит про ЭТОТ прогон,
    // а недетерминизм виден только в сравнении прогонов одного sha. Sticky перезаписывается,
    // artifact — нет, поэтому агрегат должен лежать здесь.
    verdict: verdict || undefined,
    llmFailed: llmFailed || undefined,
    llmErrors: llmErrors?.length ? llmErrors : undefined,
    counts: counts || {},
    total: total ?? (findings?.length ?? 0),
    findings: (findings || []).map(slimFinding),
    blockers: (blockers || []).map(slimFinding),
    reasoning: slimReasoning(reviews, reviewMode || 'consensus'),
    verifier: verifierStats || undefined,
    refutation: refutationStats || undefined,
    blast,
    /** E22/E23: контракт для PR-swarm оркестратора (см. docs/swarm.md). */
    swarm,
    prescan: prescanResult ? {
      findings: prescanResult.findings?.length ?? 0,
      scanned: prescanResult.scanned,
      skipped: prescanResult.skipped,
      byTool: prescanResult.byTool,
      error: prescanResult.error,
    } : undefined,
  };
}

function sarifLocation(f) {
  const uri = sarifArtifactUri(f.file);
  const loc = {
    physicalLocation: {
      artifactLocation: { uri },
    },
  };
  if (f.line > 0) {
    loc.physicalLocation.region = { startLine: f.line };
  }
  return loc;
}

/** Findings для SARIF: по умолчанию только blockers (шум minor/nit в Code Scanning). */
export function selectSarifFindings(payload, sarifAll = false) {
  if (sarifAll) return payload.findings || [];
  const blockers = payload.blockers || [];
  if (blockers.length) return blockers;
  return (payload.findings || []).filter((f) => f.consensusBlock);
}

/** SARIF 2.1.0 для upload-sarif / Code Scanning. */
export function buildSarif(payload, { repositoryUri, sarifAll = false, profile = getProfile() } = {}) {
  const findings = selectSarifFindings(payload, sarifAll);
  const SEV_TO_RULE = sarifRuleIds(profile.sarifRulePrefix);
  const tool = profile.toolName;
  const rules = new Map();
  for (const sev of SEVERITIES) {
    rules.set(SEV_TO_RULE[sev], {
      id: SEV_TO_RULE[sev],
      name: sev,
      shortDescription: { text: `AI review — ${sev}` },
      defaultConfiguration: { level: SEV_TO_SARIF_LEVEL[sev] },
    });
  }

  const results = findings.map((f) => {
    const sev = SEV_TO_RULE[f.severity] ? f.severity : 'minor';
    const title = String(f.title || 'finding').slice(0, TITLE_MAX);
    const result = {
      ruleId: SEV_TO_RULE[sev],
      level: SEV_TO_SARIF_LEVEL[sev],
      message: { text: title.slice(0, SARIF_MESSAGE_MAX) },
      locations: [sarifLocation(f)],
      properties: {
        model: f.model || undefined,
        confidence: f.confidence,
        consensusBlock: f.consensusBlock || false,
        prescan: f.prescan || undefined,
      },
    };
    return result;
  });

  const run = {
    tool: {
      driver: {
        name: tool,
        version: TOOL_VERSION,
        informationUri: profile.repositoryUri || repositoryUri || undefined,
        rules: [...rules.values()],
      },
    },
    results,
    automationDetails: {
      description: {
        text: `AI code review PR #${payload.pr?.number ?? '?'}`,
      },
    },
  };
  if (repositoryUri) {
    run.automationDetails.guid = sarifAutomationGuid(repositoryUri, tool);
  }

  return {
    $schema: SARIF_SCHEMA,
    version: '2.1.0',
    runs: [run],
  };
}

export async function writeReviewExports(cfg, payload, log = () => {}, opts = {}) {
  const exp = cfg.export;
  if (!exp?.enabled) return null;

  const tool = toolName();
  const dir = resolve(process.cwd(), exp.dir || getProfile().paths.exportDir);
  await mkdir(dir, { recursive: true });

  const repositoryUri = opts.repositoryUri
    || (cfg.github?.repo ? `https://github.com/${cfg.github.repo}` : undefined);
  const sarifAll = exp.sarifAll === true;
  const sarifFindings = selectSarifFindings(payload, sarifAll);

  const written = [];
  if (exp.json) {
    const jsonPath = join(dir, `${tool}.json`);
    await writeFile(jsonPath, `${JSON.stringify(payload, null, 2)}\n`, 'utf8');
    written.push(jsonPath);
    log(`export R9: JSON → ${jsonPath}`);
    if (payload.swarm) {
      const swarmPath = join(dir, `${tool}-swarm.json`);
      await writeFile(swarmPath, `${JSON.stringify(payload.swarm, null, 2)}\n`, 'utf8');
      written.push(swarmPath);
      log(`export E22: swarm → ${swarmPath}`);
    }
  }
  if (exp.sarif) {
    const sarifPath = join(dir, `${tool}.sarif.json`);
    const sarif = buildSarif(payload, { repositoryUri, sarifAll });
    await writeFile(sarifPath, `${JSON.stringify(sarif, null, 2)}\n`, 'utf8');
    written.push(sarifPath);
    log(`export R9: SARIF → ${sarifPath} (${sarifFindings.length} results${sarifAll ? ', all' : ', blockers'})`);
  }
  return { dir, written, sarifResults: sarifFindings.length };
}
