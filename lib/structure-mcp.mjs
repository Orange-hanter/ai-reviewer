/**
 * Фаза 2, шаг 5: lib/structure-mcp.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/structure_mcp (MCP-style JSON-RPC-ish диспетчер тулза
 * structure_context + построение structure context).
 *
 * Отступление от одношотового паттерна: MCP-сервер — долгоживущий процесс, но
 * доменная логика тут разбивается на две одношотовые операции бинаря
 * ({"op", ...args} на stdin → JSON на stdout):
 *   handleRequest         — «чистые» методы (initialize / ping / tools/list /
 *                           method-not-found / unknown-tool), доменный ответ
 *                           или null (notifications/initialized);
 *   buildStructureContext — тексты уже прочитанных пейлоадов + сырые args →
 *                           structure context (пин-проверка, parseStructure-
 *                           Payload обоих текстов, проверка kind, клэмпинг
 *                           аргументов из собственного inputSchema, BFS,
 *                           усечение бюджетом, prune висячих ссылок).
 * Серверный stdio-цикл с Content-Length-фреймингом живёт в lib/mcp-stdio.mjs
 * (общий с pr-review-mcp); bin/pr-review-structure-mcp.mjs — тонкая обёртка.
 * Полный порт транспорта — с async-стеком, Фаза 3.
 *
 * fs-I/O и часы, как у прочих модулей Фазы 2, живут в JS: чтение facts/
 * assessment (resolveContainedPath с STRUCTURE_MCP_ROOT + readFile) и обрамление
 * ответа tools/call (content[0].text = JSON.stringify(context, null, 2))
 * выполняет обёртка, поэтому сериализация ответных фреймов идёт одним и тем же
 * V8 JSON.stringify в обеих реализациях.
 *
 * Резолвинг бинаря (по аналогии с lib/structure-protocol.mjs):
 *   AI_REVIEWER_STRUCTURE_MCP_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/structure_mcp;
 *   AI_REVIEWER_STRUCTURE_MCP_FORCE_JS=1 — принудительно чистый JS.
 *
 * Если бинарь не существует (чистый клон до первого `cargo build`), обёртка
 * деградирует на чистый JS fallback ниже — эталонную копию исходной реализации.
 * Если бинарь ЕСТЬ, но упал (non-zero exit) — ошибка пробрывается с текстом
 * доменной ошибки Rust ({"error"} со stderr), fallback не используется:
 * молчаливая подмена реализации при битом Rust-коде скрывала бы регрессии.
 *
 * Нюансы паритета (проверены прогоном rust/parity/structure-mcp.parity.mjs):
 *   - пин-проверка ('headSha or snapshotFingerprint is required') продублирована
 *     в обёртке ДО чтения файлов: в JS-оригинале она тоже стояла раньше
 *     readPayload, и порядок ошибок не должен зависеть от наличия бинаря;
 *   - resolveContainedPath/ENOENT остаются JS-ошибками чтения (fs в JS),
 *     'expected structure_facts|structure_assessment at <path>' бросает бинарь
 *     с raw targetPath из аргументов — тексты совпадают посимвольно;
 *   - обе стороны парсят пейлоады одним parse_structure_payload (бинарь
 *     structure_protocol), поэтому тексты парс-ошибок совпадают при наличии
 *     бинаря на обеих сторонах; facts читается раньше assessment (в JS
 *     Promise.all порядок двух одновременных ошибок недетерминирован);
 *   - числа контекста проходят f64 (ECMA Number::toString) в Rust, поэтому
 *     экзотические лексемы ("1.0") нормализуются как после JSON.parse в V8.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { resolveContainedPath } from './util.mjs';
import { buildStructureContext } from './structure-context.mjs';
import { parseStructurePayload } from './structure-protocol.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_MCP_BIN = resolve(__dir, '../rust/target/debug/structure_mcp');

function resolveMcpBin() {
  if (process.env.AI_REVIEWER_STRUCTURE_MCP_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_STRUCTURE_MCP_BIN || '').trim() || DEFAULT_MCP_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). Ошибка бинаря пробрасывается. */
function runRust(op, args) {
  const bin = resolveMcpBin();
  if (!bin) return null;
  try {
    const raw = execFileSync(bin, [], {
      input: JSON.stringify({ op, ...args }),
      encoding: 'utf8',
      maxBuffer: 128 * 1024 * 1024,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    return JSON.parse(raw);
  } catch (error) {
    // доменные ошибки Rust: exit 2 + {"error": "..."} на stderr
    const m = String(error?.stderr || '').match(/"error":\s*"((?:[^"\\]|\\.)*)"/);
    if (m) throw new Error(JSON.parse(`"${m[1]}"`));
    throw error;
  }
}

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

