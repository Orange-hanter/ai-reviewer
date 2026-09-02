import { createHash } from 'node:crypto';
import { posix } from 'node:path';

export const EVIDENCE_RECEIPT_VERSION = 1;
export const TRUST_TIERS = Object.freeze(['signed_ci', 'trusted_local', 'untrusted']);
export const EVIDENCE_CEILINGS = Object.freeze([
  'static_inventory',
  'aggregate_lcov',
  'per_test_execution',
  'assertion_bound',
  'mutation_bound',
  'property_bound',
  'contract_verified',
]);

const SHA256_RE = /^[0-9a-f]{64}$/;
const HEAD_SHA_RE = /^[0-9a-f]{7,64}$/i;

export function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.keys(value).sort().map((key) => (
      `${JSON.stringify(key)}:${canonicalJson(value[key])}`
    )).join(',')}}`;
  }
  return JSON.stringify(value);
}

export function sha256(value) {
  const data = typeof value === 'string' || Buffer.isBuffer(value)
    ? value
    : canonicalJson(value);
  return createHash('sha256').update(data).digest('hex');
}

export function stableId(prefix, ...parts) {
  return `${prefix}:${sha256(parts.map((part) => String(part ?? '')).join('\0')).slice(0, 24)}`;
}

export function normalizeRepoPath(value) {
  const raw = String(value || '').replaceAll('\\', '/').trim();
  if (!raw || raw.includes('\0') || raw.startsWith('/') || /^[A-Za-z]:\//.test(raw)) {
    throw new Error(`unsafe repository path: ${JSON.stringify(raw)}`);
  }
  const normalized = posix.normalize(raw).replace(/^\.\//, '');
  if (!normalized || normalized === '..' || normalized.startsWith('../')) {
    throw new Error(`unsafe repository path: ${JSON.stringify(raw)}`);
  }
  return normalized;
}

function strings(values, max = 100) {
  return [...new Set((values || []).map((value) => String(value).trim()).filter(Boolean))]
    .slice(0, max)
    .sort();
}

export function validateEvidenceReceipt(receipt) {
  const errors = [];
  if (!receipt || typeof receipt !== 'object' || Array.isArray(receipt)) return ['receipt must be an object'];
  if (receipt.version !== EVIDENCE_RECEIPT_VERSION) errors.push('version must be 1');
  for (const field of ['provider', 'toolVersion', 'capability', 'headSha', 'configHash', 'scopeHash', 'observedAt']) {
    if (!String(receipt[field] || '').trim()) errors.push(`${field} is required`);
  }
  if (!HEAD_SHA_RE.test(String(receipt.headSha || ''))) errors.push('headSha must be a git-like hex sha');
  if (!SHA256_RE.test(String(receipt.configHash || ''))) errors.push('configHash must be sha256');
  if (!SHA256_RE.test(String(receipt.scopeHash || ''))) errors.push('scopeHash must be sha256');
  if (!receipt.artifact || typeof receipt.artifact !== 'object') errors.push('artifact is required');
  else {
    if (!String(receipt.artifact.uri || '').trim()) errors.push('artifact.uri is required');
    if (!SHA256_RE.test(String(receipt.artifact.sha256 || ''))) errors.push('artifact.sha256 must be sha256');
  }
  if (!receipt.observation || typeof receipt.observation !== 'object') errors.push('observation is required');
  else if (!String(receipt.observation.type || '').trim()) errors.push('observation.type is required');
  if (!TRUST_TIERS.includes(receipt.trustTier)) errors.push('invalid trustTier');
  if (!EVIDENCE_CEILINGS.includes(receipt.evidenceCeiling)) errors.push('invalid evidenceCeiling');
  if (!Array.isArray(receipt.limitations)) errors.push('limitations must be an array');
  if (Number.isNaN(Date.parse(String(receipt.observedAt || '')))) errors.push('observedAt must be ISO-8601');
  for (const ref of receipt.subjectRefs || []) {
    if (!String(ref || '').trim()) errors.push('subjectRefs cannot contain empty values');
  }
  if (receipt.observation?.path != null) {
    try { normalizeRepoPath(receipt.observation.path); } catch (error) { errors.push(error.message); }
  }
  return errors;
}

export function createEvidenceReceipt(input) {
  const observation = { ...(input.observation || {}) };
  delete observation.covered;
  if (observation.path != null) observation.path = normalizeRepoPath(observation.path);
  const receipt = {
    version: EVIDENCE_RECEIPT_VERSION,
    provider: String(input.provider || '').trim(),
    toolVersion: String(input.toolVersion || '').trim(),
    capability: String(input.capability || '').trim(),
    headSha: String(input.headSha || '').trim(),
    testRunId: input.testRunId == null ? null : String(input.testRunId),
    configHash: String(input.configHash || ''),
    scopeHash: String(input.scopeHash || ''),
    subjectRefs: strings(input.subjectRefs),
    artifact: {
      uri: String(input.artifact?.uri || '').trim(),
      sha256: String(input.artifact?.sha256 || ''),
    },
    observation,
    trustTier: input.trustTier || 'trusted_local',
    evidenceCeiling: input.evidenceCeiling || 'static_inventory',
    limitations: strings(input.limitations, 50),
    complete: input.complete !== false,
    truncated: input.truncated === true,
    exitStatus: Number.isInteger(input.exitStatus) ? input.exitStatus : null,
    observedAt: input.observedAt || new Date().toISOString(),
  };
  const errors = validateEvidenceReceipt(receipt);
  if (errors.length) throw new Error(`invalid evidence receipt: ${errors.join('; ')}`);
  return receipt;
}

export function receiptRef(receipt) {
  return `receipt:${sha256(receipt)}`;
}
