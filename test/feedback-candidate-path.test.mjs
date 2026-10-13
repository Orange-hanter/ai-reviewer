// suggestedId кандидата в fixtures собирается из полей находки и становится именем
// файла: находка с line вида "/../../x" не должна писать за пределы candidatesDir
// (RFC 20 SEC).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { buildFixtureCandidate, writeFixtureCandidates } from '../lib/feedback.mjs';
import { sanitizeFixtureId } from '../feedback/promote-candidate.mjs';

const entry = (line, title = 'unsafe eval') => ({
  pr: 7, source: 'label', signal: 'fp', label: 'ai-review-fp',
  finding: { file: 'server/a.ts', line, severity: 'major', title },
});

test('buildFixtureCandidate: suggestedId — безопасное имя файла', () => {
  assert.equal(buildFixtureCandidate(entry(12)).suggestedId, 'fp-pr-7-unsafe-eval-L12');
  const id = buildFixtureCandidate(entry('/../../../escape')).suggestedId;
  assert.equal(id, 'fp-pr-7-unsafe-eval-L----------escape');
  assert.equal(sanitizeFixtureId(id), id);
});

test('writeFixtureCandidates: line с ../ не выводит запись из candidatesDir', async () => {
  const root = await mkdtemp(join(tmpdir(), 'fb-cand-'));
  const dir = join(root, 'a/b/candidates');
  const r = await writeFixtureCandidates(dir, [entry('/../../../escape')]);
  assert.equal(r.written, 1);
  assert.deepEqual(await readdir(join(root, 'a/b')), ['candidates']);
  assert.deepEqual(await readdir(dir), ['fp-pr-7-unsafe-eval-L----------escape.json']);
});
