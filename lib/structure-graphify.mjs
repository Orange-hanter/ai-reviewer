/**
 * Фаза 2, шаг 2: lib/structure-graphify.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/structure_graphify (порт extractGraphifyFacts и buildStructureViews).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Синхронные
 * функции вызывают бинарь через execFileSync: JSON {"op", ...args} на stdin,
 * ответ — JSON в формате JS-возврата.
 *
 * Резолвинг бинаря (по аналогии с lib/structure-scip.mjs):
 *   AI_REVIEWER_STRUCTURE_GRAPHIFY_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/structure_graphify;
 *   AI_REVIEWER_STRUCTURE_GRAPHIFY_FORCE_JS=1 — принудительно чистый JS.
 *
 * Если бинарь не существует (чистый клон до первого `cargo build`), обёртка
 * деградирует на чистый JS fallback ниже — эталонную копию исходной реализации.
 * Если бинарь ЕСТЬ, но упал (non-zero exit) — ошибка пробрасывается, fallback
 * не используется: молчаливая подмена реализации при битом Rust-коде скрывала бы
 * регрессии.
 *
 * Нюансы паритета (проверены parity-прогоном rust/parity/structure-graphify.parity.mjs):
 *   - `||`-семантика (0/''/null → fallback) в label/line/community;
 *   - inferKind: method-target/'.'-префикс → method, _callable_class/method-source/
 *     inherits-source → class_like/interface, _callable/'()'-суффикс → function;
 *   - boundary: layout appDir/layerDirs из профиля, module-префиксы с чёрным
 *     списком; граница пути — самый длинный совпавший префикс;
 *   - контракты: implements/inherits из рёбер + entrypoint-дедуп по единому ключу;
 *   - отсечение сущностей ДО boundaries, рёбер — после сортировки по id;
 *   - сортировки localeCompare (ICU root), дефолтный Array.sort — побайтовый.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { normalizeRepoPath, stableId } from './structure-evidence.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_GRAPHIFY_BIN = resolve(__dir, '../rust/target/debug/structure_graphify');

function resolveGraphifyBin() {
  if (process.env.AI_REVIEWER_STRUCTURE_GRAPHIFY_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_STRUCTURE_GRAPHIFY_BIN || '').trim() || DEFAULT_GRAPHIFY_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). Ошибка бинаря пробрасывается. */
