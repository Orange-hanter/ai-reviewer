/**
 * Источник входа движка, который пишет не автор PR (граница доверия, docs/profiles.md «Доверие»).
 *
 * В потребителе рабочее дерево — checkout PR: профиль, repoRulesFile, AGENTS.md, fpPatterns,
 * sgconfig ast-grep и его каталоги правил там пишет автор PR. `REVIEW_PROFILE_TRUSTED=1`
 * обещает, что профиль взят из base-ветки; чтобы это обещание не зависело от того, что CI
 * восстановил КАЖДЫЙ файл, движок при доверии сам читает такие файлы из `REVIEW_BASE_REF`
 * через git (`git cat-file blob <sha>:./<path>`), а не с диска.
 *
 *   - доверие без `REVIEW_BASE_REF`, с невалидным ref или ref, который git не резолвит в
 *     коммит, СНИМАЕТСЯ (`trusted: false`, `reason`): fail-closed, reviewSkip не применяется;
 *   - файла нет в base (или это каталог) — он отсутствует (ENOENT), копия из рабочего дерева
 *     не подставляется;
 *   - путь вне cwd или внутри пакета движка (`.ai-reviewer-engine/` в checkout'е потребителя,
 *     стартовые fp-patterns и ruleset пакета) читается с диска: его не пишет автор PR.
 *
 * Синглтон процесса, как активный профиль (lib/profile.mjs): его ставит loadReviewConfig, а
 * читают readRepoRules, fp-patterns, collectProfileProtected и ast-grep prescan.
 */

import { readFileSync, mkdtempSync, mkdirSync, writeFileSync, realpathSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, relative, resolve } from 'node:path';
import { PACKAGE_ROOT } from './package-root.mjs';
import { envFlag } from './util.mjs';

/**
 * REVIEW_BASE_REF — только однозначное имя: полный SHA коммита (40/64 hex) или полностью
 * квалифицированный ref `refs/remotes/…` / `refs/heads/…`. Короткое `origin/main` git
 * резолвит по правилам DWIM (тег, ветка, remote с тем же именем) — автор PR, создавший в
 * checkout'е ref с таким именем, подменил бы base. Без `..`, `//`, `@{`, хвостового `/`/`.lock`.
 */
const SHA_RE = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;
const FQ_REF_RE = /^refs\/(?:remotes|heads)\/[A-Za-z0-9_][A-Za-z0-9_./-]*$/;

export function baseRefProblem(ref) {
  if (SHA_RE.test(ref)) return '';
  if (!FQ_REF_RE.test(ref) || ref.includes('..') || ref.includes('//') || /\/$|\.lock$|\/\./.test(ref)) {
    return 'REVIEW_BASE_REF должен быть SHA коммита (40/64 hex) или refs/remotes/… / refs/heads/…';
  }
  return '';
}

function runGit(args, { cwd, encoding = 'utf8' } = {}) {
  return execFileSync('git', args, { cwd, encoding, stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 64 * 1024 * 1024 });
}

function enoent(path, ref) {
  const e = new Error(`ENOENT: ${path} нет в ${ref}`);
  e.code = 'ENOENT';
  return e;
}

function realOr(p) {
  try { return realpathSync(p); } catch { return resolve(p); }
}

function inside(child, parent) {
  const r = relative(parent, child);
  return r === '' || (!r.startsWith('..') && !isAbsolute(r));
}

/** Рабочее дерево: поведение по умолчанию (профиль не помечен доверенным). */
export function worktreeSource({ cwd = process.cwd(), requested = false, reason = '' } = {}) {
  return Object.freeze({
    trusted: false,
    requested,
    reason,
    baseRef: '',
    sha: '',
    cwd,
    isRepoPath: () => false,
    readText: (file) => readFileSync(file, 'utf8'),
  });
}

/**
 * Источник входа по env. `REVIEW_PROFILE_TRUSTED=1` + `REVIEW_BASE_REF` → чтение из base.
 * @param {Record<string,string|undefined>} env
 * @param {{cwd?: string, pkgRoot?: string, git?: typeof runGit}} [opts]
 */
