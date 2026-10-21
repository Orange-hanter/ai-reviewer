// Ужесточение skip/доверия после PR #62: fail-closed разбор sgconfig, узкий встроенный skip,
// детерминированная проверка реестров lockfile, вход доверенного профиля — из base ref.
// Новые модули импортируются внутри тестов: на коде до фикса каждый тест падает сам по себе.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, readdirSync, existsSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { parseSgconfigDirs, isBuiltInSkipPath, isInertPath, collectReviewSkipped } from '../lib/router.mjs';

test('parseSgconfigDirs: всё, что разбор не понимает, — null (fail-closed)', () => {
  const failClosed = {
    'ключ в кавычках': '"ruleDirs":\n  - evil\n',
    'ключ в одинарных кавычках': "'ruleDirs':\n  - evil\n",
    'вложено под незнакомым ключом': 'foo:\n  ruleDirs:\n    - x\n',
    'маркер документа': '---\nruleDirs:\n  - rules\n',
    'флоу-маппинг в колонке 0': '{ruleDirs: [evil]}\n',
    'явный ключ ?': '? ruleDirs\n: [evil]\n',
    'merge-ключ': '<<: *base\nruleDirs:\n  - rules\n',
    'алиас вместо списка': 'ruleDirs: *dirs\n',
    'якорь у незнакомого ключа': 'base: &b\n  - x\n',
    'блочный скаляр': 'note: |\n  ruleDirs:\n  - evil\n',
    '# внутри кавычек': 'ruleDirs:\n  - "a #b"\n',
    'кавычка внутри значения': 'ruleDirs:\n  - a"b\n',
    'флоу в элементе': 'ruleDirs:\n  - [a, b]\n',
    'пустой элемент': 'ruleDirs:\n  -\n',
    'ключ:значение без пробела': 'ruleDirs:[evil]\n',
    'строка не-ключ в колонке 0': 'ruleDirs\n',
    'вложенное под скаляром': 'name: x\n  ruleDirs: [y]\n',
    'testConfigs инлайн': 'testConfigs: [a]\n',
    'testDir без значения': 'testConfigs:\n  - testDir:\n',
  };
  for (const [why, text] of Object.entries(failClosed)) assert.equal(parseSgconfigDirs(text), null, why);
});

test('parseSgconfigDirs: настоящие конфиги разбираются', () => {
  // sgconfig Orakul (ops/pr-review/ast-grep/sgconfig.yml) и пакета
  assert.deepEqual(parseSgconfigDirs('# ast-grep project for R6+ prescan (paths passed explicitly from pr-review runner).\nruleDirs:\n  - rules\n'), ['rules']);
  assert.deepEqual(parseSgconfigDirs('﻿ruleDirs:\r\n  - rules\r\n'), ['rules'], 'BOM и CRLF');
  assert.deepEqual(parseSgconfigDirs('ruleDirs:\n  - \'a b/c\'  # x\n  - ../shared\nutilDirs: []\n'), ['a b/c', '../shared']);
  assert.deepEqual(parseSgconfigDirs('testConfigs:\n  - testDir: t\n    includes:\n      - x\n    snapshotDir: s\n  -\n    testDir: t2\n'), ['t', 's', 't2']);
  assert.deepEqual(parseSgconfigDirs('customLanguages:\n  mojo:\n    libraryPath: m.so\nruleDirs: [r]\n'), ['r']);
});

test('встроенный skip: только source map JS/TS/CSS и проверяемые lockfile', () => {
  for (const p of ['dist/app.js.map', 'x.min.js.map', 'a.mjs.map', 'b.cjs.map', 'c.jsx.map', 'd.ts.map', 'e.tsx.map', 'f.CSS.MAP',
    'package-lock.json', 'app/yarn.lock', 'pnpm-lock.yaml']) {
    assert.equal(isBuiltInSkipPath(p), true, p);
  }
  for (const p of ['nginx/routes.map', 'conf/site.map', 'a.map', 'data/x.json.map', 'Cargo.lock', 'poetry.lock', 'Gemfile.lock', 'composer.lock']) {
    assert.equal(isBuiltInSkipPath(p), false, p);
  }
  assert.equal(isInertPath('nginx/routes.map'), false, 'голый *.map не инертен и под reviewSkip');
  assert.equal(isInertPath('dist/app.js.map'), true);
  assert.equal(isInertPath('Cargo.lock'), true, 'непроверяемый lockfile скрывает только доверенный reviewSkip');
  assert.deepEqual(collectReviewSkipped(['nginx/routes.map', 'Cargo.lock', 'dist/a.css.map']).builtIn, ['dist/a.css.map']);
});

