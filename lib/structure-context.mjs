const DEFAULT_CONTEXT_LIMITS = Object.freeze({
  maxSeeds: 20,
  maxEdges: 50,
  maxObligations: 10,
  maxGaps: 15,
  maxBytes: 32 * 1024,
  maxDepth: 1,
});

function estimateTokens(value) {
  return Math.ceil(JSON.stringify(value).length / 4);
}

function matchesScope(entity, scopeRefs) {
  if (!scopeRefs.length) return false;
  return scopeRefs.some((scope) => (
    entity.observedId === scope
    || entity.boundaryId === scope
    || entity.path === scope
    || entity.path.startsWith(`${scope.replace(/\/+$/, '')}/`)
  ));
}

export function buildStructureContext({
  facts,
  assessment,
  headSha,
  snapshotFingerprint,
  scopeRefs = [],
  taskRef = null,
  contractRefs = [],
  tokenBudget = 8000,
  dependencyDepth = 1,
  limits = {},
}) {
  if (!headSha && !snapshotFingerprint) {
    throw new Error('structure_context requires pinned headSha or snapshotFingerprint');
  }
  if (headSha && facts.provenance.headSha !== headSha) {
    throw new Error(`pinned headSha ${headSha} does not match facts ${facts.provenance.headSha}`);
  }
  if (snapshotFingerprint && facts.fingerprint !== snapshotFingerprint) {
    throw new Error(`snapshot fingerprint ${snapshotFingerprint} does not match facts`);
  }
  if (assessment.factsFingerprint !== facts.fingerprint) {
    throw new Error('assessment does not reference the supplied facts snapshot');
  }
  const cap = { ...DEFAULT_CONTEXT_LIMITS, ...limits };
  cap.maxDepth = Math.min(dependencyDepth, cap.maxDepth);
  const entityById = new Map(facts.facts.entities.map((entity) => [entity.observedId, entity]));
  const contractById = new Map(facts.facts.contracts.map((contract) => [contract.id, contract]));
  const seedIds = new Set();
  for (const entity of facts.facts.entities) {
    if (matchesScope(entity, scopeRefs)) seedIds.add(entity.observedId);
  }
  for (const contractRef of contractRefs) {
    const contract = contractById.get(contractRef);
    if (contract) seedIds.add(contract.entityRef);
  }
  const queue = [...seedIds].slice(0, cap.maxSeeds).map((id) => ({ id, depth: 0 }));
  const selectedIds = new Set(queue.map((item) => item.id));
  const selectedEdges = [];
  const selectedEdgeIds = new Set();
  while (queue.length && selectedEdges.length < cap.maxEdges) {
    const current = queue.shift();
    if (current.depth >= cap.maxDepth) continue;
    for (const edge of facts.facts.dependencies) {
      if (selectedEdges.length >= cap.maxEdges) break;
      if (edge.from !== current.id && edge.to !== current.id) continue;
      const edgeId = edge.id || `${edge.from}→${edge.to}:${edge.relation}`;
      if (selectedEdgeIds.has(edgeId)) continue;
      selectedEdgeIds.add(edgeId);
      selectedEdges.push(edge);
      const other = edge.from === current.id ? edge.to : edge.from;
      if (!selectedIds.has(other)) {
        selectedIds.add(other);
        queue.push({ id: other, depth: current.depth + 1 });
      }
    }
  }
  const selectedEntities = [...selectedIds].map((id) => entityById.get(id)).filter(Boolean);
  const selectedContracts = facts.facts.contracts.filter((contract) => (
    selectedIds.has(contract.entityRef) || contractRefs.includes(contract.id)
  ));
  const selectedContractIds = new Set(selectedContracts.map((contract) => contract.id));
  const scenarios = assessment.assessments.contractScenarios
    .filter((scenario) => scenario.contractRefs.some((id) => selectedContractIds.has(id)))
    .slice(0, cap.maxGaps);
  const obligationIds = new Set(scenarios.map((scenario) => scenario.obligationRef));
  const hints = assessment.hints.filter((hint) => (
    hint.subjectRefs.some((ref) => selectedIds.has(ref) || selectedContractIds.has(ref) || obligationIds.has(ref))
  )).slice(0, cap.maxGaps);
  const boundaries = facts.facts.boundaries.filter((boundary) => (
    selectedEntities.some((entity) => entity.boundaryId === boundary.id)
  ));
  const result = {
    version: 1,
    kind: 'structure_context',
    pinned: {
      headSha: facts.provenance.headSha,
      factsFingerprint: facts.fingerprint,
      assessmentFingerprint: assessment.fingerprint,
      obligationsRevision: assessment.obligationsRevision,
    },
    temporalAlignment: assessment.temporalAlignment,
    taskRef,
    scopeRefs,
    boundaries,
    entities: selectedEntities,
    contracts: selectedContracts,
    dependencies: selectedEdges,
    scenarios,
    hints,
    suggestedFiles: [...new Set([
      ...selectedEntities.map((entity) => entity.path),
      ...facts.facts.testCases
        .filter((testCase) => testCase.targetContractRefs.some((id) => selectedContractIds.has(id)))
        .map((testCase) => testCase.path),
    ])].sort(),
    truncated: seedIds.size > cap.maxSeeds || selectedEdges.length >= cap.maxEdges,
  };
  result.tokenEstimate = estimateTokens(result);
  if (Buffer.byteLength(JSON.stringify(result)) > cap.maxBytes || result.tokenEstimate > tokenBudget) {
    result.dependencies = result.dependencies.slice(0, Math.max(0, Math.floor(result.dependencies.length / 2)));
    result.entities = result.entities.slice(0, cap.maxSeeds);
    result.hints = result.hints.slice(0, Math.max(1, Math.floor(cap.maxGaps / 2)));
    result.truncated = true;
    result.tokenEstimate = estimateTokens(result);
  }
  return result;
}
