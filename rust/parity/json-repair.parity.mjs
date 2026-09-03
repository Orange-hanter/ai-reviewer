/**
 * Parity-прогон Фазы 1, шаг 2: Rust json_repair ↔ чистый JS fallback lib/json.mjs.
 *
 * Запуск: node rust/parity/json-repair.parity.mjs   (требует cargo build)
 *
 * Корпус:
 *   1. все литеральные входы из test/pr-review.test.mjs и test/github.test.mjs;
 *   2. реальные findings.json из fixtures/ (чистые, в fence, с прозой вокруг);
 *   3. генеративные обрезки реальных JSON на КАЖДОЙ позиции;
 *   4. мутации эскейпов (\d, \q, \uXX, непарные суррогаты, Windows-пути);
 *   5. repairBadEscapes на всём корпусе.
 *
 * Сравнение: успех/ошибка, флаги badEscape/truncated, deep-равенство значений,
 * вид ремонта (onRepaired). Тексты сообщений внутренних ошибок парсера V8
 * ("Unexpected token ...") сознательно не сравниваются — Rust даёт фиксированные
 * русские тексты (см. шапку lib/json.mjs); сравниваются только фиксированные
 * сообщения ('в ответе модели нет JSON' и т.п.).
 */

import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

process.env.AI_REVIEWER_JSON_REPAIR_FORCE_JS = '1';
const { extractJsonObject, repairBadEscapes } = await import('../../lib/json.mjs');

const here = dirname(fileURLToPath(import.meta.url));
const BIN = resolve(here, '../target/debug/json_repair');
const REPO = resolve(here, '../..');

const runRust = (op, args) =>
  JSON.parse(execFileSync(BIN, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  }));

const runRustOutcome = (op, args) => {
  try {
    return { ok: true, ...runRust(op, args) };
  } catch (e) {
    const raw = String(e.stderr || '').trim();
    if (!raw.startsWith('{')) {
      console.error('HARNESS: non-JSON stderr', JSON.stringify({ op, args: { text: String(args.text).slice(0, 60) } }), 'status', e.status, 'name', e.name, 'stderr', JSON.stringify(raw.slice(0, 200)));
      console.error(e.stack);
      process.exit(3);
    }
    const info = JSON.parse(raw);
    return { ok: false, badEscape: !!info.badEscape, truncated: !!info.truncated, message: info.error };
  }
};

const FIXED_MESSAGES = [
  'в ответе модели нет JSON',
  'не удалось исправить escape в оборванном JSON',
  'не удалось сбалансировать JSON из ответа модели',
  'невалидные escape в JSON ответа модели',
];

let compared = 0;
let skippedMsgCompare = 0;
const failures = [];

// Задокументированные отклонения Rust-порта (см. шапку lib/json.mjs и
// rust/src/json_repair.rs) — успех/ошибка и флаги совпадают, отличается значение:
//   - непарные \uD800/\uDC00: V8 кладёт lone surrogate, Rust — U+FFFD;
//   - "-0": Rust (serde_json arbitrary_precision) нормализует в 0, V8 даёт -0
//     (различимы только через Object.is; ===, stringify и арифметика совпадают).
const KNOWN_DEVIATIONS = new Set([
  'literal:{"s":"\\uD800"}',
  'literal:{"s":"\\uDC00"}',
  'literal:{"s":"\\uD800\\uD800"}',
  'literal:{"n":-0}',
]);
const deviations = [];

function isKnownDeviation(label) {
  return KNOWN_DEVIATIONS.has(label);
}

