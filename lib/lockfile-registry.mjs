/**
 * Детерминированная проверка источников пакетов в lockfile (не LLM).
 *
 * Проверка ДОБАВОЧНАЯ: lockfile по-прежнему уходят моделям (встроенный skip их не трогает,
 * lib/router.mjs#isBuiltInSkipPath), и пробел в разборе ничего не прячет. Модель в
 * десятках тысяч строк хэшей смену источника легко пропустит, а lockfile — это то, ОТКУДА
 * ставятся пакеты: правка источника на чужой хост подменяет зависимость, не трогая
 * package.json. Поэтому КАЖДОЕ значение в добавленных строках lockfile (не только ключ
 * `resolved`: ключи yarn, `tarball`, `repo`, любые поля) разбирается так, как его прочтёт
 * менеджер пакетов, и источник вне allowlist даёт находку major в форме prescan
 * (prescan: true, prescanBlock: true) — она идёт в гейт как находка eslint/ast-grep.
 *
 * Декодирование — как у менеджера: строка в двойных кавычках — JSON-литерал (package-lock —
 * JSON; у yarn/pnpm двойные кавычки — те же escape'ы), в одинарных — YAML (`''` → `'`), иначе
 * plain-токен. Хост берётся из `new URL()` уже декодированного значения.
 *
 * Fail-closed (находка «источник не разобран однозначно»): обратный слэш в значении, `%`
 * в источнике, userinfo (`@`) в authority, порт, URL, который не парсится, кавычка без пары.
 *
 * Классы источников:
 *   - `scheme://…` — разрешён только `https:` на хосте из allowlist без userinfo/порта;
 *     `http:` — «нешифрованный», прочие схемы (git+ssh, git+https, ssh, file://) — не реестр;
 *   - `git+…`/`git:`, `github:`/`gitlab:`/`bitbucket:`/`gist:`, scp-подобный `git@host:path`,
 *     `exec:`, `patch:` — всегда находка (не реестр);
 *   - `file:`/`link:`/`portal:` — относительный путь внутри репозитория (от каталога lockfile)
 *     допустим; абсолютный, `~`, выход выше корня — находка.
 *
 * Allowlist: registry.npmjs.org, registry.yarnpkg.com и хосты из env
 * `REVIEW_LOCKFILE_REGISTRIES` (через запятую; `host`, `https://host/path` — берётся host).
 * Профилем список не расширяется: профиль недоверенного прогона пишет автор PR.
 */

import { posix } from 'node:path';
import { extractFileDiffSections } from './router.mjs';

export const DEFAULT_LOCKFILE_REGISTRIES = Object.freeze(['registry.npmjs.org', 'registry.yarnpkg.com']);
export const LOCKFILE_REGISTRY_RULE = 'lockfile-foreign-registry';
const CHECKED = /^(?:package-lock\.json|yarn\.lock|pnpm-lock\.yaml)$/i;
const MAX_FINDINGS = 20;

