/**
 * Детерминированная проверка источников пакетов в lockfile (не LLM).
 *
 * npm/yarn/pnpm lockfile не уходят моделям (встроенный skip, lib/router.mjs#isBuiltInSkipPath):
 * десятки тысяч строк хэшей модель всё равно не прочтёт. Но lockfile — это и то, ОТКУДА
 * ставятся пакеты: правка `resolved` на чужой хост подменяет зависимость, не трогая
 * package.json. Поэтому в добавленных строках lockfile ищем URL источника (`resolved`,
 * `tarball`, `resolution`, `repo`) и на хост вне allowlist поднимаем находку major в форме
 * prescan (prescan: true, prescanBlock: true) — она идёт в гейт как находка eslint/ast-grep.
 *
 * Allowlist: registry.npmjs.org, registry.yarnpkg.com и хосты из env
 * `REVIEW_LOCKFILE_REGISTRIES` (через запятую; `host`, `https://host/path` — берётся host).
 * Профилем список не расширяется: профиль недоверенного прогона пишет автор PR.
 * Нешифрованный `http:` — находка и на разрешённом хосте.
 */

import { extractFileDiffSections } from './router.mjs';

export const DEFAULT_LOCKFILE_REGISTRIES = Object.freeze(['registry.npmjs.org', 'registry.yarnpkg.com']);
export const LOCKFILE_REGISTRY_RULE = 'lockfile-foreign-registry';
const CHECKED = /^(?:package-lock\.json|yarn\.lock|pnpm-lock\.yaml)$/i;
// Строка, где lockfile называет источник пакета (npm/yarn v1/yarn berry/pnpm).
const SOURCE_LINE = /(?:^|[\s"'{,])(?:resolved|tarball|resolution|repo)["']?\s*[:\s]/i;
const URL_RE = /\b([a-z][a-z0-9+.-]*):\/\/([^\s"',}\]]+)/gi;
const MAX_FINDINGS = 20;

/** `'a.com, https://b.org/x'` → ['a.com', 'b.org']. */
export function parseLockfileRegistries(raw) {
  return String(raw || '').split(',').map((s) => s.trim()).filter(Boolean).map((s) => {
    try { return new URL(s.includes('://') ? s : `https://${s}`).hostname.toLowerCase(); } catch { return ''; }
  }).filter(Boolean);
}

function hostOf(scheme, rest) {
  try {
    return new URL(`${scheme}://${rest}`).hostname.toLowerCase().replace(/^\[|\]$/g, '');
  } catch {
    // git+ssh://git@host:org/repo — scp-подобный путь URL не парсит
    const m = rest.match(/^(?:[^@/]+@)?([^/:]+)/);
    return m ? m[1].toLowerCase() : '';
  }
}

/**
 * Находки по добавленным строкам npm/yarn/pnpm lockfile в unified diff.
 * @param {string} diff
 * @param {{ registries?: string[], sections?: Map<string,string> }} [opts]
 */
export function scanLockfileRegistries(diff, { registries = [], sections = null } = {}) {
  const allowed = new Set([...DEFAULT_LOCKFILE_REGISTRIES, ...registries.map((h) => String(h).toLowerCase())]);
  const findings = [];
  const seen = new Set();
  for (const [path, section] of sections || extractFileDiffSections(diff || '')) {
    if (!CHECKED.test(String(path).split('/').pop() || '')) continue;
    let line = 0;
    for (const raw of String(section).split('\n')) {
      const hunk = raw.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
      if (hunk) { line = Number(hunk[1]); continue; }
      if (!line) continue;
      if (raw.startsWith('-')) continue;
      if (raw.startsWith('\\')) continue;
      const added = raw.startsWith('+');
      const cur = line;
      line += 1;
      if (!added || !SOURCE_LINE.test(raw)) continue;
      URL_RE.lastIndex = 0;
      for (const m of raw.slice(1).matchAll(URL_RE)) {
        const scheme = m[1].toLowerCase();
        const host = hostOf(scheme, m[2]);
        const foreign = !allowed.has(host);
        const insecure = scheme === 'http';
        if (!host || (!foreign && !insecure)) continue;
        const key = `${path}\0${host}\0${insecure && !foreign}`;
        if (seen.has(key)) continue;
        seen.add(key);
        const url = `${m[1]}://${m[2]}`.slice(0, 200);
        findings.push({
          file: path,
          line: cur,
          severity: 'major',
          confidence: 1,
          title: foreign
            ? `lockfile: пакет ставится не из разрешённого реестра — ${host}`
            : `lockfile: пакет ставится по нешифрованному http — ${host}`,
          body: `Добавленный источник пакета \`${url}\`. Разрешены: ${[...allowed].join(', ')} `
            + '(дополнить — env REVIEW_LOCKFILE_REGISTRIES). Lockfile не ревьюится моделями, '
            + 'смена реестра в нём подменяет зависимость без правки package.json: проверьте, '
            + 'откуда и зачем этот пакет, или пересоберите lockfile из официального реестра.',
          model: 'lockfile-registry',
          prescan: true,
          ruleId: LOCKFILE_REGISTRY_RULE,
          prescanBlock: true,
        });
        if (findings.length >= MAX_FINDINGS) return findings;
      }
    }
  }
  return findings;
}
