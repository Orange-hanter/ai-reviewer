/**
 * Parity-прогон Фазы 2, шаг 3: Rust structure_assessments ↔ чистый JS fallback
 * lib/structure-assessments.mjs.
 *
 * Запуск: node rust/parity/structure-assessments.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/structure-assessments.mjs: resolveAssessmentsBin()
 * читает AI_REVIEWER_STRUCTURE_ASSESSMENTS_FORCE_JS на каждый вызов, поэтому
 * js-сторона прогоняется с FORCE_JS=1, rust-сторона — без него. Результаты
 * сравниваются после JSON round-trip (rt): undefined отсутствуют, Infinity/NaN → null.
 *
 * Корпус (векторы из test/structure.test.mjs + edge-cases):
 *   - assessDependencyRisk: fanIn-порог (default 5 / 2 / 0 / строка '2'),
 *     duplicate observedId (execution map — последний выигрывает), contractRefs
 *     через map(id).sort() дефолтным sort'ом, confidence exact/extracted/inferred,
 *     executionStatus covered, godNodes truthy-строка, facts null;
 *   - assessDataQuality: unresolved/ambiguous join-rate, ratio null при нуле
 *     function_execution (unresolvedJoinRate ?? 0), missingAllowed-комбо
 *     (fresh/!truncated/inventory/joinRate<=threshold), missingThreshold '0.5'
 *     строкой, truthy-строка truncated/testInventoryComplete через Boolean,
 *     entityCount/edgeCount;
 *   - assessContractScenarios: draft/alignment/resolution-ветки (unresolved vs
 *     ambiguous), happy_path (assertion||property), error_path (assertion&&property),
 *     partial с missingEvidence-комбо, cappedByAggregate (aggregate_lcov),
 *     missing через dataQuality.missingAllowed, property seed 0 → '',
 *     evidence dedup+дефолтный sort, duplicate obligationRef в propertyResults,
 *     отфильтрованные неизвестные contractRefs;
 *   - buildStructureHints: graph_stale info, scenario gaps (missing→high,
 *     partial→medium, covered skip), hotspot covered skip, мутанты survived/
 *     no_coverage (killed skip), dedup subjectRefs/evidence;
 *   - buildStructureAssessment: реальные fixtures/structure/*.json, lcov null/
 *     {hash}/{totals без hash → canonical-хеш}, testRun {}/null/{id,headSha,
 *     producedAt}, receipts passthrough, thresholds {unresolvedJoinRate:0.2},
 *     testInventory === true, merge coverageObservations+mutation.observations
 *     по localeCompare(id), obligationContentHashes;
 *   - temporalAlignment / assessmentFingerprint напрямую.
 */

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_STRUCTURE_ASSESSMENTS_FORCE_JS = '1';
const A = await import('../../lib/structure-assessments.mjs');

let compared = 0;
const failures = [];

/** JSON round-trip как в проде: undefined отсутствуют, Infinity/NaN → null. */
const rt = (v) => JSON.parse(JSON.stringify(v));

function compare(label, jsResult, rustResult) {
  compared += 1;
  try {
    assert.deepEqual(rustResult, jsResult);
  } catch (e) {
    failures.push({
      label,
      js: JSON.stringify(jsResult)?.slice(0, 600),
      rust: JSON.stringify(rustResult)?.slice(0, 600),
    });
  }
}

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_STRUCTURE_ASSESSMENTS_FORCE_JS = '1';
  try {
    return { ok: true, value: rt(fn(...args)) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_STRUCTURE_ASSESSMENTS_FORCE_JS;
  }
};

const runRust = (fn, ...args) => {
  delete process.env.AI_REVIEWER_STRUCTURE_ASSESSMENTS_FORCE_JS;
  try {
    return { ok: true, value: rt(fn(...args)) };
  } catch (e) {
    const message = String(e?.message || e);
    const m = message.match(/"error":\s*"((?:[^"\\]|\\.)*)"/);
    return { ok: false, error: m ? JSON.parse(`"${m[1]}"`) : message };
  }
};

