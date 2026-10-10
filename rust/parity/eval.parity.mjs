/**
 * Parity-прогон T28 (RFC 16): `pr-review eval` ↔ `node bin/pr-review-eval.mjs`.
 *
 * Запуск: node rust/parity/eval.parity.mjs   (требует cargo build)
 *
 * JS-сторона идёт с FORCE_JS=1 на всех обёртках, которые зовёт lib/eval.mjs
 * (diff, findings, verifier, specialists, config), — эталон чистый JS, а не JS поверх
 * тех же Rust-бинарей. Сверяются побайтово stdout текстового отчёта, stdout `--json`
 * и код возврата.
 *
 * Матрица:
 *   1. fixtures/ пакета — все seed-кейсы (тот же набор и итог, что `npm run eval`);
 *   2. синтетический каталог: проваленные ожидания (shouldBlock/findingCount/
 *      blockerCount/titles/noTitles), gate-override, кейс без description,
 *      expected.shouldBlock = null, неполный каталог (пропускается);
 *   3. несуществующий каталог — 0/0, exit 0.
 */

import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');
const BIN = resolve(REPO, 'rust/target/debug/pr-review');
if (!existsSync(BIN)) {
  console.error(`нет бинаря ${BIN} — сначала cargo build`);
  process.exit(1);
}

const FORCE_JS = Object.fromEntries(
  ['DIFF', 'FINDINGS', 'VERIFIER', 'SPECIALISTS', 'CONFIG', 'JSON_REPAIR']
    .map((m) => [`AI_REVIEWER_${m}_FORCE_JS`, '1']),
);

function runJs(args) {
  return spawnSync(process.execPath, [join(REPO, 'bin/pr-review-eval.mjs'), ...args], {
    encoding: 'utf8', env: { ...process.env, ...FORCE_JS },
  });
}
function runRs(args) {
  return spawnSync(BIN, ['eval', ...args], { encoding: 'utf8', env: process.env });
}

let checks = 0;
function same(label, args) {
  for (const extra of [[], ['--json']]) {
    const js = runJs([...args, ...extra]);
    const rs = runRs([...args, ...extra]);
    assert.equal(rs.stdout, js.stdout, `${label} ${extra.join(' ')}: stdout`);
    assert.equal(rs.status, js.status, `${label} ${extra.join(' ')}: exit code`);
    checks++;
  }
}

// 1. fixtures пакета
same('fixtures/', []);

// 2. синтетика
const tmp = mkdtempSync(join(tmpdir(), 'eval-parity-'));
try {
  const diff = [
    'diff --git a/src/a.ts b/src/a.ts', '--- a/src/a.ts', '+++ b/src/a.ts',
    '@@ -0,0 +1,3 @@', '+const a = 1;', '+const b = 2;', '+export { a, b };', '',
  ].join('\n');
  const cases = {
    'z-fail-all': {
      meta: { id: 'z-fail-all', description: 'всё мимо', expected: {
        shouldBlock: true, findingCount: 7, blockerCount: 2,
        titles: ['нет такого'], noTitles: ['мелочь'],
      } },
      findings: [
        { file: 'src/a.ts', line: 1, severity: 'minor', title: 'мелочь', body: 'x', model: 'm1', confidence: 0.9 },
      ],
    },
    'a-override-block': {
      meta: { expected: { shouldBlock: true }, gate: { blockOn: ['minor'], blockSingle: ['minor'], confidenceMin: 0.1 } },
      findings: [
        { file: 'src/a.ts', line: 2, severity: 'minor', title: 'блок по override', body: 'y', model: 'm1', confidence: 0.75 },
        { file: 'src/a.ts', line: 3, severity: 'major', title: 'без claims', body: 'z', model: 'm2',
          confidence: 1, advisorClaims: [{ role: 'security', stance: 'refute', reason: 'нет' }] },
      ],
    },
    'm-null-should-block': {
      meta: { id: 'm-null-should-block', description: '', expected: { shouldBlock: null, findingCount: 1 } },
      findings: [{ file: 'src/a.ts', line: 1, severity: 'critical', title: 'крит', body: 'c', model: 'm1' }],
      verifier: { verdicts: [{ candidateIndex: 0, decision: 'downgrade', reason: 'не доказано' }] },
    },
  };
  for (const [name, c] of Object.entries(cases)) {
    const d = join(tmp, name);
    mkdirSync(d);
    writeFileSync(join(d, 'meta.json'), JSON.stringify(c.meta));
    writeFileSync(join(d, 'diff.patch'), diff);
    writeFileSync(join(d, 'findings.json'), JSON.stringify({ findings: c.findings }));
    if (c.verifier) writeFileSync(join(d, 'verifier.json'), JSON.stringify(c.verifier));
  }
  mkdirSync(join(tmp, 'incomplete'));
  writeFileSync(join(tmp, 'incomplete', 'meta.json'), '{}');
  same('синтетика', ['--fixtures', tmp]);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}

// 3. нет каталога
same('нет каталога', ['--fixtures', join(tmpdir(), 'eval-parity-nonexistent-dir')]);

console.log(`eval parity: ${checks} сверок сошлись`);