function runRust(op, args) {
  const bin = resolveGraphifyBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

/** Извлекает структурные факты из graphify-отчёта ({nodes, links|edges}). */
export function extractGraphifyFacts(raw, {
  analysis = null,
  layout = {},
  scope = '',
  includeAmbiguous = false,
  maxEntities = 5000,
  maxEdges = 15000,
} = {}) {
  const r = runRust('extractGraphifyFacts', {
    raw, analysis, layout, scope, includeAmbiguous, maxEntities, maxEdges,
  });
  if (r !== null) return r;
  return extractGraphifyFactsJs(raw, { analysis, layout, scope, includeAmbiguous, maxEntities, maxEdges });
}

/** Строит view-деревья (boundaryTree/objectTree/functionTree/dependencyGraph). */
export function buildStructureViews(facts) {
  const r = runRust('buildStructureViews', { facts });
  if (r !== null) return r;
  return buildStructureViewsJs(facts);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/structure-graphify.mjs
// до Фазы 2). Используется только когда Rust-бинарь отсутствует; при битом бинаре
// НЕ подменяет его.
// ---------------------------------------------------------------------------

const DEP_RELATIONS = new Set([
  'contains', 'method', 'inherits', 'implements', 'calls',
  'imports', 'imports_from', 're_exports', 'dynamic_import', 'references',
]);

function lineNumberJs(value) {
  const match = String(value || '').match(/(?:^|:)L?(\d+)$/);
  return match ? Number(match[1]) : null;
}

function confidenceJs(value) {
  switch (String(value || '').toUpperCase()) {
    case 'EXTRACTED': return 'extracted';
    case 'INFERRED': return 'inferred';
    case 'AMBIGUOUS': return 'ambiguous';
    default: return 'heuristic';
  }
}

function pathInScopeJs(path, scope) {
  if (!scope) return true;
  return path === scope || path.startsWith(`${scope.replace(/\/+$/, '')}/`);
}

function inferKindJs(node, methodSources, methodTargets, inheritanceSources, implementationTargets) {
  const id = String(node.id);
  const label = String(node.label || '');
  if (methodTargets.has(id) || /^\./.test(label)) return 'method';
  if (node._callable_class === true || methodSources.has(id) || inheritanceSources.has(id)) {
    return implementationTargets.has(id) ? 'interface' : 'class_like';
  }
  if (node._callable === true || /\(\)+$/.test(label)) return 'function';
  return 'symbol';
}

function boundaryCandidatesJs(paths, layout = {}) {
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
    if (layout.appDir && parts[0] === layout.appDir && parts[1]) {
      const layer = parts[1];
      if (!layout.layerDirs?.length || layout.layerDirs.includes(layer)) {
        found.set(`layer:${layer}`, {
          id: `boundary:layer:${layer}`,
          kind: 'layer',
          name: layer,
          paths: [`${layout.appDir}/${layer}`],
          source: 'profile',
          confidence: 'exact',
          evidence: [`profile:layout:${layout.appDir}/${layer}`],
        });
      }
    } else if (parts.length > 1) {
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

function boundaryForPathJs(path, boundaries) {
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

function extractGraphifyFactsJs(raw, {
  analysis = null,
  layout = {},
  scope = '',
  includeAmbiguous = false,
  maxEntities = 5000,
  maxEdges = 15000,
} = {}) {
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

  const gods = new Set((analysis?.gods || []).map((god) => (
    String(typeof god === 'string' ? god : god?.label || '').toLowerCase()
  )).filter(Boolean));
  const candidates = [];
  const warnings = [];
  for (const node of nodes) {
    if (!node || node.id == null || node.file_type && node.file_type !== 'code') continue;
    let path;
    try { path = normalizeRepoPath(node.source_file); } catch { continue; }
    if (!pathInScopeJs(path, scope)) continue;
    const label = String(node.label || node.id);
    candidates.push({
      graphId: String(node.id),
      label,
      qualifiedName: node.norm_label || label,
      kind: inferKindJs(node, methodSources, methodTargets, inheritanceSources, implementationTargets),
      path,
      line: lineNumberJs(node.source_location),
      endLine: null,
      signature: null,
      community: node.community ?? null,
      isGodNode: gods.has(label.toLowerCase()),
      confidence: node._callable != null || node._callable_class != null ? 'extracted' : 'heuristic',
      evidence: [`graphify:node:${node.id}`],
    });
  }
  candidates.sort((a, b) => (
    a.path.localeCompare(b.path) || (a.line || 0) - (b.line || 0) || a.label.localeCompare(b.label)
  ));
  const truncatedEntities = candidates.length > maxEntities;
  const kept = candidates.slice(0, maxEntities);
  if (truncatedEntities) warnings.push(`entities truncated at ${maxEntities}`);

  const boundaries = boundaryCandidatesJs([...new Set(kept.map((entity) => entity.path))], layout);
  const entities = kept.map((entity) => {
    const observedId = stableId('entity', entity.path, entity.qualifiedName, entity.kind, entity.line);
    return {
      observedId,
      lineageId: observedId,
      boundaryId: boundaryForPathJs(entity.path, boundaries),
      label: entity.label,
      qualifiedName: entity.qualifiedName,
      kind: entity.kind,
      path: entity.path,
      line: entity.line,
      endLine: entity.endLine,
      signature: entity.signature,
      community: entity.community,
      isGodNode: entity.isGodNode,
      confidence: entity.confidence,
      evidence: entity.evidence,
      externalIds: { graphify: entity.graphId },
    };
  });

  const entityByGraphId = new Map();
  for (let i = 0; i < kept.length; i++) entityByGraphId.set(kept[i].graphId, entities[i]);
  const dependencyCandidates = [];
  for (let i = 0; i < links.length; i++) {
    const edge = links[i];
    if (!edge || !DEP_RELATIONS.has(String(edge.relation || ''))) continue;
    const from = entityByGraphId.get(String(edge.source));
    const to = entityByGraphId.get(String(edge.target));
    if (!from || !to) continue;
    const conf = confidenceJs(edge.confidence);
    if (conf === 'ambiguous' && !includeAmbiguous) continue;
    dependencyCandidates.push({
      id: stableId('edge', from.observedId, to.observedId, edge.relation),
      from: from.observedId,
      to: to.observedId,
      relation: String(edge.relation),
      crossBoundary: from.boundaryId !== to.boundaryId,
      crossCommunity: from.community != null && to.community != null && from.community !== to.community,
      confidence: conf,
      evidence: [`graphify:edge:${i}`],
    });
  }
  dependencyCandidates.sort((a, b) => a.id.localeCompare(b.id));
  const dependencies = dependencyCandidates.slice(0, maxEdges);
  const truncatedEdges = dependencyCandidates.length > maxEdges;
  if (truncatedEdges) warnings.push(`dependencies truncated at ${maxEdges}`);

  const contracts = [];
  const contractKeys = new Set();
  // единый формат ключа для всех источников контрактов, иначе dedupe мёртвый
  const contractKey = (entityRef, kind, targetRef) => `${entityRef}:${kind}:${targetRef || ''}`;
  const entityByObservedId = new Map(entities.map((entity) => [entity.observedId, entity]));
  for (const edge of dependencies) {
    if (!['implements', 'inherits'].includes(edge.relation)) continue;
    const from = entityByObservedId.get(edge.from);
    const id = stableId('contract', edge.relation, edge.from, edge.to);
    contractKeys.add(contractKey(edge.from, edge.relation, edge.to));
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
  contracts.sort((a, b) => a.id.localeCompare(b.id));

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
    contracts,
    dependencies,
    testCases,
    warnings,
    truncated: truncatedEntities || truncatedEdges,
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

function buildStructureViewsJs(facts) {
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
  const functionTree = [...new Set(entities.map((entity) => entity.path))].sort().map((path) => ({
    path,
    children: entities
      .filter((entity) => entity.path === path && ['function', 'method'].includes(entity.kind))
      .map(node)
      .sort((a, b) => (a.line || 0) - (b.line || 0)),
  })).filter((entry) => entry.children.length);
  return {
    boundaryTree: (facts.boundaries || []).map((boundary) => ({
      id: boundary.id,
      name: boundary.name,
      kind: boundary.kind,
      paths: boundary.paths,
      entityRefs: entities.filter((entity) => entity.boundaryId === boundary.id).map((entity) => entity.observedId),
    })),
    objectTree,
    functionTree,
    dependencyGraph: dependencies.map((edge) => edge.id),
  };
}