function check(label, fn, ...args) {
  const js = runJs(fn, ...args);
  const rust = runRust(fn, ...args);
  if (js.ok !== rust.ok) {
    compared += 1;
    failures.push({ label, js: JSON.stringify(js)?.slice(0, 400), rust: JSON.stringify(rust)?.slice(0, 400) });
    return;
  }
  compare(label, js.value ?? js.error, rust.value ?? rust.error);
}

// ---------------------------------------------------------------------------
// Корпус: assessDependencyRisk
// ---------------------------------------------------------------------------

const FACTS = {
  entities: [
    { observedId: 'e1', isGodNode: true },
    { observedId: 'e2', isGodNode: false },
    { observedId: 'e3' },
  ],
  contracts: [
    { id: 'c1', entityRef: 'e1' },
    { id: 'c2', entityRef: 'e1' },
    { id: 'c3', entityRef: 'e2' },
  ],
  dependencies: [
    { to: 'e1', confidence: 'exact', crossBoundary: true },
    { to: 'e1', confidence: 'extracted' },
    { to: 'e1', confidence: 'inferred' },
    { to: 'e1' },
    { to: 'e2', confidence: 'exact', crossBoundary: true },
    { to: 'e2', confidence: 'exact' },
  ],
};

check('risk: дефолтный порог 5 — хотспотов нет, godNodes/crossBoundary',
  A.assessDependencyRisk, FACTS, null, {});

check('risk: highFanIn 2 — e1/e2 сортировка по fanIn, confidence inferred/extracted',
  A.assessDependencyRisk, FACTS, null, { highFanIn: 2 });

check('risk: highFanIn 2 + execution covered для e1',
  A.assessDependencyRisk, FACTS,
  [{ level: 'function_execution', subjectRef: 'e1', status: 'covered' }],
  { highFanIn: 2 });

check('risk: highFanIn 0 — e3 с fanIn 0, пустые contractRefs, confidence extracted',
  A.assessDependencyRisk, FACTS, [], { highFanIn: 0 });

check('risk: highFanIn строка "2" — коэрсия как в JS',
  A.assessDependencyRisk, FACTS, null, { highFanIn: '2' });

check('risk: duplicate observedId — execution map, последний выигрывает',
  A.assessDependencyRisk, FACTS, [
    { level: 'function_execution', subjectRef: 'e1', status: 'covered' },
    { level: 'function_execution', subjectRef: 'e1', status: 'missing' },
  ], { highFanIn: 2 });

check('risk: godNodes truthy-строка + дефолтный sort',
  A.assessDependencyRisk, {
    entities: [
      { observedId: 'b' },
      { observedId: 'a', isGodNode: 'yes' },
      { observedId: 'c', isGodNode: 0 },
    ],
    contracts: [],
    dependencies: [],
  }, null, {});

check('risk: facts null/undefined — пусто',
  A.assessDependencyRisk, null, undefined, undefined);
check('risk: facts undefined — пусто', A.assessDependencyRisk, undefined, null, {});

// ---------------------------------------------------------------------------
// Корпус: assessDataQuality
// ---------------------------------------------------------------------------

check('quality: нет function_execution — joinRate null → 0, missingAllowed false',
  A.assessDataQuality, FACTS, [], {});

check('quality: unresolved/ambiguous микс, joinRate 0.5 > дефолт 0.1',
  A.assessDataQuality, FACTS, [
    { level: 'function_execution', subjectRef: 'e1', confidence: 'unresolved' },
    { level: 'function_execution', subjectRef: 'e2', confidence: 'ambiguous' },
    { level: 'function_execution', subjectRef: 'e3', confidence: 'extracted' },
    { level: 'file_coverage', subjectRef: 'e1', confidence: 'unresolved' },
  ], { graphStatus: 'fresh', testInventoryComplete: true });

check('quality: missingAllowed true — joinRate 0, fresh, inventory, не truncated',
  A.assessDataQuality, FACTS, [
    { level: 'function_execution', subjectRef: 'e1', confidence: 'extracted' },
  ], {
    graphStatus: 'fresh',
    testInventoryComplete: true,
    truncated: false,
    missingThreshold: 0.1,
  });

