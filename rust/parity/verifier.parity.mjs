/**
 * Parity-прогон Фазы 3, шага 3: Rust verifier ↔ чистый JS fallback.
 *
 * Запуск: node rust/parity/verifier.parity.mjs   (требует cargo build)
 *
 * Матрица prove/downgrade/битый ответ для selectVerifierCandidates и
 * applyVerifierVerdicts (lib/verifier.mjs). Диспетчеризация — env
 * AI_REVIEWER_VERIFIER_FORCE_JS (читается на каждый вызов): js-сторона с
 * FORCE_JS=1, rust-сторона — бинарь rust/target/debug/verifier. Входы для
 * каждой ветки собираются заново (apply мутирует находки), сравниваются
 * findings (JSON), stats и строки лога.
 */

import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

if (!existsSync(resolve(REPO, 'rust/target/debug/verifier'))) {
  console.error('нет бинаря rust/target/debug/verifier — сначала cargo build');
  process.exit(1);
}

const { selectVerifierCandidates, applyVerifierVerdicts } = await import('../../index.mjs');

let compared = 0;
const failures = [];

/** Прогон fn на обеих ветках со свежими входами; сравнение findings/stats/logs. */
function check(label, mkInput) {
  compared += 1;
  const run = (forceJs) => {
    if (forceJs) process.env.AI_REVIEWER_VERIFIER_FORCE_JS = '1';
    else delete process.env.AI_REVIEWER_VERIFIER_FORCE_JS;
    try {
      const { findings, candidates, verdicts, gate, max, advisory, strict, log } = mkInput();
      const logs = [];
      const selected = selectVerifierCandidates(findings, gate, max, advisory);
      const applied = applyVerifierVerdicts(findings, selected, verdicts, (m) => logs.push(m), { strictUnverified: strict });
      return JSON.parse(JSON.stringify({ findings, selected, applied, logs }));
    } finally {
      delete process.env.AI_REVIEWER_VERIFIER_FORCE_JS;
    }
  };
  const js = run(true);
  const rust = run(false);
  try {
    assert.deepEqual(rust, js);
  } catch (e) {
    failures.push({
      label,
      js: JSON.stringify(js)?.slice(0, 500),
      rust: JSON.stringify(rust)?.slice(0, 500),
      err: e.message,
    });
  }
}

const GATE = { blockOn: ['critical', 'major'] };

const mkFindings = () => ([
  { file: 'a.ts', line: 1, severity: 'major', title: 'M1', body: 'b1', confidence: 0.9, model: 'm1' },
  { file: 'b.ts', line: 2, severity: 'critical', title: 'C1', body: 'b2', confidence: 0.8, model: 'm2' },
  { file: 'c.ts', line: 3, severity: 'critical', title: 'C2', body: 'b3', confidence: 0.7, model: 'm1', prescan: true },
  { file: 'd.ts', line: 4, severity: 'nit', title: 'N1', body: 'b4', confidence: 0.3, model: 'm1' },
  { file: 'e.ts', line: 5, severity: 'critical', title: 'C3', body: 'b5', confidence: 0.95, model: 'm3' },
  { file: 'f.ts', line: 6, severity: 'major', title: 'M2', body: '', confidence: null, model: 'm2' },
]);

// --- select: сортировка по severity, prescan отсечён, кап max ---

check('select: critical first, prescan skipped, cap', () => {
  const findings = mkFindings();
  return {
    findings,
    gate: GATE,
    max: 2,
    advisory: {},
    verdicts: [],
    strict: false,
    log: () => {},
  };
});

check('select: max default-ish 12, no cap hit', () => {
  const findings = mkFindings();
  return { findings, gate: GATE, max: 12, advisory: {}, verdicts: [], strict: false, log: () => {} };
});

check('select: advisory appended, dedup vs blockers, advisory cap', () => {
  const findings = mkFindings();
  return {
    findings,
    gate: GATE,
    max: 12,
    advisory: { candidates: [findings[3], findings[4], findings[2], findings[0]], max: 2 },
    verdicts: [],
    strict: false,
    log: () => {},
  };
});

check('select: advisory disabled (max 0)', () => {
  const findings = mkFindings();
  return {
    findings,
    gate: GATE,
    max: 12,
    advisory: { candidates: [findings[3]], max: 0 },
    verdicts: [],
    strict: false,
    log: () => {},
  };
});

