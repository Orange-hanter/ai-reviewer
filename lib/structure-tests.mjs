import { stableId } from './structure-evidence.mjs';

const TEST_DECLARATION_RE = /\b(?:it|test|specify)\s*\(\s*(['"`])(.+?)\1/g;
const ASSERTION_PATTERNS = [
  ['throws', /\b(?:toThrow|throws|assert\.throws)\b/],
  ['rejects', /\b(?:rejects|assert\.rejects)\b/],
  ['http_status', /\b(?:status|statusCode|toHaveStatus)\s*(?:\(|[=:])\s*(?:4\d\d|5\d\d)/],
  ['equality', /\b(?:toEqual|toBe|deepEqual|strictEqual|assert\.equal)\b/],
  ['property', /\b(?:fc\.assert|property|forall|forAll)\b/],
];

function frameworkForPath(path, source) {
  if (/\bvitest\b/.test(source)) return 'vitest';
  if (/\bjest\b/.test(source)) return 'jest';
  if (/\bnode:test\b/.test(source)) return 'node:test';
  if (/\.spec\./.test(path)) return 'unknown-spec';
  return 'unknown-test';
}

function lineAt(source, offset) {
  return source.slice(0, offset).split('\n').length;
}

function assertionsIn(source) {
  return ASSERTION_PATTERNS
    .filter(([, pattern]) => pattern.test(source))
    .map(([kind]) => ({ kind, confidence: 'heuristic' }));
}

function normalizedLabel(value) {
  return String(value || '').replace(/\(\)+$/, '').replace(/^\./, '').toLowerCase();
}

export function extractTestCasesFromSource(path, source, contracts = [], entities = []) {
  // label контракта может отсутствовать (SCIP-контракты: entityRef/kind/signature) —
  // выводим из сущности по entityRef
  const labelByEntityRef = new Map((entities || []).map((entity) => [entity.observedId, entity.label]));
  // Матчеры компилируются один раз на контракт, а не на каждую пару тест x контракт: на
  // большом репозитории это были миллионы компиляций одного и того же выражения. Флага 'g'
  // нет, поэтому у RegExp не остаётся состояния и экземпляр безопасно переиспользовать.
  const contractMatchers = [];
  for (const contract of contracts || []) {
    const label = normalizedLabel(
      contract.label || contract.entityLabel || labelByEntityRef.get(contract.entityRef) || '',
    );
    if (!label) continue;
    contractMatchers.push({
      id: contract.id,
      pattern: new RegExp(`\\b${label.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\b`, 'i'),
    });
  }
  const out = [];
  TEST_DECLARATION_RE.lastIndex = 0;
  let match;
  while ((match = TEST_DECLARATION_RE.exec(source))) {
    const start = match.index;
    const next = source.slice(TEST_DECLARATION_RE.lastIndex).search(TEST_DECLARATION_RE);
    const end = next >= 0 ? TEST_DECLARATION_RE.lastIndex + next : Math.min(source.length, start + 6000);
    const body = source.slice(start, end);
    const targetContractRefs = [];
    for (const matcher of contractMatchers) {
      if (matcher.pattern.test(body)) targetContractRefs.push(matcher.id);
    }
    const name = match[2].trim();
    const line = lineAt(source, start);
    out.push({
      id: stableId('test', path, line, name),
      name,
      path,
      line,
      framework: frameworkForPath(path, source),
      targetContractRefs: [...new Set(targetContractRefs)].sort(),
      assertions: assertionsIn(body),
      confidence: 'heuristic',
      evidence: [`source:test-declaration:${path}:${line}`],
    });
  }
  return out;
}

export function mergeTestCases(...groups) {
  const byId = new Map();
  for (const testCase of groups.flat()) {
    const previous = byId.get(testCase.id);
    if (!previous) {
      byId.set(testCase.id, testCase);
      continue;
    }
    byId.set(testCase.id, {
      ...previous,
      ...testCase,
      targetContractRefs: [...new Set([
        ...(previous.targetContractRefs || []),
        ...(testCase.targetContractRefs || []),
      ])].sort(),
      assertions: [...(previous.assertions || []), ...(testCase.assertions || [])]
        .filter((item, index, all) => all.findIndex((candidate) => candidate.kind === item.kind) === index),
      evidence: [...new Set([...(previous.evidence || []), ...(testCase.evidence || [])])].sort(),
    });
  }
  return [...byId.values()].sort((a, b) => a.path.localeCompare(b.path) || a.line - b.line || a.name.localeCompare(b.name));
}

export function generatePropertyDrafts(obligations) {
  return (obligations || [])
    .filter((obligation) => obligation.status === 'confirmed'
      && ['boundary_value', 'invariant', 'error_path'].includes(obligation.kind))
    .map((obligation) => {
      const generators = obligation.kind === 'boundary_value'
        ? ['minimum', 'maximum', 'below_minimum', 'above_maximum', 'empty']
        : obligation.kind === 'error_path'
          ? ['invalid', 'empty', 'malformed']
          : ['valid_domain'];
      return {
        id: stableId('property', obligation.id, obligation.contentHash),
        obligationRef: obligation.id,
        status: 'draft',
        kind: obligation.kind,
        statement: obligation.statement,
        preconditions: [],
        generators,
        invariant: obligation.statement,
        expectedFailures: obligation.kind === 'error_path' ? ['invalid_input_rejected'] : [],
        semanticHash: obligation.contentHash,
        requiresConfirmation: true,
        evidence: [`obligation:${obligation.id}:${obligation.contentHash}`],
      };
    })
    .sort((a, b) => a.id.localeCompare(b.id));
}

export function parsePropertyReport(report, drafts = []) {
  const byId = new Map(drafts.map((draft) => [draft.id, draft]));
  return (report?.properties || []).map((property) => {
    const draft = byId.get(property.propertyId);
    const stale = !draft || draft.semanticHash !== property.semanticHash;
    return {
      propertyId: property.propertyId,
      obligationRef: draft?.obligationRef || property.obligationRef || null,
      status: stale ? 'stale' : (property.failed ? 'failed' : 'passed'),
      numRuns: Number(property.numRuns || 0),
      discardCount: Number(property.discardCount || 0),
      seed: property.seed == null ? null : String(property.seed),
      shrinkPath: property.shrinkPath == null ? null : String(property.shrinkPath),
      counterexample: property.counterexample ?? null,
      semanticHash: property.semanticHash || '',
      confidence: stale ? 'unresolved' : 'exact',
      evidence: [`property-report:${property.propertyId}`],
    };
  }).sort((a, b) => a.propertyId.localeCompare(b.propertyId));
}