check('quality: missingThreshold "0.5" строкой, joinRate 0.5 ≤ 0.5',
  A.assessDataQuality, FACTS, [
    { level: 'function_execution', subjectRef: 'e1', confidence: 'unresolved' },
    { level: 'function_execution', subjectRef: 'e2', confidence: 'extracted' },
  ], {
    graphStatus: 'fresh',
    testInventoryComplete: true,
    missingThreshold: '0.5',
  });

check('quality: truthy-строки — truncated "yes"→true рубит missingAllowed, inventory "1"→true',
  A.assessDataQuality, FACTS, [
    { level: 'function_execution', subjectRef: 'e1', confidence: 'extracted' },
  ], {
    graphStatus: 'fresh',
    testInventoryComplete: 'yes',
    truncated: 'yes',
  });

check('quality: truncated 1 → Boolean true, graphStatus stale',
  A.assessDataQuality, FACTS, null, { graphStatus: 'stale', truncated: 1 });

check('quality: entityCount/edgeCount, facts null → 0',
  A.assessDataQuality, null, null, {});

// ---------------------------------------------------------------------------
// Корпус: assessContractScenarios
// ---------------------------------------------------------------------------

const SCEN_FACTS = {
  entities: [
    { observedId: 'e1' },
    { observedId: 'e2' },
  ],
  contracts: [
    { id: 'c1', entityRef: 'e1' },
    { id: 'c2', entityRef: 'e1' },
    { id: 'c3', entityRef: 'e2' },
  ],
  dependencies: [],
  testCases: [
    { id: 't2', targetContractRefs: ['c3'], assertions: [{ kind: 'equality' }] },
    { id: 't1', targetContractRefs: ['c1', 'c2'], assertions: [{ kind: 'throws' }] },
    { id: 't3', targetContractRefs: ['c1'] },
  ],
};

const OBL = (over = {}) => ({
  id: 'obl:1',
  kind: 'error_path',
  status: 'confirmed',
  contentHash: 'abc123',
  contractRefs: ['c1'],
  ...over,
});

const EXEC_E1 = {
  level: 'function_execution', subjectRef: 'e1', status: 'covered',
  evidence: ['z:evidence', 'a:evidence', 'z:evidence'],
};
const PROP_PASS_EXACT = [{
  obligationRef: 'obl:1', status: 'passed', confidence: 'exact', propertyId: 'p1', seed: 7,
}];

check('scenarios: draft — ветка obligation is draft',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL({ status: 'draft' })],
    coverageObservations: [], alignment: 'aligned',
  });

check('scenarios: alignment stale — ветка temporal alignment',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL()],
    coverageObservations: [], alignment: 'stale',
  });

check('scenarios: ambiguous — два валидных контракта, unique_contract',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL({ contractRefs: ['c1', 'c2', 'c1'] })],
    coverageObservations: [], alignment: 'aligned',
  });

check('scenarios: unresolved — пустые и неизвестные contractRefs отфильтрованы, contract_link',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL({ contractRefs: ['nope'] }), OBL({ id: 'obl:2', contractRefs: [] })],
    coverageObservations: [], alignment: 'aligned',
  });

check('scenarios: happy_path covered через property (assertion||property), seed 0 → ""',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL({ kind: 'happy_path' })],
    coverageObservations: [EXEC_E1],
    propertyResults: [{ obligationRef: 'obl:1', status: 'passed', confidence: 'exact', propertyId: 'p0', seed: 0 }],
    alignment: 'aligned',
  });

check('scenarios: error_path covered — assertion&&property, confidence exact, evidence dedup+sort',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL()],
    coverageObservations: [EXEC_E1],
    propertyResults: PROP_PASS_EXACT,
    alignment: 'aligned',
  });

check('scenarios: error_path partial — hasTest=false, executed=true, missing-комбо',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL({ contractRefs: ['c2'] })],
    coverageObservations: [EXEC_E1],
    propertyResults: [],
    alignment: 'aligned',
  });

