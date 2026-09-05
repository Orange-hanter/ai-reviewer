// Канонический публичный API движка (import 'ai-reviewer' / 'ai-reviewer/lib/*').

export { parseDiff, extractChangedSymbols, collectBlastSymbols, splitSegmentByHunks, buildChunks, readChangedFiles, collectAddedPaths, parseDiffNewPath, parseDiffGitPath, unescapeGitPath } from './lib/diff.mjs';
export { extractJsonObject } from './lib/json.mjs';
export { windowFileEntry, buildVerifierFileContents, windowChunkFileContents } from './lib/context-window.mjs';
export { filterFalsePositiveFindings, matchesKnownFalsePositive, clusterFindings, normalizeFinding, normalizeConfidence, resolveBlockers, applyPrescanBlockPolicy, filterStalePriorFindings, filterFindingsFixedInDiff, dedupeSameModelFindings, findingFingerprint, attachFingerprints } from './lib/findings.mjs';
export { selectVerifierCandidates, applyVerifierVerdicts, createVerifierRunner } from './lib/verifier.mjs';
export {
  parseSpecialistAssignments, applyRefutation, decideRefutation, normalizeAdvisorClaims,
  mergeSpecialistReferences, SPECIALIST_ROLES, formatRefutationLog,
} from './lib/specialists.mjs';
export {
  createObservability, slimFindings, countBySeverity,
} from './lib/observability.mjs';
export {
  eslintJsonToFindings, dedupeAgainstPrescan, selectLintablePaths, selectLintableEntries,
  normalizeAppPath, buildPrescanPathMap, resolvePrescanDiffKey, eslintAbsToAppPath,
  buildPrescanPromptSection, runEslintPrescan, runCombinedPrescan, createPrescanRunner, formatPrescanLog,
  mapEslintRuleSeverity, findingLocationKey,
} from './lib/prescan.mjs';
export {
  astgrepStreamToFindings, mapAstgrepRuleSeverity, filterAstgrepFindings,
  runAstgrepPrescan, DEFAULT_ASTGREP_BLOCK_RULES, astgrepPathToAppPath,
} from './lib/astgrep.mjs';
export { runEvalHarness, loadFixtures, runFixtureCase, formatEvalReport } from './lib/eval.mjs';
export {
  collectFeedbackEntries, parseReviewCommentFinding, parseReviewCommentFindings,
  parseStickyFindings, isTrustedReviewAuthor, parseTrustedAuthors,
  dedupeFeedbackEntries, buildFixtureCandidate, harvestFeedback, formatHarvestReport,
  DEFAULT_FP_LABEL, DEFAULT_TRUSTED_AUTHORS,
} from './lib/feedback.mjs';
export { buildMoAReferenceSection, buildChangedFilesSection, createPromptBuilder } from './lib/prompt.mjs';
export { loadReviewConfig, parseCli } from './lib/config.mjs';
export { createReviewEngine } from './lib/engine.mjs';
export { createLlmClient } from './lib/llm.mjs';
export { createOllamaClient } from './lib/ollama.mjs';
export { createCostTracker, estimateCost, formatCostStickyLine, loadPriceMap } from './lib/cost.mjs';
export {
  scoreGoldCase, aggregateBenchmark, scoreFromRecord, locationMatches, DEFAULT_LOCATION_TOLERANCE,
} from './lib/benchmark.mjs';
export { emitBlastRadiusFindings, parseBlastDependentPaths, parseBlastDependents } from './lib/blast-findings.mjs';
export {
  buildReviewJson, buildSarif, writeReviewExports, selectSarifFindings,
  extractReasoningEntries, sarifArtifactUri, sarifAutomationGuid,
  EXPORT_VERSION, SARIF_SCHEMA, SARIF_GUID_RE, TOOL_VERSION, toolName,
} from './lib/export.mjs';
export {
  buildSwarmVerdict, isStrongBlocker, suggestedScopeFiles, SWARM_CONTRACT_VERSION,
} from './lib/swarm.mjs';
export { buildSummary, buildRouterSummary, buildLlmUnavailableSummary, buildReasoningSection, buildStickyReasoningFooter, buildWalkthroughSection, diffFindings, findingIdentity, findingPointDelta, findingsStability, verdictReproducibility } from './lib/report.mjs';
export { MARKERS, hasMarker, extractStickyFooterRaw, repoRules, SEV, BLAST_FINDING_SOURCE } from './lib/constants.mjs';
export {
  DEFAULT_PROFILE, PROFILE_FILENAMES, createMarkers, normalizeProfile, loadProfileFile,
  getProfile, setProfile, resetProfile, resolveProfile,
} from './lib/profile.mjs';
export { publicErrorLabel, createAdaptiveSemaphore, redactSecretsForLog } from './lib/util.mjs';
export { parseJsonl, aggregateMetrics, formatMetricsReport } from './lib/metrics.mjs';
export { createGraphifyClient } from './lib/graphify.mjs';
export {
  analyzeStructure, STRUCTURE_TOOL_VERSION,
} from './lib/structure.mjs';
export {
  EVIDENCE_RECEIPT_VERSION, TRUST_TIERS, EVIDENCE_CEILINGS,
  // generic-хелперы живут в lib/util.mjs, structure-evidence их реэкспортирует для совместимости
  canonicalJson, sha256, stableId, normalizeRepoPath,
  validateEvidenceReceipt, createEvidenceReceipt, receiptRef,
} from './lib/structure-evidence.mjs';
export {
  STRUCTURE_PROTOCOL_VERSION, STRUCTURE_FACTS_VERSION, STRUCTURE_FACTS_SCHEMA,
  STRUCTURE_ASSESSMENT_SCHEMA, OBLIGATIONS_SCHEMA, OBLIGATIONS_EXPORT_SCHEMA,
  DEFAULT_STRUCTURE_LIMITS, assertPayloadSize,
  validateStructureFacts, validateStructureAssessment, validateObligationsExport,
  parseStructurePayload, factsFingerprint, assessmentFingerprint, temporalAlignment,
} from './lib/structure-protocol.mjs';
export { parseLcov, lcovObservations, matchLcovFunction } from './lib/structure-lcov.mjs';
export { extractGraphifyFacts, buildStructureViews } from './lib/structure-graphify.mjs';
export {
  decodeScipIndex, readScipIndex, extractScipFacts, mergeScipWithGraphify,
} from './lib/structure-scip.mjs';
export {
  extractTestCasesFromSource, mergeTestCases, generatePropertyDrafts, parsePropertyReport,
} from './lib/structure-tests.mjs';
export { parseMutationReport, criticalContractRefs } from './lib/structure-mutation.mjs';
export {
  assessDependencyRisk, assessDataQuality, assessContractScenarios,
  buildStructureHints, buildStructureAssessment,
} from './lib/structure-assessments.mjs';
export { reconcileIdentity } from './lib/structure-identity.mjs';
export { buildStructureContext } from './lib/structure-context.mjs';
export {
  STRUCTURE_CONTEXT_TOOL, callStructureContext, handleStructureMcpRequest,
} from './lib/structure-mcp.mjs';
export { buildStructureMarkdown, writeStructureExports } from './lib/structure-export.mjs';
export {
  suggestObligationLinks, computeStructureDrift, reconcileStructureFindings,
  applyStructureWaivers, triageStructureFindings,
} from './lib/structure-drift.mjs';
export { decideAutoTriage, buildTriageContext, runAutoTriage, findingLocationKey as triageLocationKey } from './lib/triage.mjs';
export { routeDiff, classifyFilePath, classifyChangedFile, extractFileDiffSections, isRenameOnlySection, formatRouteLog } from './lib/router.mjs';
export { createReviewPipeline, runGateResolution, runReview } from './pipeline.mjs';
