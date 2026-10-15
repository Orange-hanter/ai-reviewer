/**
 * Где искать Rust-бинари по умолчанию (RFC 20, доставка потребителю).
 *
 * Каталоги по порядку, побеждает первый, где бинарь есть:
 *   1. AI_REVIEWER_RUST_BIN_DIR — явный каталог (override целиком);
 *   2. rust/target/debug — сборка разработчика и CI движка (`cargo build`);
 *   3. rust/target/release — `cargo build --release`;
 *   4. rust/bin — готовые бинари из релиза: шаблон CI потребителя скачивает архив
 *      `ai-reviewer-bins-<target>.tar.gz` по тому же тегу, что и движок, и распаковывает сюда.
 *
 * Собранный локально бинарь стоит раньше скачанного: у разработчика `rust/bin` появляется
 * только руками, и тогда свежая сборка не должна прятаться за старым архивом.
 *
 * Ничего не нашлось — возвращаем путь в rust/target/debug: обёртки модулей проверяют
 * existsSync сами и уходят на deprecated JS-копию, а сообщения parity-скриптов и тестов
 * («нет rust/target/debug/<mod>») остаются прежними.
 *
 * Переменные AI_REVIEWER_<MOD>_BIN и AI_REVIEWER_<MOD>_FORCE_JS по-прежнему читает каждая
 * обёртка — они главнее этого поиска.
 */
import { existsSync } from 'node:fs';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const RUST_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '../rust');

/** Каталоги поиска в порядке приоритета (env читается на каждый вызов). */
export function rustBinDirs(env = process.env) {
  const dirs = [];
  const override = String(env.AI_REVIEWER_RUST_BIN_DIR || '').trim();
  if (override) dirs.push(resolve(override));
  dirs.push(join(RUST_DIR, 'target/debug'), join(RUST_DIR, 'target/release'), join(RUST_DIR, 'bin'));
  return dirs;
}

/** Путь к бинарю `name` по умолчанию: первый существующий, иначе путь в rust/target/debug. */
export function defaultRustBin(name, env = process.env) {
  for (const dir of rustBinDirs(env)) {
    const p = join(dir, name);
    if (existsSync(p)) return p;
  }
  return join(RUST_DIR, 'target/debug', name);
}
