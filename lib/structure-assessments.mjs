import { assessmentFingerprint, STRUCTURE_ASSESSMENT_SCHEMA, temporalAlignment } from './structure-protocol.mjs';
import { sha256, stableId } from './structure-evidence.mjs';

function ratio(part, total) {
  return total > 0 ? part / total : null;
}

export function assessDependencyRisk(facts, coverageObservations, {
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

export function assessDataQuality(facts, coverageObservations, {
  graphStatus = 'unknown',
  testInventoryComplete = false,
  truncated = false,
  missingThreshold = 0.1,
} = {}) {
  const functions = (coverageObservations || []).filter((item) => item.level === 'function_execution');
  const unresolved = functions.filter((item) => ['unresolved', 'ambiguous'].includes(item.confidence)).length;
  const unresolvedJoinRate = ratio(unresolved, functions.length) || 0;
  return {
    graphStatus,
    truncated: Boolean(truncated),
    testInventoryComplete: Boolean(testInventoryComplete),
    functionObservations: functions.length,
    unresolvedFunctionJoins: unresolved,
    unresolvedJoinRate,
    missingAllowed: graphStatus === 'fresh'
      && !truncated
      && testInventoryComplete
      && unresolvedJoinRate <= missingThreshold,
    entityCount: facts?.entities?.length || 0,
    edgeCount: facts?.dependencies?.length || 0,
  };
}

function scenarioAssertionKinds(kind) {
  switch (kind) {
    case 'error_path': return ['throws', 'rejects', 'http_status', 'property'];
    case 'boundary_value': return ['property', 'equality'];
    case 'invariant': return ['property'];
    case 'integration': return ['http_status', 'equality'];
    default: return ['equality', 'http_status', 'property'];
  }
}

function scenarioId(obligation, contractRefs) {
  return stableId('scenario', obligation.id, obligation.kind, contractRefs.join(','));
}

export function assessContractScenarios({
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
      id: scenarioId(obligation, contractRefs),
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
    const acceptedAssertions = new Set(scenarioAssertionKinds(obligation.kind));
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

export function buildStructureHints({
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
  const headSha = factsPayload.provenance.headSha;
  const alignment = temporalAlignment({
    factsHeadSha: headSha,
    testHeadSha: testRun.headSha || headSha,
    obligationsHeadSha: obligationsExport?.headSha,
  });
  const dataQuality = assessDataQuality(factsPayload.facts, coverageObservations, {
    graphStatus: factsPayload.provenance.graphStatus,
    testInventoryComplete: factsPayload.provenance.providerCapabilities?.testInventory === true,
    truncated: factsPayload.provenance.truncated,
    missingThreshold: thresholds.unresolvedJoinRate ?? 0.1,
  });
  const dependencyRisk = assessDependencyRisk(factsPayload.facts, coverageObservations, thresholds);
  const scenarios = assessContractScenarios({
    facts: factsPayload.facts,
    obligations: obligationsExport?.obligations || [],
    coverageObservations,
    propertyResults,
    alignment,
    dataQuality,
    evidenceCeiling: lcov ? 'aggregate_lcov' : 'static_inventory',
  });
  const hints = buildStructureHints({
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
