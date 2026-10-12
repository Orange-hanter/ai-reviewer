/**
 * Контракт Cursor Plugin и холодный старт MCP (RFC 09/10).
 * Без запуска Cursor и без LLM.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { chmod, cp, mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('..', import.meta.url)).replace(/\/$/, '');
const PLUGIN = join(ROOT, 'cursor-plugin');

async function readJson(rel) {
  return JSON.parse(await readFile(join(PLUGIN, rel), 'utf8'));
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
  return messages;
}

function frame(obj) {
  const body = Buffer.from(JSON.stringify(obj));
  return Buffer.concat([Buffer.from(`Content-Length: ${body.length}\r\n\r\n`), body]);
}

function spawnChild(command, args, env, { cwd = ROOT, writeHandshake = false, timeoutMs = 8000 } = {}) {
  return new Promise((resolve, reject) => {
    const started = Date.now();
    const child = spawn(command, args, {
      env: { ...process.env, ...env },
      cwd,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    const stdout = [];
    const stderr = [];
    const watchdog = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error(`timeout ${timeoutMs}ms. stderr: ${Buffer.concat(stderr).toString()}`));
    }, timeoutMs);
    child.stdout.on('data', (chunk) => stdout.push(chunk));
    child.stderr.on('data', (chunk) => stderr.push(chunk));
    if (writeHandshake) {
      child.stdin.write(frame({
        jsonrpc: '2.0',
        id: 1,
        method: 'initialize',
        params: { protocolVersion: '2025-03-26', capabilities: {}, clientInfo: { name: 'test', version: '0' } },
      }));
      child.stdin.write(frame({ jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} }));
    }
    child.stdin.end();
    child.on('error', (error) => {
      clearTimeout(watchdog);
      reject(error);
    });
    child.on('close', (code) => {
      clearTimeout(watchdog);
      resolve({
        code,
        ms: Date.now() - started,
        messages: parseMcpFrames(Buffer.concat(stdout)),
        stderr: Buffer.concat(stderr).toString(),
      });
    });
  });
}

function spawnHandshake(command, args, env, cwd = ROOT) {
  return spawnChild(command, args, env, { cwd, writeHandshake: true, timeoutMs: 8000 });
}

function spawnFailFast(command, args, env, cwd) {
  return spawnChild(command, args, env, { cwd, writeHandshake: false, timeoutMs: 4000 });
}

async function isolatedPlugin({ engineRoot } = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'mcp-plugin-'));
  await mkdir(join(dir, 'bin'));
  await cp(join(PLUGIN, 'bin/mcp-env.sh'), join(dir, 'bin/mcp-env.sh'));
  for (const launcher of ['review-mcp.sh', 'structure-mcp.sh']) {
    await cp(join(PLUGIN, 'bin', launcher), join(dir, 'bin', launcher));
    await chmod(join(dir, 'bin', launcher), 0o755);
  }
  if (engineRoot) await writeFile(join(dir, '.engine-root'), `${engineRoot}\n`);
  return dir;
}

const PATH_NO_NODE = '/usr/bin:/bin';
// Тесты node-пути фиксируют рантайм: при собранном rust/target/…/pr-review режим auto
// выбрал бы бинарь, и они проверяли бы не тот путь. Бинарный путь — тесты ниже (RFC 16, T31).
const NODE_RUNTIME = 'node';
const HOME = process.env.HOME;

test('plugin.json: JSON Schema variables без required', async () => {
  const plugin = await readJson('.cursor-plugin/plugin.json');
  assert.equal(plugin.name, 'ai-reviewer');
  assert.equal(typeof plugin.version, 'string');
  assert.equal(plugin.variables.type, 'object');
  const { properties, required } = plugin.variables;
  assert.ok(properties.AI_REVIEWER_ROOT);
  assert.ok(properties.REVIEW_MCP_ROOT);
  assert.ok(properties.REVIEW_PROFILE);
  assert.ok(properties.OLLAMA_API_KEY);
  assert.equal(properties.OLLAMA_API_KEY.default, undefined);
  assert.deepEqual(required, []);
});

test('mcp.json: лаунчер через HOME, type stdio, ключ только как ${OLLAMA_API_KEY}', async () => {
  const mcp = await readJson('mcp.json');
  const names = Object.keys(mcp.mcpServers);
  assert.deepEqual(names, ['ai-reviewer', 'ai-reviewer-structure']);
  const review = mcp.mcpServers['ai-reviewer'];
  const structure = mcp.mcpServers['ai-reviewer-structure'];
  assert.equal(review.type, 'stdio');
  assert.equal(structure.type, 'stdio');
  assert.equal(review.command, '/bin/sh');
  assert.equal(structure.command, '/bin/sh');
  assert.equal(review.args[0], '-c');
  assert.match(review.args[1], /\$HOME\/\.cursor\/plugins\/local\/ai-reviewer\/bin\/review-mcp\.sh/);
  assert.equal(structure.args[0], '-c');
  assert.match(structure.args[1], /\$HOME\/\.cursor\/plugins\/local\/ai-reviewer\/bin\/structure-mcp\.sh/);
  assert.equal(review.args.some((a) => a.includes('./bin')), false);
  assert.equal(review.env.AI_REVIEWER_ROOT, '${AI_REVIEWER_ROOT}');
  assert.equal(review.env.REVIEW_MCP_ROOT, '${REVIEW_MCP_ROOT}');
  assert.equal(review.env.OLLAMA_API_KEY, '${OLLAMA_API_KEY}');
  assert.equal(structure.env.STRUCTURE_MCP_ROOT, '${REVIEW_MCP_ROOT}');
  const dumped = JSON.stringify(mcp);
  assert.equal(dumped.includes('GITHUB_TOKEN'), false);
  assert.match(dumped, /\$\{OLLAMA_API_KEY\}/);
  assert.equal(/sk-[a-zA-Z0-9]{10,}/.test(dumped), false);
  assert.equal(dumped.includes('${AI_REVIEWER_ROOT}/cursor-plugin'), false);
});

test('W-engine: .cursor/mcp.json канонические имена без -ws', async () => {
  const mcp = JSON.parse(await readFile(join(ROOT, '.cursor/mcp.json'), 'utf8'));
  assert.deepEqual(Object.keys(mcp.mcpServers), ['ai-reviewer', 'ai-reviewer-structure']);
  const review = mcp.mcpServers['ai-reviewer'];
  assert.equal(review.type, 'stdio');
  assert.equal(review.command, '/bin/sh');
  assert.match(review.args[0], /\$\{workspaceFolder\}\/cursor-plugin\/bin\/review-mcp\.sh$/);
  assert.equal(review.env.AI_REVIEWER_ROOT, '${workspaceFolder}');
  assert.equal(review.env.REVIEW_MCP_ROOT, '${workspaceFolder}');
});

test('W-consumer: examples/cursor/mcp.json — REVIEW_MCP_ROOT=workspace, engine из env', async () => {
  const mcp = JSON.parse(await readFile(join(ROOT, 'examples/cursor/mcp.json'), 'utf8'));
  assert.deepEqual(Object.keys(mcp.mcpServers), ['ai-reviewer', 'ai-reviewer-structure']);
  const review = mcp.mcpServers['ai-reviewer'];
  assert.equal(review.type, 'stdio');
  assert.match(review.args[0], /\$\{env:AI_REVIEWER_ROOT\}\/cursor-plugin\/bin\/review-mcp\.sh$/);
  assert.equal(review.env.REVIEW_MCP_ROOT, '${workspaceFolder}');
  assert.equal(review.env.AI_REVIEWER_ROOT, '${env:AI_REVIEWER_ROOT}');
});

test('skill и rule запрещают mcp_auth / Authenticate', async () => {
  const skill = await readFile(join(PLUGIN, 'skills/review-loop/SKILL.md'), 'utf8');
  const rule = await readFile(join(PLUGIN, 'rules/preview-not-gate.mdc'), 'utf8');
  assert.match(skill, /mcp_auth/);
  assert.match(skill, /Не вызывай `mcp_auth`/);
  assert.match(skill, /wait: false/);
  assert.match(skill, /jobId/);
  assert.match(rule, /mcp_auth/);
  assert.match(rule, /Authenticate/);
});

test('bin/pr-review-mcp.mjs не импортирует pipeline на верхнем уровне', async () => {
  const src = await readFile(join(ROOT, 'bin/pr-review-mcp.mjs'), 'utf8');
  assert.equal(/(?:from|import)\s*\(?\s*['"][^'"]*pipeline\.mjs['"]/.test(src), false);
  assert.equal(/from\s+['"][^'"]*review-mcp\.mjs['"]/.test(src), false);
  assert.match(src, /import\('\.\.\/lib\/review-mcp\.mjs'\)/);
});

test('P-configure: initialize + tools/list через абсолютный node, без node в PATH', async () => {
  const result = await spawnHandshake(join(PLUGIN, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: ROOT,
    REVIEW_MCP_ROOT: ROOT,
    HOME,
    OLLAMA_API_KEY: '',
  });
  assert.equal(result.code, 0, result.stderr);
  assert.ok(result.ms < 5000, `handshake ${result.ms}ms`);
  assert.equal(result.messages[0].result.serverInfo.name, 'ai-reviewer');
  const names = result.messages[1].result.tools.map((tool) => tool.name);
  assert.ok(names.includes('review_route'));
  assert.ok(names.includes('review_run'));
});

test('Cheap-without-key: initialize без OLLAMA_API_KEY', async () => {
  const result = await spawnHandshake(process.execPath, [join(ROOT, 'bin/pr-review-mcp.mjs')], {
    REVIEW_MCP_ROOT: ROOT,
    OLLAMA_API_KEY: '',
  });
  assert.equal(result.code, 0, result.stderr);
  assert.equal(result.messages[0].result.serverInfo.name, 'ai-reviewer');
  const names = result.messages[1].result.tools.map((tool) => tool.name);
  assert.ok(names.includes('review_route'));
});

test('pr-review-mcp: плейсхолдер REVIEW_MCP_ROOT не валит initialize', async () => {
  const result = await spawnHandshake(process.execPath, [join(ROOT, 'bin/pr-review-mcp.mjs')], {
    REVIEW_MCP_ROOT: '${REVIEW_MCP_ROOT}',
  });
  assert.equal(result.code, 0, result.stderr);
  assert.equal(result.messages[0].result.serverInfo.name, 'ai-reviewer');
});

test('P-engine-install: .engine-root, пустой AI_REVIEWER_ROOT, PATH без node', async () => {
  const dir = await isolatedPlugin({ engineRoot: ROOT });
  const result = await spawnHandshake(join(dir, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: '',
    REVIEW_MCP_ROOT: '',
    HOME,
  }, dir);
  assert.equal(result.code, 0, result.stderr);
  assert.ok(result.ms < 5000, `handshake ${result.ms}ms`);
  assert.equal(result.messages[0].result.serverInfo.name, 'ai-reviewer');
  const names = result.messages[1].result.tools.map((tool) => tool.name);
  assert.ok(names.includes('review_route'));
});

test('плейсхолдеры в env резолвятся через .engine-root', async () => {
  const dir = await isolatedPlugin({ engineRoot: ROOT });
  const result = await spawnHandshake(join(dir, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: '${AI_REVIEWER_ROOT}',
    REVIEW_MCP_ROOT: '${REVIEW_MCP_ROOT}',
    HOME,
  }, dir);
  assert.equal(result.code, 0, result.stderr);
  assert.ok(result.ms < 5000, `handshake ${result.ms}ms`);
  assert.equal(result.messages[0].result.serverInfo.name, 'ai-reviewer');
});

test('клон: лаунчер без AI_REVIEWER_ROOT находит engine через cursor-plugin/../..', async () => {
  const result = await spawnHandshake(join(PLUGIN, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: '',
    REVIEW_MCP_ROOT: '',
    HOME,
  }, ROOT);
  assert.equal(result.code, 0, result.stderr);
  assert.equal(result.messages[0].result.serverInfo.name, 'ai-reviewer');
});

test('fail-fast: нет engine и нет .engine-root → exit ≠ 0', async () => {
  const dir = await isolatedPlugin();
  const result = await spawnFailFast(join(dir, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: '',
    REVIEW_MCP_ROOT: '',
    HOME,
  }, dir);
  assert.notEqual(result.code, 0);
  assert.ok(result.ms < 2000, `fail-fast ${result.ms}ms`);
  assert.match(result.stderr, /AI_REVIEWER_ROOT/);
  assert.match(result.stderr, /install-local/);
});

test('fail-fast: битый REVIEW_MCP_ROOT → exit ≠ 0', async () => {
  const dir = await isolatedPlugin({ engineRoot: ROOT });
  const result = await spawnFailFast(join(dir, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: '',
    REVIEW_MCP_ROOT: '/no/such/ai-reviewer-consumer',
    HOME,
  }, dir);
  assert.notEqual(result.code, 0);
  assert.ok(result.ms < 2000, `fail-fast ${result.ms}ms`);
  assert.match(result.stderr, /REVIEW_MCP_ROOT/);
});

test('fail-fast: битый REVIEW_MCP_ROOT в bin → exit 1', async () => {
  const result = await spawnFailFast(process.execPath, [join(ROOT, 'bin/pr-review-mcp.mjs')], {
    REVIEW_MCP_ROOT: '/no/such/ai-reviewer-consumer',
  }, ROOT);
  assert.equal(result.code, 1);
  assert.match(result.stderr, /REVIEW_MCP_ROOT не существует/);
});

test('fail-fast: AI_REVIEWER_NODE не исполняемый', async () => {
  const result = await spawnFailFast(join(PLUGIN, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_NODE: join(tmpdir(), 'no-such-node'),
    AI_REVIEWER_ROOT: ROOT,
    REVIEW_MCP_ROOT: ROOT,
    HOME,
  }, ROOT);
  assert.notEqual(result.code, 0);
  assert.match(result.stderr, /AI_REVIEWER_NODE/);
});

test('fail-fast: нет node в PATH и fallbacks', async (t) => {
  const extras = ['/opt/homebrew/bin/node', '/usr/local/bin/node'];
  if (extras.some((p) => existsSync(p))) {
    t.skip('на машине есть Homebrew/usr local node — PATH-без-node покрыт P-configure');
    return;
  }
  const home = await mkdtemp(join(tmpdir(), 'mcp-home-'));
  const dir = await isolatedPlugin({ engineRoot: ROOT });
  const result = await spawnFailFast(join(dir, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: '',
    REVIEW_MCP_ROOT: '',
    HOME: home,
  }, dir);
  assert.notEqual(result.code, 0);
  assert.match(result.stderr, /node ≥ 22 не найден/);
});

test('install-local.sh пишет .engine-root и абсолютный лаунчер, не путь engine', async () => {
  const dest = await mkdtemp(join(tmpdir(), 'plugin-dest-'));
  const result = spawnSync('/bin/sh', [join(PLUGIN, 'bin/install-local.sh'), dest], {
    encoding: 'utf8',
    cwd: ROOT,
  });
  assert.equal(result.status, 0, result.stderr);
  const engineRoot = (await readFile(join(dest, '.engine-root'), 'utf8')).trim();
  assert.equal(engineRoot, ROOT);
  const mcp = JSON.parse(await readFile(join(dest, 'mcp.json'), 'utf8'));
  assert.equal(mcp.mcpServers['ai-reviewer'].args[0], `${dest}/bin/review-mcp.sh`);
  assert.equal(mcp.mcpServers['ai-reviewer-structure'].args[0], `${dest}/bin/structure-mcp.sh`);
  assert.equal(mcp.mcpServers['ai-reviewer'].env.AI_REVIEWER_ROOT, '${AI_REVIEWER_ROOT}');
  assert.equal(mcp.mcpServers['ai-reviewer'].env.REVIEW_MCP_ROOT, '${REVIEW_MCP_ROOT}');
  const dumped = JSON.stringify(mcp);
  assert.equal(dumped.includes(ROOT), false);
});

test('абсолютный лаунчер: handshake из чужого cwd (не корень плагина)', async () => {
  const dest = await mkdtemp(join(tmpdir(), 'plugin-dest-'));
  const alien = await mkdtemp(join(tmpdir(), 'alien-cwd-'));
  const installed = spawnSync('/bin/sh', [join(PLUGIN, 'bin/install-local.sh'), dest], {
    encoding: 'utf8',
    cwd: ROOT,
  });
  assert.equal(installed.status, 0, installed.stderr);
  const launcher = `${dest}/bin/review-mcp.sh`;
  const result = await spawnHandshake('/bin/sh', [launcher], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: '',
    REVIEW_MCP_ROOT: '',
    HOME,
  }, alien);
  assert.equal(result.code, 0, result.stderr);
  assert.ok(result.ms < 5000, `handshake ${result.ms}ms`);
  const names = result.messages[1].result.tools.map((tool) => tool.name);
  assert.ok(names.includes('review_route'));
});

test('относительный ./bin из чужого cwd не находит лаунчер', async () => {
  const dest = await mkdtemp(join(tmpdir(), 'plugin-dest-'));
  const alien = await mkdtemp(join(tmpdir(), 'alien-cwd-'));
  const installed = spawnSync('/bin/sh', [join(PLUGIN, 'bin/install-local.sh'), dest], {
    encoding: 'utf8',
    cwd: ROOT,
  });
  assert.equal(installed.status, 0, installed.stderr);
  const result = await spawnFailFast('/bin/sh', ['./bin/review-mcp.sh'], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    HOME,
  }, alien);
  assert.notEqual(result.code, 0);
});

test('git-шаблон $HOME: handshake из чужого cwd после install-local в fake HOME', async (t) => {
  const home = await mkdtemp(join(tmpdir(), 'mcp-home-'));
  const dest = join(home, '.cursor/plugins/local/ai-reviewer');
  try {
    await mkdir(dest, { recursive: true });
  } catch (error) {
    t.skip(`нельзя создать $HOME/.cursor (${error.code || error.message})`);
    return;
  }
  const alien = await mkdtemp(join(tmpdir(), 'alien-cwd-'));
  const installed = spawnSync('/bin/sh', [join(PLUGIN, 'bin/install-local.sh'), dest], {
    encoding: 'utf8',
    cwd: ROOT,
  });
  assert.equal(installed.status, 0, installed.stderr);
  const mcp = JSON.parse(await readFile(join(PLUGIN, 'mcp.json'), 'utf8'));
  const result = await spawnHandshake('/bin/sh', mcp.mcpServers['ai-reviewer'].args, {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: '',
    REVIEW_MCP_ROOT: '',
    HOME: home,
  }, alien);
  assert.equal(result.code, 0, result.stderr);
  assert.ok(result.ms < 5000, `handshake ${result.ms}ms`);
  const names = result.messages[1].result.tools.map((tool) => tool.name);
  assert.ok(names.includes('review_route'));
});

test('structure handshake: tools/list без Rust/пайплайна', async () => {
  const result = await spawnHandshake(join(PLUGIN, 'bin/structure-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_MCP_RUNTIME: NODE_RUNTIME,
    AI_REVIEWER_ROOT: ROOT,
    REVIEW_MCP_ROOT: ROOT,
    HOME,
  });
  assert.equal(result.code, 0, result.stderr);
  assert.ok(result.ms < 5000, `handshake ${result.ms}ms`);
  assert.equal(result.messages[0].result.serverInfo.name, 'ai-reviewer-structure');
  const names = result.messages[1].result.tools.map((tool) => tool.name);
  assert.ok(names.includes('structure_context'));
});

// ---------------------------------------------------------------------------------------------
// Бинарный путь (RFC 16, T31): лаунчер зовёт подкоманду `pr-review`, node — откат до T33.
// ---------------------------------------------------------------------------------------------

/** Фейковый бинарь: печатает в stderr подкоманду и корни, stdout не трогает (там JSON-RPC). */
async function fakeBinary(path, tag) {
  await mkdir(join(path, '..'), { recursive: true });
  await writeFile(path, `#!/bin/sh\necho "FAKE-${tag} $* root=$REVIEW_MCP_ROOT structure=$STRUCTURE_MCP_ROOT" >&2\n`);
  await chmod(path, 0o755);
  return path;
}

