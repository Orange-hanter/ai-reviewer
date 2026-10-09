/**
 * Parity-прогон Фазы 2, шаг 5: Rust structure_mcp ↔ чистый JS fallback
 * lib/structure-mcp.mjs (MCP-style JSON-RPC-ish сервер structure_context).
 *
 * Запуск: node rust/parity/structure-mcp.parity.mjs   (требует cargo build)
 *
 * Архитектура порта — (b): серверный stdio-цикл с Content-Length-фреймингом
 * остаётся в JS (bin/pr-review-structure-mcp.mjs через lib/mcp-stdio.mjs), в
 * Rust уходит доменная логика ответа на запрос (опы бинаря structure_mcp:
 * handleRequest для «чистых» методов и buildStructureContext для tools/call).
 * Поэтому сравнение идёт КАНАЛЬНОЕ: обе стороны прогоняются через один и тот же
 * bin (FORCE_JS=1 vs бинарь по умолчанию), а сравниваются ответные фреймы
 * целиком — байт в байт (и отдельно разобранные JSON для диагностики).
 *
 * Корпус (каждая сессия — отдельный спавн сервера, порядок изолирован):
 *   - initialize (дефолт и свой protocolVersion), notifications/initialized,
 *     ping, tools/list (дескриптор тулза целиком);
 *   - tools/call structure_context: валидный вызов по headSha и по
 *     snapshotFingerprint, taskRef, dependencyDepth 0/3, contractRefs,
 *     tokenBudget 256 (усечение), аргументы за границами inputSchema
 *     (25 scopeRefs/contractRefs, tokenBudget строка '5000' и 10_000_000,
 *     dependencyDepth 99 → клэмп);
 *   - битовые аргументы: без пина, unknown tool, несовпавший headSha /
 *     snapshotFingerprint, перепутанные kind'ы файлов ('expected
 *     structure_facts at ...'), отсутствующий файл (ENOENT), выход за
 *     STRUCTURE_MCP_ROOT ('escapes allowed root'), assessment со чужим
 *     factsFingerprint;
 *   - протокол: batch с валидными/скалярными элементами, пустой batch,
 *     скалярный кадр, битый JSON, unknown method, разрезанный пополам кадр
 *     (partial reads), oversized-кадр (>8 MiB) последним в сессии.
 *
 * Отклонения (сверяются только вердикты/тексты — в корпус не включены):
 *   - фрейминг односторонний (JS), поэтому поведение транспорта на Rust не
 *     портируется в этом шаге — сессии лишь убеждаются, что он не разъезжается;
 *   - синтаксически битый JSON пейлоада: тексты V8 vs anyhow не совпадают
 *     (как в structure-protocol), но сторона parsing у обеих реализаций одна
 *     (бинарь structure_protocol) — кейс вне канального сравнения.
 */

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');
const BIN = resolve(REPO, 'rust/target/debug/structure_mcp');
const SERVER = fileURLToPath(new URL('../../bin/pr-review-structure-mcp.mjs', import.meta.url));

if (!existsSync(BIN)) {
  console.error(`бинарь не найден: ${BIN} — сначала cargo build`);
  process.exit(1);
}

// ---------------------------------------------------------------------------
// Синтетические пейлоады (как в cargo-тестах, но богаче: цепочка a→b→c)
// ---------------------------------------------------------------------------

const sha = (c) => c.repeat(64);
const HEAD = 'abcdef1';

const entity = (id, path, boundary) => ({
  observedId: id, lineageId: id, label: `${id}()`, kind: 'function',
  path, boundaryId: boundary, confidence: 'extracted', evidence: [], externalIds: {},
});
const contract = (id, entityRef) => ({
  id, entityRef, kind: 'entrypoint', confidence: 'extracted', evidence: [],
});
const edge = (id, from, to) => ({
  id, from, to, relation: 'calls', confidence: 'extracted', evidence: [],
});

const FACTS = {
  schemaRef: 'code-structure/structure-facts.v1',
  version: 1,
  kind: 'structure_facts',
  generatedAt: '2026-09-02T00:00:00.000Z',
  fingerprint: sha('a'),
  provenance: {
    headSha: HEAD, branchRef: 'main', prNumber: null, isDefaultBranch: true,
    toolVersion: '1.0.0', configHash: sha('1'), scope: 'src',
  },
  facts: {
    boundaries: [
      { id: 'b:src', kind: 'module', name: 'src', paths: ['src'] },
      { id: 'b:test', kind: 'module', name: 'test', paths: ['test'] },
    ],
    entities: [
      entity('e:a', 'src/a.ts', 'b:src'),
      entity('e:b', 'src/b.ts', 'b:src'),
      entity('e:c', 'src/c.ts', 'b:src'),
    ],
    contracts: [contract('c:a', 'e:a'), contract('c:b', 'e:b')],
    dependencies: [edge('d:ab', 'e:a', 'e:b'), edge('d:bc', 'e:b', 'e:c')],
    testCases: [{
      id: 't:1', name: 'works', path: 'src/a.test.ts', line: 1,
      targetContractRefs: ['c:a'], confidence: 'extracted', evidence: [],
    }],
  },
  identityEvents: [], views: {}, receipts: [],
};

