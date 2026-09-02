#!/usr/bin/env node
import { createInterface } from 'node:readline';
import { handleStructureMcpRequest } from '../lib/structure-mcp.mjs';

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });

for await (const line of input) {
  if (!line.trim()) continue;
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    process.stdout.write(`${JSON.stringify({
      jsonrpc: '2.0',
      id: null,
      error: { code: -32700, message: 'parse error' },
    })}\n`);
    continue;
  }
  const response = await handleStructureMcpRequest(request);
  if (response) process.stdout.write(`${JSON.stringify(response)}\n`);
}
