#!/usr/bin/env node
/**
 * stdio-вход ревью-MCP. Handshake без пайплайна (RFC 10);
 * домен — динамический import на tools/call.
 */
import { existsSync } from 'node:fs';
import { resolve } from 'node:path';
import { runMcpStdio } from '../lib/mcp-stdio.mjs';
import { handleReviewMcpHandshake } from '../lib/review-mcp-handshake.mjs';

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

const root = usableRoot(process.env.REVIEW_MCP_ROOT);
if (root) {
  const abs = resolve(root);
  if (!existsSync(abs)) die(`REVIEW_MCP_ROOT не существует: ${abs}`);
  process.chdir(abs);
}

let domain;
async function handleRequest(request, extra) {
  const handshake = handleReviewMcpHandshake(request, extra);
  if (handshake !== undefined) return handshake;
  if (!domain) domain = await import('../lib/review-mcp.mjs');
  return domain.handleReviewMcpRequest(request, extra);
}

runMcpStdio({ handleRequest });
