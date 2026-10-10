#!/usr/bin/env node
// Генератор эталонов T21 (RFC 16): прогоняет JS analyzeStructure (lib/structure.mjs)
// на наборе кейсов с файлами в памяти и подменённым git, пишет вход + выход в
// rust/parity/golden-structure/t21.json. Rust-тест structure:: сверяет с ним
// результат analyze_structure посимвольно (после JSON).
//
// Эталон — чистый JS: все AI_REVIEWER_*_FORCE_JS выставляются до импорта, иначе
// обёртки structure-* позвали бы собранные Rust-бинари и эталон сравнивал бы Rust с Rust.
//
//   node rust/parity/gen-structure-t21-golden.mjs
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
for (const name of readdirSync(join(root, 'lib'))) {
  if (!name.endsWith('.mjs')) continue;
  const src = readFileSync(join(root, 'lib', name), 'utf8');
  for (const m of src.matchAll(/AI_REVIEWER_([A-Z0-9_]+?)_FORCE_JS/g)) {
    process.env[`AI_REVIEWER_${m[1]}_FORCE_JS`] = '1';
  }
}

const { analyzeStructure } = await import('../../lib/structure.mjs');
const { extractGraphifyFacts } = await import('../../lib/structure-graphify.mjs');
const { sha256, stableId } = await import('../../lib/util.mjs');

const HEAD = 'abcdef1234567890';
const GEN = '2026-09-02T00:00:00.000Z';

