import { readFile } from 'node:fs/promises';
import { normalizeRepoPath, stableId } from './structure-evidence.mjs';

const KIND_NAMES = new Map([
  [7, 'class'], [9, 'constructor'], [11, 'enum'], [12, 'enum_member'],
  [15, 'field'], [16, 'file'], [17, 'function'], [21, 'interface'],
  [26, 'method'], [28, 'message'], [29, 'module'], [30, 'namespace'],
  [41, 'property'], [42, 'protocol'], [46, 'signature'], [49, 'struct'],
  [53, 'trait'], [54, 'type'], [55, 'type_alias'], [59, 'union'],
  [62, 'contract'], [63, 'error'], [66, 'abstract_method'], [67, 'method_specification'],
  [68, 'protocol_method'], [69, 'pure_virtual_method'], [70, 'trait_method'],
  [71, 'type_class_method'], [80, 'static_method'], [81, 'static_property'],
]);

function readVarint(buffer, start) {
  let value = 0;
  let shift = 0;
  let offset = start;
  while (offset < buffer.length) {
    const byte = buffer[offset++];
    value += (byte & 0x7f) * (2 ** shift);
    if ((byte & 0x80) === 0) return { value, offset };
    shift += 7;
    if (shift > 53) throw new Error('SCIP protobuf varint exceeds JavaScript safe integer');
  }
  throw new Error('truncated SCIP protobuf varint');
}

function fields(buffer) {
  const out = [];
  let offset = 0;
  while (offset < buffer.length) {
    const tag = readVarint(buffer, offset);
    offset = tag.offset;
    const field = Math.floor(tag.value / 8);
    const wire = tag.value & 7;
    if (wire === 0) {
      const decoded = readVarint(buffer, offset);
      out.push({ field, wire, value: decoded.value });
      offset = decoded.offset;
    } else if (wire === 2) {
      const length = readVarint(buffer, offset);
      const end = length.offset + length.value;
      if (end > buffer.length) throw new Error('truncated SCIP protobuf field');
      out.push({ field, wire, value: buffer.subarray(length.offset, end) });
      offset = end;
    } else if (wire === 1) {
      if (offset + 8 > buffer.length) throw new Error('truncated SCIP protobuf field');
      offset += 8;
    } else if (wire === 5) {
      if (offset + 4 > buffer.length) throw new Error('truncated SCIP protobuf field');
      offset += 4;
    } else {
      throw new Error(`unsupported SCIP protobuf wire type ${wire}`);
    }
  }
  return out;
}

function first(entries, field, wire = null) {
  return entries.find((entry) => entry.field === field && (wire == null || entry.wire === wire));
}

function all(entries, field, wire = null) {
  return entries.filter((entry) => entry.field === field && (wire == null || entry.wire === wire));
}

function text(entry) {
  return entry?.wire === 2 ? entry.value.toString('utf8') : '';
}

function decodePackedInts(buffer) {
  const values = [];
  let offset = 0;
  while (offset < buffer.length) {
    const decoded = readVarint(buffer, offset);
    values.push(decoded.value);
    offset = decoded.offset;
  }
  return values;
}

function decodeRange(entries) {
  const single = first(entries, 8, 2);
  if (single) {
    const value = fields(single.value);
    const line = first(value, 1, 0)?.value ?? 0;
    return {
      line: line + 1,
      endLine: line + 1,
      startCharacter: first(value, 2, 0)?.value ?? 0,
      endCharacter: first(value, 3, 0)?.value ?? 0,
    };
  }
  const multi = first(entries, 9, 2);
  if (multi) {
    const value = fields(multi.value);
    return {
      line: (first(value, 1, 0)?.value ?? 0) + 1,
      endLine: (first(value, 3, 0)?.value ?? 0) + 1,
      startCharacter: first(value, 2, 0)?.value ?? 0,
      endCharacter: first(value, 4, 0)?.value ?? 0,
    };
  }
  const legacy = first(entries, 1, 2);
  if (!legacy) return null;
  const values = decodePackedInts(legacy.value);
  if (values.length === 3) {
    return { line: values[0] + 1, endLine: values[0] + 1, startCharacter: values[1], endCharacter: values[2] };
  }
  if (values.length >= 4) {
    return { line: values[0] + 1, endLine: values[2] + 1, startCharacter: values[1], endCharacter: values[3] };
  }
  return null;
}

function decodeOccurrence(buffer) {
  const value = fields(buffer);
  return {
    symbol: text(first(value, 2, 2)),
    roles: first(value, 3, 0)?.value || 0,
    range: decodeRange(value),
  };
}

