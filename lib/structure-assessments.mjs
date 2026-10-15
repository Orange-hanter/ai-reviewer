/**
 * Фаза 2, шаг 3: lib/structure-assessments.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/structure_assessments (порт детерминистического движка оценок:
 * assessDependencyRisk / assessDataQuality / assessContractScenarios /
 * buildStructureHints / buildStructureAssessment).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Синхронные
 * функции вызывают бинарь через execFileSync: JSON {"op", ...args} на stdin,
 * ответ — JSON в формате JS-возврата.
 *
 * Резолвинг бинаря (по аналогии с lib/structure-lcov.mjs):
 *   AI_REVIEWER_STRUCTURE_ASSESSMENTS_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/structure_assessments;
 *   AI_REVIEWER_STRUCTURE_ASSESSMENTS_FORCE_JS=1 — принудительно чистый JS.
 *
 * Если бинарь не существует (чистый клон до первого `cargo build`), обёртка
 * деградирует на чистый JS fallback ниже — эталонную копию исходной реализации.
 * Если бинарь ЕСТЬ, но упал (non-zero exit) — ошибка пробрасывается, fallback
 * не используется: молчаливая подмена реализации при битом Rust-коде скрывала бы
 * регрессии.
 *
 * Нюансы паритета (проверены parity-прогоном rust/parity/structure-assessments.parity.mjs):
 *   - canonicalJson/sha256 (coverageHash/thresholdsHash/fingerprint) — побайтовый
 *     паритет с lib/util.mjs, см. шапку lib/structure-evidence.mjs;
 *   - сортировки: localeCompare там, где localeCompare в JS (id сценариев/хинтов,
 *     obligationContentHashes, coverageObservations), дефолтный Array.sort по
 *     String(x) — UTF-16 code units (contractRefs, testCaseRefs, godNodes,
 *     evidence/missingEvidence) — astral-символы упорядочиваются иначе, чем
 *     побайтово;
 *   - ключи Map (entityRef/observedId/contract.id): скаляры по значению
 *     (числа через f64), объекты — по идентичности (не дедуплируются);
 *   - stableId-хинки: 'hint' + ruleId + raw subjectRefs через String(x ?? '');
 *   - testInventoryComplete — строгое `=== true`; complete/truncated receipt'ов
 *     не здесь, а в structure-evidence;
 *   - generatedAt по умолчанию (new Date().toISOString()) применяет обёртка —
 *     часы живут в JS, Rust получает готовое значение.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { assessmentFingerprint, STRUCTURE_ASSESSMENT_SCHEMA, temporalAlignment } from './structure-protocol.mjs';
import { sha256, stableId } from './structure-evidence.mjs';
import { defaultRustBin } from './rust-bin.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_ASSESSMENTS_BIN = defaultRustBin('structure_assessments');

function resolveAssessmentsBin() {
  if (process.env.AI_REVIEWER_STRUCTURE_ASSESSMENTS_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_STRUCTURE_ASSESSMENTS_BIN || '').trim() || DEFAULT_ASSESSMENTS_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). Ошибка бинаря пробрасывается. */