const lockSection = (path, added, removed = []) => `diff --git a/${path} b/${path}
--- a/${path}
+++ b/${path}
@@ -5,${removed.length + 1} +5,${added.length + 1} @@
 ctx
${removed.map((l) => `-${l}`).join('\n')}${removed.length ? '\n' : ''}${added.map((l) => `+${l}`).join('\n')}
`;

test('scanLockfileRegistries: чужой хост в добавленных resolved/tarball — major prescan', async () => {
  const { scanLockfileRegistries, parseLockfileRegistries, LOCKFILE_REGISTRY_RULE } = await import('../lib/lockfile-registry.mjs');
  const diff = lockSection('package-lock.json', [
    '      "resolved": "https://registry.npmjs.org/a/-/a-1.0.0.tgz",',
    '      "resolved": "https://evil.example.com/b/-/b-1.0.0.tgz",',
    '      "resolved": "https://evil.example.com/c/-/c-1.0.0.tgz",',
    '      "resolved": "git+ssh://git@github.com/x/y.git#abc",',
    '      "resolved": "packages/local",',
  ], ['      "resolved": "https://other.example.org/z.tgz",'])
    + lockSection('app/yarn.lock', ['  resolved "http://registry.yarnpkg.com/d/-/d-1.0.0.tgz#sha"'])
    + lockSection('pnpm-lock.yaml', ['    resolution: {tarball: https://npm.pkg.github.com/e.tgz}', '    resolution: {integrity: sha512-x}'])
    + lockSection('src/notes.md', ['resolved "https://evil.example.com/x"']);
  const f = scanLockfileRegistries(diff);
  assert.deepEqual(f.map((x) => [x.file, x.line, x.title]), [
    ['package-lock.json', 7, 'lockfile: пакет ставится не из разрешённого реестра — evil.example.com'],
    ['package-lock.json', 9, 'lockfile: пакет ставится не из разрешённого реестра — github.com'],
    ['app/yarn.lock', 6, 'lockfile: пакет ставится по нешифрованному http — registry.yarnpkg.com'],
    ['pnpm-lock.yaml', 6, 'lockfile: пакет ставится не из разрешённого реестра — npm.pkg.github.com'],
  ]);
  for (const x of f) {
    assert.equal(x.severity, 'major');
    assert.equal(x.prescan, true);
    assert.equal(x.prescanBlock, true);
    assert.equal(x.ruleId, LOCKFILE_REGISTRY_RULE);
    assert.equal(x.model, 'lockfile-registry');
  }
  assert.ok(!f.some((x) => x.body.includes('other.example.org')), 'удалённые строки не проверяются');
  // allowlist из env
  const hosts = parseLockfileRegistries(' npm.pkg.github.com , https://github.com/org ,, ');
  assert.deepEqual(hosts, ['npm.pkg.github.com', 'github.com']);
  assert.deepEqual(scanLockfileRegistries(diff, { registries: hosts }).map((x) => x.file), ['package-lock.json', 'app/yarn.lock']);
  assert.deepEqual(scanLockfileRegistries(lockSection('package-lock.json', ['"resolved": "https://registry.npmjs.org/a.tgz"'])), []);
});

// ---------------------------------------------------------------------------
// Вход доверенного профиля — из base ref
// ---------------------------------------------------------------------------

