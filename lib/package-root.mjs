import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

/** Корень пакета ai-reviewer — от него считаются пути ast-grep, feedback и fixtures. */
export const PACKAGE_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

export function pkgPath(...segments) {
  return join(PACKAGE_ROOT, ...segments);
}
