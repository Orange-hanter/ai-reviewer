import { execFile as execFileCb } from 'node:child_process';
import { readFile as readFileDefault } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { promisify } from 'node:util';
import { createEvidenceReceipt, normalizeRepoPath, sha256 } from './structure-evidence.mjs';
import {
  factsFingerprint, STRUCTURE_FACTS_SCHEMA, validateObligationsExport,
  validateStructureAssessment, validateStructureFacts,
} from './structure-protocol.mjs';
import { buildStructureViews, extractGraphifyFacts } from './structure-graphify.mjs';
import { extractScipFacts, mergeScipWithGraphify, readScipIndex } from './structure-scip.mjs';
import { lcovObservations, parseLcov } from './structure-lcov.mjs';
import {
  extractTestCasesFromSource, generatePropertyDrafts, mergeTestCases, parsePropertyReport,
} from './structure-tests.mjs';
import { criticalContractRefs, parseMutationReport } from './structure-mutation.mjs';
import { assessDependencyRisk, buildStructureAssessment } from './structure-assessments.mjs';
import { reconcileIdentity } from './structure-identity.mjs';
import {
  applyStructureWaivers, computeStructureDrift, suggestObligationLinks, triageStructureFindings,
} from './structure-drift.mjs';

const execFileP = promisify(execFileCb);
export const STRUCTURE_TOOL_VERSION = '1.0.0';

async function optionalText(path, readFile) {
  if (!path) return null;
  try { return await readFile(path, 'utf8'); } catch { return null; }
}

async function optionalJson(path, readFile) {
  const text = await optionalText(path, readFile);
  if (text == null) return null;
  return JSON.parse(text);
}

function graphStatus(report, headSha) {
  if (!report) return 'unknown';
  const built = String(report).match(/Built from commit:\s*`?([0-9a-f]{7,64})`?/i)?.[1];
  if (!built) return 'unknown';
  return headSha.startsWith(built) || built.startsWith(headSha) ? 'fresh' : 'stale';
}

async function gitValue(exec, cwd, args, fallback) {
  try {
    const { stdout } = await exec('git', ['-C', cwd, ...args], { timeout: 5000, maxBuffer: 1024 * 1024 });
    return String(stdout).trim() || fallback;
  } catch {
    return fallback;
  }
}

function contractsWithLabels(facts) {
  const entities = new Map((facts.entities || []).map((entity) => [entity.observedId, entity]));
  return (facts.contracts || []).map((contract) => ({
    ...contract,
    label: entities.get(contract.entityRef)?.label || '',
  }));
}

async function enrichTestsFromSources(facts, repoRoot, readFile) {
  const testPaths = [...new Set([
    ...(facts.testCases || []).map((testCase) => testCase.path),
    ...(facts.entities || [])
      .filter((entity) => /(?:^|\/)(?:test|tests|__tests__|e2e)(?:\/|$)|\.(?:test|spec)\.[^.]+$/i.test(entity.path))
      .map((entity) => entity.path),
  ])].sort();
  const scanned = [];
  const contracts = contractsWithLabels(facts);
  for (const path of testPaths.slice(0, 1000)) {
    try {
      const source = await readFile(resolve(repoRoot, normalizeRepoPath(path)), 'utf8');
      scanned.push(...extractTestCasesFromSource(path, source, contracts));
    } catch { /* graph-only test case remains available */ }
  }
  return mergeTestCases(facts.testCases || [], scanned);
}

function applyBoundaries(facts) {
  const boundaries = facts.boundaries || [];
  const fallback = boundaries.find((item) => item.kind === 'repository')?.id || 'boundary:repository';
  const entities = facts.entities.map((entity) => ({ ...entity, boundaryId: entity.boundaryId || fallback }));
  const byId = new Map(entities.map((entity) => [entity.observedId, entity]));
  const dependencies = facts.dependencies.map((edge) => {
    const from = byId.get(edge.from);
    const to = byId.get(edge.to);
    return {
      ...edge,
      crossBoundary: Boolean(from && to && from.boundaryId !== to.boundaryId),
      crossCommunity: edge.crossCommunity || Boolean(
        from?.community != null && to?.community != null && from.community !== to.community
      ),
    };
  });
  return { ...facts, entities, dependencies };
}

function artifactReceipt({
  provider, toolVersion, capability, headSha, configHash, scopeHash,
  uri, contents, observation, trustTier, evidenceCeiling, limitations, generatedAt,
}) {
  return createEvidenceReceipt({
    provider,
    toolVersion,
    capability,
    headSha,
    configHash,
    scopeHash,
    artifact: { uri, sha256: sha256(contents) },
    observation,
    trustTier,
    evidenceCeiling,
    limitations,
    observedAt: generatedAt,
  });
}