/**
 * Чтение ограничено корнем: STRUCTURE_MCP_ROOT или cwd; realpath внутри.
 * resolveContainedPath не даёт обойти проверку symlink'ом, указывающим наружу.
 * Возвращает ТЕКСТ файла — парсинг и проверка kind уезжают в Rust-op
 * (parse_structure_payload + 'expected <kind> at <targetPath>').
 */
async function readPayloadText(targetPath) {
  const absolute = await resolveContainedPath(targetPath, {
    rootDir: process.env.STRUCTURE_MCP_ROOT || process.cwd(),
    mustExist: true,
  });
  return readFile(absolute, 'utf8');
}

export async function callStructureContext(args) {
  // пин-проверка до чтения файлов — как в исходном callStructureContext,
  // чтобы порядок ошибок не зависел от того, какая реализация ответит
  if (!args?.headSha && !args?.snapshotFingerprint) {
    throw new Error('headSha or snapshotFingerprint is required');
  }
  const [factsText, assessmentText] = await Promise.all([
    readPayloadText(args.factsPath),
    readPayloadText(args.assessmentPath),
  ]);
  const r = runRust('buildStructureContext', {
    factsText,
    assessmentText,
    factsPath: args.factsPath ?? null,
    assessmentPath: args.assessmentPath ?? null,
    args: args ?? null,
  });
  if (r !== null) return r;
  return callStructureContextFromTextsJs(args, factsText, assessmentText);
}

export async function handleStructureMcpRequest(request) {
  const id = request?.id ?? null;
  try {
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
    const r = runRust('handleRequest', { request: request ?? null });
    if (r !== null) return r;
    return handleStructureMcpRequestJs(request);
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

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/structure-mcp.mjs
// до Фазы 2, шага 5). Используется только когда Rust-бинарь отсутствует; при
// битом бинаре НЕ подменяет его. Обрамление ответа tools/call (text из
// JSON.stringify(context, null, 2)) общее с Rust-путём — сериализатор один.
// ---------------------------------------------------------------------------

const SCHEMA_PROPS = STRUCTURE_CONTEXT_TOOL.inputSchema.properties;

/** Границы берём из объявленной схемы, чтобы код и контракт не разъезжались. */
function boundedListJs(value, schema) {
  return Array.isArray(value) ? value.slice(0, schema.maxItems) : [];
}

function boundedIntJs(value, schema, fallback) {
  const number = Number(value);
  if (!Number.isInteger(number)) return fallback;
  return Math.min(Math.max(number, schema.minimum), schema.maximum);
}

async function callStructureContextJs(args) {
  if (!args?.headSha && !args?.snapshotFingerprint) {
    throw new Error('headSha or snapshotFingerprint is required');
  }
  const [factsText, assessmentText] = await Promise.all([
    readPayloadText(args.factsPath),
    readPayloadText(args.assessmentPath),
  ]);
  return callStructureContextFromTextsJs(args, factsText, assessmentText);
}

function callStructureContextFromTextsJs(args, factsText, assessmentText) {
  const facts = parsePayloadFromTextJs(factsText, 'structure_facts', args.factsPath);
  const assessment = parsePayloadFromTextJs(assessmentText, 'structure_assessment', args.assessmentPath);
  return buildStructureContext({
    facts,
    assessment,
    headSha: args.headSha,
    snapshotFingerprint: args.snapshotFingerprint,
    // inputSchema объявлен в tools/list, но клиент не обязан ему следовать: сервер не может
    // доверять чужой валидации. Глубина и размер ответа и так ограничены внутренними
    // потолками buildStructureContext, а вот длина списков ничем не была ограничена —
    // matchesScope перебирает scopeRefs на каждую сущность.
    scopeRefs: boundedListJs(args.scopeRefs, SCHEMA_PROPS.scopeRefs),
    taskRef: args.taskRef || null,
    contractRefs: boundedListJs(args.contractRefs, SCHEMA_PROPS.contractRefs),
    tokenBudget: boundedIntJs(args.tokenBudget, SCHEMA_PROPS.tokenBudget, 8000),
    dependencyDepth: boundedIntJs(args.dependencyDepth, SCHEMA_PROPS.dependencyDepth, 1),
  });
}

function parsePayloadFromTextJs(text, expectedKind, targetPath) {
  const payload = parseStructurePayload(text);
  if (payload.kind !== expectedKind) throw new Error(`expected ${expectedKind} at ${targetPath}`);
  return payload;
}

async function handleStructureMcpRequestJs(request) {
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
      const context = await callStructureContextJs(request.params.arguments || {});
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
