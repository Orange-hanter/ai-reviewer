#!/usr/bin/env node
import { handleStructureMcpRequest } from '../lib/structure-mcp.mjs';

// Транспорт MCP stdio: сообщения обрамлены заголовком Content-Length,
// заголовки читаются без учёта регистра, неизвестные игнорируются,
// частичные данные буферизуются до прихода целого кадра.
const MAX_MESSAGE_BYTES = 8 * 1024 * 1024;
const MAX_HEADER_BYTES = 16 * 1024;
let buffer = Buffer.alloc(0);

// graceful close: не обрываем возможную запись кадра посередине
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    if (process.stdout.writableNeedDrain) process.stdout.once('drain', () => process.exit(0));
    else process.exit(0);
  });
}

function writeMessage(payload) {
  const body = Buffer.from(JSON.stringify(payload), 'utf8');
  process.stdout.write(`Content-Length: ${body.length}\r\n\r\n`);
  process.stdout.write(body);
}

function writeError(id, code, message) {
  writeMessage({ jsonrpc: '2.0', id, error: { code, message } });
}

function isRequestObject(value) {
  return Boolean(value) && typeof value === 'object' && !Array.isArray(value);
}

async function handleSafely(request) {
  try {
    return await handleStructureMcpRequest(request);
  } catch (error) {
    // одно битое сообщение не должно ронять процесс
    return { jsonrpc: '2.0', id: request?.id ?? null, error: { code: -32603, message: `internal error: ${error.message}` } };
  }
}

async function dispatchMessage(text) {
  let request;
  try {
    request = JSON.parse(text);
  } catch {
    writeError(null, -32700, 'parse error');
    return;
  }
  if (Array.isArray(request)) {
    if (!request.length) {
      writeError(null, -32600, 'invalid request');
      return;
    }
    const responses = [];
    for (const item of request) {
      if (!isRequestObject(item)) {
        responses.push({ jsonrpc: '2.0', id: null, error: { code: -32600, message: 'invalid request' } });
        continue;
      }
      const response = await handleSafely(item);
      if (response) responses.push(response);
    }
    if (responses.length) writeMessage(responses);
    return;
  }
  if (!isRequestObject(request)) {
    writeError(null, -32600, 'invalid request');
    return;
  }
  const response = await handleSafely(request);
  if (response) writeMessage(response);
}

let skipBytes = 0; // тело отклонённого oversized-кадра, отбрасываем по мере прихода

function takeMessages() {
  const messages = [];
  while (true) {
    if (skipBytes) {
      const drop = Math.min(skipBytes, buffer.length);
      buffer = buffer.subarray(drop);
      skipBytes -= drop;
      if (skipBytes) break;
      messages.push(null); // oversized-кадр проглочен
      continue;
    }
    const headerEnd = buffer.indexOf('\r\n\r\n');
    if (headerEnd < 0) {
      // заголовки не могут расти бесконечно: защита от мусорного потока без кадров
      if (buffer.length > MAX_HEADER_BYTES) buffer = Buffer.alloc(0);
      break;
    }
    let length = null;
    for (const line of buffer.subarray(0, headerEnd).toString('latin1').split('\r\n')) {
      const colon = line.indexOf(':');
      if (colon < 0) continue;
      if (line.slice(0, colon).trim().toLowerCase() !== 'content-length') continue;
      const parsed = Number.parseInt(line.slice(colon + 1).trim(), 10);
      if (Number.isInteger(parsed) && parsed >= 0) length = parsed;
    }
    if (length == null) {
      // нет валидного Content-Length — пропускаем блок заголовков
      buffer = buffer.subarray(headerEnd + 4);
      continue;
    }
    if (length > MAX_MESSAGE_BYTES) {
      buffer = buffer.subarray(headerEnd + 4);
      skipBytes = length;
      continue;
    }
    // тело ещё не пришло целиком — ждём следующий chunk, заголовок остаётся в буфере
    if (buffer.length < headerEnd + 4 + length) break;
    messages.push(buffer.subarray(headerEnd + 4, headerEnd + 4 + length).toString('utf8'));
    buffer = buffer.subarray(headerEnd + 4 + length);
  }
  return messages;
}

for await (const chunk of process.stdin) {
  buffer = Buffer.concat([buffer, chunk]);
  for (const message of takeMessages()) {
    if (message === null) {
      writeError(null, -32600, `message exceeds ${MAX_MESSAGE_BYTES} bytes`);
      continue;
    }
    await dispatchMessage(message);
  }
}