// Токен строки lockfile: "…" (JSON-escape'ы), '…' (YAML, '' внутри), plain до разделителя.
const TOKEN_RE = /"((?:[^"\\]|\\.)*)"|'((?:[^']|'')*)'|([^\s"',{}[\]]+)/g;
// Все шаблоны спецификации якорены к началу: значение — `spec` или `name@spec`
// (`left-pad@https://…`, `@s/x@github:…`); URL в любом другом месте — неоднозначность.
const NAME_AT_RE = /^(?:@[a-z0-9._~-]+\/)?[a-z0-9._~-]+@(.+)$/i;
const PROTO_RE = /^(?:git\+[a-z0-9+.-]*|git|github|gitlab|bitbucket|gist|file|link|portal|exec|patch|https?|ssh):/i;
const URL_START_RE = /^[a-z][a-z0-9+.-]*:\/\//i;
const URL_ANYWHERE_RE = /[a-z][a-z0-9+.-]*:\/\//i;
// scp-подобный git: `git@github.com:org/repo` (npm/yarn ставят как git-зависимость).
const SCP_RE = /^[\w.-]+@[a-z0-9-]+(?:\.[a-z0-9-]+)+:(?=[^\s\d])/i;
// eslint-disable-next-line no-control-regex
const CONTROL_RE = /[\x00-\x1f\x7f]/;
const SPEC_HINT_RE = /:\/\/|(?:^|[\s"'@,{[])(?:git\+|git:|github:|gitlab:|bitbucket:|gist:|file:|link:|portal:|exec:|patch:|https?:|ssh:)/i;

/** `'a.com, https://b.org/x'` → ['a.com', 'b.org']. */
export function parseLockfileRegistries(raw) {
  return String(raw || '').split(',').map((s) => s.trim()).filter(Boolean).map((s) => {
    try { return new URL(s.includes('://') ? s : `https://${s}`).hostname.toLowerCase(); } catch { return ''; }
  }).filter(Boolean);
}

/** Значения строки, декодированные как их прочтёт менеджер пакетов; leftover — непарная кавычка. */
export function tokenizeLockfileLine(line) {
  const values = [];
  let consumed = '';
  TOKEN_RE.lastIndex = 0;
  for (const m of line.matchAll(TOKEN_RE)) {
    consumed += m[0];
    if (m[1] !== undefined) {
      let value = null;
      try { value = JSON.parse(`"${m[1]}"`); } catch { value = null; }
      values.push({ raw: m[1], value, escaped: m[1].includes('\\') });
    } else if (m[2] !== undefined) {
      values.push({ raw: m[2], value: m[2].replace(/''/g, "'"), escaped: m[2].includes('\\') });
    } else {
      values.push({ raw: m[3], value: m[3].replace(/:$/, ''), escaped: m[3].includes('\\') });
    }
  }
  const rest = line.replace(TOKEN_RE, '');
  return { values, unpaired: /["']/.test(rest) };
}

/**
 * Класс одного значения: null — не источник или разрешённый источник; иначе {kind, detail}.
 * kind: ambiguous | foreign | http | scheme | git | local.
 */
export function classifyLockfileValue(v, { allowed, lockDir = '.' } = {}) {
  const s = String(v ?? '');
  if (CONTROL_RE.test(s)) return { kind: 'ambiguous', detail: 'управляющий символ в значении' };
  // Спецификация читается с начала значения или сразу после `name@` / `@scope/name@`
  // (ключи yarn, алиасы). SCP и протоколы — до URL: `git@host:x#https://registry…` — git.
  const named = s.match(NAME_AT_RE);
  for (const spec of named ? [s, named[1]] : [s]) {
    const c = classifySpecStart(spec, { allowed, lockDir });
    if (c !== undefined) return c;
  }
  // URL где-то в середине значения: менеджер и этот разбор могут прочесть его по-разному.
  if (URL_ANYWHERE_RE.test(s)) return { kind: 'ambiguous', detail: 'URL не в начале значения' };
  return null;
}

/** Класс спецификации, начинающейся с начала строки; undefined — это не спецификация. */
function classifySpecStart(s, { allowed, lockDir }) {
  if (SCP_RE.test(s)) return { kind: 'git', detail: 'git@host:path' };
  const proto = PROTO_RE.test(s) && !URL_START_RE.test(s) ? s : null;
  if (URL_START_RE.test(s)) {
    const url = s;
    if (url.includes('%')) return { kind: 'ambiguous', detail: '%-escape в URL' };
    const authority = url.replace(/^[a-z][a-z0-9+.-]*:\/\//i, '').split(/[/?#]/)[0];
    if (authority.includes('@')) return { kind: 'ambiguous', detail: 'userinfo (@) в адресе' };
    let u;
    try { u = new URL(url); } catch { return { kind: 'ambiguous', detail: 'URL не парсится' }; }
    const host = u.hostname.toLowerCase().replace(/^\[|\]$/g, '');
    if (!host) return { kind: 'ambiguous', detail: 'URL без хоста' };
    if (u.port) return { kind: 'ambiguous', detail: `порт в адресе — ${host}:${u.port}` };
    if (u.protocol === 'https:') return allowed.has(host) ? null : { kind: 'foreign', detail: host };
    if (u.protocol === 'http:') return allowed.has(host) ? { kind: 'http', detail: host } : { kind: 'foreign', detail: host };
    return { kind: 'scheme', detail: `${u.protocol}//${host}` };
  }
  if (proto) {
    const spec = proto;
    const p = spec.slice(0, spec.indexOf(':')).toLowerCase();
    if (p === 'https' || p === 'http' || p === 'ssh') return { kind: 'ambiguous', detail: `${p}: без //` };
    if (p === 'file' || p === 'link' || p === 'portal') {
      const path = spec.slice(p.length + 1);
      if (!path || path.includes('%')) return { kind: 'ambiguous', detail: `${p}: ${path || '(пусто)'}` };
      if (path.startsWith('/') || path.startsWith('~')) return { kind: 'local', detail: `${p}:${path}` };
      const norm = posix.normalize(posix.join(lockDir, path));
      return norm === '..' || norm.startsWith('../') ? { kind: 'local', detail: `${p}:${path}` } : null;
    }
    return { kind: 'git', detail: `${p}:` };
  }
  return undefined;
}

const TITLES = {
  foreign: (d) => `lockfile: пакет ставится не из разрешённого реестра — ${d}`,
  http: (d) => `lockfile: пакет ставится по нешифрованному http — ${d}`,
  scheme: (d) => `lockfile: пакет ставится не из реестра по https — ${d}`,
  git: (d) => `lockfile: пакет ставится из git/хостинга кода — ${d}`,
  local: (d) => `lockfile: локальный источник пакета вне репозитория — ${d}`,
  ambiguous: (d) => `lockfile: источник пакета не разобран однозначно — ${d}`,
};

/**
 * Находки по добавленным строкам npm/yarn/pnpm lockfile в unified diff.
 * @param {string} diff
 * @param {{ registries?: string[], sections?: Map<string,string> }} [opts]
 */
export function scanLockfileRegistries(diff, { registries = [], sections = null } = {}) {
  const allowed = new Set([...DEFAULT_LOCKFILE_REGISTRIES, ...registries.map((h) => String(h).toLowerCase())]);
  const findings = [];
  const seen = new Set();
  const push = (path, line, kind, detail, sample) => {
    const key = `${path}\0${kind}\0${detail}`;
    if (seen.has(key)) return false;
    seen.add(key);
    findings.push({
      file: path,
      line,
      severity: 'major',
      confidence: 1,
      title: TITLES[kind](detail),
      body: `Добавленная строка lockfile: \`${sample.replace(/`/g, "'").slice(0, 200)}\`. Разрешены https-источники `
        + `на ${[...allowed].join(', ')} (дополнить — env REVIEW_LOCKFILE_REGISTRIES). Lockfile не ревьюится `
        + 'моделями, смена источника в нём подменяет зависимость без правки package.json: проверьте, '
        + 'откуда и зачем этот пакет, или пересоберите lockfile из официального реестра.',
      model: 'lockfile-registry',
      prescan: true,
      ruleId: LOCKFILE_REGISTRY_RULE,
      prescanBlock: true,
    });
    return findings.length >= MAX_FINDINGS;
  };
  for (const [path, section] of sections || extractFileDiffSections(diff || '')) {
    if (!CHECKED.test(String(path).split('/').pop() || '')) continue;
    const lockDir = posix.dirname(String(path));
    const isNpm = /package-lock\.json$/i.test(String(path));
    let line = 0;
    let funding = 0; // глубина скобок внутри `"funding": [ … ]` package-lock (ссылки на донаты)
    for (const raw of String(section).split('\n')) {
      const hunk = raw.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
      if (hunk) { line = Number(hunk[1]); funding = 0; continue; }
      if (!line || raw.startsWith('-') || raw.startsWith('\\')) continue;
      const added = raw.startsWith('+');
      const cur = line;
      line += 1;
      const text = raw.slice(1);
      // package-lock: `funding`/`url` — ссылки на страницы донатов, npm по ним ничего не ставит.
      // Только они: источник в package-lock — значение ключа (`resolved`, `version` v1,
      // спецификации в `requires`/`dependencies`), а не элемент массива.
      // Пропускается только строка ровно такой формы (одна пара на строку, как пишет npm):
      // компактный JSON с `"url": …, "resolved": …` в одной строке проверяется целиком.
      if (isNpm) {
        const STR = '"(?:[^"\\\\]|\\\\.)*"';
        const fundingOpen = new RegExp(`^\\s*"funding"\\s*:\\s*([[{])\\s*$`);
        // `url`/`type` пропускаются ТОЛЬКО внутри отслеживаемой записи funding (inside ниже).
        const fundingOne = new RegExp(`^\\s*"funding"\\s*:\\s*${STR}\\s*,?\\s*$`);
        const inside = new RegExp(`^\\s*(?:[[{]|[\\]}],?|"(?:type|url)"\\s*:\\s*${STR}\\s*,?|${STR}\\s*,?)\\s*$`);
        if (funding > 0) {
          if (inside.test(text)) {
            if (/^\s*[[{]\s*$/.test(text)) funding += 1;
            else if (/^\s*[\]}],?\s*$/.test(text)) funding -= 1;
            continue;
          }
          funding = 0; // не похоже на запись funding — режим снят, строка проверяется
        } else if (fundingOpen.test(text)) {
          funding = 1;
          continue;
        } else if (fundingOne.test(text)) {
          continue;
        }
      }
      if (!added) continue;
      const { values, unpaired } = tokenizeLockfileLine(text);
      if (unpaired && SPEC_HINT_RE.test(text)) {
        if (push(path, cur, 'ambiguous', 'кавычка без пары', text)) return findings;
        continue;
      }
      for (const t of values) {
        // Обратный слэш в значении-источнике: менеджер и этот разбор могут прочесть его
        // по-разному — fail-closed, даже если декодированный хост разрешён.
        if (t.escaped && /[:/@]/.test(t.raw)) {
          if (push(path, cur, 'ambiguous', 'обратный слэш в значении', text)) return findings;
          continue;
        }
        // Строка в кавычках, которую JSON не декодирует, и управляющие символы — не
        // угадываем, как их прочтёт менеджер: fail-closed.
        if (t.value === null) {
          if (push(path, cur, 'ambiguous', 'строка не декодируется', text)) return findings;
          continue;
        }
        if (CONTROL_RE.test(t.raw) || CONTROL_RE.test(t.value)) {
          if (push(path, cur, 'ambiguous', 'управляющий символ в значении', text)) return findings;
          continue;
        }
        const c = classifyLockfileValue(t.value, { allowed, lockDir });
        if (c && push(path, cur, c.kind, c.detail, text)) return findings;
      }
    }
  }
  return findings;
}
