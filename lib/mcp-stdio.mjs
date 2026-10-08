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
  let framing = 'lsp';
  try {
    stdout._handle?.setBlocking?.(true);
  } catch {
    /* ignore */
  }

  for (const signal of ['SIGINT', 'SIGTERM']) {
    process.on(signal, () => {
      if (stdout.writableNeedDrain) stdout.once('drain', () => process.exit(0));
      else process.exit(0);
    });
  }

  function writeMessage(payload) {
    const json = JSON.stringify(payload);
    if (framing === 'json') {
      stdout.write(`${json}\n`);
      return;
    }
    const body = Buffer.from(json, 'utf8');
    stdout.write(Buffer.concat([
      Buffer.from(`Content-Length: ${body.length}\r\n\r\n`, 'ascii'),
      body,
    ]));
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

  function takeBareJson() {
    let i = 0;
    while (i < buffer.length && (buffer[i] === 0x20 || buffer[i] === 0x09 || buffer[i] === 0x0a || buffer[i] === 0x0d)) {
      i += 1;
    }
    if (i >= buffer.length) return 'wait';
    const startChar = buffer[i];
    if (startChar !== 0x7b && startChar !== 0x5b) return null;
    let depth = 0;
    let inStr = false;
    let escape = false;
    for (let j = i; j < buffer.length; j += 1) {
      const c = buffer[j];
      if (inStr) {
        if (escape) {
          escape = false;
          continue;
        }
        if (c === 0x5c) {
          escape = true;
          continue;
        }
        if (c === 0x22) inStr = false;
        continue;
      }
      if (c === 0x22) {
        inStr = true;
        continue;
      }
      if (c === 0x7b || c === 0x5b) depth += 1;
      else if (c === 0x7d || c === 0x5d) {
        depth -= 1;
        if (depth === 0) {
          const end = j + 1;
          let consume = end;
          if (buffer[consume] === 0x0d) consume += 1;
          if (buffer[consume] === 0x0a) consume += 1;
          return {
            message: buffer.subarray(i, end).toString('utf8'),
            rest: buffer.subarray(consume),
          };
        }
      }
    }
    return 'wait';
  }

  function takeMessages() {
    const messages = [];
    while (true) {
      if (skipBytes) {
        const drop = Math.min(skipBytes, buffer.length);
        buffer = buffer.subarray(drop);
        skipBytes -= drop;
        if (skipBytes) break;
        messages.push({ text: null, framing });
        continue;
      }
      const bare = takeBareJson();
      if (bare && bare !== 'wait') {
        messages.push({ text: bare.message, framing: 'json' });
        buffer = bare.rest;
        continue;
      }
      if (bare === 'wait') break;
      const crlf = buffer.indexOf('\r\n\r\n');
      const lf = buffer.indexOf('\n\n');
      let headerEnd = -1;
      let headerWidth = 4;
      if (crlf >= 0 && (lf < 0 || crlf <= lf)) {
        headerEnd = crlf;
        headerWidth = 4;
      } else if (lf >= 0) {
        headerEnd = lf;
        headerWidth = 2;
      }
      if (headerEnd < 0) {
        if (buffer.length > MAX_HEADER_BYTES) buffer = Buffer.alloc(0);
        break;
      }
      let length = null;
      for (const rawLine of buffer.subarray(0, headerEnd).toString('latin1').split('\n')) {
        const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
        const colon = line.indexOf(':');
        if (colon < 0) continue;
        if (line.slice(0, colon).trim().toLowerCase() !== 'content-length') continue;
        const parsed = Number.parseInt(line.slice(colon + 1).trim(), 10);
        if (Number.isInteger(parsed) && parsed >= 0) length = parsed;
      }
      if (length == null) {
        buffer = buffer.subarray(headerEnd + headerWidth);
        continue;
      }
      if (length > MAX_MESSAGE_BYTES) {
        buffer = buffer.subarray(headerEnd + headerWidth);
        skipBytes = length;
        continue;
      }
      if (buffer.length < headerEnd + headerWidth + length) break;
      messages.push({
        text: buffer.subarray(headerEnd + headerWidth, headerEnd + headerWidth + length).toString('utf8'),
        framing: 'lsp',
      });
      buffer = buffer.subarray(headerEnd + headerWidth + length);
    }
    return messages;
  }

  return (async () => {
    for await (const chunk of stdin) {
      buffer = Buffer.concat([buffer, chunk]);
      for (const message of takeMessages()) {
        if (message.framing) framing = message.framing;
        if (message.text === null) {
          writeError(null, -32600, `message exceeds ${MAX_MESSAGE_BYTES} bytes`);
          continue;
        }
        await dispatchMessage(message.text);
      }
    }
  })();
}