/** Клон движка без JS (каким он станет после T33): rust/Cargo.toml + cursor-plugin/mcp.json. */
async function rustOnlyEngine({ release = false, debug = false } = {}) {
  const root = await mkdtemp(join(tmpdir(), 'mcp-engine-'));
  await mkdir(join(root, 'rust'), { recursive: true });
  await mkdir(join(root, 'cursor-plugin'), { recursive: true });
  await writeFile(join(root, 'rust/Cargo.toml'), '[package]\nname = "fake"\n');
  await writeFile(join(root, 'cursor-plugin/mcp.json'), '{}\n');
  if (release) await fakeBinary(join(root, 'rust/target/release/pr-review'), 'release');
  if (debug) await fakeBinary(join(root, 'rust/target/debug/pr-review'), 'debug');
  return root;
}

test('bin: auto берёт release-сборку клона раньше debug и зовёт подкоманду', async () => {
  const engine = await rustOnlyEngine({ release: true, debug: true });
  for (const [launcher, sub] of [['review-mcp.sh', 'review-mcp'], ['structure-mcp.sh', 'structure-mcp']]) {
    const result = await spawnFailFast(join(PLUGIN, 'bin', launcher), [], {
      PATH: PATH_NO_NODE,
      AI_REVIEWER_ROOT: engine,
      REVIEW_MCP_ROOT: '',
      AI_REVIEWER_BIN: '',
      AI_REVIEWER_MCP_RUNTIME: '',
      HOME,
    }, engine);
    assert.equal(result.code, 0, result.stderr);
    assert.match(result.stderr, new RegExp(`FAKE-release ${sub} root=${engine} structure=${engine}`));
  }
});

