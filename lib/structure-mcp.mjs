import { readFile } from 'node:fs/promises';
import { resolveContainedPath } from './util.mjs';
import { buildStructureContext } from './structure-context.mjs';
import { parseStructurePayload } from './structure-protocol.mjs';

export const STRUCTURE_CONTEXT_TOOL = Object.freeze({
  name: 'structure_context',
  description: 'Return a bounded structure/contracts/test-scenario context pinned to a facts snapshot.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['factsPath', 'assessmentPath'],
    anyOf: [
      { required: ['headSha'] },
      { required: ['snapshotFingerprint'] },
    ],
    properties: {
      factsPath: { type: 'string', minLength: 1 },
      assessmentPath: { type: 'string', minLength: 1 },
      headSha: { type: 'string', pattern: '^[0-9a-fA-F]{7,64}$' },
      snapshotFingerprint: { type: 'string', pattern: '^[0-9a-f]{64}$' },
      scopeRefs: { type: 'array', maxItems: 20, items: { type: 'string', maxLength: 1000 } },
      taskRef: { type: ['string', 'null'], maxLength: 500 },
      contractRefs: { type: 'array', maxItems: 20, items: { type: 'string', maxLength: 500 } },
      tokenBudget: { type: 'integer', minimum: 256, maximum: 32000 },
      dependencyDepth: { type: 'integer', minimum: 0, maximum: 3 },
    },
  },
});

// чтение ограничено корнем: STRUCTURE_MCP_ROOT или cwd; realpath внутри
// resolveContainedPath не даёт обойти проверку symlink'ом, указывающим наружу
async function readPayload(targetPath, expectedKind) {
  const absolute = await resolveContainedPath(targetPath, {
    rootDir: process.env.STRUCTURE_MCP_ROOT || process.cwd(),
    mustExist: true,
  });
  const payload = parseStructurePayload(await readFile(absolute, 'utf8'));
  if (payload.kind !== expectedKind) throw new Error(`expected ${expectedKind} at ${targetPath}`);
  return payload;
}

const SCHEMA_PROPS = STRUCTURE_CONTEXT_TOOL.inputSchema.properties;

/** Границы берём из объявленной схемы, чтобы код и контракт не разъезжались. */
function boundedList(value, schema) {
  return Array.isArray(value) ? value.slice(0, schema.maxItems) : [];
}

function boundedInt(value, schema, fallback) {
  const number = Number(value);
  if (!Number.isInteger(number)) return fallback;
  return Math.min(Math.max(number, schema.minimum), schema.maximum);
}

export async function callStructureContext(args) {
  if (!args?.headSha && !args?.snapshotFingerprint) {
    throw new Error('headSha or snapshotFingerprint is required');
  }
  const [facts, assessment] = await Promise.all([
    readPayload(args.factsPath, 'structure_facts'),
    readPayload(args.assessmentPath, 'structure_assessment'),
  ]);
  return buildStructureContext({
    facts,
    assessment,
    headSha: args.headSha,
    snapshotFingerprint: args.snapshotFingerprint,
    // inputSchema объявлен в tools/list, но клиент не обязан ему следовать: сервер не может
    // доверять чужой валидации. Глубина и размер ответа и так ограничены внутренними
    // потолками buildStructureContext, а вот длина списков ничем не была ограничена —
    // matchesScope перебирает scopeRefs на каждую сущность.
    scopeRefs: boundedList(args.scopeRefs, SCHEMA_PROPS.scopeRefs),
    taskRef: args.taskRef || null,
    contractRefs: boundedList(args.contractRefs, SCHEMA_PROPS.contractRefs),
    tokenBudget: boundedInt(args.tokenBudget, SCHEMA_PROPS.tokenBudget, 8000),
    dependencyDepth: boundedInt(args.dependencyDepth, SCHEMA_PROPS.dependencyDepth, 1),
  });
}

export async function handleStructureMcpRequest(request) {
  const id = request?.id ?? null;
  try {
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
    if (request?.method === 'tools/call') {
      if (request.params?.name !== STRUCTURE_CONTEXT_TOOL.name) throw new Error(`unknown tool: ${request.params?.name}`);
      const context = await callStructureContext(request.params.arguments || {});
      return {
        jsonrpc: '2.0',
        id,
        result: {
          content: [{ type: 'text', text: JSON.stringify(context, null, 2) }],
          structuredContent: context,
        },
      };
    }
    throw new Error(`method not found: ${request?.method}`);
  } catch (error) {
    return {
      jsonrpc: '2.0',
      id,
      error: {
        code: error.message.startsWith('method not found') ? -32601 : -32602,
        message: error.message,
      },
    };
  }
}
