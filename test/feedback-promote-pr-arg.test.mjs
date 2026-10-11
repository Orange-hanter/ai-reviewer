/**
 * fetchPrFileDiff: номер PR приходит из файла кандидата (fromFeedback.pr) — произвольный
 * JSON. До `--` в argv gh он не должен становиться опцией, в URL API — путём.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, chmodSync, existsSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fetchPrFileDiff } from '../feedback/promote-candidate.mjs';

test('fetchPrFileDiff: нечисловой pr не вызывает gh и не идёт в API', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'promote-pr-'));
  const marker = join(dir, 'gh-called');
  writeFileSync(join(dir, 'gh'), `#!/bin/sh\necho "$@" > '${marker}'\necho 'diff --git a/x b/x'\n`);
  chmodSync(join(dir, 'gh'), 0o755);
  const prevPath = process.env.PATH;
  process.env.PATH = `${dir}:${prevPath}`;
  const prevFetch = globalThis.fetch;
  let fetched = 0;
  globalThis.fetch = async () => { fetched++; throw new Error('no network'); };
  t.after(() => {
    process.env.PATH = prevPath;
    globalThis.fetch = prevFetch;
    rmSync(dir, { recursive: true, force: true });
  });
  for (const pr of ['--repo=evil/x', '-R', '1/../../user', '7 --repo=evil/x']) {
    assert.equal(await fetchPrFileDiff({ pr, repo: 'o/r', file: 'a.ts', token: 'tok' }), '', pr);
    assert.equal(existsSync(marker), false, `gh вызван с pr=${pr}`);
  }
  assert.equal(fetched, 0);
  assert.equal(await fetchPrFileDiff({ pr: 42, repo: 'o/r', file: 'a.ts', token: 'tok' }), 'diff --git a/x b/x\n');
});