check('scenarios: cappedByAggregate — aggregate_lcov рубит error_path, per_test_evidence',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL()],
    coverageObservations: [EXEC_E1],
    propertyResults: PROP_PASS_EXACT,
    alignment: 'aligned',
    evidenceCeiling: 'aggregate_lcov',
  });

check('scenarios: missing — dataQuality.missingAllowed, integration+happy missing-комбо',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL({ id: 'obl:2', kind: 'integration', contractRefs: ['c3'] })],
    coverageObservations: [],
    alignment: 'aligned',
    dataQuality: { missingAllowed: true },
  });

check('scenarios: inventory не позволяет missing — complete_test_inventory',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL({ id: 'obl:2', kind: 'integration', contractRefs: ['c3'] })],
    coverageObservations: [],
    alignment: 'aligned',
    dataQuality: { missingAllowed: false },
  });

check('scenarios: property passed но confidence inferred — propertyBound false, partial',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL({ kind: 'happy_path' })],
    coverageObservations: [EXEC_E1],
    propertyResults: [{ obligationRef: 'obl:1', status: 'passed', confidence: 'inferred', propertyId: 'p1' }],
    alignment: 'aligned',
  });

check('scenarios: duplicate obligationRef в propertyResults — последний выигрывает',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: [OBL()],
    coverageObservations: [EXEC_E1],
    propertyResults: [
      { obligationRef: 'obl:1', status: 'failed', confidence: 'exact', propertyId: 'pX' },
      ...PROP_PASS_EXACT,
    ],
    alignment: 'aligned',
  });

check('scenarios: obligations null → []',
  A.assessContractScenarios, {
    facts: SCEN_FACTS, obligations: null, coverageObservations: [], alignment: 'aligned',
  });

// ---------------------------------------------------------------------------
// Корпус: buildStructureHints
// ---------------------------------------------------------------------------

const DQ_FRESH = { graphStatus: 'fresh', missingAllowed: false };
const DQ_STALE = { graphStatus: 'stale', missingAllowed: false };
const RISK_HOT = {
  highFanIn: [
    { entityRef: 'e1', contractRefs: ['c1', 'c1', 'c2'], fanIn: 4, executionStatus: 'missing', confidence: 'inferred' },
    { entityRef: 'e2', contractRefs: [], fanIn: 2, executionStatus: 'covered', confidence: 'extracted' },
  ],
  crossBoundaryEdges: 1,
  godNodes: ['e1'],
};
const SCEN_GAP = [
  {
    id: 'scenario:gap-missing', obligationRef: 'obl:m', contractRefs: ['c1'],
    status: 'missing', kind: 'error_path', confidence: 'extracted',
    evidence: ['obligation:obl:m:hash'], missingEvidence: ['linked_test'],
  },
  {
    id: 'scenario:gap-partial', obligationRef: 'obl:p', contractRefs: ['c2'],
    status: 'partial', kind: 'happy_path', confidence: 'heuristic',
    evidence: ['test:t9'], missingEvidence: ['function_execution', 'function_execution'],
  },
  {
    id: 'scenario:gap-covered', obligationRef: 'obl:c', contractRefs: ['c3'],
    status: 'covered', kind: 'integration', confidence: 'exact',
    evidence: ['test:t1'], missingEvidence: [],
  },
];
const MUTANTS = [
  { subjectRef: 'contract:c1', mutator: 'BooleanLiteral', path: 'src/a.ts', line: 12, mutationStatus: 'survived', evidence: ['m:1'], confidence: 'extracted' },
  { subjectRef: 'contract:c2', mutator: 'Arithmetic', path: 'src/b.ts', line: 3, mutationStatus: 'no_coverage', evidence: ['m:2'], confidence: 'inferred' },
  { subjectRef: 'contract:c3', mutator: 'StringLiteral', path: 'src/c.ts', line: 7, mutationStatus: 'killed', evidence: ['m:3'], confidence: 'exact' },
];

