/**
 * Общий stdio-транспорт MCP: Content-Length кадры, частичные чтения, batch,
 * oversized-кадры. Доменная логика — в handleRequest; этот модуль ничего не знает
 * ни про structure, ни про ревью.
 *
 * handleRequest(request, { notify }) → JSON-RPC response | null (notification ack).
 * notify(method, params) пишет notification без id — логи длинного review_run.
 */

const MAX_MESSAGE_BYTES = 8 * 1024 * 1024;
const MAX_HEADER_BYTES = 16 * 1024;

export { MAX_MESSAGE_BYTES, MAX_HEADER_BYTES };

export function runMcpStdio({ handleRequest, stdin = process.stdin, stdout = process.stdout } = {}) {
  let buffer = Buffer.alloc(0);

  for (const signal of ['SIGINT', 'SIGTERM']) {
    process.on(signal, () => {
      if (stdout.writableNeedDrain) stdout.once('drain', () => process.exit(0));
      else process.exit(0);
    });
  }

  function writeMessage(payload) {
    const body = Buffer.from(JSON.stringify(payload), 'utf8');
    stdout.write(`Content-Length: ${body.length}\r\n\r\n`);
    stdout.write(body);
  }

  function writeError(id, code, message) {
    writeMessage({ jsonrpc: '2.0', id, error: { code, message } });
  }

  function notify(method, params) {
    writeMessage({ jsonrpc: '2.0', method, params });
  }

  function isRequestObject(value) {
    return Boolean(value) && typeof value === 'object' && !Array.isArray(value);
  }

  async function handleSafely(request) {
    try {
      return await handleRequest(request, { notify });
    } catch (error) {
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

  let skipBytes = 0;

  function takeMessages() {
    const messages = [];
    while (true) {
      if (skipBytes) {
        const drop = Math.min(skipBytes, buffer.length);
        buffer = buffer.subarray(drop);
        skipBytes -= drop;
        if (skipBytes) break;
        messages.push(null);
        continue;
      }
      const headerEnd = buffer.indexOf('\r\n\r\n');
      if (headerEnd < 0) {
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
        buffer = buffer.subarray(headerEnd + 4);
        continue;
      }
      if (length > MAX_MESSAGE_BYTES) {
        buffer = buffer.subarray(headerEnd + 4);
        skipBytes = length;
        continue;
      }
      if (buffer.length < headerEnd + 4 + length) break;
      messages.push(buffer.subarray(headerEnd + 4, headerEnd + 4 + length).toString('utf8'));
      buffer = buffer.subarray(headerEnd + 4 + length);
    }
    return messages;
  }

  return (async () => {
    for await (const chunk of stdin) {
      buffer = Buffer.concat([buffer, chunk]);
      for (const message of takeMessages()) {
        if (message === null) {
          writeError(null, -32600, `message exceeds ${MAX_MESSAGE_BYTES} bytes`);
          continue;
        }
        await dispatchMessage(message);
      }
    }
  })();
}
