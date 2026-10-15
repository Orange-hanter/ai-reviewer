/**
 * Поиск Rust-бинарей (lib/rust-bin.mjs, RFC 20): потребитель получает бинари архивом релиза
 * в rust/bin, разработчик — сборкой в rust/target. Порядок решает, какой из них победит.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { defaultRustBin, rustBinDirs } from '../lib/rust-bin.mjs';

const ROOT = fileURLToPath(new URL('..', import.meta.url));

test('rust-bin: порядок каталогов — override, debug, release, архив релиза', () => {
  const dirs = rustBinDirs({ AI_REVIEWER_RUST_BIN_DIR: '/opt/bins' });
  assert.equal(dirs[0], '/opt/bins');
  assert.deepEqual(dirs.slice(1).map((d) => d.slice(ROOT.length)), ['rust/target/debug', 'rust/target/release', 'rust/bin']);
  assert.equal(rustBinDirs({}).length, 3, 'пустой override не добавляет каталог');
});

test('rust-bin: override-каталог побеждает, отсутствие бинаря даёт путь в target/debug', () => {
  const dir = mkdtempSync(join(tmpdir(), 'rust-bin-'));
  writeFileSync(join(dir, 'router-test-only'), '');
  assert.equal(defaultRustBin('router-test-only', { AI_REVIEWER_RUST_BIN_DIR: dir }), join(dir, 'router-test-only'));
  assert.equal(defaultRustBin('no-such-bin', { AI_REVIEWER_RUST_BIN_DIR: dir }), join(ROOT, 'rust/target/debug/no-such-bin'));
});

test('rust-bin: ни одна обёртка не держит путь к бинарю мимо defaultRustBin', () => {
  // Обёртка с захардкоженным rust/target/debug не увидит архив релиза: у потребителя она
  // молча уйдёт на deprecated JS-копию, и ни один тест машины с cargo этого не заметит.
  for (const f of readdirSync(join(ROOT, 'lib')).filter((n) => n.endsWith('.mjs') && n !== 'rust-bin.mjs')) {
    const code = readFileSync(join(ROOT, 'lib', f), 'utf8').split('\n')
      .filter((l) => !/^\s*(\*|\/\/)/.test(l)).join('\n');
    assert.ok(!/rust\/target\/(debug|release)\//.test(code), `lib/${f} ищет бинарь мимо defaultRustBin`);
  }
});