const ASSESSMENT = {
  schemaRef: 'code-structure/structure-assessment.v1',
  version: 1,
  kind: 'structure_assessment',
  generatedAt: '2026-09-02T00:00:00.000Z',
  fingerprint: sha('b'),
  factsFingerprint: sha('a'),
  coverageHash: sha('c'),
  thresholdsHash: sha('d'),
  temporalAlignment: 'aligned',
  obligationsRevision: 'rev:1',
  coverageObservations: [],
  assessments: {
    contractScenarios: [{
      id: 's:1', contractRefs: ['c:a'], obligationRef: 'o:1',
      status: 'covered', missingEvidence: [],
    }],
  },
  hints: [{ id: 'h:1', subjectRefs: ['e:a'], kind: 'note', message: 'check' }],
  receipts: [],
};

// валидный по схеме, но ссылается на другой facts-снапшот
const ASSESSMENT_UNLINKED = { ...ASSESSMENT, fingerprint: sha('e'), factsFingerprint: sha('f') };

const root = mkdtempSync(join(tmpdir(), 'structure-mcp-parity-'));
writeFileSync(join(root, 'facts.json'), JSON.stringify(FACTS));
writeFileSync(join(root, 'assessment.json'), JSON.stringify(ASSESSMENT));
writeFileSync(join(root, 'assessment-unlinked.json'), JSON.stringify(ASSESSMENT_UNLINKED));

// ---------------------------------------------------------------------------
// Канальный прогон сервера
// ---------------------------------------------------------------------------

let compared = 0;
const failures = [];

function frame(payload) {
  const body = Buffer.from(typeof payload === 'string' ? payload : JSON.stringify(payload));
  // регистр заголовка произвольный, неизвестный заголовок игнорируется
  return Buffer.concat([
    Buffer.from(`cOnTeNt-LeNgTh: ${body.length}\r\nX-Unknown: ignored\r\n\r\n`),
    body,
  ]);
}

