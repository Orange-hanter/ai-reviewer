import { normalizeRepoPath, sha256 } from './structure-evidence.mjs';

export const STRUCTURE_PROTOCOL_VERSION = 1;
export const STRUCTURE_FACTS_SCHEMA = 'code-structure/structure-facts.v1';
export const STRUCTURE_ASSESSMENT_SCHEMA = 'code-structure/structure-assessment.v1';
export const OBLIGATIONS_SCHEMA = 'code-structure/obligations-export.v1';

export const DEFAULT_STRUCTURE_LIMITS = Object.freeze({
  maxBytes: 20 * 1024 * 1024,
  maxEntities: 5000,
  maxEdges: 15000,
  maxTests: 10000,
  maxObligations: 10000,
  maxPathLength: 1000,
  maxStringLength: 10000,
  maxDepth: 30,
});

export function assertPayloadSize(text, limits = DEFAULT_STRUCTURE_LIMITS) {
  const bytes = Buffer.byteLength(String(text || ''));
  if (bytes > limits.maxBytes) throw new Error(`structure payload exceeds ${limits.maxBytes} bytes`);
  return bytes;
}

function inspectValue(value, limits, depth = 0) {
  if (depth > limits.maxDepth) throw new Error(`structure payload nesting exceeds ${limits.maxDepth}`);
  if (typeof value === 'string' && value.length > limits.maxStringLength) {
    throw new Error(`structure payload string exceeds ${limits.maxStringLength} characters`);
  }
  if (Array.isArray(value)) {
    for (const item of value) inspectValue(item, limits, depth + 1);
  } else if (value && typeof value === 'object') {
    for (const item of Object.values(value)) inspectValue(item, limits, depth + 1);
  }
}

function assertKind(payload, kind, schemaRef) {
  if (!payload || typeof payload !== 'object' || Array.isArray(payload)) throw new Error(`${kind} payload must be an object`);
  if (payload.version !== STRUCTURE_PROTOCOL_VERSION) throw new Error(`unsupported ${kind} version: ${payload.version}`);
  if (payload.kind !== kind) throw new Error(`expected kind=${kind}`);
  if (payload.schemaRef !== schemaRef) throw new Error(`expected schemaRef=${schemaRef}`);
}

function assertPaths(records, pathField = 'path') {
  for (const record of records || []) {
    if (record?.[pathField] != null) normalizeRepoPath(record[pathField]);
  }
}

export function validateStructureFacts(payload, limits = DEFAULT_STRUCTURE_LIMITS) {
  assertKind(payload, 'structure_facts', STRUCTURE_FACTS_SCHEMA);
  inspectValue(payload, limits);
  const facts = payload.facts || {};
  if (!Array.isArray(facts.boundaries) || !Array.isArray(facts.entities)
    || !Array.isArray(facts.contracts) || !Array.isArray(facts.dependencies)
    || !Array.isArray(facts.testCases)) {
    throw new Error('structure facts arrays are required');
  }
  if (facts.entities.length > limits.maxEntities) throw new Error('entity limit exceeded');
  if (facts.dependencies.length > limits.maxEdges) throw new Error('dependency limit exceeded');
  if (facts.testCases.length > limits.maxTests) throw new Error('test case limit exceeded');
  assertPaths(facts.entities);
  assertPaths(facts.testCases);
  for (const boundary of facts.boundaries) {
    for (const path of boundary.paths || []) {
      if (normalizeRepoPath(path).length > limits.maxPathLength) throw new Error('path length limit exceeded');
    }
  }
  if (!/^[0-9a-f]{64}$/.test(String(payload.fingerprint || ''))) throw new Error('facts fingerprint must be sha256');
  return payload;
}

export function validateObligationsExport(payload, limits = DEFAULT_STRUCTURE_LIMITS) {
  assertKind(payload, 'obligations_export', OBLIGATIONS_SCHEMA);
  inspectValue(payload, limits);
  if (!Array.isArray(payload.obligations)) throw new Error('obligations array is required');
  if (payload.obligations.length > limits.maxObligations) throw new Error('obligation limit exceeded');
  for (const obligation of payload.obligations) {
    if (!obligation?.id || !obligation?.contentHash || !obligation?.statement) {
      throw new Error('obligation id, contentHash and statement are required');
    }
  }
  return payload;
}

export function validateStructureAssessment(payload, limits = DEFAULT_STRUCTURE_LIMITS) {
  assertKind(payload, 'structure_assessment', STRUCTURE_ASSESSMENT_SCHEMA);
  inspectValue(payload, limits);
  for (const field of ['fingerprint', 'factsFingerprint', 'coverageHash', 'thresholdsHash']) {
    if (!/^[0-9a-f]{64}$/.test(String(payload[field] || ''))) throw new Error(`${field} must be sha256`);
  }
  if (!['aligned', 'mismatch', 'unknown'].includes(payload.temporalAlignment)) {
    throw new Error('invalid temporalAlignment');
  }
  if (!Array.isArray(payload.coverageObservations)
    || !Array.isArray(payload.assessments?.contractScenarios)
    || !Array.isArray(payload.hints)
    || !Array.isArray(payload.receipts)) {
    throw new Error('assessment arrays are required');
  }
  return payload;
}

export function parseStructurePayload(text, limits = DEFAULT_STRUCTURE_LIMITS) {
  assertPayloadSize(text, limits);
  const payload = JSON.parse(text);
  if (payload?.kind === 'structure_facts') return validateStructureFacts(payload, limits);
  if (payload?.kind === 'structure_assessment') return validateStructureAssessment(payload, limits);
  if (payload?.kind === 'obligations_export') return validateObligationsExport(payload, limits);
  throw new Error(`unsupported structure payload kind: ${payload?.kind}`);
}

export function factsFingerprint({ headSha, providerVersion, configHash, scope }) {
  return sha256(['structure_facts', headSha, providerVersion, configHash, scope || ''].join('\0'));
}

export function assessmentFingerprint({ facts, obligationsRevision, coverageHash, thresholdsHash }) {
  return sha256([
    'structure_assessment',
    facts,
    obligationsRevision || 'none',
    coverageHash,
    thresholdsHash,
  ].join('\0'));
}

export function temporalAlignment({ factsHeadSha, testHeadSha, obligationsHeadSha }) {
  const facts = String(factsHeadSha || '');
  const test = String(testHeadSha || '');
  const docs = String(obligationsHeadSha || '');
  if (!facts || !test) return 'unknown';
  if (facts !== test) return 'mismatch';
  if (docs && docs !== facts) return 'mismatch';
  return 'aligned';
}
