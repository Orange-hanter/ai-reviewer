import { normalizeRepoPath, stableId } from './structure-evidence.mjs';

const DEP_RELATIONS = new Set([
  'contains', 'method', 'inherits', 'implements', 'calls',
  'imports', 'imports_from', 're_exports', 'dynamic_import', 'references',
]);

// потолки совпадают с maxItems в schemas/structure-facts.v1.schema.json: поднять их
// через конфиг нельзя, иначе продюсер выдаст факты, которые отвергнет собственная схема
const HARD_LIMITS = Object.freeze({ entities: 5000, edges: 15000, contracts: 5000 });

function clampLimit(value, max) {
  const number = Number(value);
  if (!Number.isFinite(number)) return max;
  return Math.min(Math.max(1, Math.floor(number)), max);
}

function lineNumber(value) {
  // номер строки, а не хвостовая колонка: 'src/a.ts:10:5' → 10, 'src/a.ts#L10' → 10
  const match = String(value || '').match(/(?:^|[:#])L?(\d+)(?::\d+)?$/);
  return match ? Number(match[1]) : null;
}

function confidence(value) {
  switch (String(value || '').toUpperCase()) {
    case 'EXTRACTED': return 'extracted';
    case 'INFERRED': return 'inferred';
    case 'AMBIGUOUS': return 'ambiguous';
    default: return 'heuristic';
  }
}

function pathInScope(path, scope) {
  if (!scope) return true;
  return path === scope || path.startsWith(`${scope.replace(/\/+$/, '')}/`);
}

function inferKind(node, methodSources, methodTargets, inheritanceSources, implementationTargets) {
  const id = String(node.id);
  const label = String(node.label || '');
  if (methodTargets.has(id) || /^\./.test(label)) return 'method';
  if (node._callable_class === true || methodSources.has(id) || inheritanceSources.has(id)) {
    return implementationTargets.has(id) ? 'interface' : 'class_like';
  }
  if (node._callable === true || /\(\)+$/.test(label)) return 'function';
  return 'symbol';
}

function boundaryCandidates(paths, layout = {}) {
  const boundaries = [{
    id: 'boundary:repository',
    kind: 'repository',
    name: 'repository',
    paths: ['.'],
    source: 'path',
    confidence: 'exact',
    evidence: ['repository-root'],
  }];
  const found = new Map();
  for (const path of paths) {
    const parts = path.split('/');
    // parts.length > 2 обязателен: иначе parts[1] — это файл (src/index.ts),
    // и он становится «слоем»
    const layer = layout.appDir && parts[0] === layout.appDir && parts.length > 2 ? parts[1] : '';
    const layered = Boolean(layer) && (!layout.layerDirs?.length || layout.layerDirs.includes(layer));
    if (layered) {
      found.set(`layer:${layer}`, {
        id: `boundary:layer:${layer}`,
        kind: 'layer',
        name: layer,
        paths: [`${layout.appDir}/${layer}`],
        source: 'profile',
        confidence: 'exact',
        evidence: [`profile:layout:${layout.appDir}/${layer}`],
      });
      continue;
    }
    // путь под appDir с незаявленным слоем раньше проваливался мимо обеих веток
    // и молча оказывался в boundary:repository — теперь у него остаётся модуль
    if (parts.length > 1) {
      const module = parts[0];
      if (!['node_modules', 'vendor', 'dist', 'build', 'coverage'].includes(module)) {
        found.set(`module:${module}`, {
          id: `boundary:module:${module}`,
          kind: 'module',
          name: module,
          paths: [module],
          source: 'path',
          confidence: 'heuristic',
          evidence: [`path-prefix:${module}`],
        });
      }
    }
  }
  boundaries.push(...found.values());
  return boundaries.sort((a, b) => a.id.localeCompare(b.id));
}

function boundaryForPath(path, boundaries) {
  let best = null;
  for (const boundary of boundaries) {
    if (boundary.kind === 'repository') continue;
    for (const prefix of boundary.paths) {
      if ((path === prefix || path.startsWith(`${prefix}/`))
        && (!best || prefix.length > best.prefix.length)) {
        best = { id: boundary.id, prefix };
      }
    }
  }
  return best?.id || 'boundary:repository';
}

export function extractGraphifyFacts(raw, {
  analysis = null,
  layout = {},
  scope = '',
  includeAmbiguous = false,
  maxEntities = HARD_LIMITS.entities,
  maxEdges = HARD_LIMITS.edges,
  maxContracts = HARD_LIMITS.contracts,
} = {}) {
  const entityLimit = clampLimit(maxEntities, HARD_LIMITS.entities);
  const edgeLimit = clampLimit(maxEdges, HARD_LIMITS.edges);
  const contractLimit = clampLimit(maxContracts, HARD_LIMITS.contracts);
  const nodes = Array.isArray(raw?.nodes) ? raw.nodes : [];
  const links = Array.isArray(raw?.links) ? raw.links
    : Array.isArray(raw?.edges) ? raw.edges : [];

  const methodSources = new Set();
  const methodTargets = new Set();
  const inheritanceSources = new Set();
  const implementationTargets = new Set();
  for (const edge of links) {
    if (edge?.relation === 'method') {
      methodSources.add(String(edge.source));
      methodTargets.add(String(edge.target));
    }
    if (edge?.relation === 'inherits') inheritanceSources.add(String(edge.source));
    if (edge?.relation === 'implements') implementationTargets.add(String(edge.target));
  }

  // god-узел, адресованный id или файлом, однозначен; голое имя — нет
  const godIds = new Set();
  const godPaths = new Set();
  const godLabels = new Set();
  for (const god of analysis?.gods || []) {
    const label = String(typeof god === 'string' ? god : god?.label || '').toLowerCase();
    if (god && typeof god === 'object' && god.id != null) { godIds.add(String(god.id)); continue; }
    if (god && typeof god === 'object' && god.source_file && label) {
      try { godPaths.add(`${normalizeRepoPath(god.source_file)}\0${label}`); continue; } catch { /* имя ниже */ }
    }
    if (label) godLabels.add(label);
  }
  const candidates = [];
  const warnings = [];
  let skippedUnsafePath = 0;
  let unparsedLocation = 0;
  for (const node of nodes) {
    if (!node || node.id == null || node.file_type && node.file_type !== 'code') continue;
    let path;
    try { path = normalizeRepoPath(node.source_file); } catch { skippedUnsafePath++; continue; }
    if (!pathInScope(path, scope)) continue;
    const label = String(node.label || node.id);
    const line = lineNumber(node.source_location);
    if (line == null && String(node.source_location ?? '').trim()) unparsedLocation++;
    candidates.push({
      graphId: String(node.id),
      label,
      qualifiedName: node.norm_label || label,
      kind: inferKind(node, methodSources, methodTargets, inheritanceSources, implementationTargets),
      path,
      line,
      endLine: null,
      signature: null,
      community: node.community ?? null,
      confidence: node._callable != null || node._callable_class != null ? 'extracted' : 'heuristic',
      evidence: [`graphify:node:${node.id}`],
      externalIds: { graphify: String(node.id) },
    });
  }
  candidates.sort((a, b) => (
    a.path.localeCompare(b.path) || (a.line || 0) - (b.line || 0) || a.label.localeCompare(b.label)
  ));
  if (skippedUnsafePath) warnings.push(`${skippedUnsafePath} node(s) skipped: unsafe source_file path`);
  if (unparsedLocation) warnings.push(`${unparsedLocation} node(s) kept without a line: unparsed source_location`);
  const truncatedEntities = candidates.length > entityLimit;
  const kept = candidates.slice(0, entityLimit);
  if (truncatedEntities) warnings.push(`entities truncated at ${entityLimit}`);

  const labelCounts = new Map();
  for (const entity of kept) {
    const key = entity.label.toLowerCase();
    labelCounts.set(key, (labelCounts.get(key) || 0) + 1);
  }
  const ambiguousGods = new Map();
  const isGodNode = (entity) => {
    if (godIds.has(entity.graphId)) return true;
    const label = entity.label.toLowerCase();
    if (godPaths.has(`${entity.path}\0${label}`)) return true;
    if (!godLabels.has(label)) return false;
    const count = labelCounts.get(label) || 0;
    // имя встречается в нескольких файлах — метить наугад нельзя, понижаем
    if (count > 1) { ambiguousGods.set(label, count); return false; }
    return true;
  };

  const boundaries = boundaryCandidates([...new Set(kept.map((entity) => entity.path))], layout);
  const entities = kept.map((entity) => {
    const observedId = stableId('entity', entity.path, entity.qualifiedName, entity.kind, entity.line);
    return {
      observedId,
      lineageId: observedId,
      boundaryId: boundaryForPath(entity.path, boundaries),
      label: entity.label,
      qualifiedName: entity.qualifiedName,
      kind: entity.kind,
      path: entity.path,
      line: entity.line,
      endLine: entity.endLine,
      signature: entity.signature,
      community: entity.community,
      isGodNode: isGodNode(entity),
      confidence: entity.confidence,
      evidence: entity.evidence,
      externalIds: entity.externalIds,
    };
  });

  for (const [label, count] of [...ambiguousGods.entries()].sort((a, b) => a[0].localeCompare(b[0]))) {
    warnings.push(`god node label is ambiguous: ${label} (${count} entities)`);
  }

  const entityByGraphId = new Map();
  for (let i = 0; i < kept.length; i++) entityByGraphId.set(kept[i].graphId, entities[i]);
  // одинаковые рёбра дают один и тот же stableId; без дедупа они удваивают fanIn
  const dependencyById = new Map();
  for (let i = 0; i < links.length; i++) {
    const edge = links[i];
    if (!edge || !DEP_RELATIONS.has(String(edge.relation || ''))) continue;
    const from = entityByGraphId.get(String(edge.source));
    const to = entityByGraphId.get(String(edge.target));
    if (!from || !to) continue;
    const conf = confidence(edge.confidence);
    if (conf === 'ambiguous' && !includeAmbiguous) continue;
    const dependencyId = stableId('edge', from.observedId, to.observedId, edge.relation);
    if (dependencyById.has(dependencyId)) continue;
    dependencyById.set(dependencyId, {
      id: dependencyId,
      from: from.observedId,
      to: to.observedId,
      relation: String(edge.relation),
      crossBoundary: from.boundaryId !== to.boundaryId,
      crossCommunity: from.community != null && to.community != null && from.community !== to.community,
      confidence: conf,
      evidence: [`graphify:edge:${i}`],
    });
  }
  const dependencyCandidates = [...dependencyById.values()].sort((a, b) => a.id.localeCompare(b.id));
  const dependencies = dependencyCandidates.slice(0, edgeLimit);
  const truncatedEdges = dependencyCandidates.length > edgeLimit;
  if (truncatedEdges) warnings.push(`dependencies truncated at ${edgeLimit}`);

  const contracts = [];
  const contractKeys = new Set();
  // единый формат ключа для всех источников контрактов, иначе dedupe мёртвый
  const contractKey = (entityRef, kind, targetRef) => `${entityRef}:${kind}:${targetRef || ''}`;
  const entityByObservedId = new Map(entities.map((entity) => [entity.observedId, entity]));
  for (const edge of dependencies) {
    if (!['implements', 'inherits'].includes(edge.relation)) continue;
    const from = entityByObservedId.get(edge.from);
    const key = contractKey(edge.from, edge.relation, edge.to);
    if (contractKeys.has(key)) continue;
    contractKeys.add(key);
    const id = stableId('contract', edge.relation, edge.from, edge.to);
    contracts.push({
      id,
      entityRef: edge.from,
      kind: edge.relation,
      targetRef: edge.to,
      signature: from?.signature || null,
      confidence: edge.confidence,
      evidence: edge.evidence,
    });
  }
  for (const entity of entities) {
    if (!['function', 'method', 'class_like', 'interface'].includes(entity.kind)) continue;
    const key = contractKey(entity.observedId, 'entrypoint', null);
    if (contractKeys.has(key)) continue;
    contractKeys.add(key);
    contracts.push({
      id: stableId('contract', 'entrypoint', entity.observedId),
      entityRef: entity.observedId,
      kind: 'entrypoint',
      targetRef: null,
      signature: entity.signature,
      confidence: 'heuristic',
      evidence: [...entity.evidence, 'graphify:callable-contract-candidate'],
    });
  }
  // при обрезке первыми уходят эвристические entrypoint, доказанные контракты остаются
  const contractRank = (contract) => (contract.kind === 'entrypoint' ? 1 : 0);
  contracts.sort((a, b) => contractRank(a) - contractRank(b) || a.id.localeCompare(b.id));
  const truncatedContracts = contracts.length > contractLimit;
  if (truncatedContracts) warnings.push(`contracts truncated at ${contractLimit}`);
  const keptContracts = contracts.slice(0, contractLimit).sort((a, b) => a.id.localeCompare(b.id));

  const testCases = entities
    .filter((entity) => /(?:^|\/)(?:test|tests|__tests__|e2e)(?:\/|$)|\.(?:test|spec)\.[^.]+$/i.test(entity.path)
      && ['function', 'method'].includes(entity.kind))
    .map((entity) => ({
      id: stableId('test', entity.path, entity.line, entity.label),
      name: entity.label,
      path: entity.path,
      line: entity.line || 1,
      framework: null,
      targetContractRefs: [],
      assertions: [],
      confidence: entity.confidence,
      evidence: entity.evidence,
    }))
    .sort((a, b) => a.id.localeCompare(b.id));

  return {
    boundaries,
    entities,
    contracts: keptContracts,
    dependencies,
    testCases,
    warnings,
    truncated: truncatedEntities || truncatedEdges || truncatedContracts,
    capabilities: {
      symbols: 'inferred',
      exactKinds: false,
      sourceRanges: false,
      signatures: false,
      references: false,
      implementations: true,
      imports: true,
      calls: true,
      topology: true,
    },
  };
}

export function buildStructureViews(facts) {
  const entities = facts.entities || [];
  const dependencies = facts.dependencies || [];
  const childrenByParent = new Map();
  for (const edge of dependencies) {
    if (!['contains', 'method'].includes(edge.relation)) continue;
    if (!childrenByParent.has(edge.from)) childrenByParent.set(edge.from, []);
    childrenByParent.get(edge.from).push(edge.to);
  }
  const byId = new Map(entities.map((entity) => [entity.observedId, entity]));
  const node = (entity) => ({
    id: entity.observedId,
    label: entity.label,
    kind: entity.kind,
    path: entity.path,
    line: entity.line,
  });
  // одна группировка вместо фильтра по каждому пути и каждой границе:
  // на потолке в 5000 сущностей это были миллионы лишних сравнений
  const byPath = new Map();
  const refsByBoundary = new Map();
  for (const entity of entities) {
    if (!byPath.has(entity.path)) byPath.set(entity.path, []);
    byPath.get(entity.path).push(entity);
    if (!refsByBoundary.has(entity.boundaryId)) refsByBoundary.set(entity.boundaryId, []);
    refsByBoundary.get(entity.boundaryId).push(entity.observedId);
  }
  const objectTree = entities
    .filter((entity) => ['class_like', 'interface'].includes(entity.kind))
    .map((entity) => ({
      ...node(entity),
      bestEffort: true,
      children: (childrenByParent.get(entity.observedId) || [])
        .map((id) => byId.get(id))
        .filter(Boolean)
        .map(node)
        .sort((a, b) => a.label.localeCompare(b.label)),
    }))
    .sort((a, b) => a.path.localeCompare(b.path) || a.label.localeCompare(b.label));
  const functionTree = [...byPath.keys()].sort().map((path) => ({
    path,
    children: byPath.get(path)
      .filter((entity) => ['function', 'method'].includes(entity.kind))
      .map(node)
      .sort((a, b) => (a.line || 0) - (b.line || 0)),
  })).filter((entry) => entry.children.length);
  return {
    boundaryTree: (facts.boundaries || []).map((boundary) => ({
      id: boundary.id,
      name: boundary.name,
      kind: boundary.kind,
      paths: boundary.paths,
      entityRefs: refsByBoundary.get(boundary.id) || [],
    })),
    objectTree,
    functionTree,
    dependencyGraph: dependencies.map((edge) => edge.id),
  };
}
