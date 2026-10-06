/**
 * FP-семейство 7: «файла/модуля нет» о пути, который есть в репозитории, но не в PR.
 */
import './helpers/reference-profile.mjs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  buildRepoPathIndex, findAbsentButExistingPath, resolveMentionedPath, listRepoFiles,
} from '../lib/absent-path.mjs';
import { filterFalsePositiveFindings } from '../lib/findings.mjs';

const TREE = ['src/a.ts', 'src/helpers.ts', 'src/lib/db/index.ts', 'config/app.json'];
const idx = buildRepoPathIndex(TREE, ['src/a.ts']);
const claim = (body, file = 'src/a.ts') => findAbsentButExistingPath({ title: 't', body, file }, idx);

test('absent-path: утверждение об отсутствии существующего пути → путь из дерева', () => {
  assert.equal(claim('Импорт `./helpers` указывает на несуществующий файл'), 'src/helpers.ts');
  assert.equal(claim('Файл `src/helpers.ts` не существует в репозитории'), 'src/helpers.ts');
  assert.equal(claim("Cannot find module '@/lib/db'"), 'src/lib/db/index.ts');
  assert.equal(claim('импорт несуществующего модуля `./helpers`'), 'src/helpers.ts');
  assert.equal(claim('helpers.ts не найден — сборка упадёт'), 'src/helpers.ts');
  assert.equal(claim('The file `config/app.json` does not exist in the repo.'), 'config/app.json');
});

test('absent-path: путь, которого нет в дереве, — находка верная, не снимаем', () => {
  assert.equal(claim('Файл `src/missing.ts` не существует'), null);
  assert.equal(claim("Cannot find module './nope'"), null);
});

test('absent-path: отсутствие как условие рантайма — не утверждение о дереве', () => {
  assert.equal(claim('Если `config/app.json` не существует, readFileSync бросит ENOENT'), null);
  assert.equal(claim('When `config/app.json` does not exist the loader crashes'), null);
});

test('absent-path: отсутствие символа в существующем файле — не семейство 7', () => {
  assert.equal(claim('`src/helpers.ts` вызывает несуществующую функцию foo'), null);
  assert.equal(claim('Экспорт `parse` не найден в `src/helpers.ts`'), null);
  assert.equal(claim('валидация отсутствует в `src/helpers.ts`'), null);
});

test('absent-path: голое имя файла ищем только в каталоге находки', () => {
  assert.equal(claim('helpers.ts не найден', 'other/x.ts'), null);
  assert.equal(resolveMentionedPath('helpers.ts', 'src/a.ts', idx), 'src/helpers.ts');
  assert.equal(resolveMentionedPath('../src/helpers', 'src/a.ts', idx), 'src/helpers.ts');
  assert.equal(resolveMentionedPath('https://x.io/a.ts', 'src/a.ts', idx), null);
});

test('absent-path: чужое или пустое дерево → индекса нет, фильтр выключен', () => {
  assert.equal(buildRepoPathIndex(['x/y.ts'], ['a.ts', 'b.ts']), null);
  assert.equal(buildRepoPathIndex(null, ['a.ts']), null);
  assert.equal(buildRepoPathIndex([], []), null);
  assert.equal(findAbsentButExistingPath({ body: 'Файл `src/helpers.ts` не существует' }, null), null);
});

test('absent-path: listRepoFiles fail-open при ошибке git', async () => {
  assert.equal(await listRepoFiles({ exec: async () => { throw new Error('not a git repo'); } }), null);
  assert.deepEqual(await listRepoFiles({ exec: async () => ({ stdout: 'a.ts\0b/c.ts\0' }) }), ['a.ts', 'b/c.ts']);
});

test('filterFalsePositiveFindings: repoIndex снимает находку семейства 7, без индекса — нет', () => {
  const findings = [
    { file: 'src/a.ts', line: 1, severity: 'critical', title: 'Модуль не найден', body: "Cannot find module './helpers'", model: 'm1' },
    { file: 'src/a.ts', line: 2, severity: 'major', title: 'IDOR', body: 'нет проверки userId', model: 'm1' },
  ];
  const logs = [];
  const kept = filterFalsePositiveFindings(findings, ['src/a.ts'], (m) => logs.push(m), { repoIndex: idx });
  assert.deepEqual(kept.map((f) => f.title), ['IDOR']);
  assert.ok(logs.some((m) => m.includes('src/helpers.ts')));
  assert.equal(filterFalsePositiveFindings(findings, ['src/a.ts']).length, 2);
});
