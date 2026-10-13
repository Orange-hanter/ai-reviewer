/**
 * Строки диффа делятся только по `\n`. `\r`, U+2028 и U+2029 внутри строки кода автора PR —
 * содержимое, а не граница: с флагом `/m` JS принимал их за начало строки, и подделанный
 * заголовок секции выключал prescan на настоящем файле (`deleted file mode`) или уводил
 * весь PR в skip (`similarity index 100%`). Rust-порт делит по `\n` — обе стороны сверены.
 */
import './helpers/reference-profile.mjs';
import { test } from 'node:test';
import assert from 'node:assert/strict';

process.env.AI_REVIEWER_ROUTER_FORCE_JS = '1';
process.env.AI_REVIEWER_DIFF_FORCE_JS = '1';
const { collectAddedPaths, collectDeletedPaths, buildChunks, parseDiff } = await import('../lib/diff.mjs');
const { routeDiff } = await import('../lib/router.mjs');

const SEPS = { CR: '\r', LS: ' ', PS: ' ' };
const section = (path, body) =>
  `diff --git a/${path} b/${path}\nindex 1..2 100644\n--- a/${path}\n+++ b/${path}\n@@ -1,1 +1,2 @@\n ctx\n${body}\n`;

for (const [name, sep] of Object.entries(SEPS)) {
  test(`router: «similarity index 100%» за ${name} в строке кода не делает правку rename-only`, () => {
    const diff = section('app/server/auth.ts', `-  return check(user);\n+  return true; //${sep}similarity index 100%`);
    const r = routeDiff(['app/server/auth.ts'], diff);
    assert.equal(r.route, 'full', r.reason);
  });

  test(`collectDeletedPaths/collectAddedPaths: заголовок за ${name} в строке кода не подделывает секцию`, () => {
    const diff = section('a.ts', `+x${sep}diff --git a/real.ts b/real.ts${sep}deleted file mode 100644${sep}new file mode 100644`)
      + section('real.ts', '+evil()');
    assert.deepEqual([...collectDeletedPaths(diff)], []);
    assert.deepEqual([...collectAddedPaths(diff)], []);
  });

  test(`buildChunks (JS): ${name} в строке кода не режет секцию на два файла`, () => {
    const diff = section('a.ts', `+x${sep}diff --git a/real.ts b/real.ts`);
    const annotated = parseDiff(diff).annotated;
    const chunks = buildChunks(annotated, new Map(), { maxDiffBytes: 100000, maxContextBytes: 100000, maxChunks: 4, chunkBytes: 100000 });
    const paths = chunks.flatMap((c) => c.paths || c.files || []);
    assert.ok(!paths.includes('real.ts'), JSON.stringify(paths));
  });
}

test('настоящие удаление и rename по-прежнему распознаются', () => {
  const del = 'diff --git a/gone.ts b/gone.ts\ndeleted file mode 100644\nindex 1..0\n--- a/gone.ts\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n';
  assert.deepEqual([...collectDeletedPaths(del)], ['gone.ts']);
  const ren = 'diff --git a/app/old.ts b/app/new.ts\nsimilarity index 100%\nrename from app/old.ts\nrename to app/new.ts\n';
  assert.equal(routeDiff(['app/new.ts'], ren).route, 'skip');
});
