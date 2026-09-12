/**
 * Handshake ревью-MCP без pipeline.mjs (RFC 10).
 * tools/call уходит в lib/review-mcp.mjs.
 */
import { listedTools } from './review-mcp-tools.mjs';

export function handleReviewMcpHandshake(request, extra = {}) {
  const id = request?.id ?? null;
  if (request?.method === 'initialize') {
    return {
      jsonrpc: '2.0',
      id,
      result: {
        protocolVersion: request.params?.protocolVersion || '2025-06-18',
        capabilities: { tools: {}, logging: {} },
        serverInfo: { name: 'ai-reviewer', version: '1.0.0' },
      },
    };
  }
  if (request?.method === 'notifications/initialized') return null;
  if (request?.method === 'ping') return { jsonrpc: '2.0', id, result: {} };
  if (request?.method === 'tools/list') {
    return { jsonrpc: '2.0', id, result: { tools: listedTools(extra) } };
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