function decodeRelationship(buffer) {
  const value = fields(buffer);
  return {
    symbol: text(first(value, 1, 2)),
    isReference: Boolean(first(value, 2, 0)?.value),
    isImplementation: Boolean(first(value, 3, 0)?.value),
    isTypeDefinition: Boolean(first(value, 4, 0)?.value),
    isDefinition: Boolean(first(value, 5, 0)?.value),
  };
}

function decodeSignature(entry) {
  if (!entry) return null;
  const value = fields(entry.value);
  return text(first(value, 5, 2)) || null;
}

function decodeSymbol(buffer) {
  const value = fields(buffer);
  return {
    symbol: text(first(value, 1, 2)),
    documentation: all(value, 3, 2).map(text),
    relationships: all(value, 4, 2).map((entry) => decodeRelationship(entry.value)),
    kind: first(value, 5, 0)?.value || 0,
    displayName: text(first(value, 6, 2)),
    signature: decodeSignature(first(value, 7, 2)),
    enclosingSymbol: text(first(value, 8, 2)),
  };
}

export function decodeScipIndex(buffer) {
  const root = fields(Buffer.isBuffer(buffer) ? buffer : Buffer.from(buffer));
  return {
    documents: all(root, 2, 2).map((documentEntry) => {
      const value = fields(documentEntry.value);
      return {
        relativePath: text(first(value, 1, 2)),
        occurrences: all(value, 2, 2).map((entry) => decodeOccurrence(entry.value)),
        symbols: all(value, 3, 2).map((entry) => decodeSymbol(entry.value)),
        language: text(first(value, 4, 2)),
      };
    }),
    externalSymbols: all(root, 3, 2).map((entry) => decodeSymbol(entry.value)),
  };
}

export async function readScipIndex(path) {
  return decodeScipIndex(await readFile(path));
}