function runRust(op, args) {
  const bin = resolveAssessmentsBin();
  if (!bin) return null;
  try {
    const raw = execFileSync(bin, [], {
      input: JSON.stringify({ op, ...args }),
      encoding: 'utf8',
      maxBuffer: 128 * 1024 * 1024,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    return JSON.parse(raw);
  } catch (error) {
    const m = String(error?.stderr || '').match(/"error":\s*"((?:[^"\\]|\\.)*)"/);
    if (m) throw new Error(JSON.parse(`"${m[1]}"`));
    throw error;
  }
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

/** High fan-in хотспоты, crossBoundary-рёбра и god-ноды графа зависимостей. */
export function assessDependencyRisk(facts, coverageObservations, options = {}) {
  const r = runRust('assessDependencyRisk', { facts, coverageObservations, options });
  if (r !== null) return r;
  return assessDependencyRiskJs(facts, coverageObservations, options);
}

/** Полнота/свежесть инвентаря: join-rate function_execution и missing-допуск. */
export function assessDataQuality(facts, coverageObservations, options = {}) {
  const r = runRust('assessDataQuality', { facts, coverageObservations, options });
  if (r !== null) return r;
  return assessDataQualityJs(facts, coverageObservations, options);
}

/** Покрытие обязательств сценариями: статус/уверенность/доказательства. */
export function assessContractScenarios({
  facts,
  obligations,
  coverageObservations,
  propertyResults = [],
  alignment = 'unknown',
  dataQuality,
  evidenceCeiling = 'static_inventory',
}) {
  const r = runRust('assessContractScenarios', {
    facts,
    obligations,
    coverageObservations,
    propertyResults,
    alignment,
    dataQuality,
    evidenceCeiling,
  });
  if (r !== null) return r;
  return assessContractScenariosJs({
    facts,
    obligations,
    coverageObservations,
    propertyResults,
    alignment,
    dataQuality,
    evidenceCeiling,
  });
}

/** Промпт-хинки из data quality, scenario-гэпов, хотспотов и мутантов. */
export function buildStructureHints({
  facts,
  dependencyRisk,
  dataQuality,
  scenarios,
  mutationObservations = [],
}) {
  const r = runRust('buildStructureHints', {
    dependencyRisk,
    dataQuality,
    scenarios,
    mutationObservations,
  });
  if (r !== null) return r;
  return buildStructureHintsJs({ facts, dependencyRisk, dataQuality, scenarios, mutationObservations });
}

/** Итоговый structure_assessment-пейлоад (fingerprint, сценарии, хинки, receipts). */
export function buildStructureAssessment({
  factsPayload,
  obligationsExport,
  lcov,
  coverageObservations = [],
  propertyResults = [],
  mutation = { observations: [] },
  receipts = [],
  testRun = {},
  thresholds = {},
  generatedAt = new Date().toISOString(),
}) {
  const r = runRust('buildStructureAssessment', {
    factsPayload,
    obligationsExport,
    lcov,
    coverageObservations,
    propertyResults,
    mutation,
    receipts,
    testRun,
    thresholds,
    generatedAt,
  });
  if (r !== null) return r;
  return buildStructureAssessmentJs({
    factsPayload,
    obligationsExport,
    lcov,
    coverageObservations,
    propertyResults,
    mutation,
    receipts,
    testRun,
    thresholds,
    generatedAt,
  });
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/structure-assessments.mjs
// до Фазы 2). Используется только когда Rust-бинарь отсутствует; при битом бинаре
// НЕ подменяет его.
// ---------------------------------------------------------------------------

function ratioJs(part, total) {
  return total > 0 ? part / total : null;
}

function assessDependencyRiskJs(facts, coverageObservations, {
  highFanIn = 5,
} = {}) {
  const incoming = new Map();
  for (const edge of facts?.dependencies || []) {
    if (!incoming.has(edge.to)) incoming.set(edge.to, []);
    incoming.get(edge.to).push(edge);
  }
  const executionBySubject = new Map((coverageObservations || [])
    .filter((item) => item.level === 'function_execution')
    .map((item) => [item.subjectRef, item]));
  const contractsByEntity = new Map();
  for (const contract of facts?.contracts || []) {
    if (!contractsByEntity.has(contract.entityRef)) contractsByEntity.set(contract.entityRef, []);
    contractsByEntity.get(contract.entityRef).push(contract);
  }
  const hotspots = [];
  for (const entity of facts?.entities || []) {
    const edges = incoming.get(entity.observedId) || [];
    const execution = executionBySubject.get(entity.observedId);
    if (edges.length >= highFanIn) {
      hotspots.push({
        entityRef: entity.observedId,
        contractRefs: (contractsByEntity.get(entity.observedId) || []).map((item) => item.id).sort(),
        fanIn: edges.length,
        executionStatus: execution?.status || 'unavailable',
        crossBoundaryCallers: edges.filter((edge) => edge.crossBoundary).length,
        confidence: edges.every((edge) => ['exact', 'extracted'].includes(edge.confidence)) ? 'extracted' : 'inferred',
      });
    }
  }
  hotspots.sort((a, b) => b.fanIn - a.fanIn || a.entityRef.localeCompare(b.entityRef));
  return {
    highFanIn: hotspots,
    crossBoundaryEdges: (facts?.dependencies || []).filter((edge) => edge.crossBoundary).length,
    godNodes: (facts?.entities || []).filter((entity) => entity.isGodNode).map((entity) => entity.observedId).sort(),
  };
}

function assessDataQualityJs(facts, coverageObservations, {
  graphStatus = 'unknown',
  testInventoryComplete = false,
  truncated = false,
  missingThreshold = 0.1,
} = {}) {
  const functions = (coverageObservations || []).filter((item) => item.level === 'function_execution');
  const unresolved = functions.filter((item) => ['unresolved', 'ambiguous'].includes(item.confidence)).length;
  // ratio() отдаёт null при нуле наблюдений — это «нет данных», а не «джойны идеальны»
  const joinRate = ratioJs(unresolved, functions.length);
  const unresolvedJoinRate = joinRate ?? 0;
  return {
    graphStatus,
    truncated: Boolean(truncated),
    testInventoryComplete: Boolean(testInventoryComplete),
    functionObservations: functions.length,
    unresolvedFunctionJoins: unresolved,
    unresolvedJoinRate,
    // без единого function_execution судить о полноте нечего: иначе прогон без lcov
    // выдаёт «теста нет» с confidence extracted, то есть выдаёт незнание за наблюдение
    missingAllowed: graphStatus === 'fresh'
      && !truncated
      && testInventoryComplete
      && joinRate !== null
      && joinRate <= missingThreshold,
    entityCount: facts?.entities?.length || 0,
    edgeCount: facts?.dependencies?.length || 0,
  };
}

function scenarioAssertionKindsJs(kind) {
  switch (kind) {
    case 'error_path': return ['throws', 'rejects', 'http_status', 'property'];
    case 'boundary_value': return ['property', 'equality'];
    case 'invariant': return ['property'];
    case 'integration': return ['http_status', 'equality'];
    default: return ['equality', 'http_status', 'property'];
  }
}

function scenarioIdJs(obligation, contractRefs) {
  return stableId('scenario', obligation.id, obligation.kind, contractRefs.join(','));
}

function assessContractScenariosJs({
  facts,
  obligations,
  coverageObservations,
  propertyResults = [],
  alignment = 'unknown',
  dataQuality,
  evidenceCeiling = 'static_inventory',
}) {
  const contractById = new Map((facts?.contracts || []).map((contract) => [contract.id, contract]));
  const executionByEntity = new Map((coverageObservations || [])
    .filter((item) => item.level === 'function_execution')
    .map((item) => [item.subjectRef, item]));
  const tests = facts?.testCases || [];
  const propertiesByObligation = new Map(propertyResults.map((result) => [result.obligationRef, result]));
  const out = [];

  for (const obligation of obligations || []) {
    const contractRefs = [...new Set((obligation.contractRefs || []).filter((id) => contractById.has(id)))].sort();
    const resolution = contractRefs.length === 1 ? 'unique' : contractRefs.length > 1 ? 'ambiguous' : 'unresolved';
    const scenario = {
      id: scenarioIdJs(obligation, contractRefs),
      obligationRef: obligation.id,
      contractRefs,
      testCaseRefs: [],
      kind: obligation.kind,
      status: 'unverifiable',
      statusReason: 'insufficient evidence',
      contractResolution: resolution,
      confidence: 'unresolved',
      evidence: [`obligation:${obligation.id}:${obligation.contentHash}`],
      missingEvidence: [],
    };
    if (obligation.status !== 'confirmed') {
      scenario.statusReason = 'obligation is draft';
      scenario.missingEvidence.push('confirmed_obligation');
      out.push(scenario);
      continue;
    }
    if (alignment !== 'aligned') {
      scenario.statusReason = `temporal alignment is ${alignment}`;
      scenario.missingEvidence.push('temporal_alignment');
      out.push(scenario);
      continue;
    }
    if (resolution !== 'unique') {
      scenario.statusReason = `contract resolution is ${resolution}`;
      scenario.missingEvidence.push(resolution === 'ambiguous' ? 'unique_contract' : 'contract_link');
      out.push(scenario);
      continue;
    }
    const contract = contractById.get(contractRefs[0]);
    const matchingTests = tests.filter((testCase) => (testCase.targetContractRefs || []).includes(contract.id));
    const execution = executionByEntity.get(contract.entityRef);
    const acceptedAssertions = new Set(scenarioAssertionKindsJs(obligation.kind));
    const assertionTests = matchingTests.filter((testCase) => (
      (testCase.assertions || []).some((assertion) => acceptedAssertions.has(assertion.kind))
    ));
    const property = propertiesByObligation.get(obligation.id);
    scenario.testCaseRefs = matchingTests.map((testCase) => testCase.id).sort();
    if (matchingTests.length) scenario.evidence.push(...matchingTests.map((testCase) => `test:${testCase.id}`));
    if (execution) scenario.evidence.push(...execution.evidence);
    if (assertionTests.length) scenario.evidence.push(...assertionTests.map((testCase) => `assertion:${testCase.id}`));
    if (property?.status === 'passed') scenario.evidence.push(`property:${property.propertyId}:${property.seed || ''}`);

    const hasTest = matchingTests.length > 0;
    const executed = execution?.status === 'covered';
    const assertionBound = assertionTests.length > 0;
    const propertyBound = property?.status === 'passed' && property.confidence === 'exact';
    const cappedByAggregate = evidenceCeiling === 'aggregate_lcov'
      && ['error_path', 'boundary_value', 'invariant'].includes(obligation.kind);
    const covered = hasTest && executed
      && (obligation.kind === 'happy_path' ? assertionBound || propertyBound : assertionBound && propertyBound)
      && !cappedByAggregate;
    if (covered) {
      scenario.status = 'covered';
      scenario.statusReason = 'test link, execution and scenario-specific evidence satisfied';
      scenario.confidence = propertyBound ? 'exact' : 'extracted';
    } else if (hasTest || executed || assertionBound || propertyBound) {
      scenario.status = 'partial';
      scenario.statusReason = cappedByAggregate
        ? 'aggregate coverage cannot prove this edge case'
        : 'some evidence exists but scenario threshold is not met';
      scenario.confidence = hasTest && executed ? 'inferred' : 'heuristic';
      if (!hasTest) scenario.missingEvidence.push('linked_test');
      if (!executed) scenario.missingEvidence.push('function_execution');
      if (!assertionBound) scenario.missingEvidence.push('scenario_assertion');
      if (['error_path', 'boundary_value', 'invariant'].includes(obligation.kind) && !propertyBound) {
        scenario.missingEvidence.push('confirmed_property');
      }
      if (cappedByAggregate) scenario.missingEvidence.push('per_test_evidence');
    } else if (dataQuality?.missingAllowed) {
      scenario.status = 'missing';
      scenario.statusReason = 'fresh complete inventory contains no linked test evidence';
      scenario.confidence = 'extracted';
      scenario.missingEvidence.push('linked_test', 'function_execution');
    } else {
      scenario.statusReason = 'inventory quality does not allow a missing conclusion';
      scenario.missingEvidence.push('complete_test_inventory');
    }
    scenario.evidence = [...new Set(scenario.evidence)].sort();
    scenario.missingEvidence = [...new Set(scenario.missingEvidence)].sort();
    out.push(scenario);
  }
  return out.sort((a, b) => a.id.localeCompare(b.id));
}

function buildStructureHintsJs({
  facts,
  dependencyRisk,
  dataQuality,
  scenarios,
  mutationObservations = [],
}) {
  const hints = [];
  const add = (ruleId, subjectRefs, priority, remediationTarget, remediation, evidence, missingEvidence, confidence) => {
    hints.push({
      id: stableId('hint', ruleId, ...subjectRefs),
      ruleId,
      priority,
      subjectRefs: [...new Set(subjectRefs)].sort(),
      evidence: [...new Set(evidence || [])].sort(),
      missingEvidence: [...new Set(missingEvidence || [])].sort(),
      remediationTarget,
      remediation,
      promptSnippet: remediation,
      confidence,
    });
  };
  if (dataQuality.graphStatus !== 'fresh') {
    add('structure.graph_stale', ['structure:graph'], 'info', 'code',
      'Rebuild the Graphify graph at the pinned commit before strict conclusions.',
      [`graph-status:${dataQuality.graphStatus}`], ['fresh_graph'], 'exact');
  }
  for (const scenario of scenarios || []) {
    if (scenario.status === 'covered') continue;
    const priority = scenario.status === 'missing' ? 'high' : 'medium';
    add('scenario.must_obligation_gap', [scenario.obligationRef, ...scenario.contractRefs],
      priority, 'test', `Add or strengthen a focused ${scenario.kind} test for ${scenario.obligationRef}.`,
      scenario.evidence, scenario.missingEvidence, scenario.confidence);
  }
  for (const hotspot of dependencyRisk?.highFanIn || []) {
    if (hotspot.executionStatus === 'covered') continue;
    add('coverage.function_unexecuted_high_fan_in',
      [hotspot.entityRef, ...(hotspot.contractRefs || [])], 'high', 'test',
      `Add a regression test for the high fan-in entity (${hotspot.fanIn} incoming dependencies).`,
      [`fan-in:${hotspot.fanIn}`], ['function_execution'], hotspot.confidence);
  }
  for (const mutant of mutationObservations || []) {
    if (!['survived', 'no_coverage'].includes(mutant.mutationStatus)) continue;
    add('scenario.surviving_contract_mutant', [mutant.subjectRef], 'high', 'test',
      `Add an assertion that kills the ${mutant.mutator} mutant at ${mutant.path}:${mutant.line}.`,
      mutant.evidence, ['mutation_killed'], mutant.confidence);
  }
  return hints.sort((a, b) => a.id.localeCompare(b.id));
}

function buildStructureAssessmentJs({
  factsPayload,
  obligationsExport,
  lcov,
  coverageObservations = [],
  propertyResults = [],
  mutation = { observations: [] },
  receipts = [],
  testRun = {},
  thresholds = {},
  generatedAt = new Date().toISOString(),
}) {
  const headSha = factsPayload.provenance.headSha;
  const alignment = temporalAlignment({
    factsHeadSha: headSha,
    testHeadSha: testRun.headSha || headSha,
    obligationsHeadSha: obligationsExport?.headSha,
  });
  const dataQuality = assessDataQualityJs(factsPayload.facts, coverageObservations, {
    graphStatus: factsPayload.provenance.graphStatus,
    testInventoryComplete: factsPayload.provenance.providerCapabilities?.testInventory === true,
    truncated: factsPayload.provenance.truncated,
    missingThreshold: thresholds.unresolvedJoinRate ?? 0.1,
  });
  const dependencyRisk = assessDependencyRiskJs(factsPayload.facts, coverageObservations, thresholds);
  const scenarios = assessContractScenariosJs({
    facts: factsPayload.facts,
    obligations: obligationsExport?.obligations || [],
    coverageObservations,
    propertyResults,
    alignment,
    dataQuality,
    evidenceCeiling: lcov ? 'aggregate_lcov' : 'static_inventory',
  });
  const hints = buildStructureHintsJs({
    facts: factsPayload.facts,
    dependencyRisk,
    dataQuality,
    scenarios,
    mutationObservations: mutation.observations,
  });
  const obligationsRevision = obligationsExport?.revision || 'none';
  // без явного hash дискриминатором служит хэш канонического содержимого,
  // иначе разные payload'ы схлопываются в sha256('none')
  const coverageHash = lcov?.hash || (lcov ? sha256(lcov) : sha256('none'));
  const thresholdsHash = sha256(thresholds);
  const fingerprint = assessmentFingerprint({
    facts: factsPayload.fingerprint,
    obligationsRevision,
    coverageHash,
    thresholdsHash,
  });
  return {
    schemaRef: STRUCTURE_ASSESSMENT_SCHEMA,
    version: 1,
    kind: 'structure_assessment',
    generatedAt,
    fingerprint,
    factsFingerprint: factsPayload.fingerprint,
    obligationsRevision,
    obligationContentHashes: Object.fromEntries((obligationsExport?.obligations || [])
      .map((item) => [item.id, item.contentHash]).sort(([a], [b]) => a.localeCompare(b))),
    coverageHash,
    thresholdsHash,
    testRun: {
      id: String(testRun.id || `local-${headSha.slice(0, 12)}`),
      headSha: String(testRun.headSha || headSha),
      producedAt: testRun.producedAt || generatedAt,
    },
    temporalAlignment: alignment,
    coverageObservations: [...coverageObservations, ...(mutation.observations || [])]
      .sort((a, b) => a.id.localeCompare(b.id)),
    assessments: {
      codeCoverage: {
        totals: lcov?.totals || null,
        files: lcov?.files?.length || 0,
      },
      dependencyRisk,
      contractScenarios: scenarios,
      dataQuality,
    },
    hints,
    receipts,
  };
}
