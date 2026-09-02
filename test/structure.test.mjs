import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

import {
  analyzeStructure,
  buildStructureContext,
  handleStructureMcpRequest,
  createEvidenceReceipt,
  extractGraphifyFacts,
  extractScipFacts,
  generatePropertyDrafts,
  normalizeRepoPath,
  parseLcov,
  parseMutationReport,
  parseStructurePayload,
  reconcileIdentity,
  sha256,
  stableId,
  validateEvidenceReceipt,
  validateStructureAssessment,
  validateStructureFacts,
} from '../index.mjs';

const HEAD = 'abcdef1234567890';

const GRAPH = {
  nodes: [
    { id: 'api', label: 'validate()', file_type: 'code', source_file: 'src/auth.ts', source_location: 'L10', _callable: true, community: 1 },
    { id: 'caller1', label: 'login()', file_type: 'code', source_file: 'src/login.ts', source_location: 'L4', _callable: true, community: 2 },
    { id: 'caller2', label: 'refresh()', file_type: 'code', source_file: 'src/refresh.ts', source_location: 'L5', _callable: true, community: 2 },
    { id: 'iface', label: 'TokenValidator', file_type: 'code', source_file: 'src/contracts.ts', source_location: 'L1', _callable_class: true, community: 1 },
    { id: 'impl', label: 'JwtValidator', file_type: 'code', source_file: 'src/auth.ts', source_location: 'L2', _callable_class: true, community: 1 },
    { id: 'test', label: 'reject invalid token()', file_type: 'code', source_file: 'test/auth.test.ts', source_location: 'L3', _callable: true, community: 3 },
  ],
  links: [
    { source: 'caller1', target: 'api', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'caller2', target: 'api', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'test', target: 'api', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'impl', target: 'iface', relation: 'implements', confidence: 'EXTRACTED' },
  ],
};

const LCOV = `TN:
SF:src/auth.ts
FN:10,validate
FNDA:2,validate
DA:10,2
DA:11,0
BRDA:11,0,0,0
end_of_record
SF:src/login.ts
FN:4,login
FNDA:1,login
DA:4,1
end_of_record
`;

test('canonical protocol fixtures are accepted by runtime validators', async () => {
  for (const name of [
    'structure-facts.v1.json',
    'obligations-export.v1.json',
    'structure-assessment.v1.json',
  ]) {
    const text = await readFile(new URL(`../fixtures/structure/${name}`, import.meta.url), 'utf8');
    assert.equal(parseStructurePayload(text).version, 1);
  }
  for (const name of [
    'evidence-receipt.v1.schema.json',
    'structure-facts.v1.schema.json',
    'obligations-export.v1.schema.json',
    'structure-assessment.v1.schema.json',
  ]) {
    const schema = JSON.parse(await readFile(new URL(`../schemas/${name}`, import.meta.url), 'utf8'));
    assert.equal(schema.$schema, 'https://json-schema.org/draft/2020-12/schema');
    assert.ok(schema.$id);
  }
});

test('EvidenceReceipt strips agent-provided covered and validates provenance', () => {
  const receipt = createEvidenceReceipt({
    provider: 'unit',
    toolVersion: '1',
    capability: 'test',
    headSha: HEAD,
    configHash: sha256('config'),
    scopeHash: sha256('.'),
    artifact: { uri: 'memory:test', sha256: sha256('artifact') },
    observation: { type: 'sample', value: true, covered: true },
    trustTier: 'trusted_local',
    evidenceCeiling: 'static_inventory',
    limitations: ['test only'],
    observedAt: '2026-09-02T00:00:00.000Z',
  });
  assert.equal(receipt.observation.covered, undefined);
  assert.deepEqual(validateEvidenceReceipt(receipt), []);
  assert.match(assert.throws(() => createEvidenceReceipt({ ...receipt, headSha: 'bad' })).message, /headSha/);
});

test('repository path normalization rejects escapes and absolute paths', () => {
  assert.equal(normalizeRepoPath('./src/x.ts'), 'src/x.ts');
  assert.equal(normalizeRepoPath('.'), '.');
  assert.throws(() => normalizeRepoPath('../secret'));
  assert.throws(() => normalizeRepoPath('/etc/passwd'));
  assert.throws(() => normalizeRepoPath('C:\\secret'));
});

