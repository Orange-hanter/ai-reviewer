// Канонический публичный API движка (import 'ai-reviewer' / 'ai-reviewer/lib/*').

export { parseDiff, extractChangedSymbols, collectBlastSymbols, splitSegmentByHunks, buildChunks, readChangedFiles, parseDiffNewPath, parseDiffGitPath, unescapeGitPath } from './lib/diff.mjs';
export { extractJsonObject } from './lib/json.mjs';
export { filterFalsePositiveFindings, matchesKnownFalsePositive, clusterFindings, normalizeFinding, normalizeConfidence, resolveBlockers, applyPrescanBlockPolicy, filterStalePriorFindings, filterFindingsFixedInDiff, dedupeSameModelFindings, findingFingerprint } from './lib/findings.mjs';
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
export { scoreGoldCase, aggregateBenchmark, scoreFromRecord } from './lib/benchmark.mjs';
export { emitBlastRadiusFindings, parseBlastDependentPaths } from './lib/blast-findings.mjs';
export {
  buildReviewJson, buildSarif, writeReviewExports, selectSarifFindings,
  extractReasoningEntries, sarifArtifactUri, sarifAutomationGuid,
  EXPORT_VERSION, SARIF_SCHEMA, SARIF_GUID_RE, TOOL_VERSION, toolName,
} from './lib/export.mjs';
export { buildSummary, buildRouterSummary, buildLlmUnavailableSummary, buildReasoningSection, buildStickyReasoningFooter, diffFindings, findingIdentity, findingPointDelta, findingsStability, verdictReproducibility } from './lib/report.mjs';
export { MARKERS, hasMarker, extractStickyFooterRaw, repoRules, SEV } from './lib/constants.mjs';
export {
  DEFAULT_PROFILE, PROFILE_FILENAMES, createMarkers, normalizeProfile, loadProfileFile,
  getProfile, setProfile, resetProfile, resolveProfile,
} from './lib/profile.mjs';
export { publicErrorLabel, createAdaptiveSemaphore } from './lib/util.mjs';
export { parseJsonl, aggregateMetrics, formatMetricsReport } from './lib/metrics.mjs';
export { createGraphifyClient } from './lib/graphify.mjs';
export { decideAutoTriage, buildTriageContext, runAutoTriage, findingLocationKey as triageLocationKey } from './lib/triage.mjs';
export { routeDiff, classifyFilePath, classifyChangedFile, extractFileDiffSections, isRenameOnlySection, formatRouteLog } from './lib/router.mjs';
export { createReviewPipeline, runGateResolution, runReview } from './pipeline.mjs';
