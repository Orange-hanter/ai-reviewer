#!/usr/bin/env node
/**
 * stdio-вход structure-MCP. Handshake без graph/Rust (RFC 10);
 * домен — динамический import на tools/call.
 */
import { existsSync } from 'node:fs';
import { resolve } from 'node:path';
import { runMcpStdio } from '../lib/mcp-stdio.mjs';
import { handleStructureMcpHandshake } from '../lib/structure-mcp-handshake.mjs';

function die(message) {
  process.stderr.write(`ai-reviewer MCP: ${message}\n`);
  process.exit(1);
}

function usableRoot(value) {
  if (!value || typeof value !== 'string') return '';
  const trimmed = value.trim();
  if (!trimmed || /\$\{[A-Za-z0-9_:.-]+\}/.test(trimmed)) return '';
  return trimmed;
}

const root = usableRoot(process.env.STRUCTURE_MCP_ROOT) || usableRoot(process.env.REVIEW_MCP_ROOT);
if (root) {
  const abs = resolve(root);
  if (!existsSync(abs)) die(`STRUCTURE_MCP_ROOT не существует: ${abs}`);
  process.chdir(abs);
}

let domain;
async function handleRequest(request, extra) {
  const handshake = handleStructureMcpHandshake(request);
  if (handshake !== undefined) return handshake;
  if (!domain) domain = await import('../lib/structure-mcp.mjs');
  return domain.handleStructureMcpRequest(request, extra);
}

runMcpStdio({ handleRequest });
