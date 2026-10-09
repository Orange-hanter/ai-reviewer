#!/usr/bin/env node
// Генератор эталонов T18 (RFC 16): прогоняет JS reconcileIdentity и
// buildStructureContext на пейлоадах из fixtures/structure/ (обогащённых
// сущностями/рёбрами/сценариями) и пишет вход + выход в
// rust/parity/golden-structure/t18.json. Rust-тесты structure_identity:: и
// structure_context:: сверяют с ним свой результат посимвольно (после JSON).
//
//   node rust/parity/gen-structure-t18-golden.mjs
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { reconcileIdentity } from '../../lib/structure-identity.mjs';
import { buildStructureContext } from '../../lib/structure-context.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const load = (name) => JSON.parse(readFileSync(join(root, 'fixtures', 'structure', name), 'utf8'));
const baseFacts = load('structure-facts.v1.json');
const baseAssessment = load('structure-assessment.v1.json');

const ent = (id, path, label, extra = {}) => ({
  observedId: id, lineageId: id, path, label, qualifiedName: `pkg/${label}`,
  kind: 'function', signature: `${label}()`, boundaryId: 'boundary:repository', ...extra,
});

function enrichedFacts() {
  const facts = structuredClone(baseFacts);
  facts.facts.boundaries.push({ id: 'boundary:src/api', kind: 'module', name: 'api', paths: ['src/api'], source: 'path', confidence: 'exact', evidence: [] });
  facts.facts.entities = [
    ent('e:a', 'src/api/a.ts', 'a', { boundaryId: 'boundary:src/api' }),
    ent('e:b', 'src/api/b.ts', 'b', { boundaryId: 'boundary:src/api' }),
    ent('e:c', 'src/core/c.ts', 'c'),
    ent('e:d', 'src/core/d.ts', 'd'),
    ent('e:é', 'src/ünï/é.ts', 'é'),
    ent('e:z', 'src/zz.ts', 'z'),
  ];
  facts.facts.contracts = [
    { id: 'c:a', entityRef: 'e:a', kind: 'entrypoint' },
    { id: 'c:d', entityRef: 'e:d', kind: 'entrypoint' },
    { id: 'c:z', entityRef: 'e:z', kind: 'entrypoint' },
  ];
  facts.facts.dependencies = [
    { id: 'd1', from: 'e:a', to: 'e:b', relation: 'calls' },
    { id: 'd1', from: 'e:a', to: 'e:b', relation: 'calls' },
    { from: 'e:b', to: 'e:c', relation: 'calls' },
    { from: 'e:c', to: 'e:d', relation: 'imports' },
    { id: 'd4', from: 'e:é', to: 'e:a', relation: 'calls' },
    { id: 'd5', from: 'e:d', to: 'e:z', relation: 'calls' },
  ];
  facts.facts.testCases = [
    { id: 't1', path: 'test/a.test.ts', targetContractRefs: ['c:a'] },
    { id: 't2', path: 'test/d.test.ts', targetContractRefs: ['c:d'] },
    { id: 't3', path: 'test/Ä.test.ts', targetContractRefs: ['c:a', 'c:z'] },
  ];
  return facts;
}

function enrichedAssessment() {
  const a = structuredClone(baseAssessment);
  a.assessments.contractScenarios = [
    { contractRefs: ['c:a'], obligationRef: 'obl:1', status: 'missing' },
    { contractRefs: ['c:d'], obligationRef: 'obl:2', status: 'partial' },
    { contractRefs: ['c:z', 'c:a'], obligationRef: 'obl:3', status: 'covered' },
  ];
  a.hints = [
    { id: 'h1', subjectRefs: ['e:a'], text: 'x' },
    { id: 'h2', subjectRefs: ['c:d'], text: 'y' },
    { id: 'h3', subjectRefs: ['obl:3'], text: 'z' },
    { id: 'h4', subjectRefs: ['e:nope'], text: 'w' },
    { id: 'h5', subjectRefs: ['e:b', 'e:c'], text: 'v' },
  ];
  return a;
}

const HEAD = baseFacts.provenance.headSha;
const FP = baseFacts.fingerprint;
const contextCases = [
  ['fixture-head-pin', 'base', { headSha: HEAD }],
  ['fixture-fingerprint-pin', 'base', { snapshotFingerprint: FP, scopeRefs: ['src'] }],
  ['no-pin', 'base', {}],
  ['head-mismatch', 'base', { headSha: 'fffffff' }],
  ['fingerprint-mismatch', 'base', { snapshotFingerprint: 'f'.repeat(64) }],
  ['assessment-mismatch', 'foreign', { headSha: HEAD }],
  ['scope-entity-depth1', 'rich', { headSha: HEAD, scopeRefs: ['e:a'], taskRef: 'T-1' }],
  ['scope-prefix-depth3-capped', 'rich', { headSha: HEAD, scopeRefs: ['src/api/'], dependencyDepth: 3 }],
  ['scope-prefix-raised-depth', 'rich', { headSha: HEAD, scopeRefs: ['src/api'], dependencyDepth: 3, limits: { maxDepth: 3 } }],
  ['scope-boundary', 'rich', { headSha: HEAD, scopeRefs: ['boundary:src/api'] }],
  ['contract-refs', 'rich', { snapshotFingerprint: FP, contractRefs: ['c:d', 'c:missing'] }],
  ['depth-zero', 'rich', { headSha: HEAD, scopeRefs: ['src'], dependencyDepth: 0 }],
  ['max-edges-truncated', 'rich', { headSha: HEAD, scopeRefs: ['src'], limits: { maxEdges: 2 } }],
  ['max-seeds-truncated', 'rich', { headSha: HEAD, scopeRefs: ['src'], limits: { maxSeeds: 2 } }],
  ['secondary-truncation', 'rich', { headSha: HEAD, scopeRefs: ['e:a'], dependencyDepth: 2, limits: { maxBytes: 1, maxSeeds: 1, maxDepth: 2 } }],
  ['token-budget-truncation', 'rich', { headSha: HEAD, scopeRefs: ['src'], tokenBudget: 300, limits: { maxGaps: 3 } }],
  // tokenEstimate без собственного поля = 442: на границе бюджета усечения нет
  ['token-budget-boundary', 'rich', { headSha: HEAD, scopeRefs: ['e:a'], taskRef: 'T-1', tokenBudget: 442 }],
  ['token-budget-boundary-minus-one', 'rich', { headSha: HEAD, scopeRefs: ['e:a'], taskRef: 'T-1', tokenBudget: 441 }],
  ['unicode-scope', 'rich', { headSha: HEAD, scopeRefs: ['src/ünï'] }],
];

