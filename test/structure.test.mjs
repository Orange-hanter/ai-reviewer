import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  analyzeStructure,
  applyStructureWaivers,
  buildStructureAssessment,
  buildStructureContext,
  callStructureContext,
  computeStructureDrift,
  handleStructureMcpRequest,
  createEvidenceReceipt,
  decodeScipIndex,
  extractGraphifyFacts,
  extractScipFacts,
  extractTestCasesFromSource,
  generatePropertyDrafts,
  loadReviewConfig,
  matchLcovFunction,
  mergeScipWithGraphify,
  normalizeRepoPath,
  parseLcov,
  parseMutationReport,
  parseStructurePayload,
  reconcileIdentity,
  suggestObligationLinks,
  triageStructureFindings,
  sha256,
  stableId,
  validateEvidenceReceipt,
  validateStructureAssessment,
  validateStructureFacts,
  writeStructureExports,
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
  assert.throws(() => createEvidenceReceipt({ ...receipt, headSha: 'bad' }), /headSha/);
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
    obligationsExport: makeObligationsExport([obligation]),
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

test('drift suggestions, stable findings and expiring waivers preserve facts', async () => {
  const result = await analyzeStructure({
    graphText: JSON.stringify(GRAPH),
    graphReportText: `Built from commit: \`${HEAD}\``,
    repoRoot: '/repo',
    headSha: HEAD,
    branchRef: 'main',
    generatedAt: '2026-09-02T00:00:00.000Z',
  }, { readFile: async () => { throw new Error('no files'); } });
  const obligation = {
    id: 'obl:validate',
    source: 'cod-doc-section',
    contentHash: sha256('validate'),
    priority: 'must',
    kind: 'happy_path',
    statement: 'validate MUST accept valid tokens',
    status: 'confirmed',
    contractRefs: [],
  };
  const suggestions = suggestObligationLinks([obligation], result.facts.facts);
  assert.ok(suggestions[0].candidates.length >= 1);
  const drift = computeStructureDrift({
    facts: result.facts,
    assessment: result.assessment,
    obligationsExport: makeObligationsExport([obligation], { revision: '1' }),
    linkSuggestions: suggestions,
  });
  const unlinked = drift.findings.find((item) => item.ruleId === 'docs.obligation_unlinked');
  assert.ok(unlinked);
  const waived = applyStructureWaivers(drift.findings, [{
    findingFingerprint: unlinked.fingerprint,
    owner: 'team',
    reason: 'legacy',
    expiresAt: '2026-10-01T00:00:00.000Z',
  }], new Date('2026-09-02T00:00:00.000Z'));
  assert.equal(waived.find((item) => item.fingerprint === unlinked.fingerprint).suppressed, true);
  assert.equal(triageStructureFindings(waived).some((item) => item.fingerprint === unlinked.fingerprint), false);
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

function makeObligationsExport(obligations, extras = {}) {
  return {
    schemaRef: 'code-structure/obligations-export.v1',
    version: 1,
    kind: 'obligations_export',
    project: 'demo',
    revision: 'docs-1',
    headSha: HEAD,
    exportedAt: '2026-09-02T00:00:00.000Z',
    obligations,
    ...extras,
  };
}

function obligationFor(api, extras = {}) {
  const contractId = stableId('contract', 'entrypoint', api.observedId);
  return {
    id: 'obl:auth#token',
    source: 'cod-doc-section',
    docRef: 'modules/auth/contracts',
    sectionAnchor: 'token',
    contentHash: sha256('token obligation'),
    priority: 'must',
    kind: 'happy_path',
    statement: 'MUST accept a valid token',
    status: 'confirmed',
    contractRefs: [contractId],
    provenance: 'manual',
    ...extras,
  };
}

async function analyzeWith(overrides = {}, deps = {}) {
  const api = extractGraphifyFacts(GRAPH).entities.find((entity) => entity.label === 'validate()');
  return analyzeStructure({
    graphText: JSON.stringify(GRAPH),
    graphReportText: `Built from commit: \`${HEAD}\``,
    lcovText: LCOV,
    obligationsExport: makeObligationsExport([obligationFor(api)]),
    repoRoot: '/repo',
    headSha: HEAD,
    branchRef: 'main',
    testHeadSha: HEAD,
    generatedAt: '2026-09-02T00:00:00.000Z',
    thresholds: { highFanIn: 2 },
    ...overrides,
  }, {
    readFile: async (path) => {
      if (String(path).endsWith('test/auth.test.ts')) {
        return `import { test } from 'node:test';
test('accepts a valid token', () => {
  expect(validate('good')).toEqual(true);
});`;
      }
      throw Object.assign(new Error('not found'), { code: 'ENOENT' });
    },
    ...deps,
  });
}

test('coverage commit mismatch keeps scenarios unverifiable', async () => {
  const result = await analyzeWith({ testHeadSha: 'ffffffffffffffff' });
  const scenario = result.assessment.assessments.contractScenarios[0];
  assert.equal(result.assessment.temporalAlignment, 'mismatch');
  assert.equal(scenario.status, 'unverifiable');
  assert.ok(scenario.missingEvidence.includes('temporal_alignment'));
});

test('unresolved contract links stay unverifiable, never missing', async () => {
  const api = extractGraphifyFacts(GRAPH).entities.find((entity) => entity.label === 'validate()');
  const result = await analyzeWith({
    obligationsExport: makeObligationsExport([obligationFor(api, { contractRefs: [] })]),
  });
  const scenario = result.assessment.assessments.contractScenarios[0];
  assert.equal(scenario.contractResolution, 'unresolved');
  assert.equal(scenario.status, 'unverifiable');
  assert.notEqual(scenario.status, 'missing');
});

test('FNDA>0 without a linked test is partial, never covered', async () => {
  const result = await analyzeWith({}, {
    readFile: async () => {
      throw Object.assign(new Error('not found'), { code: 'ENOENT' });
    },
  });
  const scenario = result.assessment.assessments.contractScenarios[0];
  assert.equal(scenario.status, 'partial');
  assert.ok(scenario.missingEvidence.includes('linked_test'));
  assert.notEqual(scenario.status, 'covered');
});

test('happy_path is covered only with linked test, execution and assertion', async () => {
  const result = await analyzeWith();
  const scenario = result.assessment.assessments.contractScenarios[0];
  assert.equal(scenario.status, 'covered');
  assert.match(scenario.statusReason, /test link, execution and scenario-specific evidence/);
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

async function analyzeGraph(graph = GRAPH) {
  return analyzeStructure({
    graphText: JSON.stringify(graph),
    graphReportText: `Built from commit: \`${HEAD}\``,
    repoRoot: '/repo',
    headSha: HEAD,
    branchRef: 'main',
    generatedAt: '2026-09-02T00:00:00.000Z',
  }, { readFile: async () => { throw new Error('no files'); } });
}

function parseMcpFrames(buffer) {
  const messages = [];
  let rest = buffer;
  while (true) {
    const headerEnd = rest.indexOf('\r\n\r\n');
    if (headerEnd < 0) break;
    const header = rest.subarray(0, headerEnd).toString('latin1');
    const match = /content-length:\s*(\d+)/i.exec(header);
    assert.ok(match, `missing Content-Length in frame: ${header}`);
    const length = Number(match[1]);
    assert.ok(rest.length >= headerEnd + 4 + length, 'truncated frame body');
    messages.push(JSON.parse(rest.subarray(headerEnd + 4, headerEnd + 4 + length).toString('utf8')));
    rest = rest.subarray(headerEnd + 4 + length);
  }
  assert.equal(rest.length, 0, 'unparsed trailing bytes');
  return messages;
}

async function runMcpServer(payloads, { split = false } = {}) {
  const child = spawn(process.execPath, [
    fileURLToPath(new URL('../bin/pr-review-structure-mcp.mjs', import.meta.url)),
  ]);
  // watchdog: зависший child не должен вешать прогон тестов
  const watchdog = setTimeout(() => child.kill('SIGKILL'), 10000);
  const stdout = [];
  const stderr = [];
  child.stdout.on('data', (chunk) => stdout.push(chunk));
  child.stderr.on('data', (chunk) => stderr.push(chunk));
  for (const payload of payloads) {
    if (Buffer.isBuffer(payload)) { // готовый сырой кадр (например oversized)
      child.stdin.write(payload);
      continue;
    }
    const body = Buffer.from(typeof payload === 'string' ? payload : JSON.stringify(payload));
    // регистр заголовка произвольный, неизвестный заголовок должен игнорироваться
    const framed = Buffer.concat([
      Buffer.from(`content-length: ${body.length}\r\nX-Unknown: ignored\r\n\r\n`),
      body,
    ]);
    if (split) {
      const mid = Math.floor(framed.length / 2);
      child.stdin.write(framed.subarray(0, mid));
      await new Promise((resolvePromise) => setTimeout(resolvePromise, 50));
      child.stdin.write(framed.subarray(mid));
    } else {
      child.stdin.write(framed);
    }
  }
  child.stdin.end();
  const exitCode = await new Promise((resolvePromise) => child.on('close', resolvePromise));
  clearTimeout(watchdog);
  assert.equal(exitCode, 0, Buffer.concat(stderr).toString());
  return parseMcpFrames(Buffer.concat(stdout));
}

test('MCP stdio transport: Content-Length framing, partial reads, batches, invalid requests', async () => {
  const [single, batch, emptyBatch, scalar, broken] = await runMcpServer([
    { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
    [{ jsonrpc: '2.0', id: 2, method: 'ping' }, 42, { jsonrpc: '2.0', id: 3, method: 'tools/list' }],
    [],
    42,
    '{not json',
  ], { split: true });
  assert.equal(single.id, 1);
  assert.equal(single.result.serverInfo.name, 'ai-reviewer-structure');
  assert.ok(Array.isArray(batch));
  assert.equal(batch.length, 3);
  assert.deepEqual(batch[0], { jsonrpc: '2.0', id: 2, result: {} });
  assert.equal(batch[1].error.code, -32600);
  assert.equal(batch[2].result.tools[0].name, 'structure_context');
  assert.equal(emptyBatch.error.code, -32600);
  assert.equal(scalar.error.code, -32600);
  assert.equal(broken.error.code, -32700);
});

test('structure_context clamps dependencyDepth to the configured maxDepth', async () => {
  const chainGraph = {
    nodes: [...GRAPH.nodes, {
      id: 'deep', label: 'audit()', file_type: 'code',
      source_file: 'src/audit.ts', source_location: 'L1', _callable: true, community: 2,
    }],
    links: [...GRAPH.links, { source: 'deep', target: 'caller1', relation: 'calls', confidence: 'EXTRACTED' }],
  };
  const result = await analyzeGraph(chainGraph);
  const api = result.facts.facts.entities.find((entity) => entity.label === 'validate()');
  const build = (limits, depth = 3) => buildStructureContext({
    facts: result.facts,
    assessment: result.assessment,
    headSha: HEAD,
    scopeRefs: [api.observedId],
    dependencyDepth: depth,
    limits,
  });
  const clamped = build({});
  // clamp реально срабатывает: запрошенная глубина 3 схлопывается до дефолтной 1
  const depthOne = build({}, 1);
  assert.deepEqual(
    clamped.entities.map((entity) => entity.observedId),
    depthOne.entities.map((entity) => entity.observedId),
  );
  assert.deepEqual(clamped.dependencies, depthOne.dependencies);
  assert.ok(!clamped.entities.some((entity) => entity.label === 'audit()'),
    'depth 2 neighbour must stay out of the default maxDepth=1 context');
  const raised = build({ maxDepth: 2 });
  assert.ok(raised.entities.some((entity) => entity.label === 'audit()'));
});

test('structure_context does not duplicate edges between selected seeds', async () => {
  const result = await analyzeGraph();
  const api = result.facts.facts.entities.find((entity) => entity.label === 'validate()');
  const caller = result.facts.facts.entities.find((entity) => entity.label === 'login()');
  const context = buildStructureContext({
    facts: result.facts,
    assessment: result.assessment,
    headSha: HEAD,
    scopeRefs: [api.observedId, caller.observedId],
    dependencyDepth: 1,
  });
  const ids = context.dependencies.map((edge) => edge.id);
  assert.equal(new Set(ids).size, ids.length);
});

test('assessment coverageHash discriminates lcov payloads without a hash field', async () => {
  const { facts } = await analyzeGraph();
  const base = { factsPayload: facts, generatedAt: '2026-09-02T00:00:00.000Z' };
  const first = buildStructureAssessment({ ...base, lcov: { totals: { lines: { found: 1, hit: 1 } }, files: [] } });
  const second = buildStructureAssessment({ ...base, lcov: { totals: { lines: { found: 2, hit: 1 } }, files: [] } });
  const absent = buildStructureAssessment(base);
  assert.notEqual(first.coverageHash, second.coverageHash);
  assert.notEqual(first.coverageHash, sha256('none'));
  assert.equal(absent.coverageHash, sha256('none'));
  assert.notEqual(first.fingerprint, second.fingerprint);
});

test('structure_context MCP reads are confined to STRUCTURE_MCP_ROOT', async (t) => {
  const { facts, assessment } = await analyzeGraph();
  const root = await mkdtemp(join(tmpdir(), 'structure-mcp-'));
  const outside = await mkdtemp(join(tmpdir(), 'structure-mcp-outside-'));
  await writeFile(join(root, 'facts.json'), JSON.stringify(facts));
  await writeFile(join(root, 'assessment.json'), JSON.stringify(assessment));
  await writeFile(join(outside, 'facts.json'), JSON.stringify(facts));
  // symlink внутри root → файл снаружи (обход lexical-проверки) и легитимный alias внутри root
  await symlink(join(outside, 'facts.json'), join(root, 'linked-facts.json'));
  await symlink(join(root, 'facts.json'), join(root, 'alias-facts.json'));
  const previous = process.env.STRUCTURE_MCP_ROOT;
  process.env.STRUCTURE_MCP_ROOT = root;
  t.after(async () => {
    if (previous === undefined) delete process.env.STRUCTURE_MCP_ROOT;
    else process.env.STRUCTURE_MCP_ROOT = previous;
    await rm(root, { recursive: true, force: true });
    await rm(outside, { recursive: true, force: true });
  });
  const api = facts.facts.entities.find((entity) => entity.label === 'validate()');
  const call = (factsPath, assessmentPath = 'assessment.json') => callStructureContext({
    factsPath,
    assessmentPath,
    headSha: HEAD,
    scopeRefs: [api.observedId],
  });
  const context = await call('facts.json');
  assert.equal(context.kind, 'structure_context');
  assert.equal((await call('alias-facts.json')).kind, 'structure_context', 'symlink внутри root легитимен');
  await assert.rejects(() => call('../facts.json'), /escapes allowed root/);
  await assert.rejects(() => call('/etc/passwd'), /escapes allowed root/);
  await assert.rejects(() => call('facts.json', '../assessment.json'), /escapes allowed root/);
  await assert.rejects(() => call('linked-facts.json'), /escapes allowed root/,
    'symlink, указывающий за пределы root, отклоняется');
});

test('writeStructureExports confines writes to the allowed root', async (t) => {
  const result = await analyzeGraph();
  const root = await mkdtemp(join(tmpdir(), 'structure-export-'));
  const outside = await mkdtemp(join(tmpdir(), 'structure-export-outside-'));
  // symlink-каталог внутри root, указывающий наружу
  await symlink(outside, join(root, 'linked'));
  t.after(async () => {
    await rm(root, { recursive: true, force: true });
    await rm(outside, { recursive: true, force: true });
  });
  const written = await writeStructureExports(result, {
    factsPath: join(root, 'facts.json'),
    assessmentPath: join(root, 'nested', 'assessment.json'),
    rootDir: root,
  });
  assert.equal(written.length, 2);
  assert.ok(written.every((path) => path.startsWith(`${root}${sep}`)));
  await assert.rejects(() => writeStructureExports(result, {
    factsPath: join(root, '..', 'escape.json'),
    rootDir: root,
  }), /escapes allowed root/);
  await assert.rejects(() => writeStructureExports(result, {
    assessmentPath: join(root, '..', 'escape-assessment.json'),
    rootDir: root,
  }), /escapes allowed root/);
  await assert.rejects(() => writeStructureExports(result, {
    markdownPath: join(tmpdir(), 'escape.md'),
    rootDir: root,
  }), /escapes allowed root/);
  await assert.rejects(() => writeStructureExports(result, {
    factsPath: join(root, 'linked', 'facts.json'),
    rootDir: root,
  }), /escapes allowed root/);
});

test('drift finding fingerprints are independent of subjectRefs order', async () => {
  const { facts, assessment } = await analyzeGraph();
  const driftWith = (contractRefs) => computeStructureDrift({
    facts,
    assessment,
    obligationsExport: makeObligationsExport([{
      id: 'obl:broken',
      status: 'confirmed',
      kind: 'happy_path',
      statement: 'MUST keep links valid',
      contentHash: sha256('broken'),
      contractRefs,
    }], { revision: '1' }),
    linkSuggestions: [],
  }).findings.find((item) => item.ruleId === 'docs.contract_link_broken');
  const first = driftWith(['contract:a', 'contract:b']);
  const second = driftWith(['contract:b', 'contract:a']);
  assert.ok(first);
  assert.equal(first.fingerprint, second.fingerprint);
  assert.deepEqual(first.subjectRefs, second.subjectRefs);
});

test('entrypoint contract dedupe uses the same key format as edge contracts', () => {
  // два graph-узла с одинаковым observedId (дубликаты в выгрузке graphify)
  const facts = extractGraphifyFacts({
    nodes: [
      { id: 'dup1', label: 'run()', file_type: 'code', source_file: 'src/run.ts', source_location: 'L10', _callable: true },
      { id: 'dup2', label: 'run()', file_type: 'code', source_file: 'src/run.ts', source_location: 'L10', _callable: true },
    ],
    links: [],
  });
  const entrypoints = facts.contracts.filter((contract) => contract.kind === 'entrypoint');
  assert.equal(entrypoints.length, 1);
  // edge-контракт не подавляет entrypoint: форматы ключей согласованы, kind различается
  const withImplements = extractGraphifyFacts(GRAPH, { maxEntities: 100, maxEdges: 100 });
  const impl = withImplements.entities.find((entity) => entity.label === 'JwtValidator');
  const kinds = withImplements.contracts
    .filter((contract) => contract.entityRef === impl.observedId)
    .map((contract) => contract.kind);
  assert.ok(kinds.includes('implements'));
  assert.ok(kinds.includes('entrypoint'));
});

test('mutation receipt attests raw file bytes, not re-serialized JSON', async () => {
  // намеренно «неканоничный» JSON: лишние пробелы и порядок ключей
  const raw = '{\n  "files": {},  "schemaVersion": "1"\n}\n';
  const result = await analyzeStructure({
    graphText: JSON.stringify(GRAPH),
    graphReportText: `Built from commit: \`${HEAD}\``,
    repoRoot: '/repo',
    headSha: HEAD,
    branchRef: 'main',
    mutationReportPath: '/repo/mutation-report.json',
    generatedAt: '2026-09-02T00:00:00.000Z',
  }, {
    readFile: async (path) => {
      if (String(path).endsWith('mutation-report.json')) return raw;
      throw Object.assign(new Error('not found'), { code: 'ENOENT' });
    },
  });
  const receipt = result.assessment.receipts.find((item) => item.provider === 'mutation-testing-elements');
  assert.ok(receipt);
  assert.equal(receipt.artifact.sha256, sha256(raw));
  assert.notEqual(receipt.artifact.sha256, sha256(JSON.stringify(JSON.parse(raw))));
});

test('LCOV parser rejects absolute SF without explicit repoRoot', () => {
  const text = 'TN:\nSF:/repo/src/x.ts\nDA:1,1\nend_of_record\n';
  const parsed = parseLcov(text);
  assert.equal(parsed.files.length, 0);
  assert.ok(parsed.warnings.some((warning) => /repoRoot/.test(warning)));
  const rooted = parseLcov(text, { repoRoot: '/repo' });
  assert.equal(rooted.files[0].path, 'src/x.ts');
});

test('LCOV parser rejects DA/BRDA records with empty fields', () => {
  const parsed = parseLcov('TN:\nSF:src/x.ts\nDA:,5\nDA:3,\nDA:4,2\nBRDA:4,0,0,\nBRDA:,0,0,1\nend_of_record\n');
  const file = parsed.byPath.get('src/x.ts');
  assert.deepEqual(file.lines.entries, [{ line: 4, hits: 2 }]);
  assert.equal(file.branches.found, 0);
});

test('matchLcovFunction matches SCIP constructor-kind entities', () => {
  const match = matchLcovFunction({ line: 3, name: 'Widget' }, [
    { kind: 'constructor', label: 'Widget', line: 3, observedId: 'ctor' },
  ]);
  assert.equal(match.entity?.observedId, 'ctor');
  assert.equal(match.confidence, 'exact');
});

test('identity reconciliation classifies path+label change as moved_renamed', () => {
  const previous = [{
    observedId: 'old', lineageId: 'lineage', path: 'src/old.ts',
    label: 'run', qualifiedName: 'pkg/run', kind: 'function', signature: 'run(): void',
  }];
  const current = [{
    observedId: 'new', lineageId: 'new', path: 'src/new.ts',
    label: 'execute', qualifiedName: 'pkg/run', kind: 'function', signature: 'run(): void',
  }];
  const result = reconcileIdentity(current, previous);
  assert.equal(result.events[0].kind, 'moved_renamed');
  assert.equal(result.entities[0].lineageId, 'lineage');
});

test('identity event sort is separator-safe for ids containing colons', () => {
  const previous = [
    { observedId: 'x:y', lineageId: 'x:y', path: 'src/one.ts', label: 'one', qualifiedName: 'pkg/one', kind: 'function', signature: 's' },
    { observedId: 'x', lineageId: 'x', path: 'src/two.ts', label: 'two', qualifiedName: 'pkg/two', kind: 'function', signature: 's' },
  ];
  const current = [
    { observedId: 'p', lineageId: 'p', path: 'src/one.ts', label: 'oneRenamed', qualifiedName: 'pkg/one', kind: 'function', signature: 's' },
    { observedId: 'y:p', lineageId: 'y:p', path: 'src/two.ts', label: 'twoRenamed', qualifiedName: 'pkg/two', kind: 'function', signature: 's' },
  ];
  const renamed = (prev, curr) => reconcileIdentity(curr, prev).events
    .filter((event) => event.kind === 'renamed')
    .map((event) => `${event.from}→${event.to}`);
  const first = renamed(previous, current);
  const second = renamed([...previous].reverse(), [...current].reverse());
  // ':'-конкат дал бы одинаковые ключи 'renamed:x:y:p' — порядок зависел бы от входа
  assert.deepEqual(first, ['x:y→p', 'x→y:p']);
  assert.deepEqual(second, first);
});

test('structure graphAnalysis follows the graph override unless set explicitly', () => {
  const cli = { dryRun: true, repo: '', prNumber: 0 };
  const fallback = loadReviewConfig({ OLLAMA_API_KEY: 'x' }, cli);
  assert.equal(fallback.structure.graph, '.graphify/graphify-out/graph.json');
  assert.equal(fallback.structure.graphAnalysis, '.graphify/graphify-out/.graphify_analysis.json');
  const custom = loadReviewConfig({ OLLAMA_API_KEY: 'x', GRAPH: 'out/custom/graph.json' }, cli);
  assert.equal(custom.graphify.graph, 'out/custom/graph.json');
  assert.equal(custom.structure.graph, 'out/custom/graph.json');
  assert.equal(custom.structure.graphAnalysis, join('out', 'custom', '.graphify_analysis.json'));
  const explicit = loadReviewConfig({
    OLLAMA_API_KEY: 'x',
    GRAPH: 'out/custom/graph.json',
    REVIEW_STRUCTURE_GRAPH_ANALYSIS: 'analysis/custom.json',
  }, cli);
  assert.equal(explicit.structure.graphAnalysis, 'analysis/custom.json');
});

test('schema path pattern forbids backslashes and dot-dot segments', async () => {
  for (const name of ['structure-facts.v1.schema.json', 'evidence-receipt.v1.schema.json']) {
    const schema = JSON.parse(await readFile(new URL(`../schemas/${name}`, import.meta.url), 'utf8'));
    const pattern = new RegExp(schema.$defs.path.pattern);
    assert.ok(pattern.test('src/auth.ts'));
    assert.ok(!pattern.test('src\\..\\secret'));
    assert.ok(!pattern.test('../secret'));
    assert.ok(!pattern.test('/etc/passwd'));
  }
});

test('relative schema $refs resolve to files in schemas/', async () => {
  for (const name of ['structure-facts.v1.schema.json', 'structure-assessment.v1.schema.json']) {
    const schema = JSON.parse(await readFile(new URL(`../schemas/${name}`, import.meta.url), 'utf8'));
    const refs = [];
    const walk = (node) => {
      if (Array.isArray(node)) return node.forEach(walk);
      if (node && typeof node === 'object') {
        if (typeof node.$ref === 'string' && !node.$ref.startsWith('#')) refs.push(node.$ref);
        Object.values(node).forEach(walk);
      }
    };
    walk(schema);
    assert.ok(refs.length >= 1);
    for (const ref of refs) {
      const target = JSON.parse(await readFile(new URL(`../schemas/${ref}`, import.meta.url), 'utf8'));
      assert.equal(target.$id, `https://orange-hanter.github.io/ai-reviewer/schemas/${ref}`);
    }
  }
});

const SCIP_INDEX_TWO_DOCS = {
  documents: [{
    relativePath: 'src/a.ts',
    language: 'typescript',
    occurrences: [{
      symbol: 'npm pkg 1 src/a.ts/foo().',
      roles: 1,
      range: { line: 1, endLine: 1 },
    }],
    symbols: [{
      symbol: 'npm pkg 1 src/a.ts/foo().',
      displayName: 'foo',
      kind: 17,
      signature: 'function foo(): void',
      relationships: [],
    }],
  }, {
    relativePath: 'src/b.ts',
    language: 'typescript',
    occurrences: [{
      symbol: 'npm pkg 1 src/b.ts/bar().',
      roles: 1,
      range: { line: 1, endLine: 1 },
    }],
    symbols: [{
      symbol: 'npm pkg 1 src/b.ts/bar().',
      displayName: 'bar',
      kind: 17,
      signature: 'function bar(): void',
      relationships: [],
    }],
  }],
};

const GRAPH_TWO_FUNCS = {
  nodes: [
    { id: 'a', label: 'foo', file_type: 'code', source_file: 'src/a.ts', source_location: 'L1', _callable: true },
    { id: 'b', label: 'bar', file_type: 'code', source_file: 'src/b.ts', source_location: 'L1', _callable: true },
  ],
  links: [{ source: 'a', target: 'b', relation: 'calls', confidence: 'EXTRACTED' }],
};

test('mergeScipWithGraphify recomputes ids of rewritten edges and contracts', () => {
  const graph = extractGraphifyFacts(GRAPH_TWO_FUNCS);
  const scip = extractScipFacts(SCIP_INDEX_TWO_DOCS);
  const merged = mergeScipWithGraphify(scip, graph);
  const foo = merged.entities.find((entity) => entity.path === 'src/a.ts');
  const bar = merged.entities.find((entity) => entity.path === 'src/b.ts');
  assert.notEqual(foo.observedId, graph.entities[0].observedId, 'сущность заменена SCIP-версией');
  const edge = merged.dependencies.find((item) => item.relation === 'calls');
  assert.equal(edge.from, foo.observedId);
  assert.equal(edge.to, bar.observedId);
  assert.equal(edge.id, stableId('edge', foo.observedId, bar.observedId, 'calls'),
    'id пересчитан из новых from/to — сортировка и lookup по id снова согласованы');
  const entrypoint = merged.contracts.find((item) => item.kind === 'entrypoint' && item.entityRef === foo.observedId);
  assert.equal(entrypoint.id, stableId('contract', 'entrypoint', foo.observedId));
  const ids = merged.dependencies.map((item) => item.id);
  assert.deepEqual(ids, [...ids].sort((a, b) => a.localeCompare(b)));
});

test('merged capabilities stay flat and combine providers', () => {
  const graph = extractGraphifyFacts(GRAPH_TWO_FUNCS);
  const scip = extractScipFacts(SCIP_INDEX_TWO_DOCS);
  const merged = mergeScipWithGraphify(scip, graph);
  // плоская форма (контракт provenance.providerCapabilities), не { graphify, scip }
  assert.equal(merged.capabilities.graphify, undefined);
  assert.equal(merged.capabilities.symbols, 'exact', 'лучший строковый уровень побеждает');
  assert.equal(merged.capabilities.topology, true, 'capability доступна, если её даёт хотя бы один провайдер');
  assert.equal(merged.capabilities.calls, true);
  assert.equal(merged.capabilities.signatures, true);
  assert.equal(merged.capabilities.positionEncoding, 'from-index');
});

test('SCIP contracts bind to test cases via entityRef-derived labels', () => {
  const scip = extractScipFacts(SCIP_INDEX_TWO_DOCS);
  const contract = scip.contracts.find((item) => item.kind === 'signature');
  assert.ok(contract);
  assert.equal(contract.label, undefined, 'SCIP-контракт не несёт label');
  const source = `import { test } from 'node:test';
test('foo works', () => {
  expect(foo()).toEqual(true);
});`;
  const bound = extractTestCasesFromSource('test/a.test.ts', source, scip.contracts, scip.entities);
  assert.deepEqual(bound[0].targetContractRefs, [contract.id]);
  const bare = extractTestCasesFromSource('test/a.test.ts', source, scip.contracts);
  assert.deepEqual(bare[0].targetContractRefs, [], 'без entities label вывести неоткуда');
});

test('SCIP decoder rejects truncated fixed-width fields', () => {
  // tag: field 1, wire 1 (fixed64) — но в буфере меньше 8 байт payload'а
  assert.throws(() => decodeScipIndex(Buffer.from([0b00001001, 0x00, 0x00])), /truncated SCIP protobuf/);
});

test('SCIP edge truncation is sorted-first and reported in warnings', () => {
  const index = {
    documents: [{
      relativePath: 'src/a.ts',
      language: 'typescript',
      occurrences: [
        { symbol: 'npm pkg 1 src/a.ts/foo().', roles: 1, range: { line: 1, endLine: 1 } },
      ],
      symbols: [{
        symbol: 'npm pkg 1 src/a.ts/foo().',
        displayName: 'foo',
        kind: 17,
        signature: 'function foo(): void',
        relationships: [{ symbol: 'npm pkg 1 src/a.ts/Bar#.', isImplementation: true }],
      }, {
        symbol: 'npm pkg 1 src/a.ts/Bar#.',
        displayName: 'Bar',
        kind: 21,
        signature: null,
        relationships: [],
      }],
      occurrencesExtra: undefined,
    }],
  };
  index.documents[0].occurrences.push({
    symbol: 'npm pkg 1 src/a.ts/Bar#.', roles: 1, range: { line: 5, endLine: 5 },
  });
  const full = extractScipFacts(index);
  assert.equal(full.dependencies.length, 1);
  const limited = extractScipFacts(index, { maxEdges: 0 });
  assert.equal(limited.dependencies.length, 0);
  assert.equal(limited.truncated, true);
  assert.ok(limited.warnings.some((warning) => /SCIP dependencies truncated at 0/.test(warning)));
});

test('invalid graph JSON fails clearly, invalid mutation JSON degrades to warning', async () => {
  await assert.rejects(() => analyzeStructure({
    graphText: '{nope',
    repoRoot: '/repo',
    headSha: HEAD,
    branchRef: 'main',
  }, { readFile: async () => { throw new Error('no files'); } }), /invalid structure graph JSON/);
  const result = await analyzeStructure({
    graphText: JSON.stringify(GRAPH),
    graphReportText: `Built from commit: \`${HEAD}\``,
    repoRoot: '/repo',
    headSha: HEAD,
    branchRef: 'main',
    mutationReportPath: '/repo/mutation.json',
    generatedAt: '2026-09-02T00:00:00.000Z',
  }, {
    readFile: async (path) => {
      if (String(path).endsWith('mutation.json')) return '{broken';
      throw Object.assign(new Error('not found'), { code: 'ENOENT' });
    },
  });
  assert.ok(result.diagnostics.warnings.some((warning) => /mutation report ignored: invalid JSON/.test(warning)));
  assert.ok(!result.assessment.receipts.some((item) => item.provider === 'mutation-testing-elements'));
});

test('MCP stdio transport rejects oversized frames and stays in sync', async () => {
  const bigBody = Buffer.alloc(8 * 1024 * 1024 + 1, 0x78);
  const bigFrame = Buffer.concat([
    Buffer.from(`Content-Length: ${bigBody.length}\r\n\r\n`),
    bigBody,
  ]);
  const [tooBig, pong] = await runMcpServer([bigFrame, { jsonrpc: '2.0', id: 9, method: 'ping' }]);
  assert.equal(tooBig.error.code, -32600);
  assert.match(tooBig.error.message, /exceeds/);
  assert.deepEqual(pong, { jsonrpc: '2.0', id: 9, result: {} });
});

test('pr-review-structure rejects unknown options with exit code 2', async () => {
  const child = spawn(process.execPath, [
    fileURLToPath(new URL('../bin/pr-review-structure.mjs', import.meta.url)),
    '--bogus-flag',
  ]);
  const stderr = [];
  child.stderr.on('data', (chunk) => stderr.push(chunk));
  child.stdin.end();
  const watchdog = setTimeout(() => child.kill('SIGKILL'), 10000);
  const exitCode = await new Promise((resolvePromise) => child.on('close', resolvePromise));
  clearTimeout(watchdog);
  assert.equal(exitCode, 2);
  assert.match(Buffer.concat(stderr).toString(), /unknown option: --bogus-flag/);
});
