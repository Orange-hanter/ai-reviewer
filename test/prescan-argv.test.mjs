// Пути изменённых файлов приходят из диффа PR и уходят в argv eslint/ast-grep.
// Файл `app/--config=evil.ts` не должен стать опцией линтера (RFC 16 SEC).

import './helpers/reference-profile.mjs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { chmod, mkdir, mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { runEslintPrescan } from '../lib/prescan.mjs';
import { runAstgrepPrescan } from '../lib/astgrep.mjs';
import { toolPathArg } from '../lib/prescan-shared.mjs';

const EVIL = 'app/--config=evil.ts';

async function script(path, body) {
  await mkdir(join(path, '..'), { recursive: true });
  await writeFile(path, `#!/bin/sh\n${body}`);
  await chmod(path, 0o755);
}

async function inTmp(fn) {
  const prev = process.cwd();
  const dir = await mkdtemp(join(tmpdir(), 'prescan-argv-'));
  process.chdir(dir);
  try {
    return await fn(dir);
  } finally {
    process.chdir(prev);
  }
}

test('toolPathArg: путь с ведущим дефисом не читается как опция', () => {
  assert.equal(toolPathArg('--config=evil.ts'), './--config=evil.ts');
  assert.equal(toolPathArg('-r.ts'), './-r.ts');
  assert.equal(toolPathArg('server/a.ts'), 'server/a.ts');
  assert.equal(toolPathArg('a-b.ts'), 'a-b.ts');
});

test('runEslintPrescan: путь из диффа с ведущим дефисом уходит позиционным аргументом', async () => {
  await inTmp(async (dir) => {
    await mkdir(join(dir, 'app/node_modules/eslint'), { recursive: true });
    await writeFile(join(dir, 'app/node_modules/eslint/package.json'), '{}');
    await script(
      join(dir, 'app/node_modules/.bin/eslint'),
      '[ "$4" = ./--config=evil.ts ] || { echo "bad args: $*" >&2; exit 9; }\necho "[]"\n',
    );
    const r = await runEslintPrescan([EVIL], { prescan: { enabled: true, appDir: 'app', maxFiles: 30 } });
    assert.equal(r.error, undefined, JSON.stringify(r));
    assert.equal(r.scanned, 1);
  });
});

test('runAstgrepPrescan: путь из диффа с ведущим дефисом уходит позиционным аргументом', async () => {
  await inTmp(async (dir) => {
    await mkdir(join(dir, 'app'), { recursive: true });
    await writeFile(join(dir, 'sgconfig.yml'), 'ruleDirs: []\n');
    await script(
      join(dir, 'bin/ast-grep'),
      '[ "$5" = ./--config=evil.ts ] || { echo "bad args: $*" >&2; exit 9; }\necho "{}"\n',
    );
    const r = await runAstgrepPrescan([EVIL], {
      prescan: {
        enabled: true, appDir: 'app', maxFiles: 30,
        astgrep: { enabled: true, configPath: join(dir, 'sgconfig.yml'), bin: './bin/ast-grep' },
      },
    });
    assert.equal(r.error, undefined, JSON.stringify(r));
    assert.equal(r.scanned, 1);
  });
});
