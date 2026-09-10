/**
 * Контракт Cursor Plugin: два MCP, нет секрета в файле, переменные из plugin.json.
 * Без запуска Cursor и без LLM.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('..', import.meta.url));
const PLUGIN = join(ROOT, 'cursor-plugin');

async function readJson(rel) {
  return JSON.parse(await readFile(join(PLUGIN, rel), 'utf8'));
}

test('plugin.json объявляет name, version и required variables', async () => {
  const plugin = await readJson('.cursor-plugin/plugin.json');
  assert.equal(plugin.name, 'ai-reviewer');
  assert.equal(typeof plugin.version, 'string');
  assert.ok(plugin.variables.AI_REVIEWER_ROOT);
  assert.ok(plugin.variables.REVIEW_MCP_ROOT);
  assert.ok(plugin.variables.REVIEW_PROFILE);
  assert.ok(plugin.variables.OLLAMA_API_KEY);
  assert.equal(plugin.variables.OLLAMA_API_KEY.default, undefined);
});

test('mcp.json: два сервера, ключ только как ${OLLAMA_API_KEY}, нет GITHUB_TOKEN', async () => {
  const mcp = await readJson('mcp.json');
  const names = Object.keys(mcp.mcpServers);
  assert.deepEqual(names, ['ai-reviewer', 'ai-reviewer-structure']);
  const review = mcp.mcpServers['ai-reviewer'];
  const structure = mcp.mcpServers['ai-reviewer-structure'];
  assert.equal(review.command, 'node');
  assert.match(review.args[0], /pr-review-mcp\.mjs$/);
  assert.match(structure.args[0], /pr-review-structure-mcp\.mjs$/);
  assert.equal(review.env.REVIEW_MCP_ROOT, '${REVIEW_MCP_ROOT}');
  assert.equal(review.env.OLLAMA_API_KEY, '${OLLAMA_API_KEY}');
  assert.equal(structure.env.STRUCTURE_MCP_ROOT, '${REVIEW_MCP_ROOT}');
  const dumped = JSON.stringify(mcp);
  assert.equal(dumped.includes('GITHUB_TOKEN'), false);
  assert.match(dumped, /\$\{OLLAMA_API_KEY\}/);
  assert.equal(/sk-[a-zA-Z0-9]{10,}/.test(dumped), false);
});