function displayName(symbol) {
  const normalized = String(symbol || '').replace(/[.#/()]+$/g, '');
  const segment = normalized.split(/[.#/]/).filter(Boolean).at(-1);
  return segment || symbol || 'symbol';
}

export function extractScipFacts(index, { scope = '', maxEntities = 5000, maxEdges = 15000 } = {}) {
  const candidates = [];
  const definitionsBySymbol = new Map();
  for (const document of index?.documents || []) {
    let path;
    try { path = normalizeRepoPath(document.relativePath); } catch { continue; }
    if (scope && path !== scope && !path.startsWith(`${scope.replace(/\/+$/, '')}/`)) continue;
    for (const occurrence of document.occurrences || []) {
      if ((occurrence.roles & 1) !== 0 && occurrence.symbol && occurrence.range) {
        if (!definitionsBySymbol.has(occurrence.symbol)) definitionsBySymbol.set(occurrence.symbol, []);
        definitionsBySymbol.get(occurrence.symbol).push({ path, range: occurrence.range });
      }
    }
    for (const symbol of document.symbols || []) {
      const definitions = definitionsBySymbol.get(symbol.symbol) || [];
      const definition = definitions.find((item) => item.path === path) || definitions[0] || null;
      const kind = KIND_NAMES.get(symbol.kind) || 'symbol';
      const label = symbol.displayName || displayName(symbol.symbol);
      candidates.push({
        scipSymbol: symbol.symbol,
        label,
        qualifiedName: symbol.symbol,
        kind,
        path,
        line: definition?.range?.line || null,
        endLine: definition?.range?.endLine || null,
        signature: symbol.signature,
        enclosingSymbol: symbol.enclosingSymbol || null,
        relationships: symbol.relationships || [],
        documentation: symbol.documentation || [],
      });
    }
  }
  candidates.sort((a, b) => a.path.localeCompare(b.path) || (a.line || 0) - (b.line || 0) || a.scipSymbol.localeCompare(b.scipSymbol));
  const kept = candidates.slice(0, maxEntities);
  const entityBySymbol = new Map();
  const entities = kept.map((item) => {
    const observedId = stableId('entity', item.path, item.scipSymbol, item.kind);
    const entity = {
      observedId,
      lineageId: observedId,
      boundaryId: null,
      label: item.label,
      qualifiedName: item.qualifiedName,
      kind: item.kind,
      path: item.path,
      line: item.line,
      endLine: item.endLine,
      signature: item.signature,
      community: null,
      isGodNode: false,
      confidence: 'exact',
      evidence: [`scip:symbol:${item.scipSymbol}`],
      externalIds: { scip: item.scipSymbol },
    };
    entityBySymbol.set(item.scipSymbol, entity);
    return entity;
  });

  const dependencies = [];
  const contracts = [];
  for (const item of kept) {
    const from = entityBySymbol.get(item.scipSymbol);
    if (!from) continue;
    if (item.signature && ['class', 'interface', 'function', 'method', 'constructor'].includes(item.kind)) {
      contracts.push({
        id: stableId('contract', 'signature', from.observedId, item.signature),
        entityRef: from.observedId,
        kind: 'signature',
        targetRef: null,
        signature: item.signature,
        confidence: 'exact',
        evidence: [`scip:signature:${item.scipSymbol}`],
      });
    }
    for (const relation of item.relationships) {
      const to = entityBySymbol.get(relation.symbol);
      if (!to) continue;
      const kinds = [];
      if (relation.isImplementation) kinds.push('implements');
      if (relation.isReference) kinds.push('references');
      if (relation.isTypeDefinition) kinds.push('type_definition');
      if (relation.isDefinition) kinds.push('definition');
      for (const kind of kinds) {
        const edge = {
          id: stableId('edge', from.observedId, to.observedId, kind),
          from: from.observedId,
          to: to.observedId,
          relation: kind,
          crossBoundary: false,
          crossCommunity: false,
          confidence: 'exact',
          evidence: [`scip:relationship:${item.scipSymbol}:${relation.symbol}`],
        };
        dependencies.push(edge);
        if (kind === 'implements') {
          contracts.push({
            id: stableId('contract', kind, from.observedId, to.observedId),
            entityRef: from.observedId,
            kind,
            targetRef: to.observedId,
            signature: item.signature,
            confidence: 'exact',
            evidence: edge.evidence,
          });
        }
      }
    }
  }
  const sortedDependencies = dependencies.sort((a, b) => a.id.localeCompare(b.id));
  const warnings = [];
  if (candidates.length > maxEntities) warnings.push(`SCIP entities truncated at ${maxEntities}`);
  if (sortedDependencies.length > maxEdges) warnings.push(`SCIP dependencies truncated at ${maxEdges}`);
  return {
    entities,
    contracts: contracts.sort((a, b) => a.id.localeCompare(b.id)),
    // сортировка до среза: иначе в выживших остаётся недетерминированное подмножество
    dependencies: sortedDependencies.slice(0, maxEdges),
    warnings,
    truncated: candidates.length > maxEntities || sortedDependencies.length > maxEdges,
    capabilities: {
      symbols: 'exact',
      exactKinds: true,
      sourceRanges: true,
      signatures: contracts.some((contract) => contract.signature),
      references: true,
      implementations: true,
      imports: false,
      calls: false,
      topology: false,
      positionEncoding: 'from-index',
    },
  };
}

// плоская карта — контракт provenance.providerCapabilities (см. схему facts):
// capability доступна, если её даёт хотя бы один провайдер; строковые уровни — лучший
function mergeCapabilities(graphCaps = {}, scipCaps = {}) {
  const rank = { none: 0, inferred: 1, extracted: 2, exact: 3 };
  const merged = { ...graphCaps };
  for (const [key, value] of Object.entries(scipCaps || {})) {
    const previous = merged[key];
    if (previous == null) merged[key] = value;
    else if (typeof value === 'boolean' && typeof previous === 'boolean') merged[key] = previous || value;
    else if (typeof value === 'string' && typeof previous === 'string') {
      merged[key] = (rank[value] || 0) >= (rank[previous] || 0) ? value : previous;
    } else if (value != null) merged[key] = value;
  }
  return merged;
}

/** Строгий ключ: путь + метка как есть. TS/JS регистрозависимы — `User` и `user` разные. */
function matchKey(entity) {
  return `${entity.path}\0${String(entity.label || '')}`;
}

/**
 * Нестрогий ключ на случай, когда SCIP и graphify по-разному нормализуют регистр метки.
 * Применяется ТОЛЬКО когда кандидат ровно один: раньше ключ был всегда нестрогим, и файл
 * с `class User` и `const user` схлопывался в одну сущность — SCIP-данные про `User`
 * приписывались `user`, вторая сущность исчезала из фактов, а рёбра на неё оставались
 * висеть на несуществующем observedId (валидация фактов ссылочную целостность не проверяла).
 */
function looseKey(entity) {
  return `${entity.path}\0${String(entity.label || '').toLowerCase()}`;
}

function indexEntities(entities, keyOf) {
  const index = new Map();
  for (const entity of entities) {
    const key = keyOf(entity);
    if (index.has(key)) index.get(key).push(entity);
    else index.set(key, [entity]);
  }
  return index;
}

export function mergeScipWithGraphify(scip, graph) {
  const graphEntities = graph.entities || [];
  const exactIndex = indexEntities(graphEntities, matchKey);
  const looseIndex = indexEntities(graphEntities, looseKey);
  const consumed = new Set();
  let ambiguous = 0;
  const replacedGraphIds = new Map();
  const entities = [];
  for (const scipEntity of scip.entities || []) {
    // строгое совпадение важнее регистронезависимого; неоднозначность — повод не сливать
    // вовсе, а не выбирать «последнего»: потеря сущности дороже несостоявшегося уточнения
    const exact = exactIndex.get(matchKey(scipEntity)) || [];
    const pool = (exact.length ? exact : looseIndex.get(looseKey(scipEntity)) || [])
      .filter((candidate) => !consumed.has(candidate));
    if (pool.length > 1) ambiguous++;
    const graphEntity = pool.length === 1 ? pool[0] : null;
    // null/undefined из SCIP не затирают graphify-поля: предпочитаем определённые значения,
    // неизвестные (новые) топологические поля graphify-сущности сохраняются
    const definedScip = Object.fromEntries(
      Object.entries(scipEntity).filter(([, value]) => value != null),
    );
    const merged = graphEntity ? {
      ...graphEntity,
      ...definedScip,
      boundaryId: graphEntity.boundaryId,
      community: graphEntity.community,
      isGodNode: graphEntity.isGodNode,
      evidence: [...new Set([...graphEntity.evidence, ...scipEntity.evidence])].sort(),
      externalIds: { ...graphEntity.externalIds, ...scipEntity.externalIds },
    } : scipEntity;
    if (graphEntity) {
      consumed.add(graphEntity);
      replacedGraphIds.set(graphEntity.observedId, merged.observedId);
    }
    entities.push(merged);
  }
  // выбывает ровно та сущность, которую слили, — не все делящие с ней ключ
  entities.push(...graphEntities.filter((entity) => !consumed.has(entity)));
  entities.sort((a, b) => a.path.localeCompare(b.path) || (a.line || 0) - (b.line || 0) || a.label.localeCompare(b.label));

  const rewriteEdge = (edge) => {
    const from = replacedGraphIds.get(edge.from) || edge.from;
    const to = replacedGraphIds.get(edge.to) || edge.to;
    // id выведен из (from,to,relation): после переписывания пересчитываем, иначе сортировка по id ломается
    return from === edge.from && to === edge.to ? edge : {
      ...edge,
      id: stableId('edge', from, to, edge.relation),
      from,
      to,
    };
  };
  const dependencyMap = new Map();
  for (const edge of [...(graph.dependencies || []).map(rewriteEdge), ...(scip.dependencies || [])]) {
    const key = `${edge.from}\0${edge.to}\0${edge.relation}`;
    const previous = dependencyMap.get(key);
    if (!previous || edge.confidence === 'exact') dependencyMap.set(key, edge);
  }
  const contractMap = new Map();
  for (const contract of [...(graph.contracts || []), ...(scip.contracts || [])]) {
    const rewritten = {
      ...contract,
      entityRef: replacedGraphIds.get(contract.entityRef) || contract.entityRef,
      targetRef: replacedGraphIds.get(contract.targetRef) || contract.targetRef,
    };
    if (rewritten.entityRef !== contract.entityRef || rewritten.targetRef !== contract.targetRef) {
      // id контракта выведен из refs — пересчитываем в формате producer'а
      rewritten.id = rewritten.kind === 'signature'
        ? stableId('contract', 'signature', rewritten.entityRef, rewritten.signature)
        : rewritten.kind === 'entrypoint'
          ? stableId('contract', 'entrypoint', rewritten.entityRef)
          : stableId('contract', rewritten.kind, rewritten.entityRef, rewritten.targetRef);
    }
    const key = `${rewritten.entityRef}\0${rewritten.kind}\0${rewritten.targetRef || ''}`;
    const previous = contractMap.get(key);
    if (!previous || rewritten.confidence === 'exact') contractMap.set(key, rewritten);
  }
  return {
    ...graph,
    entities,
    dependencies: [...dependencyMap.values()].sort((a, b) => a.id.localeCompare(b.id)),
    contracts: [...contractMap.values()].sort((a, b) => a.id.localeCompare(b.id)),
    warnings: [
      ...(graph.warnings || []),
      ...(scip.warnings || []),
      ...(ambiguous ? [`SCIP merge skipped for ${ambiguous} ambiguous label(s)`] : []),
    ],
    truncated: graph.truncated || scip.truncated,
    capabilities: mergeCapabilities(graph.capabilities, scip.capabilities),
  };
}