test('Graphify extraction builds entities, contracts, boundaries and typed dependencies', () => {
  const facts = extractGraphifyFacts(GRAPH, {
    layout: { appDir: 'src', layerDirs: ['auth'] },
    maxEntities: 100,
    maxEdges: 100,
  });
  assert.equal(facts.entities.length, 6);
  assert.ok(facts.dependencies.some((edge) => edge.relation === 'implements'));
  assert.ok(facts.contracts.some((contract) => contract.kind === 'implements'));
  assert.ok(facts.contracts.some((contract) => contract.kind === 'entrypoint'));
  assert.ok(facts.entities.find((entity) => entity.label === 'validate()').kind === 'function');
});

test('LCOV parser preserves exact file/function/branch observations', () => {
  const parsed = parseLcov(LCOV, { repoRoot: '/repo' });
  assert.equal(parsed.totals.lines.found, 3);
  assert.equal(parsed.totals.lines.hit, 2);
  assert.equal(parsed.totals.functions.found, 2);
  assert.equal(parsed.totals.functions.hit, 2);
  assert.equal(parsed.byPath.get('src/auth.ts').functions.entries[0].hits, 2);
  assert.equal(parsed.byPath.get('src/auth.ts').branches.hit, 0);
});

test('SCIP semantic facts provide exact kind/range/signature without replacing calls', () => {
  const scip = extractScipFacts({
    documents: [{
      relativePath: 'src/auth.ts',
      language: 'typescript',
      occurrences: [{
        symbol: 'npm pkg 1 src/auth.ts/validate().',
        roles: 1,
        range: { line: 10, endLine: 12 },
      }],
      symbols: [{
        symbol: 'npm pkg 1 src/auth.ts/validate().',
        displayName: 'validate',
        kind: 17,
        signature: 'function validate(token: string): boolean',
        relationships: [],
      }],
    }],
  });
  assert.equal(scip.entities[0].kind, 'function');
  assert.equal(scip.entities[0].line, 10);
  assert.equal(scip.entities[0].endLine, 12);
  assert.equal(scip.entities[0].confidence, 'exact');
  assert.equal(scip.dependencies.length, 0, 'SCIP occurrences are not silently treated as calls');
});

test('identity reconciliation carries lineage across a move', () => {
  const previous = [{
    observedId: 'old',
    lineageId: 'lineage',
    path: 'src/old.ts',
    label: 'run',
    qualifiedName: 'pkg/run',
    kind: 'function',
    signature: 'run(): void',
  }];
  const current = [{
    observedId: 'new',
    lineageId: 'new',
    path: 'src/new.ts',
    label: 'run',
    qualifiedName: 'pkg/run',
    kind: 'function',
    signature: 'run(): void',
  }];
  const result = reconcileIdentity(current, previous);
  assert.equal(result.entities[0].lineageId, 'lineage');
  assert.equal(result.events[0].kind, 'moved');
});

test('end-to-end analysis caps error scenario at partial under aggregate LCOV', async () => {
  const baseFacts = extractGraphifyFacts(GRAPH);
  const api = baseFacts.entities.find((entity) => entity.label === 'validate()');
  const contractId = stableId('contract', 'entrypoint', api.observedId);
  const obligation = {
    id: 'obl:auth#invalid-token',
    source: 'cod-doc-section',
    docRef: 'modules/auth/contracts',
    sectionAnchor: 'invalid-token',
    contentHash: sha256('MUST reject invalid token'),
    priority: 'must',
    kind: 'error_path',
    statement: 'MUST reject invalid token',
    status: 'confirmed',
    contractRefs: [contractId],
    provenance: 'manual',
  };
  const mutationReport = {
    schemaVersion: '1',
    files: {
      'src/auth.ts': {
        mutants: [{
          id: 'm1',
          mutatorName: 'ConditionalExpression',
          replacement: 'true',
          status: 'Survived',
          location: { start: { line: 10, column: 0 }, end: { line: 10, column: 8 } },
          killedByTests: [],
        }],
      },
    },
  };
  const source = `import { test } from 'node:test';
test('reject invalid token', () => {
  expect(() => validate('bad')).toThrow();
});`;
  const result = await analyzeStructure({
    graphText: JSON.stringify(GRAPH),
    graphReportText: `Built from commit: \`${HEAD}\``,
    lcovText: LCOV,
    mutationReport,
    obligationsExport: {
      schemaRef: 'code-structure/obligations-export.v1',
      version: 1,
      kind: 'obligations_export',
      project: 'demo',
      revision: 'docs-1',
      headSha: HEAD,
      exportedAt: '2026-09-02T00:00:00.000Z',
      obligations: [obligation],
    },
    repoRoot: '/repo',
    headSha: HEAD,
    branchRef: 'main',
    testHeadSha: HEAD,
    generatedAt: '2026-09-02T00:00:00.000Z',
    thresholds: { highFanIn: 2 },
  }, {
    readFile: async (path) => {
      if (String(path).endsWith('test/auth.test.ts')) return source;
      throw Object.assign(new Error('not found'), { code: 'ENOENT' });
    },
  });
  validateStructureFacts(result.facts);
  validateStructureAssessment(result.assessment);
  const scenario = result.assessment.assessments.contractScenarios[0];
  assert.equal(scenario.status, 'partial');
  assert.ok(scenario.missingEvidence.includes('per_test_evidence'));
  assert.ok(result.assessment.hints.some((hint) => hint.ruleId === 'scenario.surviving_contract_mutant'));
  assert.ok(result.propertyDrafts.some((draft) => draft.obligationRef === obligation.id));
});