export async function analyzeStructure(options = {}, deps = {}) {
  const readFile = deps.readFile || readFileDefault;
  const exec = deps.execFileP || execFileP;
  const repoRoot = resolve(options.repoRoot || process.cwd());
  const generatedAt = options.generatedAt || new Date().toISOString();
  const headSha = String(options.headSha
    || await gitValue(exec, repoRoot, ['rev-parse', 'HEAD'], '0000000'));
  const branchRef = String(options.branchRef
    || await gitValue(exec, repoRoot, ['symbolic-ref', '--short', 'HEAD'], 'unknown'));
  const scope = options.scope ? normalizeRepoPath(options.scope) : '';
  const configInput = {
    scope,
    includeAmbiguous: options.includeAmbiguous === true,
    layout: options.layout || {},
    maxEntities: options.maxEntities || 5000,
    maxEdges: options.maxEdges || 15000,
  };
  const configHash = sha256(configInput);
  const scopeHash = sha256(scope || '.');
  const trustTier = options.trustTier || 'trusted_local';
  const warnings = [];
  const receipts = [];

  const graphText = options.graphText ?? await optionalText(options.graphPath, readFile);
  const graph = graphText ? JSON.parse(graphText) : { nodes: [], links: [] };
  const graphReport = options.graphReportText ?? await optionalText(options.graphReportPath, readFile);
  const analysis = options.graphAnalysis
    || await optionalJson(options.graphAnalysisPath
      || (options.graphPath ? join(dirname(options.graphPath), '.graphify_analysis.json') : ''), readFile);
  const currentGraphStatus = graphText ? graphStatus(graphReport, headSha) : 'missing';
  let extracted = extractGraphifyFacts(graph, {
    analysis,
    layout: options.layout || {},
    scope,
    includeAmbiguous: options.includeAmbiguous,
    maxEntities: configInput.maxEntities,
    maxEdges: configInput.maxEdges,
  });
  warnings.push(...extracted.warnings);
  if (graphText) {
    receipts.push(artifactReceipt({
      provider: 'graphify',
      toolVersion: options.graphifyVersion || 'unknown',
      capability: 'topology',
      headSha,
      configHash,
      scopeHash,
      uri: options.graphPath || 'memory:graph.json',
      contents: graphText,
      observation: { type: 'graph_inventory', value: { nodes: graph.nodes?.length || 0, edges: (graph.links || graph.edges || []).length } },
      trustTier,
      evidenceCeiling: 'static_inventory',
      limitations: ['symbol kinds and source ranges may be heuristic'],
      generatedAt,
    }));
  } else warnings.push('graph unavailable');

  const scipPath = options.scipPath;
  if (scipPath || options.scipIndex) {
    try {
      const index = options.scipIndex || await readScipIndex(scipPath);
      const scip = extractScipFacts(index, {
        scope,
        maxEntities: configInput.maxEntities,
        maxEdges: configInput.maxEdges,
      });
      extracted = mergeScipWithGraphify(scip, extracted);
      const scipContents = options.scipBuffer || (scipPath ? await readFile(scipPath) : Buffer.from(JSON.stringify(index)));
      receipts.push(artifactReceipt({
        provider: 'scip-typescript',
        toolVersion: options.scipVersion || '0.4.0',
        capability: 'semantic_symbols',
        headSha,
        configHash,
        scopeHash,
        uri: scipPath || 'memory:index.scip',
        contents: scipContents,
        observation: { type: 'symbol_inventory', value: { entities: scip.entities.length, references: scip.dependencies.length } },
        trustTier,
        evidenceCeiling: 'static_inventory',
        limitations: ['SCIP relationships are references, not runtime calls'],
        generatedAt,
      }));
    } catch (error) {
      warnings.push(`SCIP unavailable: ${error.message}`);
    }
  }

  extracted = applyBoundaries(extracted);
  extracted.testCases = await enrichTestsFromSources(extracted, repoRoot, readFile);
  const previous = options.previousFacts?.facts?.entities || [];
  const identity = reconcileIdentity(extracted.entities, previous);
  extracted.entities = identity.entities;
  const facts = {
    boundaries: extracted.boundaries,
    entities: extracted.entities,
    contracts: extracted.contracts,
    dependencies: extracted.dependencies,
    testCases: extracted.testCases,
  };
  const providerVersion = [
    `graphify:${options.graphifyVersion || 'unknown'}`,
    scipPath || options.scipIndex ? `scip:${options.scipVersion || '0.4.0'}` : 'scip:none',
  ].join('|');
  const fingerprint = factsFingerprint({ headSha, providerVersion, configHash, scope });
  const factsPayload = {
    schemaRef: STRUCTURE_FACTS_SCHEMA,
    version: 1,
    kind: 'structure_facts',
    generatedAt,
    fingerprint,
    provenance: {
      headSha,
      branchRef,
      prNumber: options.prNumber || null,
      isDefaultBranch: options.isDefaultBranch === true,
      toolVersion: STRUCTURE_TOOL_VERSION,
      configHash,
      scope,
      graphStatus: currentGraphStatus,
      providerCapabilities: extracted.capabilities,
      warnings: [...new Set(warnings)].sort(),
      truncated: extracted.truncated === true,
    },
    facts,
    identityEvents: identity.events,
    views: buildStructureViews(facts),
    receipts,
  };
  validateStructureFacts(factsPayload);

  const obligationsExport = options.obligationsExport
    || await optionalJson(options.obligationsPath, readFile)
    || {
      schemaRef: 'code-structure/obligations-export.v1',
      version: 1,
      kind: 'obligations_export',
      project: options.project || 'unknown',
      revision: 'none',
      headSha: null,
      exportedAt: generatedAt,
      obligations: [],
    };
  validateObligationsExport(obligationsExport);

  const lcovText = options.lcovText ?? await optionalText(options.lcovPath, readFile);
  const lcov = lcovText ? parseLcov(lcovText, { repoRoot }) : null;
  const entityByPath = new Map();
  for (const entity of facts.entities) {
    if (!entityByPath.has(entity.path)) entityByPath.set(entity.path, []);
    entityByPath.get(entity.path).push(entity);
  }
  const coverageObservations = lcov ? lcovObservations(lcov, entityByPath) : [];
  const assessmentReceipts = [];
  if (lcovText) {
    assessmentReceipts.push(artifactReceipt({
      provider: 'lcov',
      toolVersion: options.lcovVersion || '1',
      capability: 'aggregate_coverage',
      headSha: options.testHeadSha || headSha,
      configHash,
      scopeHash,
      uri: options.lcovPath || 'memory:lcov.info',
      contents: lcovText,
      observation: { type: 'coverage_inventory', value: lcov.totals },
      trustTier,
      evidenceCeiling: 'aggregate_lcov',
      limitations: ['aggregate LCOV cannot prove which test executed a function'],
      generatedAt,
    }));
  }

  const propertyDrafts = generatePropertyDrafts(obligationsExport.obligations);
  const propertyReport = options.propertyReport || await optionalJson(options.propertyReportPath, readFile);
  const propertyResults = propertyReport ? parsePropertyReport(propertyReport, propertyDrafts) : [];
  const preliminaryRisk = assessDependencyRisk(facts, coverageObservations, options.thresholds);
  const mutationReport = options.mutationReport || await optionalJson(options.mutationReportPath, readFile);
  const mutation = mutationReport
    ? parseMutationReport(mutationReport, facts, {
        scopeContractRefs: criticalContractRefs(facts, preliminaryRisk, obligationsExport.obligations),
      })
    : { observations: [], warnings: [], summary: { total: 0, killed: 0, survived: 0, noCoverage: 0, timeout: 0 } };
  if (mutationReport) {
    assessmentReceipts.push(artifactReceipt({
      provider: 'mutation-testing-elements',
      toolVersion: String(mutationReport.schemaVersion || options.mutationVersion || 'unknown'),
      capability: 'mutation_testing',
      headSha: options.testHeadSha || headSha,
      configHash,
      scopeHash,
      uri: options.mutationReportPath || 'memory:mutation.json',
      contents: JSON.stringify(mutationReport),
      observation: { type: 'mutation_inventory', value: mutation.summary },
      trustTier,
      evidenceCeiling: 'mutation_bound',
      limitations: ['equivalent mutants may survive without indicating a test defect'],
      generatedAt,
    }));
  }

  const assessmentPayload = buildStructureAssessment({
    factsPayload,
    obligationsExport,
    lcov,
    coverageObservations,
    propertyResults,
    mutation,
    receipts: assessmentReceipts,
    testRun: {
      id: options.testRunId,
      headSha: options.testHeadSha || headSha,
      producedAt: options.testProducedAt || generatedAt,
    },
    thresholds: options.thresholds || {},
    generatedAt,
  });
  validateStructureAssessment(assessmentPayload);
  const linkSuggestions = suggestObligationLinks(obligationsExport.obligations, facts);
  const drift = computeStructureDrift({
    facts: factsPayload,
    assessment: assessmentPayload,
    obligationsExport,
    confirmedClaims: options.confirmedClaims || [],
    linkSuggestions,
  });
  drift.findings = applyStructureWaivers(drift.findings, options.waivers || []);
  return {
    facts: factsPayload,
    assessment: assessmentPayload,
    propertyDrafts,
    drift,
    triage: triageStructureFindings(drift.findings),
    diagnostics: {
      warnings: [...new Set([...warnings, ...(lcov?.warnings || []), ...mutation.warnings])].sort(),
      scipUsed: Boolean(scipPath || options.scipIndex) && receipts.some((item) => item.provider === 'scip-typescript'),
      evidenceCeiling: lcov ? 'aggregate_lcov' : 'static_inventory',
    },
  };
}