check('hints: graph_stale info + gap-хинки (missing→high, partial→medium, covered skip)',
  A.buildStructureHints, {
    facts: SCEN_FACTS, dependencyRisk: RISK_HOT, dataQuality: DQ_STALE,
    scenarios: SCEN_GAP, mutationObservations: [],
  });

check('hints: hotspot covered skip + surviving мутанты (killed skip), dedup subjectRefs',
  A.buildStructureHints, {
    facts: SCEN_FACTS, dependencyRisk: RISK_HOT, dataQuality: DQ_FRESH,
    scenarios: [], mutationObservations: MUTANTS,
  });

check('hints: всё чисто — только hotspot-хинк',
  A.buildStructureHints, {
    facts: SCEN_FACTS, dependencyRisk: RISK_HOT, dataQuality: DQ_FRESH,
    scenarios: [SCEN_GAP[2]], mutationObservations: [MUTANTS[2]],
  });

check('hints: аргументы undefined/null',
  A.buildStructureHints, { facts: null, dependencyRisk: null, dataQuality: null, scenarios: null });

// ---------------------------------------------------------------------------
// Корпус: buildStructureAssessment (fixtures/structure + кастомные payload'ы)
// ---------------------------------------------------------------------------

const FACTS_FIXTURE = JSON.parse(readFileSync(resolve(REPO, 'fixtures/structure/structure-facts.v1.json'), 'utf8'));
const OBL_FIXTURE = JSON.parse(readFileSync(resolve(REPO, 'fixtures/structure/obligations-export.v1.json'), 'utf8'));
const NOW = '2026-09-03T00:00:00.000Z';

check('assessment: fixtures, lcov null, дефолтный testRun',
  A.buildStructureAssessment, {
    factsPayload: FACTS_FIXTURE,
    obligationsExport: OBL_FIXTURE,
    lcov: null,
    generatedAt: NOW,
  });

check('assessment: fixtures, lcov {hash} — aggregate ceiling, testRun null → TypeError',
  A.buildStructureAssessment, {
    factsPayload: FACTS_FIXTURE,
    obligationsExport: OBL_FIXTURE,
    lcov: { hash: 'deadbeef', totals: { lines: { total: 10, covered: 5 } }, files: [1, 2] },
    testRun: null,
    generatedAt: NOW,
  });

check('assessment: fixtures, lcov без hash — canonical-хеш с дробными fractions',
  A.buildStructureAssessment, {
    factsPayload: FACTS_FIXTURE,
    obligationsExport: OBL_FIXTURE,
    lcov: {
      totals: {
        lines: { total: 3, covered: 1 },
        fractions: { line: 0.3333333333333333, branch: 1e-7, function: 812782.2875976562 },
      },
      files: [],
    },
    testRun: { id: 'run-1', headSha: 'abcdef1', producedAt: '2026-09-02T12:00:00.000Z' },
    thresholds: { unresolvedJoinRate: 0.2 },
    generatedAt: NOW,
  });

const RICH_FACTS_PAYLOAD = {
  schemaRef: 'code-structure/structure-facts.v1',
  version: 1,
  kind: 'structure_facts',
  generatedAt: NOW,
  fingerprint: 'f'.repeat(64),
  provenance: {
    headSha: 'abcdef1234567890',
    graphStatus: 'fresh',
    truncated: false,
    providerCapabilities: { symbols: 'exact', topology: true, testInventory: true },
  },
  facts: {
    entities: FACTS.entities,
    contracts: SCEN_FACTS.contracts,
    dependencies: FACTS.dependencies,
    testCases: SCEN_FACTS.testCases,
  },
};
const RICH_OBLIGATIONS = {
  revision: 'docs-42',
  headSha: 'abcdef1234567890',
  obligations: [
    OBL({ id: 'obl:rich#1', contentHash: 'a'.repeat(64), contractRefs: ['c1', 'c2', 'nope'] }),
    OBL({ id: 'obl:rich#2', contentHash: 'b'.repeat(64), kind: 'happy_path', contractRefs: ['c1'] }),
    OBL({ id: 'obl:rich#3', contentHash: 'c'.repeat(64), kind: 'integration', contractRefs: ['c3'], status: 'draft' }),
  ],
};

