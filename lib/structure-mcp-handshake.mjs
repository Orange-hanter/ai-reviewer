/**
 * Handshake structure-MCP без structure-context / Rust (RFC 10).
 */
import { STRUCTURE_CONTEXT_TOOL } from './structure-mcp-tools.mjs';

export function handleStructureMcpHandshake(request) {
  const id = request?.id ?? null;
  if (request?.method === 'initialize') {
    return {
      jsonrpc: '2.0',
      id,
      result: {
        protocolVersion: request.params?.protocolVersion || '2025-06-18',
        capabilities: { tools: {} },
        serverInfo: { name: 'ai-reviewer-structure', version: '1.0.0' },
      },
    };
  }
  if (request?.method === 'notifications/initialized') return null;
  if (request?.method === 'ping') return { jsonrpc: '2.0', id, result: {} };
  if (request?.method === 'tools/list') {
    return { jsonrpc: '2.0', id, result: { tools: [STRUCTURE_CONTEXT_TOOL] } };
  }
  if (request?.method === 'resources/list') {
    return { jsonrpc: '2.0', id, result: { resources: [] } };
  }
  if (request?.method === 'prompts/list') {
    return { jsonrpc: '2.0', id, result: { prompts: [] } };
  }
  if (request?.method === 'logging/setLevel') {
    return { jsonrpc: '2.0', id, result: {} };
  }
  return undefined;
}