function compareExtract(label, text) {
  compared += 1;
  let jsResult;
  try {
    let jsRepaired = null;
    const value = extractJsonObject(text, { onRepaired: (k) => { jsRepaired = k; } });
    jsResult = { ok: true, value, repaired: jsRepaired };
  } catch (e) {
    jsResult = { ok: false, badEscape: !!e.badEscape, truncated: !!e.truncated, message: e.message };
  }

  const rustResult = runRustOutcome('extractJsonObject', { text });

  const fail = (why) => {
    const entry = { label, text: text.slice(0, 120), why, jsResult, rustResult };
    (isKnownDeviation(label, text) ? deviations : failures).push(entry);
  };

  if (jsResult.ok !== rustResult.ok) {
    return fail(`success mismatch: js=${jsResult.ok} rust=${rustResult.ok}`);
  }
  if (!jsResult.ok) {
    if (jsResult.badEscape !== rustResult.badEscape || jsResult.truncated !== rustResult.truncated) {
      return fail(`flags mismatch: js={badEscape:${jsResult.badEscape},truncated:${jsResult.truncated}} rust={badEscape:${rustResult.badEscape},truncated:${rustResult.truncated}}`);
    }
    const fixed = FIXED_MESSAGES.includes(jsResult.message);
    if (fixed && jsResult.message !== rustResult.message) {
      return fail(`fixed message mismatch: js="${jsResult.message}" rust="${rustResult.message}"`);
    }
    if (!fixed) skippedMsgCompare += 1;
    return;
  }
  // оба успешны: repaired-вид и значение
  if (jsResult.repaired !== rustResult.repaired) {
    return fail(`repaired mismatch: js=${jsResult.repaired} rust=${rustResult.repaired}`);
  }
  try {
    assert.deepEqual(rustResult.value, jsResult.value);
  } catch {
    fail(`value mismatch: js=${JSON.stringify(jsResult.value)?.slice(0, 200)} rust=${JSON.stringify(rustResult.value)?.slice(0, 200)}`);
  }
}

function compareRepair(label, s) {
  compared += 1;
  const js = repairBadEscapes(s);
  const rust = runRust('repairBadEscapes', { s });
  if (js !== rust) {
    failures.push({ label, text: s.slice(0, 120), why: 'repairBadEscapes mismatch', js, rust });
  }
}

// ---------------------------------------------------------------------------
// Корпус
// ---------------------------------------------------------------------------

const literalCases = [
  '{"findings":[]}',
  '```json\n{"a":1}\n```',
  '{"findings":[{"file":"a.ts","line":10,"body":"partial no close',
  '{"findings":[{"file":"a.ts","line":1},',
  '{"findings":[{"body":"use rm -rf { danger"',
  '{"findings":[{"body":"match \\\\d+ in path C:\\\\Users\\\\foo"}]}',
  '{"summary":"bad \\q escape"}',
  '{"findings":[{"line":',
  'нет json тут',
  '{"title":"\\d \\q"}',
  '{"c":"\\u00"}',
  '{"c":"\\uXYZ"}',
  // фиксированные сообщения / флаги
  '{"a":1}',
  '{"a":[1,2,],"b":{"c":1,},',
  '{"findings":[{"file":"b.ts","line":2,"title":"t"}],"runs":[],"sha":"dead',
  // fence-варианты
  '```js\n{"a":1}\n```',
  'преамбула ```json\n{"x":{"y":[1,2]}}\n``` постамбула',
  'вокруг ```json {"a":2} без закрытия',
  '```JSON\n{"a":3}\n```',
  'нет закрытия ```',
  '````\n{"a":4}\n````',
  '```json',
  // суррогаты / юникод
  '{"s":"\\uD800"}',
  '{"s":"\\uDC00"}',
  '{"s":"\\uD83D\\uDE00"}',
  '{"s":"\\uD800\\uD800"}',
  '{"s":"😀 emoji и текст"}',
  '{"s":"текст с é и \u{2028}"}',
  // числа
  '{"n":1e999}',
  '{"n":-0}',
  '{"n":1.0}',
  '{"n":12345678901234567890123}',
  '{"n":0.30000000000000004}',
  '{"n":1E4}',
  '{"n":-1.5e-10}',
  // глубина (guard в Rust на >4096)
  `{"a":${'{"b":'.repeat(100)}1${'}'.repeat(100)}}`,
  // пустые/мелкие структуры
  '{}',
  '{"a":[]}',
  '{"a":{}}',
  'x {"a":1} y',
  '"просто строка {"',
  '\\{"a":1}',
  '[{"a":1}]',
  '{"a":1} {"b":2}',
];

