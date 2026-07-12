#!/usr/bin/env node
/** Validate or rewrite fp-feedback.jsonl to the current schema version. */

import { readFile, rename, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { migrateFeedbackText } from '../lib/feedback-schema.mjs';
import { pkgPath } from '../lib/package-root.mjs';

export async function migrateFeedbackLog(path, { write = false } = {}) {
  const target = resolve(path);
  let raw = '';
  try {
    raw = await readFile(target, 'utf8');
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  const result = migrateFeedbackText(raw);
  if (write && result.stats.invalid) {
    throw new Error(`migration refused: ${result.stats.invalid} invalid row(s)`);
  }
  if (write && result.text !== raw) {
    const temporary = `${target}.tmp-${process.pid}`;
    await writeFile(temporary, result.text, 'utf8');
    await rename(temporary, target);
  }
  return { ...result, path: target, changed: result.text !== raw };
}

async function main() {
  const argv = process.argv.slice(2);
  const flag = (name) => {
    const index = argv.indexOf(name);
    return index >= 0 ? argv[index + 1] : undefined;
  };
  const path = flag('--file') || pkgPath('feedback/fp-feedback.jsonl');
  const write = argv.includes('--write');
  const result = await migrateFeedbackLog(path, { write });
  console.log(JSON.stringify({
    path: result.path,
    write,
    changed: result.changed,
    ...result.stats,
  }));
  process.exit(result.stats.invalid ? 1 : 0);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    console.error('[migrate-feedback]', error.stack || error.message);
    process.exit(1);
  });
}