test('bin: только debug-сборка — берётся она; клон без JS резолвится через .engine-root', async () => {
  const engine = await rustOnlyEngine({ debug: true });
  const dir = await isolatedPlugin({ engineRoot: engine });
  const result = await spawnFailFast(join(dir, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_ROOT: '${AI_REVIEWER_ROOT}',
    REVIEW_MCP_ROOT: '${REVIEW_MCP_ROOT}',
    AI_REVIEWER_BIN: '${AI_REVIEWER_BIN}',
    HOME,
  }, dir);
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stderr, /FAKE-debug review-mcp/);
});

test('bin: AI_REVIEWER_BIN без клона движка — standalone, корень ревью = cwd', async () => {
  const dir = await isolatedPlugin();
  const bin = await fakeBinary(join(await mkdtemp(join(tmpdir(), 'mcp-bin-')), 'pr-review'), 'explicit');
  const consumer = await mkdtemp(join(tmpdir(), 'mcp-consumer-'));
  const result = await spawnFailFast(join(dir, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_ROOT: '',
    REVIEW_MCP_ROOT: '',
    AI_REVIEWER_BIN: bin,
    HOME,
  }, consumer);
  assert.equal(result.code, 0, result.stderr);
  // macOS: tmpdir за симлинком /var → /private/var, pwd отдаёт физический путь.
  assert.match(result.stderr, /FAKE-explicit review-mcp root=\S*mcp-consumer-/);
});

