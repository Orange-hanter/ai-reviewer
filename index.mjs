// Канонические re-export API @orakul/ai-reviewer.

export { parseDiff, extractChangedSymbols, splitSegmentByHunks, buildChunks, readChangedFiles } from './lib/diff.mjs';
export { extractJsonObject } from './lib/json.mjs';
export { filterFalsePositiveFindings, matchesKnownFalsePositive, clusterFindings, normalizeFinding, normalizeConfidence, resolveBlockers, applyPrescanBlockPolicy } from './lib/findings.mjs';
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
export { runEvalHarness, loadFixtures, runFixtureCase, formatEvalReport, DEFAULT_FIXTURES_DIR } from './lib/eval.mjs';
export {
  collectFeedbackEntries, parseReviewCommentFinding, parseReviewCommentFindings,
  parseStickyFindings, isTrustedReviewAuthor, parseTrustedAuthors,
  dedupeFeedbackEntries, buildFixtureCandidate, harvestFeedback, formatHarvestReport,
  DEFAULT_FP_LABEL, DEFAULT_TRUSTED_AUTHORS,
} from './lib/feedback.mjs';
export { buildMoAReferenceSection } from './lib/prompt.mjs';
export { loadReviewConfig, parseCli } from './lib/config.mjs';
export { createReviewEngine } from './lib/engine.mjs';
export {
  buildReviewJson, buildSarif, writeReviewExports, selectSarifFindings,
  extractReasoningEntries, sarifArtifactUri, sarifAutomationGuid,
  EXPORT_VERSION, SARIF_SCHEMA, SARIF_GUID_RE, TOOL_NAME,
} from './lib/export.mjs';
export { buildSummary, buildRouterSummary, buildReasoningSection, buildStickyReasoningFooter } from './lib/report.mjs';
export { decideAutoTriage, buildTriageContext, runAutoTriage, findingLocationKey as triageLocationKey } from './lib/triage.mjs';
export { routeDiff, classifyFilePath, classifyChangedFile, extractFileDiffSections, isRenameOnlySection, formatRouteLog } from './lib/router.mjs';
export { createReviewPipeline, runReview } from './pipeline.mjs';
