/**
 * Контракт доставки Rust-бинарей потребителю (RFC 20): шаблон examples/ci/pr-review.yml
 * качает архив релиза, а .github/workflows/release-binaries.yml его публикует. Стороны
 * связаны только именами — разойдутся, и потребитель молча останется на JS-копиях.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('..', import.meta.url));
const consumer = readFileSync(`${ROOT}examples/ci/pr-review.yml`, 'utf8');
const release = readFileSync(`${ROOT}.github/workflows/release-binaries.yml`, 'utf8');

/** Текст шага по его имени — до следующего `- name:` того же уровня. */
function step(src, name) {
  const at = src.indexOf(`- name: ${name}`);
  assert.ok(at >= 0, `шаг «${name}» не найден`);
  const next = src.indexOf('\n      - ', at + 1);
  return src.slice(at, next < 0 ? undefined : next);
}

test('шаблон потребителя: бинари качаются по пину движка и до запуска ревью', () => {
  const dl = step(consumer, 'Download engine binaries');
  assert.ok(consumer.indexOf('- name: Checkout pinned AI review engine') < consumer.indexOf('- name: Download engine binaries'));
  assert.ok(consumer.indexOf('- name: Download engine binaries') < consumer.indexOf('- name: Run AI review'));
  assert.match(dl, /REF: \$\{\{ steps\.pin\.outputs\.ref \}\}/, 'тот же ref, что у чекаута движка');
  assert.match(dl, /releases\/download\/\$REF/);
  assert.doesNotMatch(dl.slice(dl.indexOf('run: |')), /\$\{\{/, 'выражения ${{ }} внутри run: — script-injection');
});

test('шаблон потребителя: sha256 сверяется до распаковки, распаковка — в rust/bin движка', () => {
  const dl = step(consumer, 'Download engine binaries');
  const check = dl.indexOf('sha256sum -c');
  const unpack = dl.indexOf('tar -xzf');
  assert.ok(check > 0 && unpack > check, 'распаковка только после сверки sha256');
  assert.match(dl, /-C \.ai-reviewer-engine\/rust\/bin/, 'каталог, который ищет lib/rust-bin.mjs');
});

test('provenance: релиз подписывает архивы, шаблон без проверки подписи их не ставит', () => {
  const dl = step(consumer, 'Download engine binaries');
  const verify = dl.indexOf('gh attestation verify');
  assert.ok(verify > 0 && verify < dl.indexOf('tar -xzf'), 'распаковка только после проверки provenance');
  assert.match(dl, /--signer-workflow Orange-hanter\/ai-reviewer\/\.github\/workflows\/release-binaries\.yml/);
  assert.match(release, /actions\/attest-build-provenance@/);
  assert.match(release, /subject-path: dist\/\*\.tar\.gz/);
  // Право записи — только у джоба без кода репозитория (см. шапку workflow).
  const build = release.slice(release.indexOf('\n  build:'), release.indexOf('\n  upload:'));
  assert.doesNotMatch(build, /contents: write|id-token: write/);
});

test('шаблон потребителя и релиз называют архив одинаково и знают одни платформы', () => {
  assert.match(consumer, /NAME="ai-reviewer-bins-\$T\.tar\.gz"/);
  assert.match(release, /NAME="ai-reviewer-bins-\$\{\{ matrix\.target \}\}\.tar\.gz"/);
  const targets = [...release.matchAll(/target: ([a-z0-9-]+), os:/g)].map((m) => m[1]).sort();
  const known = [...consumer.matchAll(/\) T=([a-z0-9-]+) ;;/g)].map((m) => m[1]).sort();
  assert.deepEqual(known, targets, 'платформы шаблона = матрица релиза');
});

test('релиз пакует каждый бинарь из rust/src/bin и собирает по Cargo.lock', () => {
  assert.match(release, /for src in rust\/src\/bin\/\*\.rs/, 'состав архива = все [[bin]] крейта');
  assert.match(release, /cargo build --release --locked/);
  assert.ok(readdirSync(`${ROOT}rust/src/bin`).includes('llm.rs'));
});
