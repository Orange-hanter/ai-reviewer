// Доставка диффов gold-кейсов: локальные клоны своих репозиториев (gold-private).
// Запуск: node --test test/gold-fetch.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, writeFile, rm, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const { goldRepoPaths, localRepoPath, ensureCaseDiff, cachedPatchName } = await import('../lib/gold-fetch.mjs');

test('goldRepoPaths: «имя=путь» через запятую, пробелы и мусор игнорируются', () => {
  const m = goldRepoPaths({ REVIEW_GOLD_REPOS: 'orakul=/a/Orakul, restate = /b/Restate ,broken,' });
  assert.deepEqual([...m], [['orakul', '/a/Orakul'], ['restate', '/b/Restate']]);
  assert.equal(goldRepoPaths({}).size, 0);
});

test('localRepoPath: нет пути к клону — ошибка с подсказкой переменной', () => {
  assert.throws(() => localRepoPath({ id: 'x', source: { local: 'orakul' } }, {}), /REVIEW_GOLD_REPOS="orakul=/);
});

test('ensureCaseDiff: кейс source.local — дифф base..sha из клона, в кэш, повторно из кэша', async () => {
  const repo = await mkdtemp(join(tmpdir(), 'gold-repo-'));
  const cache = await mkdtemp(join(tmpdir(), 'gold-cache-'));
  const caseDir = await mkdtemp(join(tmpdir(), 'gold-case-'));
  const git = (...a) => execFileSync('git', ['-C', repo, ...a], { encoding: 'utf8' }).trim();
  try {
    git('init', '-q');
    git('config', 'user.email', 't@t');
    git('config', 'user.name', 't');
    await writeFile(join(repo, 'a.js'), 'const a = 1;\n');
    git('add', '.');
    git('commit', '-qm', 'base');
    const base = git('rev-parse', 'HEAD');
    await writeFile(join(repo, 'a.js'), 'const a = 2;\n');
    git('commit', '-qam', 'change');
    const sha = git('rev-parse', 'HEAD');
    const meta = { id: 'p-1', source: { local: 'p', base, sha } };
    const env = { REVIEW_GOLD_REPOS: `p=${repo}` };

    const first = await ensureCaseDiff(caseDir, meta, { cacheDir: cache, env });
    assert.equal(first.source, 'fetched');
    assert.equal(first.path, join(cache, cachedPatchName(meta)));
    const patch = await readFile(first.path, 'utf8');
    assert.match(patch, /-const a = 1;/);
    assert.match(patch, /\+const a = 2;/);
    assert.equal((await ensureCaseDiff(caseDir, meta, { cacheDir: cache, env })).source, 'cache');

    await assert.rejects(ensureCaseDiff(caseDir, { id: 'q', source: { local: 'p', sha } }, { cacheDir: cache, env }), /source\.\{repo\|local,sha,base\}/);
  } finally {
    for (const d of [repo, cache, caseDir]) await rm(d, { recursive: true, force: true });
  }
});