const GRAPH = {
  nodes: [
    { id: 'api', label: 'validate()', file_type: 'code', source_file: 'src/auth.ts', source_location: 'L10', _callable: true, community: 1 },
    { id: 'caller1', label: 'login()', file_type: 'code', source_file: 'src/login.ts', source_location: 'L4', _callable: true, community: 2 },
    { id: 'caller2', label: 'refresh()', file_type: 'code', source_file: 'src/refresh.ts', source_location: 'L5', _callable: true, community: 2 },
    { id: 'iface', label: 'TokenValidator', file_type: 'code', source_file: 'src/contracts.ts', source_location: 'L1', _callable_class: true, community: 1 },
    { id: 'impl', label: 'JwtValidator', file_type: 'code', source_file: 'src/auth.ts', source_location: 'L2', _callable_class: true, community: 1 },
    { id: 'test', label: 'reject invalid token()', file_type: 'code', source_file: 'test/auth.test.ts', source_location: 'L3', _callable: true, community: 3 },
    { id: 'util', label: 'helper()', file_type: 'code', source_file: 'lib/ünï/helper.ts', source_location: 'L7', _callable: true },
  ],
  links: [
    { source: 'caller1', target: 'api', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'caller2', target: 'api', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'test', target: 'api', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'impl', target: 'iface', relation: 'implements', confidence: 'EXTRACTED' },
    { source: 'api', target: 'util', relation: 'calls', confidence: 'INFERRED' },
    { source: 'util', target: 'caller1', relation: 'calls', confidence: 'AMBIGUOUS' },
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

const TEST_SOURCE = `import { test } from 'node:test';
test('reject invalid token', () => {
  expect(() => validate('bad')).toThrow();
});
test('accepts a valid token', () => {
  assert.equal(validate('good'), true);
});`;

const api = extractGraphifyFacts(GRAPH).entities.find((entity) => entity.label === 'validate()');
const contractId = stableId('contract', 'entrypoint', api.observedId);

function obligationsExport(obligations, extras = {}) {
  return {
    schemaRef: 'code-structure/obligations-export.v1',
    version: 1,
    kind: 'obligations_export',
    project: 'demo',
    revision: 'docs-1',
    headSha: HEAD,
    exportedAt: GEN,
    obligations,
    ...extras,
  };
}

const mustError = {
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
const happy = {
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
};
const edge = {
  id: 'obl:auth#limits',
  source: 'cod-doc-section',
  contentHash: sha256('limits'),
  priority: 'should',
  kind: 'edge_case',
  statement: 'validate SHOULD reject tokens below minimum length',
  status: 'confirmed',
  contractRefs: [],
  inputs: { token: { type: 'string', minLength: 8 } },
};
const unlinked = {
  id: 'obl:validate',
  source: 'cod-doc-section',
  contentHash: sha256('validate'),
  priority: 'must',
  kind: 'happy_path',
  statement: 'validate MUST accept valid tokens',
  status: 'confirmed',
  contractRefs: [],
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

const SCIP_INDEX = {
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
  externalSymbols: [],
};

// options — то, что передаётся analyzeStructure; files — абсолютный путь → текст;
// git — 'args через пробел' → stdout (нет ключа — git падает).
const cases = [
  {
    name: 'e2e: lcov + in-memory mutation + obligations + test source',
    options: {
      graphText: JSON.stringify(GRAPH),
      graphReportText: `Built from commit: \`${HEAD}\``,
      lcovText: LCOV,
      mutationReport,
      obligationsExport: obligationsExport([mustError, happy, edge]),
      repoRoot: '/repo',
      headSha: HEAD,
      branchRef: 'main',
      testHeadSha: HEAD,
      testRunId: 'run-1',
      generatedAt: GEN,
      thresholds: { highFanIn: 2 },
      prNumber: 42,
      isDefaultBranch: true,
    },
    files: { '/repo/test/auth.test.ts': TEST_SOURCE },
  },
  {
    name: 'drift: unlinked obligation, active and expired waivers, confirmed claims',
    options: {
      graphText: JSON.stringify(GRAPH),
      graphReportText: 'Built from commit: `1234567`',
      repoRoot: '/repo',
      headSha: HEAD,
      branchRef: 'feature/x',
      generatedAt: GEN,
      obligationsExport: obligationsExport([unlinked, happy], { revision: '1' }),
      waivers: [
        { findingFingerprint: 'nope', owner: 'team', reason: 'legacy', expiresAt: '2099-01-01T00:00:00.000Z' },
        { findingFingerprint: 'old', owner: 'team', reason: 'legacy', expiresAt: '2000-01-01T00:00:00.000Z' },
      ],
      confirmedClaims: [],
    },
    files: {},
  },
  {
    name: 'no graph, git unavailable: fallback sha and branch',
    options: { repoRoot: '/repo', generatedAt: GEN },
    files: {},
  },
  {
    name: 'git values from exec, default obligations, project name',
    options: { graphText: JSON.stringify(GRAPH), repoRoot: '/repo', generatedAt: GEN, project: 'demo' },
    files: { '/repo/test/auth.test.ts': TEST_SOURCE },
    git: { 'rev-parse HEAD': `${HEAD}\n`, 'symbolic-ref --short HEAD': 'main\n' },
  },
  {
    name: 'paths: graph/report/analysis/lcov/obligations/property/mutation files',
    options: {
      graphPath: '/repo/.graphify/graph.json',
      graphReportPath: '/repo/.graphify/GRAPH_REPORT.md',
      lcovPath: '/repo/coverage/lcov.info',
      obligationsPath: '/repo/obligations.json',
      propertyReportPath: '/repo/property.json',
      mutationReportPath: '/repo/mutation.json',
      repoRoot: '/repo',
      headSha: HEAD,
      branchRef: 'main',
      generatedAt: GEN,
      graphifyVersion: '0.9.1',
      lcovVersion: '2',
      trustTier: 'signed_ci',
      testProducedAt: '2026-09-01T00:00:00.000Z',
    },
    files: {
      '/repo/.graphify/graph.json': JSON.stringify(GRAPH, null, 2),
      '/repo/.graphify/GRAPH_REPORT.md': `# Report\nBuilt from commit: ${HEAD.slice(0, 9)}\n`,
      '/repo/.graphify/.graphify_analysis.json': JSON.stringify({ communities: { 1: ['api', 'iface', 'impl'] } }),
      '/repo/coverage/lcov.info': LCOV,
      '/repo/obligations.json': JSON.stringify(obligationsExport([mustError, edge])),
      '/repo/property.json': JSON.stringify({ results: [{ id: 'nope', status: 'passed' }] }),
      '/repo/mutation.json': '{\n  "files": {},  "schemaVersion": "1"\n}\n',
      '/repo/test/auth.test.ts': TEST_SOURCE,
    },
  },
  {
    name: 'invalid mutation JSON degrades to warning, scope and previous facts',
    options: {
      graphText: JSON.stringify(GRAPH),
      graphReportText: `Built from commit: \`${HEAD}\``,
      repoRoot: '/repo',
      headSha: HEAD,
      branchRef: 'main',
      mutationReportPath: '/repo/mutation.json',
      generatedAt: GEN,
      scope: './src/',
      includeAmbiguous: true,
      maxEntities: 3,
      previousFacts: {
        facts: {
          entities: [{
            observedId: 'old', lineageId: 'lineage', path: 'src/old.ts', label: 'validate()',
            qualifiedName: 'src/auth.ts:validate()', kind: 'function', signature: '',
          }],
        },
      },
    },
    files: { '/repo/mutation.json': '{broken' },
  },
  {
    name: 'SCIP index in memory merges with graphify',
    options: {
      graphText: JSON.stringify(GRAPH),
      graphReportText: `Built from commit: \`${HEAD}\``,
      scipIndex: SCIP_INDEX,
      scipVersion: '0.5.0',
      lcovText: LCOV,
      repoRoot: '/repo',
      headSha: HEAD,
      branchRef: 'main',
      generatedAt: GEN,
    },
    files: {},
  },
  {
    name: 'invalid graph JSON fails',
    options: { graphText: '{nope', repoRoot: '/repo', headSha: HEAD, branchRef: 'main', generatedAt: GEN },
    files: {},
  },
];

const out = [];
for (const item of cases) {
  const files = item.files || {};
  const git = item.git || {};
  const deps = {
    readFile: async (path) => {
      const key = String(path);
      if (Object.hasOwn(files, key)) return files[key];
      throw Object.assign(new Error(`ENOENT: ${key}`), { code: 'ENOENT' });
    },
    execFileP: async (_cmd, args) => {
      const key = args.slice(2).join(' ');
      if (Object.hasOwn(git, key)) return { stdout: git[key], stderr: '' };
      throw new Error('git unavailable');
    },
  };
  let output = null;
  let error = null;
  try {
    output = await analyzeStructure(structuredClone(item.options), deps);
  } catch (e) {
    error = String(e.message).split(':')[0];
  }
  out.push({ name: item.name, options: item.options, files, git, output, error });
}

writeFileSync(join(root, 'rust', 'parity', 'golden-structure', 't21.json'), `${JSON.stringify({ cases: out }, null, 2)}\n`);
console.log(`t21: ${out.length} cases`);