export function resolveInputSource(env = process.env, { cwd = process.cwd(), pkgRoot = PACKAGE_ROOT, git = runGit } = {}) {
  if (!envFlag('REVIEW_PROFILE_TRUSTED', false, env)) return worktreeSource({ cwd });
  const ref = String(env.REVIEW_BASE_REF || '').trim();
  if (!ref) return worktreeSource({ cwd, requested: true, reason: 'REVIEW_BASE_REF не задан' });
  const problem = baseRefProblem(ref);
  if (problem) {
    return worktreeSource({ cwd, requested: true, reason: `REVIEW_BASE_REF невалиден (${ref.slice(0, 80)}): ${problem}` });
  }
  let sha = '';
  try {
    // Только после проверки формы: `--end-of-options` — ref не прочтётся как опция.
    sha = git(['rev-parse', '--verify', '--quiet', '--end-of-options', `${ref}^{commit}`], { cwd }).trim();
  } catch { sha = ''; }
  if (!/^[0-9a-f]{40,64}$/.test(sha)) {
    return worktreeSource({ cwd, requested: true, reason: `REVIEW_BASE_REF ${ref} не резолвится в коммит` });
  }
  const absCwd = resolve(cwd);
  // Оба написания корня: process.cwd() отдаёт realpath (/private/var/… на macOS), а пути
  // профиля строятся от того, что передали (/var/…). Иначе файл репозитория сочли бы внешним.
  const forms = (p) => [...new Set([resolve(p), realOr(p)])];
  const cwdForms = forms(cwd);
  const pkgForms = forms(pkgRoot);
  const cwdRootOf = (abs) => cwdForms.find((r) => inside(abs, r)) ?? null;
  // Путь, который пишет автор PR: внутри cwd и не внутри пакета движка.
  const isRepoPath = (file) => {
    const abs = resolve(absCwd, file);
    return cwdRootOf(abs) !== null && !pkgForms.some((r) => inside(abs, r));
  };
  const relSpec = (abs) => relative(cwdRootOf(abs) ?? absCwd, abs).split('\\').join('/');
  return Object.freeze({
    trusted: true,
    requested: true,
    reason: '',
    baseRef: ref,
    sha,
    cwd: absCwd,
    isRepoPath,
    /** Текст файла из base; нет в base (или каталог) — ENOENT, как readFileSync. */
    readText(file) {
      const abs = resolve(absCwd, file);
      if (!isRepoPath(abs)) return readFileSync(abs, 'utf8');
      const rel = relSpec(abs);
      if (!rel) throw enoent(abs, ref);
      try {
        return git(['cat-file', 'blob', `${sha}:./${rel}`], { cwd: absCwd });
      } catch {
        throw enoent(rel, ref);
      }
    },
    /** Blob'ы base под каталогом/файлом: [{path (от cwd), mode}]; симлинки и сабмодули — мимо. */
    listTree(file) {
      const abs = resolve(absCwd, file);
      if (!isRepoPath(abs)) return [];
      const rel = relSpec(abs);
      let out = '';
      try {
        out = git(['ls-tree', '-r', '-z', sha, '--', rel ? `./${rel}` : '.'], { cwd: absCwd });
      } catch { return []; }
      const items = [];
      for (const rec of out.split('\0')) {
        const m = rec.match(/^(\d+) (\w+) [0-9a-f]+\t(.+)$/s);
        if (!m || m[2] !== 'blob' || m[1] === '120000') continue;
        items.push({ path: m[3], mode: m[1] });
      }
      return items;
    },
  });
}

let active = worktreeSource();

/** Активный источник входа прогона (ставит loadReviewConfig). */
export function getInputSource() {
  return active;
}

export function setInputSource(source) {
  active = source || worktreeSource();
  return active;
}

/** Текст файла через активный источник (base при доверии, иначе диск). */
export function readInputText(file) {
  return active.readText(file);
}

/**
 * Копия поддеревьев base в свежий временный каталог с той же раскладкой от cwd — для
 * инструментов, которые читают файлы сами (ast-grep: sgconfig + ruleDirs/utilDirs).
 * @param {object} source  trusted-источник (resolveInputSource)
 * @param {string[]} relPaths  пути от cwd (файлы или каталоги); вне репозитория — пропуск
 * @returns {{root: string, files: number}}
 */
export function materializeFromBase(source, relPaths) {
  const root = mkdtempSync(join(tmpdir(), 'ai-review-base-'));
  let files = 0;
  const seen = new Set();
  for (const p of relPaths) {
    for (const { path } of source.listTree(p)) {
      if (seen.has(path) || path.startsWith('..') || isAbsolute(path)) continue;
      seen.add(path);
      let text;
      try { text = source.readText(resolve(source.cwd, path)); } catch { continue; }
      const dest = join(root, path);
      if (!inside(dest, root)) continue;
      mkdirSync(dirname(dest), { recursive: true });
      writeFileSync(dest, text);
      files += 1;
    }
  }
  return { root, files };
}