check('select: blockOn without major', () => {
  const findings = mkFindings();
  return { findings, gate: { blockOn: ['critical'] }, max: 12, advisory: {}, verdicts: [], strict: false, log: () => {} };
});

// --- apply: матрица prove / downgrade / битый ответ ---

check('apply: verified keeps, rejected removed, downgrade nit', () => {
  const findings = mkFindings();
  const selected = [findings[1], findings[4], findings[0]];
  return {
    findings,
    candidates: selected,
    gate: GATE,
    max: 12,
    advisory: {},
    verdicts: [
      { candidateIndex: 0, decision: 'verified', reason: 'по коду видно' },
      { candidateIndex: 1, decision: 'rejected', reason: 'уже исправлено' },
      { candidateIndex: 2, decision: 'downgrade', reason: 'не блокер', severity: 'nit' },
    ],
    strict: false,
    log: () => {},
  };
});

check('apply: downgrade severity garbage → minor; missing reason → ""', () => {
  const findings = mkFindings();
  const selected = [findings[1], findings[0]];
  return {
    findings,
    candidates: selected,
    gate: GATE,
    max: 12,
    advisory: {},
    verdicts: [
      { candidateIndex: 0, decision: 'downgrade', severity: 'critical' },
      { candidateIndex: 1, decision: 'verified', reason: null },
    ],
    strict: false,
    log: () => {},
  };
});

check('apply: битый ответ — нет verdicts (fail-open) и strict (fail-closed)', () => {
  const findings = mkFindings();
  const selected = [findings[1], findings[0]];
  return {
    findings,
    candidates: selected,
    gate: GATE,
    max: 12,
    advisory: {},
    verdicts: [{ candidateIndex: 5, decision: 'verified' }, { candidateIndex: 1.5, decision: 'rejected' }],
    strict: true,
    log: () => {},
  };
});

check('apply: частично missing verdicts, strict', () => {
  const findings = mkFindings();
  const selected = [findings[1], findings[4], findings[0]];
  return {
    findings,
    candidates: selected,
    gate: GATE,
    max: 12,
    advisory: {},
    verdicts: [{ candidateIndex: 1, decision: 'verified', reason: 'ок' }],
    strict: true,
    log: () => {},
  };
});

check('apply: последний вердикт побеждает; не-строковый truthy decision → verified', () => {
  const findings = mkFindings();
  const selected = [findings[1], findings[0]];
  return {
    findings,
    candidates: selected,
    gate: GATE,
    max: 12,
    advisory: {},
    verdicts: [
      { candidateIndex: 0, decision: 'rejected', reason: 'первый' },
      { candidateIndex: 0, decision: 'verified', reason: 'последний' },
      { candidateIndex: 1, decision: true },
    ],
    strict: false,
    log: () => {},
  };
});

check('apply: advisory-кандидаты (nit/minor) проходят ту же матрицу', () => {
  const findings = mkFindings();
  const selected = selectVerifierCandidates(findings, GATE, 12, { candidates: [findings[3], findings[5]], max: 2 });
  return {
    findings,
    candidates: selected,
    gate: GATE,
    max: 12,
    advisory: {},
    verdicts: [
      { candidateIndex: selected.length - 2, decision: 'verified', reason: 'доказано' },
      { candidateIndex: selected.length - 1, decision: 'downgrade', reason: 'мелочь', severity: 'minor' },
    ],
    strict: false,
    log: () => {},
  };
});

check('apply: пустые verdicts, не-strict — находки как есть', () => {
  const findings = mkFindings();
  const selected = [findings[1], findings[4]];
  return {
    findings,
    candidates: selected,
    gate: GATE,
    max: 12,
    advisory: {},
    verdicts: [],
    strict: false,
    log: () => {},
  };
});

console.log(`parity cases compared: ${compared}`);
if (failures.length) {
  console.log(`FAILURES: ${failures.length}`);
  for (const f of failures.slice(0, 20)) {
    console.log('---', f.label);
    console.log('  err :', f.err);
    console.log('  js  :', f.js);
    console.log('  rust:', f.rust);
  }
  process.exit(1);
}
console.log('PARITY OK');