function parseFrames(buffer) {
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

async function runServer(payloads, { forceJs, split = false }) {
  const env = {
    ...process.env,
    STRUCTURE_MCP_ROOT: root,
    AI_REVIEWER_STRUCTURE_MCP_BIN: BIN,
  };
  if (forceJs) env.AI_REVIEWER_STRUCTURE_MCP_FORCE_JS = '1';
  else delete env.AI_REVIEWER_STRUCTURE_MCP_FORCE_JS;
  const child = spawn(process.execPath, [SERVER], { env });
  const watchdog = setTimeout(() => child.kill('SIGKILL'), 15000);
  const stdout = [];
  const stderr = [];
  child.stdout.on('data', (chunk) => stdout.push(chunk));
  child.stderr.on('data', (chunk) => stderr.push(chunk));
  for (const payload of payloads) {
    if (Buffer.isBuffer(payload)) {
      child.stdin.write(payload);
      continue;
    }
    const framed = frame(payload);
    if (split) {
      const mid = Math.floor(framed.length / 2);
      child.stdin.write(framed.subarray(0, mid));
      await new Promise((r) => setTimeout(r, 50));
      child.stdin.write(framed.subarray(mid));
    } else {
      child.stdin.write(framed);
    }
  }
  child.stdin.end();
  const exitCode = await new Promise((r) => child.on('close', r));
  clearTimeout(watchdog);
  return {
    exitCode,
    stderr: Buffer.concat(stderr).toString('utf8'),
    raw: Buffer.concat(stdout),
  };
}

/** Сессия против обеих реализаций: байт-равенство фреймов + deepEqual разбора. */
async function checkSession(label, payloads, opts = {}) {
  compared += 1;
  const js = await runServer(payloads, { ...opts, forceJs: true });
  const rust = await runServer(payloads, { ...opts, forceJs: false });
  for (const [side, result] of [['js', js], ['rust', rust]]) {
    if (result.exitCode !== 0) {
      failures.push({ label, js: `js exit ${js.exitCode}: ${js.stderr.slice(0, 300)}`, rust: `rust exit ${rust.exitCode}: ${rust.stderr.slice(0, 300)}` });
      return;
    }
  }
  if (js.raw.equals(rust.raw)) return;
  try {
    assert.deepEqual(parseFrames(rust.raw), parseFrames(js.raw));
    failures.push({
      label: `${label} (семантика совпала, байты фреймов разошлись)`,
      js: js.raw.subarray(0, 400).toString('utf8'),
      rust: rust.raw.subarray(0, 400).toString('utf8'),
    });
  } catch (error) {
    failures.push({
      label: `${label} (фреймы разошлись)`,
      js: JSON.stringify(parseFrames(js.raw))?.slice(0, 600),
      rust: JSON.stringify(parseFrames(rust.raw))?.slice(0, 600),
    });
  }
}

const call = (toolArgs, id = 100) => ({
  jsonrpc: '2.0', id, method: 'tools/call',
  params: { name: 'structure_context', arguments: toolArgs },
});
const baseArgs = { factsPath: 'facts.json', assessmentPath: 'assessment.json', headSha: HEAD };

// --- сессия 1: чистые методы + протокольные края ---
await checkSession('pure methods + protocol edges', [
  { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
  { jsonrpc: '2.0', id: 2, method: 'initialize', params: { protocolVersion: '2024-11-05' } },
  { jsonrpc: '2.0', method: 'notifications/initialized' },
  { jsonrpc: '2.0', id: 3, method: 'ping' },
  { jsonrpc: '2.0', id: 4, method: 'tools/list' },
  { jsonrpc: '2.0', id: 5, method: 'nope/method' },
  { jsonrpc: '2.0', id: 6, method: 'tools/call', params: { name: 'mystery', arguments: {} } },
  [{ jsonrpc: '2.0', id: 7, method: 'ping' }, 42, { jsonrpc: '2.0', id: 8, method: 'tools/list' }],
  [],
  42,
  'null',
  '{not json',
], { split: true });

// --- сессия 2: валидные tools/call ---
await checkSession('tools/call: headSha, snapshot, depth, truncation, clamp', [
  call(baseArgs),
  call({ ...baseArgs, snapshotFingerprint: sha('a') }, 101), // оба пина сразу
  call({ ...baseArgs, snapshotFingerprint: sha('a'), taskRef: 'task-9' }, 102),
  call({ ...baseArgs, scopeRefs: ['e:a'], dependencyDepth: 0 }, 103),
  call({ ...baseArgs, scopeRefs: ['e:a'], dependencyDepth: 3 }, 104),
  call({ ...baseArgs, scopeRefs: ['src/a.ts'], dependencyDepth: 3 }, 105), // префиксный матч по пути
  call({ ...baseArgs, contractRefs: ['c:a'] }, 106),
  call({ ...baseArgs, scopeRefs: ['e:a'], tokenBudget: 256 }, 107), // усечение бюджетом
  call({
    ...baseArgs,
    scopeRefs: Array.from({ length: 25 }, (_, i) => `scope-${i}`),
    contractRefs: Array.from({ length: 25 }, (_, i) => `c-${i}`),
    tokenBudget: '5000', // строка → Number
    dependencyDepth: 99, // клэмп до 3
  }, 108),
  call({ ...baseArgs, tokenBudget: 10_000_000 }, 109), // выше максимума → 32000
  call({ factsPath: 'facts.json', assessmentPath: 'assessment.json' }, 110), // без пина
  call({ ...baseArgs, scopeRefs: [] }, 111), // пустой scope — пустой контекст
]);

// --- сессия 3: битовые аргументы и файлы ---
await checkSession('tools/call: broken args and files', [
  call({ ...baseArgs, headSha: 'deadbeef' }, 200),
  call({ factsPath: 'facts.json', assessmentPath: 'assessment.json', snapshotFingerprint: sha('f') }, 201),
  call({ ...baseArgs, assessmentPath: 'assessment-unlinked.json' }, 202),
  call({ factsPath: 'assessment.json', assessmentPath: 'assessment.json', headSha: HEAD }, 203), // перепутан kind
  call({ factsPath: 'missing.json', assessmentPath: 'assessment.json', headSha: HEAD }, 204), // ENOENT
  call({ factsPath: '../facts.json', assessmentPath: 'assessment.json', headSha: HEAD }, 205), // escape
  call({ factsPath: 'facts.json', assessmentPath: '../assessment.json', headSha: HEAD }, 206), // escape assessment
  call({ ...baseArgs, arguments: null }, 207), // arguments || {}
].map((m) => (m.id === 207 ? { ...m, params: { name: 'structure_context', arguments: null } } : m)));

// --- сессия 4: id разных типов и не-объектные запросы ---
await checkSession('ids and non-object requests', [
  { jsonrpc: '2.0', id: 'string-id', method: 'ping' },
  { jsonrpc: '2.0', id: null, method: 'tools/list' },
  { jsonrpc: '2.0', method: 'ping' }, // без id
  { id: 1, method: 42 }, // нестроковый method
  { jsonrpc: '2.0', id: 2.5, method: 'initialize', params: 42 }, // params не объект
]);

// --- сессия 5: oversized-кадр последним ---
{
  const big = 'x'.repeat(9 * 1024 * 1024);
  const body = Buffer.from(JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'ping', params: big }));
  const oversized = Buffer.concat([
    Buffer.from(`Content-Length: ${body.length}\r\n\r\n`),
    body,
  ]);
  await checkSession('oversized frame', [
    { jsonrpc: '2.0', id: 1, method: 'ping' },
    oversized,
  ]);
}

rmSync(root, { recursive: true, force: true });

console.log(`parity sessions compared: ${compared}`);
if (failures.length) {
  console.log(`FAILURES: ${failures.length}`);
  for (const f of failures.slice(0, 10)) {
    console.log('---', f.label);
    console.log('  js  :', f.js);
    console.log('  rust:', f.rust);
  }
  process.exit(1);
}
console.log('PARITY OK');
