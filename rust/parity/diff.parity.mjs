/**
 * Parity-прогон: Rust diff ↔ чистый JS fallback lib/diff.mjs — извлечение символов
 * для Graphify blast-radius (extractChangedSymbols / collectBlastSymbols).
 *
 * Запуск: node rust/parity/diff.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/diff.mjs: resolveDiffBin() читает
 * AI_REVIEWER_DIFF_FORCE_JS на каждый вызов, поэтому js-сторона прогоняется
 * с FORCE_JS=1, rust-сторона — без него.
 *
 * Корпус: changedText всех diff.patch из fixtures/, литеральные JS- и Python-кейсы,
 * границы строк, на которых флаг `m` в JS и `(?m)` в Rust разошлись бы
 * (`\r\n`, голый `\r`, U+2028), и добор из полного текста по расширениям.
 */

import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

if (!existsSync(resolve(REPO, 'rust/target/debug/diff'))) {
  console.error('diff parity: нет rust/target/debug/diff — сначала cargo build');
  process.exit(1);
}

const D = await import('../../lib/diff.mjs');

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_DIFF_FORCE_JS = '1';
  try {
    return fn(...args);
  } finally {
    delete process.env.AI_REVIEWER_DIFF_FORCE_JS;
  }
};

let compared = 0;
const failures = [];

function compare(label, jsResult, rustResult) {
  compared += 1;
  try {
    assert.deepEqual(rustResult, jsResult);
  } catch {
    failures.push({ label, js: JSON.stringify(jsResult), rust: JSON.stringify(rustResult) });
  }
}

function checkExtract(label, text, max = 25) {
  compare(
    `extractChangedSymbols ${label}`,
    runJs(D.extractChangedSymbols, text, max),
    D.extractChangedSymbols(text, max),
  );
}

function checkCollect(label, changed, contentMap, max = 25) {
  compare(
    `collectBlastSymbols ${label}`,
    runJs(D.collectBlastSymbols, changed, contentMap, max),
    D.collectBlastSymbols(changed, contentMap, max),
  );
}

const literals = {
  js: 'export function foo() {}\nexport const barBaz = async () => {}\nexport class Widget {}\nexport { a1b, c2d as e3f };',
  python: 'def row_id_for_slug(session, slug):\nasync def fetch_all(x):\nclass ProjectNotFoundError(LookupError):\nclass Plain:\ndef _private(x):\n    def method(self):\ndef ab(x):',
  pythonAdjacent: 'class Aaa:\nclass Bbb(Aaa):\ndef ccc():\ndef ddd():',
  crlf: 'def crlf_one(x):\r\ndef crlf_two(y):\r\nclass CrlfThree:\r\n',
  u2028: 'x = 1 def not_a_line_start(x):\ndef real_one(x):',
  cr: 'x = 1\rdef after_bare_cr(x):\n',
  mixed: 'export function shared() {}\ndef py_side(x):\nclass JsClass {}\nclass PyClass(object):',
  tabs: 'async\tdef\ttabbed_name\t(x):\nclass\tTabbed\t:',
  unicode: 'def ünïcode(x):\ndef ascii_ok(x):',
};
for (const [label, text] of Object.entries(literals)) {
  checkExtract(label, text);
  checkExtract(`${label} max=2`, text, 2);
}

const fixturesDir = resolve(REPO, 'fixtures');
for (const id of readdirSync(fixturesDir)) {
  const patch = resolve(fixturesDir, id, 'diff.patch');
  if (!existsSync(patch)) continue;
  const { changedText } = runJs(D.parseDiff, readFileSync(patch, 'utf8'));
  checkExtract(`fixture ${id}`, changedText);
}

const pyFile = 'import x\n\n\ndef public_one(a):\n    return a\n\n\ndef _private(a):\n    pass\n\n\nclass Model(Base):\n    def method(self):\n        pass\n';
const jsFile = 'export function fromJs() {}\nexport const notFn = 1;\n';
checkCollect('py full file', '    return a\n', new Map([['pkg/mod.py', pyFile]]));
checkCollect('pyi stub', '', new Map([['pkg/mod.pyi', 'def stubbed(a: int) -> int: ...\n']]));
checkCollect('PY upper ext', '', new Map([['pkg/MOD.PY', pyFile]]));
checkCollect('non-code skipped', '', new Map([['README.md', 'def looks_like_code(x):\n'], ['pkg/mod.py', pyFile]]));
checkCollect('mixed order', 'def from_hunk(x):\n', new Map([['a.ts', jsFile], ['b.py', pyFile]]));
checkCollect('cap', '', new Map([['b.py', pyFile], ['a.ts', jsFile]]), 2);

if (failures.length) {
  console.error(`diff parity: ${failures.length}/${compared} расхождений`);
  for (const f of failures) console.error(`  ${f.label}\n    js:   ${f.js}\n    rust: ${f.rust}`);
  process.exit(1);
}
console.log(`diff parity: ${compared} сравнений, расхождений нет`);
