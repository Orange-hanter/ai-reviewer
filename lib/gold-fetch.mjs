/**
 * Доставка диффов публичных gold-кейсов по пину (RFC 03).
 *
 * Чужие диффы НЕ лежат в репозитории: кейс из `gold-public/` несёт только `meta.json` с
 * пином `source.{repo,pr,sha}`, а сам патч тянется по `sha` в gitignored-кэш. Причины —
 * лицензии чужих репозиториев и вес монорепо-диффов; воспроизводимость держится на том,
 * что пин указывает на неизменяемый коммит, а не на «текущее состояние PR».
 *
 * Неявного «взять свежее» в контракте нет: нет `sha` → ошибка, а не догадка.
 */
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdir, readFile, writeFile, access } from 'node:fs/promises';
import { join } from 'node:path';

const runFile = promisify(execFile);

const API = 'https://api.github.com';
/** Пустой diff у смерженного коммита — почти наверняка сбой доставки, а не «пустой PR». */
const MIN_DIFF_BYTES = 16;

let tokenPromise;

/** Токен GitHub: env → `gh auth token`. Без токена работаем анонимно (хватает на публичные диффы). */
export async function resolveGithubToken(env = process.env) {
  if (env.GITHUB_TOKEN) return env.GITHUB_TOKEN;
  if (env.GH_TOKEN) return env.GH_TOKEN;
  if (!tokenPromise) {
    tokenPromise = runFile('gh', ['auth', 'token'])
      .then(({ stdout }) => stdout.trim() || null)
      .catch(() => null);
  }
  return tokenPromise;
}

async function githubRequest(path, { accept = 'application/vnd.github+json', env = process.env } = {}) {
  const token = await resolveGithubToken(env);
  const res = await fetch(`${API}${path}`, {
    headers: {
      Accept: accept,
      'User-Agent': 'ai-reviewer-benchmark',
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
  });
  if (!res.ok) {
    const hint = res.status === 403 ? ' (лимит запросов? задайте GITHUB_TOKEN)' : '';
    throw new Error(`GitHub ${res.status} на ${path}${hint}`);
  }
  return accept.includes('diff') ? res.text() : res.json();
}

/** JSON-запрос к GitHub API — общий вход для харвестера и доставки диффов. */
export function githubJson(path, opts = {}) {
  return githubRequest(path, opts);
}

/** Дифф коммита (для merge/squash-коммита PR это ровно дифф самого PR). */
export function githubCommitDiff(repo, sha, opts = {}) {
  return githubRequest(`/repos/${repo}/commits/${sha}`, {
    ...opts,
    accept: 'application/vnd.github.v3.diff',
  });
}

/**
 * Локальные клоны своих репозиториев: `REVIEW_GOLD_REPOS="orakul=/path/Orakul,restate=/path/Restate"`.
 * Кейс из `gold-private/` несёт `source.local` (имя) вместо `source.repo` — код закрытых проектов
 * в репозиторий ai-reviewer не попадает, дифф строится из клона в gitignored-кэш.
 */
export function goldRepoPaths(env = process.env) {
  const out = new Map();
  for (const part of String(env.REVIEW_GOLD_REPOS || '').split(',')) {
    const i = part.indexOf('=');
    if (i > 0) out.set(part.slice(0, i).trim(), part.slice(i + 1).trim());
  }
  return out;
}

/** Путь к локальному клону кейса или ошибка с подсказкой, какую переменную задать. */
export function localRepoPath(meta, env = process.env) {
  const name = meta?.source?.local;
  const path = goldRepoPaths(env).get(name);
  if (!path) throw new Error(`кейс ${meta?.id}: нет пути к клону «${name}» — задайте REVIEW_GOLD_REPOS="${name}=/путь/к/клону"`);
  return path;
}

/** Дифф единицы истории из локального клона: `base..sha` (первый родитель → коммит PR). */
async function localCommitDiff(meta, env) {
  const repo = localRepoPath(meta, env);
  const { base, sha } = meta.source;
  const { stdout } = await runFile('git', ['-C', repo, 'diff', '--no-color', '--no-renames', base, sha], { maxBuffer: 256 * 1024 * 1024 });
  return stdout;
}

/** Имя файла в кэше содержит пин: смена `sha` в мете сама инвалидирует кэш. */
export function cachedPatchName(meta) {
  const sha = String(meta?.source?.sha || '');
  return `${meta.id}.${sha.slice(0, 12)}.patch`;
}

async function exists(path) {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

/**
 * Путь к диффу кейса: локальный `diff.patch`, если он есть (набор `gold/` — свои PR),
 * иначе скачанный по пину патч в кэше.
 * @param {string} caseDir — каталог кейса
 * @param {object} meta — его meta.json
 * @param {{ cacheDir: string, env?: object }} opts
 * @returns {Promise<{ path: string, source: 'local'|'cache'|'fetched' }>}
 */
export async function ensureCaseDiff(caseDir, meta, { cacheDir, env = process.env } = {}) {
  const local = join(caseDir, 'diff.patch');
  if (await exists(local)) return { path: local, source: 'local' };

  const src = meta?.source || {};
  if ((!src.repo && !src.local) || !src.sha || (src.local && !src.base)) {
    throw new Error(`кейс ${meta?.id}: нет ни diff.patch, ни пина source.{repo|local,sha${src.local ? ',base' : ''}}`);
  }
  const target = join(cacheDir, cachedPatchName(meta));
  if (await exists(target)) return { path: target, source: 'cache' };

  const diff = src.local ? await localCommitDiff(meta, env) : await githubCommitDiff(src.repo, src.sha, { env });
  if (!diff || diff.length < MIN_DIFF_BYTES) {
    throw new Error(`кейс ${meta.id}: пустой дифф по пину ${src.repo || src.local}@${src.sha}`);
  }
  await mkdir(cacheDir, { recursive: true });
  await writeFile(target, diff, 'utf8');
  return { path: target, source: 'fetched' };
}

/** Чтение меты кейса. */
export async function readCaseMeta(caseDir) {
  return JSON.parse(await readFile(join(caseDir, 'meta.json'), 'utf8'));
}