check('assessment: rich payload — scenarios+ hints + merge наблюдений + receipts passthrough',
  A.buildStructureAssessment, {
    factsPayload: RICH_FACTS_PAYLOAD,
    obligationsExport: RICH_OBLIGATIONS,
    lcov: { hash: 'lcov-hash' },
    coverageObservations: [
      { id: 'obs:b', level: 'function_execution', subjectRef: 'e1', status: 'covered', evidence: ['x'] },
      { id: 'obs:a', level: 'function_execution', subjectRef: 'e2', confidence: 'unresolved' },
    ],
    propertyResults: PROP_PASS_EXACT.map((p) => ({ ...p, obligationRef: 'obl:rich#2' })),
    mutation: { observations: [
      { id: 'obs:m', level: 'mutation', subjectRef: 'contract:c1', mutationStatus: 'survived', mutator: 'BooleanLiteral', path: 'src/a.ts', line: 12, evidence: ['m:1'], confidence: 'extracted' },
    ] },
    receipts: [{ any: 'thing', n: 1 }],
    testRun: {},
    thresholds: { unresolvedJoinRate: 0.2, highFanIn: 2 },
    generatedAt: NOW,
  });

check('assessment: obligationsExport null, headSha testRun mismatch → alignment',
  A.buildStructureAssessment, {
    factsPayload: RICH_FACTS_PAYLOAD,
    obligationsExport: null,
    lcov: null,
    testRun: { headSha: 'ffffffffffff' },
    generatedAt: NOW,
  });

// ---------------------------------------------------------------------------
// temporalAlignment / assessmentFingerprint напрямую через бинарь
// ---------------------------------------------------------------------------

import { execFileSync } from 'node:child_process';

const ASSESSMENTS_BIN = resolve(REPO, 'rust/target/debug/structure_assessments');

function rustOp(op, args) {
  const raw = execFileSync(ASSESSMENTS_BIN, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
  });
  return JSON.parse(raw);
}

const { temporalAlignment, assessmentFingerprint } = await import('../../lib/structure-protocol.mjs');

for (const [i, args] of [
  [{ factsHeadSha: 'a', testHeadSha: 'a', obligationsHeadSha: 'a' }],
  [{ factsHeadSha: 'a', testHeadSha: 'b', obligationsHeadSha: 'a' }],
  [{ factsHeadSha: 'a', testHeadSha: 'a' }],
  [{ factsHeadSha: 'a' }],
  [{}],
  [{ factsHeadSha: 'a', testHeadSha: 'a', obligationsHeadSha: null }],
].entries()) {
  compared += 1;
  const js = temporalAlignment(...args);
  const rust = rustOp('temporalAlignment', args[0]);
  if (rust !== js) failures.push({ label: `temporalAlignment[${i}]`, js, rust });
}

for (const [i, args] of [
  [{ facts: 'f'.repeat(64), obligationsRevision: 'rev1', coverageHash: 'c1', thresholdsHash: 't1' }],
  [{ facts: 'f'.repeat(64), obligationsRevision: null, coverageHash: 'c1', thresholdsHash: 't1' }],
  [{ facts: 'f'.repeat(64), coverageHash: 'c1', thresholdsHash: 't1' }],
].entries()) {
  compared += 1;
  const js = assessmentFingerprint(...args);
  const rust = rustOp('assessmentFingerprint', args[0]);
  if (rust !== js) failures.push({ label: `assessmentFingerprint[${i}]`, js, rust });
}

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

console.log(`parity cases compared: ${compared}`);
if (failures.length) {
  console.log(`FAILURES: ${failures.length}`);
  for (const f of failures.slice(0, 20)) {
    console.log('---', f.label);
    console.log('  js  :', f.js);
    console.log('  rust:', f.rust);
  }
  process.exit(1);
}
console.log('PARITY OK');
