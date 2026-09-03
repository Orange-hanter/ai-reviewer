/**
 * Контракт: гейт merge не знает про structure-контур (RFC 01, п. 1).
 *
 * Правило соблюдается с момента появления контура, но до этого теста держалось ни на чём:
 * первый `import` structure-модуля из `pipeline.mjs` растворил бы границу, не уронив ни
 * одной проверки. Тест держит именно границу, а не реализацию, поэтому читает исходники
 * как текст и не импортирует ни одну из сторон.
 *
 * Файл отдельный намеренно: `node --test` запускает каждый файл в своём процессе, а профиль
 * process-wide — здесь профиль не грузится вообще, и подмешивать эти проверки в наборы,
 * которые его выставляют, нельзя.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('..', import.meta.url));

/** Импорты вида `from './structure-x.mjs'` / `import('./lib/structure.mjs')` — в любой форме. */
const STRUCTURE_IMPORT = /(?:from|import)\s*\(?\s*['"][^'"]*\bstructure[^'"]*\.mjs['"]/g;
const PIPELINE_IMPORT = /(?:from|import)\s*\(?\s*['"][^'"]*\bpipeline\.mjs['"]/g;

async function libFiles() {
  const names = await readdir(join(ROOT, 'lib'));
  return names.filter((n) => n.endsWith('.mjs'));
}

function matches(source, re) {
  return [...source.matchAll(re)].map((m) => m[0]);
}

test('граница: pipeline.mjs не импортирует structure-модули', async () => {
  const src = await readFile(join(ROOT, 'pipeline.mjs'), 'utf8');
  const hits = matches(src, STRUCTURE_IMPORT);
  assert.deepEqual(hits, [], `pipeline.mjs импортирует structure: ${hits.join(', ')}`);
});

test('граница: lib/*.mjs вне structure-контура не импортируют structure-модули', async () => {
  const offenders = [];
  for (const name of await libFiles()) {
    if (name.startsWith('structure')) continue;
    const src = await readFile(join(ROOT, 'lib', name), 'utf8');
    const hits = matches(src, STRUCTURE_IMPORT);
    if (hits.length) offenders.push(`lib/${name}: ${hits.join(', ')}`);
  }
  assert.deepEqual(offenders, [], `гейт узнал про structure:\n${offenders.join('\n')}`);
});

test('граница: structure-модули не импортируют pipeline — контур ничего не блокирует', async () => {
  const offenders = [];
  for (const name of await libFiles()) {
    if (!name.startsWith('structure')) continue;
    const src = await readFile(join(ROOT, 'lib', name), 'utf8');
    const hits = matches(src, PIPELINE_IMPORT);
    if (hits.length) offenders.push(`lib/${name}: ${hits.join(', ')}`);
  }
  assert.deepEqual(offenders, [], `structure тянет пайплайн:\n${offenders.join('\n')}`);
});

test('граница: bin/pr-review.mjs (вход гейта) не импортирует structure-модули', async () => {
  const src = await readFile(join(ROOT, 'bin', 'pr-review.mjs'), 'utf8');
  const hits = matches(src, STRUCTURE_IMPORT);
  assert.deepEqual(hits, [], `вход гейта импортирует structure: ${hits.join(', ')}`);
});

test('сам детектор ловит нарушение — иначе тест зелёный по недосмотру', () => {
  const fake = "import { assessStructure } from './structure-assessments.mjs';";
  assert.equal(matches(fake, STRUCTURE_IMPORT).length, 1);
  const dyn = "const m = await import('../lib/structure.mjs');";
  assert.equal(matches(dyn, STRUCTURE_IMPORT).length, 1);
  // соседние имена не должны попадать под правило
  assert.deepEqual(matches("from './restructure-helper.mjs'", STRUCTURE_IMPORT), []);
  assert.deepEqual(matches("from './util.mjs'", STRUCTURE_IMPORT), []);
});
