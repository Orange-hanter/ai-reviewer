#!/usr/bin/env node
import { access } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { analyzeStructure } from '../lib/structure.mjs';
import { writeStructureExports } from '../lib/structure-export.mjs';
import { loadReviewConfig } from '../lib/config.mjs';

function parseArgs(argv) {
  const value = (name) => {
    const index = argv.indexOf(name);
    return index >= 0 ? argv[index + 1] : undefined;
  };
  return {
    help: argv.includes('--help') || argv.includes('-h'),
    graphPath: value('--graph'),
    graphReportPath: value('--graph-report'),
    graphAnalysisPath: value('--graph-analysis'),
    scipPath: value('--scip'),
    lcovPath: value('--lcov'),
    obligationsPath: value('--obligations'),
    mutationReportPath: value('--mutation-report'),
    propertyReportPath: value('--property-report'),
    previousFactsPath: value('--previous-facts'),
    repoRoot: value('--repo-root'),
    scope: value('--scope'),
    headSha: value('--head-sha'),
    branchRef: value('--branch'),
    testHeadSha: value('--test-head-sha'),
    testRunId: value('--test-run-id'),
    factsPath: value('--facts-out'),
    assessmentPath: value('--assessment-out'),
    propertyDraftsPath: value('--property-drafts-out'),
    driftPath: value('--drift-out'),
    markdownPath: value('--markdown'),
    trustTier: value('--trust-tier'),
    includeAmbiguous: argv.includes('--include-ambiguous'),
    json: argv.includes('--json'),
  };
}

function usage() {
  return `Usage: pr-review-structure [options]

Inputs:
  --graph <graph.json>             Graphify node-link graph
  --graph-report <report.md>       Graphify freshness report
  --scip <index.scip>              SCIP semantic index (TS/JS pilot)
  --lcov <lcov.info>               Aggregate LCOV evidence
  --obligations <obligations.json> cod-doc obligations_export.v1
  --mutation-report <report.json>  Mutation Testing Elements/Stryker report
  --property-report <report.json>  Confirmed property-run evidence
  --previous-facts <facts.json>    Prior snapshot for identity lineage

Scope and provenance:
  --repo-root <path> --scope <prefix> --head-sha <sha> --branch <ref>
  --test-head-sha <sha> --test-run-id <id> --trust-tier <tier>

Outputs:
  --facts-out <facts.json> --assessment-out <assessment.json>
  --property-drafts-out <drafts.json> --drift-out <drift.json>
  --markdown <summary.md> --json
`;
}

async function readable(path) {
  if (!path) return false;
  try { await access(path); return true; } catch { return false; }
}

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  console.log(usage());
  process.exit(0);
}

const cfg = loadReviewConfig();
const structure = cfg.structure;
const repoRoot = resolve(args.repoRoot || process.cwd());
const outputDir = resolve(repoRoot, structure.exportDir);
const graphPath = resolve(repoRoot, args.graphPath || structure.graph);
const graphReportPath = resolve(repoRoot, args.graphReportPath || structure.graphReport);
const graphAnalysisPath = resolve(repoRoot, args.graphAnalysisPath || structure.graphAnalysis);
const scipPath = args.scipPath || structure.scip ? resolve(repoRoot, args.scipPath || structure.scip) : '';
const lcovPath = args.lcovPath || structure.lcov ? resolve(repoRoot, args.lcovPath || structure.lcov) : '';
const obligationsPath = args.obligationsPath || structure.obligations
  ? resolve(repoRoot, args.obligationsPath || structure.obligations) : '';
const mutationReportPath = args.mutationReportPath || structure.mutationReport
  ? resolve(repoRoot, args.mutationReportPath || structure.mutationReport) : '';
const propertyReportPath = args.propertyReportPath || structure.propertyReport
  ? resolve(repoRoot, args.propertyReportPath || structure.propertyReport) : '';

try {
  if (!await readable(graphPath) && !await readable(scipPath) && !await readable(lcovPath)) {
    throw new Error('no readable structure source: provide --graph, --scip or --lcov');
  }
  const previousFacts = args.previousFactsPath
    ? JSON.parse(await (await import('node:fs/promises')).readFile(resolve(repoRoot, args.previousFactsPath), 'utf8'))
    : null;
  const result = await analyzeStructure({
    repoRoot,
    graphPath,
    graphReportPath,
    graphAnalysisPath,
    scipPath: await readable(scipPath) ? scipPath : '',
    lcovPath: await readable(lcovPath) ? lcovPath : '',
    obligationsPath: await readable(obligationsPath) ? obligationsPath : '',
    mutationReportPath: await readable(mutationReportPath) ? mutationReportPath : '',
    propertyReportPath: await readable(propertyReportPath) ? propertyReportPath : '',
    previousFacts,
    scope: args.scope ?? structure.scope,
    headSha: args.headSha,
    branchRef: args.branchRef,
    testHeadSha: args.testHeadSha,
    testRunId: args.testRunId,
    trustTier: args.trustTier || structure.trustTier,
    includeAmbiguous: args.includeAmbiguous || structure.includeAmbiguous,
    maxEntities: structure.maxEntities,
    maxEdges: structure.maxEdges,
    thresholds: structure.thresholds,
    layout: cfg.profile.layout,
  });
  const written = await writeStructureExports(result, {
    factsPath: resolve(repoRoot, args.factsPath || join(outputDir, 'structure-facts.json')),
    assessmentPath: resolve(repoRoot, args.assessmentPath || join(outputDir, 'structure-assessment.json')),
    propertyDraftsPath: resolve(repoRoot, args.propertyDraftsPath || join(outputDir, 'property-drafts.json')),
    driftPath: resolve(repoRoot, args.driftPath || join(outputDir, 'structure-drift.json')),
    markdownPath: resolve(repoRoot, args.markdownPath || join(outputDir, 'code-structure.md')),
    owner: cfg.profile.toolName,
  });
  const summary = {
    factsFingerprint: result.facts.fingerprint,
    assessmentFingerprint: result.assessment.fingerprint,
    entities: result.facts.facts.entities.length,
    dependencies: result.facts.facts.dependencies.length,
    scenarios: result.assessment.assessments.contractScenarios.length,
    hints: result.assessment.hints.length,
    diagnostics: result.diagnostics,
    written,
  };
  if (args.json) console.log(JSON.stringify(summary, null, 2));
  else {
    console.log(`[pr-review.structure] ${summary.entities} entities · ${summary.dependencies} dependencies · ${summary.scenarios} scenarios · ${summary.hints} hints`);
    for (const path of written) console.log(`[pr-review.structure] wrote ${path}`);
    for (const warning of result.diagnostics.warnings) console.error(`[pr-review.structure] warning: ${warning}`);
  }
} catch (error) {
  console.error(`[pr-review.structure] ${error.message}`);
  process.exitCode = /Usage:|unknown option/.test(error.message) ? 2 : 1;
}
