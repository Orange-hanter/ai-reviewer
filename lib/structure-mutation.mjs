import { normalizeRepoPath, stableId } from './structure-evidence.mjs';

const MUTANT_STATUS = new Map([
  ['Killed', 'killed'],
  ['Survived', 'survived'],
  ['NoCoverage', 'no_coverage'],
  ['Timeout', 'timeout'],
  ['CompileError', 'compile_error'],
  ['RuntimeError', 'runtime_error'],
  ['Ignored', 'ignored'],
]);

function locationOf(mutant) {
  const location = mutant.location || {};
  return {
    line: Number(location.start?.line || mutant.line || 0) || null,
    endLine: Number(location.end?.line || location.start?.line || mutant.line || 0) || null,
  };
}

function contractForMutant(path, location, contracts, entitiesById) {
  const candidates = [];
  for (const contract of contracts || []) {
    const entity = entitiesById.get(contract.entityRef);
    if (!entity || entity.path !== path) continue;
    if (entity.line && location.line && entity.line > location.line) continue;
    if (entity.endLine && location.line && entity.endLine < location.line) continue;
    candidates.push({ contract, entity });
  }
  // без location.line оба guard'а выше пропускаются, а сортировка по расстоянию вырождается
  // в «первый контракт файла» — это ложное evidence, поэтому привязываем только однозначное
  if (!location.line) return candidates.length === 1 ? candidates[0].contract : null;
  candidates.sort((a, b) => {
    const aDistance = Math.abs((a.entity.line || location.line || 0) - (location.line || 0));
    const bDistance = Math.abs((b.entity.line || location.line || 0) - (location.line || 0));
    return aDistance - bDistance;
  });
  return candidates[0]?.contract || null;
}

export function parseMutationReport(report, facts, { scopeContractRefs = [] } = {}) {
  const entitiesById = new Map((facts?.entities || []).map((entity) => [entity.observedId, entity]));
  const allowed = new Set(scopeContractRefs || []);
  const observations = [];
  const warnings = [];
  const files = report?.files && typeof report.files === 'object' ? report.files : {};
  for (const [rawPath, file] of Object.entries(files)) {
    let path;
    try { path = normalizeRepoPath(rawPath); } catch (error) {
      warnings.push(error.message);
      continue;
    }
    for (const mutant of file?.mutants || []) {
      const location = locationOf(mutant);
      const contract = contractForMutant(path, location, facts?.contracts || [], entitiesById);
      if (allowed.size && (!contract || !allowed.has(contract.id))) continue;
      const mutationStatus = MUTANT_STATUS.get(mutant.status) || String(mutant.status || 'unknown').toLowerCase();
      observations.push({
        id: stableId('mutant', path, mutant.id, mutant.mutatorName, location.line),
        level: 'contract_edge_coverage',
        subjectRef: contract?.id || `file:${path}`,
        contractRef: contract?.id || null,
        path,
        line: location.line,
        endLine: location.endLine,
        mutator: String(mutant.mutatorName || mutant.type || 'unknown'),
        replacement: mutant.replacement == null ? null : String(mutant.replacement),
        status: mutationStatus === 'killed' ? 'covered'
          : ['survived', 'no_coverage'].includes(mutationStatus) ? 'uncovered' : 'partial',
        mutationStatus,
        killedBy: [...new Set(mutant.killedByTests || mutant.killedBy || [])].map(String).sort(),
        confidence: contract ? 'exact' : 'heuristic',
        evidence: [`mutation:${path}:${mutant.id}`],
      });
    }
  }
  observations.sort((a, b) => a.id.localeCompare(b.id));
  return {
    observations,
    warnings,
    summary: {
      total: observations.length,
      killed: observations.filter((item) => item.mutationStatus === 'killed').length,
      survived: observations.filter((item) => item.mutationStatus === 'survived').length,
      noCoverage: observations.filter((item) => item.mutationStatus === 'no_coverage').length,
      timeout: observations.filter((item) => item.mutationStatus === 'timeout').length,
    },
  };
}

export function criticalContractRefs(facts, dependencyRisk, obligations) {
  const must = new Set((obligations || [])
    .filter((obligation) => obligation.status === 'confirmed' && obligation.priority === 'must')
    .flatMap((obligation) => obligation.contractRefs || []));
  for (const hotspot of dependencyRisk?.highFanIn || []) must.add(hotspot.contractRef || hotspot.entityRef);
  for (const entity of facts?.entities || []) {
    if (entity.isGodNode) {
      for (const contract of facts.contracts || []) {
        if (contract.entityRef === entity.observedId) must.add(contract.id);
      }
    }
  }
  return [...must].filter(Boolean).sort();
}
