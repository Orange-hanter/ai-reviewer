#!/usr/bin/env node
import { handleStructureMcpRequest } from '../lib/structure-mcp.mjs';

// Транспорт MCP stdio: сообщения обрамлены заголовком Content-Length,
// заголовки читаются без учёта регистра, неизвестные игнорируются,
// частичные данные буферизуются до прихода целого кадра.
let buffer = Buffer.alloc(0);

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
      const response = await handleStructureMcpRequest(item);
      if (response) responses.push(response);
    }
    if (responses.length) writeMessage(responses);
    return;
  }
  if (!isRequestObject(request)) {
    writeError(null, -32600, 'invalid request');
    return;
  }
  const response = await handleStructureMcpRequest(request);
  if (response) writeMessage(response);
}

function takeMessages() {
  const messages = [];
  while (true) {
    const headerEnd = buffer.indexOf('\r\n\r\n');
    if (headerEnd < 0) break;
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
    if (buffer.length < headerEnd + 4 + length) break;
    messages.push(buffer.subarray(headerEnd + 4, headerEnd + 4 + length).toString('utf8'));
    buffer = buffer.subarray(headerEnd + 4 + length);
  }
  return messages;
}

for await (const chunk of process.stdin) {
  buffer = Buffer.concat([buffer, chunk]);
  for (const message of takeMessages()) await dispatchMessage(message);
}