const payloads = {
  base: [baseFacts, baseAssessment],
  rich: [enrichedFacts(), enrichedAssessment()],
  foreign: [baseFacts, { ...baseAssessment, factsFingerprint: '9'.repeat(64) }],
};

const context = contextCases.map(([name, payload, args]) => {
  const [facts, assessment] = payloads[payload];
  try {
    const out = buildStructureContext({ facts, assessment, ...args });
    return { name, payload, args, expected: JSON.parse(JSON.stringify(out)) };
  } catch (error) {
    return { name, payload, args, error: error.message };
  }
});

const id = (observedId, path, label, extra = {}) => ({
  observedId, lineageId: observedId, path, label, qualifiedName: `pkg/${label}`, kind: 'function', signature: 's', ...extra,
});
const identityCases = [
  ['fixture-empty', baseFacts.facts.entities, []],
  ['exact-keeps-lineage', [id('a', 'src/a.ts', 'a', { lineageId: 'a-new' })], [id('a', 'src/a.ts', 'a', { lineageId: 'L' })]],
  ['exact-without-lineage', [{ observedId: 'a', path: 'p', label: 'a', kind: 'function' }], [{ observedId: 'a', path: 'p', label: 'a', kind: 'function' }]],
  ['moved', [id('new', 'src/new.ts', 'run')], [id('old', 'src/old.ts', 'run', { lineageId: 'lineage' })]],
  ['renamed-by-qualified', [id('n', 'src/a.ts', 'execute', { qualifiedName: 'pkg/run' })], [id('o', 'src/a.ts', 'run')]],
  ['moved-renamed', [id('n', 'src/b.ts', 'execute', { qualifiedName: 'pkg/run' })], [id('o', 'src/a.ts', 'run')]],
  ['label-fallback-no-signature', [{ observedId: 'n', path: 'x', label: 'L', kind: 'class' }], [{ observedId: 'o', path: 'y', label: 'L', kind: 'class', signature: '' }]],
  ['ambiguous', [id('n', 'src/c.ts', 'run')], [id('o2', 'src/b.ts', 'run'), id('o1', 'src/a.ts', 'run')]],
  ['created-and-removed', [id('fresh', 'src/f.ts', 'f')], [id('gone', 'src/g.ts', 'g')]],
  ['colon-ids', [
    id('p', 'src/one.ts', 'oneRenamed', { qualifiedName: 'pkg/one' }),
    id('y:p', 'src/two.ts', 'twoRenamed', { qualifiedName: 'pkg/two' }),
  ], [
    id('x:y', 'src/one.ts', 'one', { qualifiedName: 'pkg/one' }),
    id('x', 'src/two.ts', 'two', { qualifiedName: 'pkg/two' }),
  ]],
  ['punctuation-order', [
    id('A-1', 'a', 'q1'), id('a_1', 'a', 'q2'), id('a.1', 'a', 'q3'), id('a1', 'a', 'q4'), id('B', 'a', 'q5'), id('a,1', 'a', 'q6'),
  ], [id('a 1', 'b', 'gone1'), id('a$', 'b', 'gone2'), id('Ab', 'b', 'gone3')]],
  ['mixed', [
    id('same', 'src/s.ts', 's'), id('mv', 'src/m2.ts', 'm'), id('amb', 'src/x.ts', 'dup'), id('new1', 'src/n.ts', 'n'),
  ], [
    id('same', 'src/s.ts', 's', { lineageId: '' }), id('mv-old', 'src/m1.ts', 'm'), id('d1', 'src/d1.ts', 'dup'), id('d2', 'src/d2.ts', 'dup'), id('rm', 'src/r.ts', 'r'),
  ]],
];
const identity = identityCases.map(([name, current, previous]) => ({
  name, current, previous, expected: JSON.parse(JSON.stringify(reconcileIdentity(current, previous))),
}));

const out = { generatedBy: 'rust/parity/gen-structure-t18-golden.mjs', payloads, context, identity };
writeFileSync(join(root, 'rust', 'parity', 'golden-structure', 't18.json'), `${JSON.stringify(out, null, 2)}\n`);
console.log(`context ${context.length}, identity ${identity.length}`);