test('bin: неисполняемый AI_REVIEWER_BIN → fail-fast, без тихого отката на node', async () => {
  const result = await spawnFailFast(join(PLUGIN, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_ROOT: ROOT,
    REVIEW_MCP_ROOT: ROOT,
    AI_REVIEWER_BIN: join(tmpdir(), 'no-such-pr-review'),
    HOME,
  }, ROOT);
  assert.notEqual(result.code, 0);
  assert.ok(result.ms < 2000, `fail-fast ${result.ms}ms`);
  assert.match(result.stderr, /AI_REVIEWER_BIN не исполняемый/);
});

test('bin: AI_REVIEWER_MCP_RUNTIME=bin без бинаря → fail-fast; мусор в рантайме → fail-fast', async () => {
  const engine = await rustOnlyEngine();
  const strict = await spawnFailFast(join(PLUGIN, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_ROOT: engine,
    REVIEW_MCP_ROOT: engine,
    AI_REVIEWER_MCP_RUNTIME: 'bin',
    HOME,
  }, engine);
  assert.notEqual(strict.code, 0);
  assert.match(strict.stderr, /бинарь pr-review не найден/);
  // auto без бинаря и без JS — понятная ошибка, а не exec несуществующего скрипта.
  const none = await spawnFailFast(join(PLUGIN, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_ROOT: engine,
    REVIEW_MCP_ROOT: engine,
    HOME,
  }, engine);
  assert.notEqual(none.code, 0);
  assert.match(none.stderr, /нет бинаря pr-review/);
  const bogus = await spawnFailFast(join(PLUGIN, 'bin/review-mcp.sh'), [], {
    PATH: PATH_NO_NODE,
    AI_REVIEWER_ROOT: ROOT,
    AI_REVIEWER_MCP_RUNTIME: 'python',
    HOME,
  }, ROOT);
  assert.notEqual(bogus.code, 0);
  assert.match(bogus.stderr, /AI_REVIEWER_MCP_RUNTIME/);
});

test('bin: настоящий pr-review — handshake обоих серверов без node', async (t) => {
  const bin = join(ROOT, 'rust/target/debug/pr-review');
  if (!existsSync(bin)) {
    t.skip('rust/target/debug/pr-review не собран — бинарный handshake покрыт cargo test');
    return;
  }
  for (const [launcher, server, tool] of [
    ['review-mcp.sh', 'ai-reviewer', 'review_route'],
    ['structure-mcp.sh', 'ai-reviewer-structure', 'structure_context'],
  ]) {
    const result = await spawnHandshake(join(PLUGIN, 'bin', launcher), [], {
      PATH: PATH_NO_NODE,
      AI_REVIEWER_ROOT: ROOT,
      REVIEW_MCP_ROOT: ROOT,
      AI_REVIEWER_BIN: bin,
      AI_REVIEWER_MCP_RUNTIME: 'bin',
      HOME,
      OLLAMA_API_KEY: '',
    });
    assert.equal(result.code, 0, result.stderr);
    assert.equal(result.messages[0].result.serverInfo.name, server);
    assert.ok(result.messages[1].result.tools.map((entry) => entry.name).includes(tool));
  }
});