const fixtureFiles = [
  'fixtures/consensus-major-two-models/findings.json',
  'fixtures/gate-critical-high-conf/findings.json',
  'fixtures/gate-critical-low-conf/findings.json',
  'fixtures/fp-pr-254-file-in-diff/findings.json',
  'fixtures/fp-pr-257-file-and-e2e-in-diff/findings.json',
  'fixtures/verifier-rejects-fp-block/findings.json',
  'fixtures/refutation-all-rejected/findings.json',
  'fixtures/refutation-verified-keeps/findings.json',
  'fixtures/verifier-downgrade-critical/findings.json',
  'fixtures/prescan-astgrep-xff/findings.json',
];

const realJson = [];
for (const rel of fixtureFiles) {
  realJson.push({ label: rel, text: readFileSync(resolve(REPO, rel), 'utf8') });
}

// 1. литеральные кейсы
for (const [i, text] of literalCases.entries()) {
  compareExtract(`literal:${text}`, text);
  compareRepair(`literal#${i} repair`, text);
}

// 2. реальные findings.json: чистые / fence / с прозой
for (const { label, text } of realJson) {
  compareExtract(`${label} clean`, text);
  compareExtract(`${label} fenced`, '```json\n' + text + '\n```');
  compareExtract(`${label} prose`, 'Вот ответ модели:\n' + text + '\nКонец.');
  compareRepair(`${label} repair`, text);
}

// 3. генеративные обрезки на КАЖДОЙ позиции нескольких реальных JSON
for (const { label, text } of realJson.slice(0, 4)) {
  for (let i = 0; i <= text.length; i += 1) {
    compareExtract(`${label} trunc@${i}`, text.slice(0, i));
  }
}

// 4. мутации эскейпов: вставляем битые эскейпы внутрь строк значений
const mutationTargets = [
  realJson[0].text,
  realJson[3].text,
  '{"findings":[{"file":"a.ts","line":1,"body":"plain body text"}],"summary":"ok"}',
];
const badEscapes = ['\\d', '\\q', '\\w', '\\U', '\\ ', '\\-', '\\u00', '\\u0', '\\u', '\\uXYZ', '\\', '\\\\\\d'];
for (const [t, target] of mutationTargets.entries()) {
  // вставляем после каждой найденной кавычки значения (ограниченно — первые 40 позиций)
  const positions = [];
  for (let i = 0; i < target.length && positions.length < 40; i += 1) {
    if (target[i] === '"' && ': "'.includes(target.slice(i - 1, i + 1)) ) positions.push(i + 1);
  }
  for (const pos of positions) {
    for (const esc of badEscapes) {
      compareExtract(`mutate t${t}@${pos} ${esc}`, target.slice(0, pos) + esc + target.slice(pos));
    }
  }
}

// 5. repairBadEscapes на обрезках и мутированных вариантах
for (const { label, text } of realJson.slice(0, 3)) {
  for (let i = 0; i <= text.length; i += 7) {
    compareRepair(`${label} repair trunc@${i}`, text.slice(0, i));
  }
}

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

console.log(`parity cases compared: ${compared}`);
console.log(`engine-message cases (message not compared, flags compared): ${skippedMsgCompare}`);
console.log(`known documented deviations (value differs, success/flags match): ${deviations.length}`);
for (const d of deviations) {
  console.log('  ~', d.label.slice(0, 80), '—', d.why);
}
if (failures.length) {
  console.log(`FAILURES: ${failures.length}`);
  for (const f of failures.slice(0, 20)) {
    console.log('---', f.label);
    console.log('  text :', JSON.stringify(f.text));
    console.log('  why  :', f.why);
    console.log('  js   :', JSON.stringify(f.jsResult)?.slice(0, 300));
    console.log('  rust :', JSON.stringify(f.rust)?.slice(0, 300));
  }
  process.exit(1);
}
console.log('PARITY OK');