test('mutation report binds critical mutant to a contract, not aggregate score', () => {
  const graph = extractGraphifyFacts(GRAPH);
  const api = graph.entities.find((entity) => entity.label === 'validate()');
  const contract = graph.contracts.find((item) => item.entityRef === api.observedId);
  const parsed = parseMutationReport({
    files: {
      'src/auth.ts': {
        mutants: [{
          id: '1',
          mutatorName: 'EqualityOperator',
          status: 'Killed',
          location: { start: { line: 10 }, end: { line: 10 } },
          killedByTests: ['test/auth.test.ts:reject'],
        }],
      },
    },
  }, graph, { scopeContractRefs: [contract.id] });
  assert.equal(parsed.observations[0].subjectRef, contract.id);
  assert.equal(parsed.observations[0].mutationStatus, 'killed');
  assert.deepEqual(parsed.observations[0].killedBy, ['test/auth.test.ts:reject']);
});

test('property drafts are generated only for confirmed edge obligations', () => {
  const obligations = [{
    id: 'obl:range',
    kind: 'boundary_value',
    status: 'confirmed',
    statement: 'MUST accept values from 0 through 10',
    contentHash: sha256('range'),
  }, {
    id: 'obl:draft',
    kind: 'invariant',
    status: 'draft',
    statement: 'draft',
    contentHash: sha256('draft'),
  }];
  const drafts = generatePropertyDrafts(obligations);
  assert.equal(drafts.length, 1);
  assert.ok(drafts[0].generators.includes('below_minimum'));
  assert.equal(drafts[0].requiresConfirmation, true);
});

test('structure_context requires and preserves a pinned snapshot', async () => {
  const result = await analyzeStructure({
    graphText: JSON.stringify(GRAPH),
    graphReportText: `Built from commit: \`${HEAD}\``,
    repoRoot: '/repo',
    headSha: HEAD,
    branchRef: 'main',
    generatedAt: '2026-09-02T00:00:00.000Z',
  }, { readFile: async () => { throw new Error('no files'); } });
  assert.throws(() => buildStructureContext({ facts: result.facts, assessment: result.assessment }), /pinned/);
  const api = result.facts.facts.entities.find((entity) => entity.label === 'validate()');
  const context = buildStructureContext({
    facts: result.facts,
    assessment: result.assessment,
    headSha: HEAD,
    scopeRefs: [api.observedId],
    dependencyDepth: 1,
  });
  assert.equal(context.pinned.headSha, HEAD);
  assert.ok(context.entities.some((entity) => entity.observedId === api.observedId));
  assert.ok(context.dependencies.length >= 1);
});

test('MCP advertises structure_context and rejects unpinned calls', async () => {
  const listed = await handleStructureMcpRequest({
    jsonrpc: '2.0',
    id: 1,
    method: 'tools/list',
  });
  assert.equal(listed.result.tools[0].name, 'structure_context');
  const called = await handleStructureMcpRequest({
    jsonrpc: '2.0',
    id: 2,
    method: 'tools/call',
    params: {
      name: 'structure_context',
      arguments: {
        factsPath: 'facts.json',
        assessmentPath: 'assessment.json',
      },
    },
  });
  assert.equal(called.error.code, -32602);
  assert.match(called.error.message, /headSha or snapshotFingerprint/);
});