function git(cwd, ...args) {
  return execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@t', '-c', 'commit.gpgsign=false', ...args],
    { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
}

function put(root, rel, text) {
  mkdirSync(join(root, rel, '..'), { recursive: true });
  writeFileSync(join(root, rel), text);
}

/** base: профиль + правила + fp + sgconfig/rules/a.yml; рабочее дерево — правки «автора PR». */
function makeRepo() {
  const root = mkdtempSync(join(tmpdir(), 'ai-review-trusted-'));
  git(root, 'init', '-q', '-b', 'main');
  put(root, 'ops/ai-review/profile.json', JSON.stringify({
    name: 'base', repoRulesFile: 'repo-rules.md', layout: { reviewSkip: '^docs/gen/' },
    paths: { fpPatterns: 'ops/fp.jsonl', astgrepConfig: 'ops/sg/sgconfig.yml' },
  }));
  put(root, 'ops/ai-review/repo-rules.md', 'BASE RULES');
  put(root, 'AGENTS.md', 'BASE AGENTS');
  put(root, 'ops/fp.jsonl', '{"id":"base-fp","regex":"base-only"}\n');
  put(root, 'ops/sg/sgconfig.yml', 'ruleDirs:\n  - rules\n');
  put(root, 'ops/sg/rules/a.yml', 'id: a\n');
  git(root, 'add', '-A');
  git(root, 'commit', '-q', '-m', 'base');
  // «PR»: правит всё, что читает движок
  put(root, 'ops/ai-review/profile.json', JSON.stringify({
    name: 'evil', repoRulesFile: 'repo-rules.md', layout: { reviewSkip: '.*' },
    paths: { fpPatterns: 'ops/fp.jsonl', astgrepConfig: 'ops/sg/sgconfig.yml' },
  }));
  put(root, 'ops/ai-review/repo-rules.md', 'PR RULES');
  put(root, 'AGENTS.md', 'PR AGENTS');
  put(root, 'ops/fp.jsonl', '{"id":"evil-fp","regex":".*"}\n');
  put(root, 'ops/sg/sgconfig.yml', 'ruleDirs:\n  - rules\n  - pr-rules\n');
  rmSync(join(root, 'ops/sg/rules/a.yml'));
  put(root, 'ops/sg/rules/b.yml', 'id: b\n');
  put(root, 'ops/sg/pr-rules/c.yml', 'id: c\n');
  put(root, 'ops/new-in-pr.md', 'NEW');
  return root;
}

test('resolveInputSource: доверие только с валидным REVIEW_BASE_REF, файлы — из base', async () => {
  const { resolveInputSource } = await import('../lib/trusted-input.mjs');
  const root = makeRepo();
  try {
    const off = resolveInputSource({}, { cwd: root });
    assert.equal(off.trusted, false);
    assert.equal(off.requested, false);
    assert.equal(off.readText(join(root, 'ops/ai-review/repo-rules.md')), 'PR RULES', 'недоверенный — рабочее дерево');

    for (const [ref, reason] of [['', 'REVIEW_BASE_REF не задан'], ['-x', 'невалиден'], ['a..b', 'невалиден'], ['nope', 'не резолвится']]) {
      const s = resolveInputSource({ REVIEW_PROFILE_TRUSTED: '1', REVIEW_BASE_REF: ref }, { cwd: root });
      assert.equal(s.trusted, false, ref);
      assert.equal(s.requested, true, ref);
      assert.ok(s.reason.includes(reason), `${ref}: ${s.reason}`);
    }

    const src = resolveInputSource({ REVIEW_PROFILE_TRUSTED: '1', REVIEW_BASE_REF: 'main' }, { cwd: root });
    assert.equal(src.trusted, true);
    assert.equal(src.readText(join(root, 'ops/ai-review/repo-rules.md')), 'BASE RULES');
    assert.equal(src.readText('AGENTS.md'), 'BASE AGENTS', 'относительный путь — от cwd');
    assert.throws(() => src.readText(join(root, 'ops/new-in-pr.md')), { code: 'ENOENT' }, 'нет в base — нет вовсе');
    assert.throws(() => src.readText(join(root, 'ops/sg')), { code: 'ENOENT' }, 'каталог — не файл');
    assert.ok(src.readText(new URL('../package.json', import.meta.url).pathname).includes('"ai-reviewer"'), 'вне cwd — диск');
    // пакет движка внутри cwd потребителя (.ai-reviewer-engine/) читается с диска
    put(root, '.ai-reviewer-engine/x.txt', 'ENGINE');
    const nested = resolveInputSource({ REVIEW_PROFILE_TRUSTED: '1', REVIEW_BASE_REF: 'main' }, { cwd: root, pkgRoot: join(root, '.ai-reviewer-engine') });
    assert.equal(nested.readText(join(root, '.ai-reviewer-engine/x.txt')), 'ENGINE');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('доверенный прогон: профиль, repoRulesFile, AGENTS.md, fpPatterns — из base, а не из PR', async () => {
  const { resolveInputSource, setInputSource, worktreeSource } = await import('../lib/trusted-input.mjs');
  const { loadProfileFile, normalizeProfile } = await import('../lib/profile.mjs');
  const { getFpPatterns } = await import('../lib/fp-patterns.mjs');
  const root = makeRepo();
  const cwd = process.cwd();
  try {
    process.chdir(root);
    setInputSource(resolveInputSource({ REVIEW_PROFILE_TRUSTED: '1', REVIEW_BASE_REF: 'main' }, { cwd: root }));
    const p = loadProfileFile(join(root, 'ops/ai-review/profile.json'));
    assert.equal(p.name, 'base', 'сам профиль — из base');
    assert.equal(p.layout.reviewSkip.source, '^docs\\/gen\\/');
    assert.equal(p.repoRules, 'BASE RULES');
    // без repoRulesFile — AGENTS.md в cwd, тоже из base
    assert.equal(normalizeProfile({}, { baseDir: join(root, 'ops/ai-review') }).repoRules, 'BASE AGENTS');
    assert.deepEqual(getFpPatterns(join(root, 'ops/fp.jsonl')).map((x) => x.id), ['base-fp']);

    setInputSource(worktreeSource({ cwd: root }));
    assert.equal(loadProfileFile(join(root, 'ops/ai-review/profile.json')).repoRules, 'PR RULES');
    assert.deepEqual(getFpPatterns(join(root, 'ops/fp.jsonl')).map((x) => x.id), ['evil-fp'], 'кэш fp различает источник');
  } finally {
    process.chdir(cwd);
    setInputSource(null);
    rmSync(root, { recursive: true, force: true });
  }
});

test('loadReviewConfig: REVIEW_BASE_REF включает чтение из base, без него доверие снято', async () => {
  const { loadReviewConfig } = await import('../lib/config.mjs');
  const { setInputSource } = await import('../lib/trusted-input.mjs');
  const { resetProfile } = await import('../lib/profile.mjs');
  const root = makeRepo();
  const cwd = process.cwd();
  try {
    process.chdir(root);
    resetProfile();
    const env = { REVIEW_PROFILE: 'ops/ai-review/profile.json', REVIEW_PROFILE_TRUSTED: '1', REVIEW_BASE_REF: 'main' };
    const cli = { dryRun: true, repo: '', prNumber: 0 };
    const on = loadReviewConfig(env, cli);
    assert.deepEqual({ ...on.trust }, { requested: true, trusted: true, baseRef: 'main', reason: '' });
    assert.equal(on.profile.repoRules, 'BASE RULES');
    resetProfile();
    const off = loadReviewConfig({ ...env, REVIEW_BASE_REF: '' }, cli);
    assert.equal(off.trust.trusted, false);
    assert.equal(off.trust.reason, 'REVIEW_BASE_REF не задан');
    assert.equal(off.profile.repoRules, 'PR RULES', 'без доверия — рабочее дерево, reviewSkip не применится');
  } finally {
    process.chdir(cwd);
    setInputSource(null);
    resetProfile();
    rmSync(root, { recursive: true, force: true });
  }
});

test('ast-grep доверенного прогона: sgconfig и ruleDirs — копия base во временном каталоге', async () => {
  const { resolveInputSource } = await import('../lib/trusted-input.mjs');
  const { materializeAstgrepConfig } = await import('../lib/astgrep.mjs');
  const { collectProfileProtected } = await import('../lib/router.mjs');
  const root = makeRepo();
  try {
    const src = resolveInputSource({ REVIEW_PROFILE_TRUSTED: '1', REVIEW_BASE_REF: 'main' }, { cwd: root });
    const m = materializeAstgrepConfig(join(root, 'ops/sg/sgconfig.yml'), () => {}, src);
    try {
      assert.equal(readFileSync(m.configPath, 'utf8'), 'ruleDirs:\n  - rules\n');
      assert.deepEqual(readdirSync(join(m.root, 'ops/sg/rules')), ['a.yml'], 'удалённое в PR правило на месте, добавленное — нет');
      assert.equal(existsSync(join(m.root, 'ops/sg/pr-rules')), false);
    } finally {
      rmSync(m.root, { recursive: true, force: true });
    }
    assert.deepEqual(materializeAstgrepConfig(join(root, 'ops/none.yml'), () => {}, src), { missing: true });
    assert.equal(materializeAstgrepConfig(join(root, 'ops/sg/sgconfig.yml'), () => {}, resolveInputSource({}, { cwd: root })), null);

    // защита от reviewSkip — каталоги обеих версий sgconfig
    const prot = collectProfileProtected({ profile: { paths: { astgrepConfig: 'ops/sg/sgconfig.yml' } } },
      { cwd: root, baseReadText: src.readText });
    assert.ok(prot.prefixes.includes('ops/sg/rules/') && prot.prefixes.includes('ops/sg/pr-rules/'), prot.prefixes.join(','));
    assert.equal(prot.anyYaml, false);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
