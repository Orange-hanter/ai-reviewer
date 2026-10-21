/**
 * Parity-прогон Фазы 1, шаг 4: Rust router ↔ чистый JS fallback lib/router.mjs.
 *
 * Запуск: node rust/parity/router.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/router.mjs: resolveRouterBin() читает
 * AI_REVIEWER_ROUTER_FORCE_JS на каждый вызов, поэтому js-сторона прогоняется
 * с FORCE_JS=1, rust-сторона — без него (диспетчеризация проверяется тоже).
 *
 * Корпус:
 *   1. classifyFilePath: батарея путей (из тестов + edge-кейсы регексов);
 *   2. extractFileDiffSections: quoted-пути с пробелами, rename-only, повторные
 *      заголовки, diff без `diff --git`, все diff.patch из fixtures/;
 *   3. isRenameOnlySection: rename from/similarity 100/контентные строки/пусто;
 *   4. classifyChangedFile + routeDiff на diff.patch всех fixtures-кейсов
 *      (changedPaths — ключи секций + добавленные вручную пути без секций),
 *      литеральные кейсы из test/pr-review.eval.test.mjs, enabled/пустые пути;
 *   5. formatRouteLog для каждого прогона routeDiff;
 *   6. routeDiff со skipPaths (пути layout.reviewSkip профиля);
 *   7. isCodePath на батарее путей корпуса 1.
 */

import { readFileSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

process.env.AI_REVIEWER_ROUTER_FORCE_JS = '1';
const R = await import('../../lib/router.mjs');

const runJs = (fn, ...args) => {
  process.env.AI_REVIEWER_ROUTER_FORCE_JS = '1';
  try {
    return { ok: true, value: fn(...args) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  } finally {
    delete process.env.AI_REVIEWER_ROUTER_FORCE_JS;
  }
};

let compared = 0;
const failures = [];

function compare(label, jsResult, rustResult) {
  compared += 1;
  try {
    assert.deepEqual(rustResult, jsResult);
  } catch (e) {
    failures.push({
      label,
      js: JSON.stringify(jsResult)?.slice(0, 400),
      rust: JSON.stringify(rustResult)?.slice(0, 400),
    });
  }
}

function checkClassify(label, path) {
  const js = runJs(R.classifyFilePath, path).value;
  const rust = R.classifyFilePath(path);
  compare(`classifyFilePath ${label}`, js, rust);
}

function checkSections(label, diff) {
  const js = runJs(R.extractFileDiffSections, diff).value;
  const rust = R.extractFileDiffSections(diff);
  compare(`extractFileDiffSections ${label} keys`, [...js.keys()], [...rust.keys()]);
  compare(`extractFileDiffSections ${label}`, js, rust);
}

function checkRename(label, section) {
  const js = runJs(R.isRenameOnlySection, section).value;
  const rust = R.isRenameOnlySection(section);
  compare(`isRenameOnlySection ${label}`, js, rust);
}

function checkRoute(label, changedPaths, diff, opts) {
  const js = runJs(R.routeDiff, changedPaths, diff, opts).value;
  const rust = R.routeDiff(changedPaths, diff, opts);
  compare(`routeDiff ${label}`, js, rust);
  const jsLog = runJs(R.formatRouteLog, js).value;
  const rustLog = R.formatRouteLog(rust);
  compare(`formatRouteLog ${label}`, jsLog, rustLog);
}

// ---------------------------------------------------------------------------
// Корпус 1: classifyFilePath
// ---------------------------------------------------------------------------

const PATHS = [
  'package-lock.json', 'app/yarn.lock', 'pnpm-lock.yaml', 'composer.lock',
  'Cargo.lock', 'poetry.lock', 'Gemfile.lock', 'sub/dir/Cargo.lock',
  'app/COMPOSE.LOCK', 'my.lock.json',
  'dist/bundle.js', 'app/dist/x.js', 'graphify-out/g.json', 'node_modules/m/index.js',
  '.next/chunks.js', 'coverage/lcov-report/x.js', 'build/out.js', 'app/build/x.js',
  'x.min.js', 'app/x.min.js', 'x.js.map', 'x.d.ts.map', 'x.generated.ts',
  'app/__generated__/g.ts', 'src/x.GENERATED.ts',
  'docs/guide.md', 'guide.MD', 'a/readme.rst', 'x.txt', 'src/docs/inner.md', 'app/docs/x.mdx',
  'README.md', 'README', 'docs/README.ru.md', 'src/ReadMe.txt', 'READMEFILE.mdx',
  'app/server/api.ts', 'server/api.ts', 'shared/foo.ts', 'app/shared/foo.tsx',
  'client/x.jsx', 'app/client/x.mjs', 'app/server/dir/deep/util.cjs',
  'APP/SERVER/API.TS', 'appserver/x.ts', 'app/serverless/x.ts',
  'src/util.ts', 'deep/dir/x.vue', 'x.svelte', 'x.JS',
  'config.yml', 'x.yaml', 'x.json', 'Cargo.toml', 'x.sh', 'x.sql', '.env.example',
  'x.env.example', 'x.yml.bak',
  'LICENSE', 'assets/logo.png', '.gitignore', 'Makefile', 'Dockerfile', '',
];
for (const [i, p] of PATHS.entries()) checkClassify(`#${i} ${p}`, p);
for (const [i, p] of PATHS.entries()) {
  compare(`isCodePath #${i} ${p}`, runJs(R.isCodePath, p).value, R.isCodePath(p));
}

// ---------------------------------------------------------------------------
// Корпус 2: extractFileDiffSections
// ---------------------------------------------------------------------------

checkSections('rename-only', 'diff --git a/app/shared/old.ts b/app/shared/new.ts\nsimilarity index 100%\nrename from app/shared/old.ts\nrename to app/shared/new.ts\n');
checkSections('quoted spaces', 'diff --git "a/Price copy/f.ts" "b/Price copy/f.ts"\n--- "a/Price copy/f.ts"\n+++ "b/Price copy/f.ts"\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/plain.ts b/plain.ts\n--- a/plain.ts\n+++ b/plain.ts\n@@ -1 +1 @@\n-a\n+b');
checkSections('duplicate header', 'diff --git a/x.ts b/x.ts\n+one\ndiff --git a/y.ts b/y.ts\n+two\ndiff --git a/x.ts b/x.ts\n+three');
checkSections('no git header', '@@ -1 +1 @@\n+x\n');
checkSections('unparseable header', 'diff --git\n+lost\ndiff --git a/ok.ts b/ok.ts\n+kept');
checkSections('empty', '');
checkSections('trailing newline', 'diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -1 +1 @@\n+x\n');

// ---------------------------------------------------------------------------
// Корпус 3: isRenameOnlySection
// ---------------------------------------------------------------------------

checkRename('rename from', 'rename from a\nrename to b');
checkRename('similarity 100', 'similarity index 100%\nrename from a\nrename to b');
checkRename('similarity 100.000', 'similarity index 100.0000%\nrename from a');
checkRename('similarity 90 + rename from', 'similarity index 90%\nrename from a\nrename to b');
checkRename('similarity 99', 'similarity index 99%\nrename from a\nrename to b');
checkRename('content plus', '@@ -1 +1 @@\n-old\n+new');
// RFC 17: rename с хунками — не rename-only
checkRename('rename + hunks', 'similarity index 88%\nrename from a.ts\nrename to b.ts\n--- a/a.ts\n+++ b/b.ts\n@@ -1 +1 @@\n-return check(u);\n+return true;');
checkRename('plus only', '+added');
checkRename('minus only', '-removed');
checkRename('headers only', '--- a/x\n+++ b/x\n@@ -0,0 +1 @@');
checkRename('null', null);
checkRename('empty', '');
checkRename('similarity 1000', 'similarity index 1000%');

// ---------------------------------------------------------------------------
// Корпус 4: routeDiff на fixtures + литеральные кейсы
// ---------------------------------------------------------------------------

const fixtureDirs = readdirSync(resolve(REPO, 'fixtures'), { withFileTypes: true })
  .filter((d) => d.isDirectory())
  .map((d) => d.name);

let fixtureDiffs = 0;
for (const dir of fixtureDirs) {
  let diff;
  try {
    diff = readFileSync(resolve(REPO, 'fixtures', dir, 'diff.patch'), 'utf8');
  } catch { continue; }
  fixtureDiffs += 1;
  const sections = R.extractFileDiffSections(diff);
  const paths = [...sections.keys()];
  checkSections(`fixture ${dir}`, diff);
  checkRoute(`fixture ${dir} (paths from diff)`, paths, diff);
  checkRoute(`fixture ${dir} (+extra paths)`, [...paths, 'package-lock.json', 'docs/extra.md'], diff);
  checkRoute(`fixture ${dir} (router off)`, paths, diff, { enabled: false });
}

// литеральные кейсы из test/pr-review.eval.test.mjs
checkRoute('lockfile-only', ['package-lock.json'], 'diff --git a/package-lock.json b/package-lock.json\n--- a/package-lock.json\n+++ b/package-lock.json\n@@ -1 +1 @@\n+  "version": "2"');
checkRoute('docs-only', ['docs/guide.md'], 'diff --git a/docs/guide.md b/docs/guide.md\n--- a/docs/guide.md\n+++ b/docs/guide.md\n@@ -1 +1 @@\n+# Title');
checkRoute('server logic', ['app/server/api.ts'], 'diff --git a/app/server/api.ts b/app/server/api.ts\n--- a/app/server/api.ts\n+++ b/app/server/api.ts\n@@ -1 +1 @@\n+export const x = 1;');
checkRoute('shared logic', ['shared/foo.ts'], 'diff --git a/shared/foo.ts b/shared/foo.ts\n--- a/shared/foo.ts\n+++ b/shared/foo.ts\n@@ -1 +1 @@\n+export const x = 1;');
checkRoute('docs + server', ['docs/a.md', 'app/server/x.ts'], 'diff');
checkRoute('rename-only → skip', ['app/shared/new.ts'], 'diff --git a/app/shared/old.ts b/app/shared/new.ts\nsimilarity index 100%\nrename from app/shared/old.ts\nrename to app/shared/new.ts\n');
checkRoute('router off', ['package-lock.json'], '', { enabled: false });
checkRoute('empty paths', [], 'diff --git a/x.ts b/x.ts');
checkRoute('no opts', ['package-lock.json'], 'diff');
checkRoute('config+docs → light', ['config.yml', 'docs/a.md'], 'diff');
checkRoute('other only → light', ['LICENSE'], 'diff');
checkRoute('logic + skip', ['package-lock.json', 'server/x.ts'], 'diff');
checkRoute('rename + logic', ['app/shared/new.ts', 'client/x.ts'], 'diff --git a/app/shared/old.ts b/app/shared/new.ts\nsimilarity index 100%\nrename from app/shared/old.ts\nrename to app/shared/new.ts\n');
checkRoute('generated + docs', ['dist/bundle.js', 'docs/a.md'], 'diff');
// skipPaths (layout.reviewSkip профиля): путь из списка — класс skip на обеих сторонах
const OPENAPI = 'docs/08-technical/openapi/orakul-api.yaml';
checkRoute('skipPaths: generated yaml + logic', [OPENAPI, 'app/server/x.ts'], 'diff', { skipPaths: [OPENAPI] });
checkRoute('skipPaths: only profile-skip logic', ['app/client/api/client.ts'], 'diff', { skipPaths: ['app/client/api/client.ts'] });
checkRoute('skipPaths: profile-skip + docs', ['app/server/apiDocs/openapi.json', 'docs/a.md'], 'diff', { skipPaths: ['app/server/apiDocs/openapi.json'] });
checkRoute('skipPaths: not in changedPaths', ['app/server/x.ts'], 'diff', { skipPaths: ['other.json'] });
checkRoute('skipPaths: empty', ['config.yml'], 'diff', { skipPaths: [] });
checkRoute('skipPaths: router off', ['app/server/x.ts'], 'diff', { enabled: false, skipPaths: ['app/server/x.ts'] });

// ---------------------------------------------------------------------------
// Итог
// ---------------------------------------------------------------------------

console.log(`fixtures with diff.patch: ${fixtureDiffs}`);
console.log(`parity cases compared: ${compared}`);
if (failures.length) {
  console.log(`FAILURES: ${failures.length}`);
  for (const f of failures.slice(0, 20)) {
    console.log('---', f.label);
    console.log('  js  :', f.js);
    console.log('  rust:', f.rust);
  }
  process.exit(1);
}
console.log('PARITY OK');
