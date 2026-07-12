import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

/** Корень npm-пакета @orakul/ai-reviewer (пути по умолчанию для ast-grep, feedback, fixtures). */
export const PACKAGE_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

export function pkgPath(...segments) {
  return join(PACKAGE_ROOT, ...segments);
}
