/**
 * Фаза 3, шаг 4: lib/github.mjs — асинхронная обёртка над Rust-бинарём
 * rust/target/debug/github (порт транспортного слоя GitHub REST: ghRaw с
 * ретраями/backoff и ghList с Link-пагинацией; заголовки X-GitHub-Api-Version
 * 2022-11-28, Bearer-токен, User-Agent профиля). Второй async/сетевой модуль
 * миграции после lib/llm.mjs: бинарь делает HTTP сам (reqwest blocking),
 * обёртка зовёт его через spawn (execFile+input виснет на Node 22), публичное
 * API (возвращаемый createGithubClient-объект) не изменилось.
 *
 * Резолвинг бинаря (по аналогии с lib/llm.mjs):
 *   AI_REVIEWER_GITHUB_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/github;
 *   AI_REVIEWER_GITHUB_FORCE_JS=1 — принудительно чистый JS (паритетные прогоны/отладка).
 *
 * Дополнительное условие деградации на JS: fetch инжектирован в createGithubClient
 * (DI-моки, test/github.test.mjs) ИЛИ globalThis.fetch перехвачен с момента
 * загрузки модуля (test/pr-review.engine.test.mjs). Rust-бинарь эту подмену не
 * видит — при ней работает эталонный JS, сохраняющий injectable-точку.
 *
 * Граница JS/Rust: в бинарь перенесён ТОЛЬКО транспорт (op `request` = один
 * ghRaw-вызов, op `list` = ghList-склейка страниц). Вся оркестрация осталась
 * в JS — это multi-step-операции, опирающиеся на процессное состояние:
 *   - upsertSticky = list → edit/create (маркеры профиля MARKERS из
 *     lib/constants.mjs, живой профиль через getProfile());
 *   - deleteStaleInline/postReview — post-then-delete порядок и фильтры тредов
 *     (in_reply_to_id, pull_request_review_id);
 *   - readPriorStickyData — разбор sticky-футера (extractStickyFooterRaw) и
 *     ремонт обрезанного JSON через extractJsonObject (lib/json.mjs → Rust
 *     json_repair, переиспользуется как есть);
 *   - listUnresolvedReviewThreads — graphql-пагинация по курсорам.
 * По вызову на op — один spawn; upsert = 2 spawn'а, deleteStaleInline = 2 + N.
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm
 * test/eval идут раньше cargo build, см. .github/workflows/ci.yml), обёртка
 * деградирует на чистый JS fallback — эталонную копию прежней реализации.
 * Если бинарь ЕСТЬ, но упал вне контракта (non-zero exit без JSON-пейлоада) —
 * ошибка пробрасывается как есть: молчаливая подмена реализации при битом
 * Rust-коде скрывала бы регрессии.
 */

import { existsSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { sleep, isTransient, backoffMs, isTrustedReviewAuthor, parseTrustedAuthors } from './util.mjs';
import { MARKERS, hasMarker, extractStickyFooterRaw } from './constants.mjs';
import { extractJsonObject } from './json.mjs';
import { getProfile } from './profile.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_GITHUB_BIN = resolve(__dir, '../rust/target/debug/github');
// Нативный fetch на момент загрузки модуля: перехват = мок/DI → эталонный JS.
/**
 * Свой комментарий бота — маркер в начале тела И автор из allowlist (`cfg.github.trustedAuthors`).
 * Одного маркера мало: его пишет любой, кто может комментировать PR. Поддельный sticky
 * подсовывал бы свой футер в промпт агрегатора («прошлые находки») и Δ-блок, а upsert
 * перезаписывал бы чужой комментарий вместо своего.
 */
export function isOwnBotComment(c, marker, trustedAuthors) {
  return hasMarker(c?.body, marker) && isTrustedReviewAuthor(c?.user?.login, trustedAuthors, c?.user);
}

/** Последний свой sticky; комментарии с маркером от чужих авторов — в лог и мимо. */
function pickOwnSticky(comments, trustedAuthors, log) {
  const marked = (comments || []).filter((c) => hasMarker(c?.body, MARKERS.review));
  const own = [...marked].reverse().find((c) => isOwnBotComment(c, MARKERS.review, trustedAuthors)) || null;
  const foreign = marked.filter((c) => !isOwnBotComment(c, MARKERS.review, trustedAuthors));
  if (foreign.length) {
    const who = [...new Set(foreign.map((c) => c?.user?.login || '?'))].join(', ');
    log(`sticky-маркер в комментариях недоверенных авторов (${who}) — игнорирую`);
  }
  return own;
}

/** Inline-комментарий с маркером, но не от бота: не удаляем и сообщаем. */
function isForeignInline(c, trustedAuthors) {
  return hasMarker(c?.body, MARKERS.inline) && !isOwnBotComment(c, MARKERS.inline, trustedAuthors);
}

const NATIVE_FETCH = typeof globalThis.fetch === 'function' ? globalThis.fetch : null;

/**
 * Тело review-сводки над inline-комментариями. Вынесено, чтобы обёртка `withIcons`
 * (lib/icons.mjs) подставляла ту же строку с пиктограммой через `postReview({ body })`.
 */
export function reviewSummaryBody(inlineCount) {
  return inlineCount
    ? '🤖 Авто-ревью: инлайн-замечания ниже, общий вывод — в закреплённом комментарии.'
    : '🤖 Авто-ревью: инлайн-замечаний нет, общий вывод — в закреплённом комментарии.';
}

export function createGithubClient(cfg, log, { fetch: fetchImpl = globalThis.fetch } = {}) {
  // Эталонная JS-реализация — фабрика ниже (createGithubClientJs); диспетчер
  // на каждый публичный метод выбирает Rust-путь или JS-fallback.
  const jsClient = createGithubClientJs(cfg, log, fetchImpl);
  const { github: ghCfg, httpRetries } = cfg;
  const { api, token, repo, prNumber } = ghCfg;
  // Самодельный cfg без поля (тесты, внешние вызывающие) получает дефолт, а не «доверять всем».
  const trustedAuthors = ghCfg.trustedAuthors ?? parseTrustedAuthors();

  function resolveGithubBin() {
    if (process.env.AI_REVIEWER_GITHUB_FORCE_JS === '1') return null;
    // fetch инжектирован (DI) ИЛИ globalThis.fetch перехвачен после загрузки —
    // Rust-бинарь подмену не видит, сохраняем эталонный JS.
    if (!NATIVE_FETCH || fetchImpl !== NATIVE_FETCH || globalThis.fetch !== NATIVE_FETCH) return null;
    const bin = (process.env.AI_REVIEWER_GITHUB_BIN || '').trim() || DEFAULT_GITHUB_BIN;
    return existsSync(bin) ? bin : null;
  }

  const toolName = () => cfg.profile?.toolName || getProfile().toolName;
  const rustBase = () => ({ api, token, toolName: toolName(), httpRetries });
  // Worst-case: (httpRetries+1) попыток × 30с таймаут + httpRetries × 15с backoff.
  const execTimeoutMs = Math.max((httpRetries + 1) * 35_000 + 15_000, 120_000);

  async function rustRaw(bin, { method = 'GET', body = null, accept = null, path }) {
    return runRustGithub(bin, { op: 'request', ...rustBase(), method, path, body, accept }, execTimeoutMs);
  }

  async function rustGh(bin, path, opts = {}) {
    const { text } = await rustRaw(bin, { ...opts, path });
    const accept = opts.accept || 'application/vnd.github+json';
    return accept.includes('diff') ? text : (text ? JSON.parse(text) : {});
  }

  async function rustList(bin, path) {
    return runRustGithub(bin, { op: 'list', ...rustBase(), method: 'GET', path, body: null, accept: null }, execTimeoutMs);
  }

  async function rustGraphql(bin, query, variables = {}) {
    const { text } = await rustRaw(bin, { method: 'POST', path: '/graphql', body: { query, variables } });
    const parsed = text ? JSON.parse(text) : {};
    if (parsed.errors?.length) {
      throw new Error(parsed.errors.map((e) => e.message).join('; '));
    }
    return parsed.data;
  }

  async function gh(path, opts = {}) {
    const bin = resolveGithubBin();
    if (!bin) return jsClient.gh(path, opts);
    return rustGh(bin, path, opts);
  }

  async function loadDiff() {
    if (!repo || !prNumber || !token) return null;
    const bin = resolveGithubBin();
    if (!bin) return jsClient.loadDiff();
    const pr = await rustGh(bin, `/repos/${repo}/pulls/${prNumber}`);
    const headSha = pr.head?.sha || '';
    if (pr.draft) return { draft: true };
    const diff = await rustGh(bin, `/repos/${repo}/pulls/${prNumber}`, { accept: 'application/vnd.github.v3.diff' });
    return { pr, headSha, diff };
  }

  async function readPriorStickyData() {
    const empty = { findings: [], runs: [], sha: '' };
    if (!repo || !prNumber || !token) return { ...empty };
    const bin = resolveGithubBin();
    if (!bin) return jsClient.readPriorStickyData();
    try {
      const comments = await rustList(bin, `/repos/${repo}/issues/${prNumber}/comments?per_page=100`);
      const mine = pickOwnSticky(comments, trustedAuthors, log);
      // ПОСЛЕДНЕЕ вхождение маркера (находка могла процитировать его в тексте — PR #531).
      const raw = mine && extractStickyFooterRaw(mine.body);
      if (raw == null) return { ...empty };
      let data;
      try {
        data = JSON.parse(raw.trim());
      } catch {
        // Обрезанный под лимит GitHub футер чиним через json_repair (как JS-ветка).
        data = extractJsonObject(raw, { onRepaired: () => {} });
      }
      const isFinding = (f) => f && typeof f === 'object' && typeof f.file === 'string';
      const isRun = (r) => r && typeof r === 'object' && typeof r.sha === 'string';
      return {
        findings: Array.isArray(data.findings) ? data.findings.filter(isFinding).slice(0, 40) : [],
        runs: Array.isArray(data.runs) ? data.runs.filter(isRun).slice(-5) : [],
        sha: typeof data.sha === 'string' ? data.sha : '',
      };
    } catch { return { ...empty }; }
  }

  /** Совместимость: прежние вызывающие ждут массив находок. */
  async function readPriorFindings() {
    return (await readPriorStickyData()).findings;
  }

  async function upsertSticky(body) {
    const bin = resolveGithubBin();
    if (!bin) return jsClient.upsertSticky(body);
    const comments = await rustList(bin, `/repos/${repo}/issues/${prNumber}/comments?per_page=100`);
    const mine = pickOwnSticky(comments, trustedAuthors, log);
    if (mine) {
      await rustGh(bin, `/repos/${repo}/issues/comments/${mine.id}`, { method: 'PATCH', body: { body } });
      return 'updated';
    }
    await rustGh(bin, `/repos/${repo}/issues/${prNumber}/comments`, { method: 'POST', body: { body } });
    return 'created';
  }

  async function deleteStaleInline({ exceptReviewId = null } = {}) {
    const bin = resolveGithubBin();
    if (!bin) return jsClient.deleteStaleInline({ exceptReviewId });
    let removed = 0, kept = 0, foreign = 0;
    try {
      const comments = await rustList(bin, `/repos/${repo}/pulls/${prNumber}/comments?per_page=100`);
      const hasReplies = new Set(comments.map((c) => c.in_reply_to_id).filter(Boolean));
      for (const c of comments) {
        if (isForeignInline(c, trustedAuthors)) { foreign++; continue; }
        if (!hasMarker(c.body, MARKERS.inline)) continue;
        // Только что запощенный review (post-then-delete: при сбое постинга старые не теряем).
        if (exceptReviewId && c.pull_request_review_id === exceptReviewId) continue;
        // Тред с ответами не трогаем — удаление корня рвёт обсуждение.
        if (hasReplies.has(c.id)) { kept++; continue; }
        await rustGh(bin, `/repos/${repo}/pulls/comments/${c.id}`, { method: 'DELETE' });
        removed++;
      }
    } catch (e) { log('очистка старых инлайнов не удалась (не критично):', e.message); }
    if (kept) log(`старых инлайн-тредов с ответами оставлено: ${kept}`);
    if (foreign) log(`инлайнов с маркером от недоверенных авторов не тронуто: ${foreign}`);
    return removed;
  }

  async function postReview({ inline, headSha, body = reviewSummaryBody(inline.length) }) {
    const bin = resolveGithubBin();
    if (!bin) return jsClient.postReview({ inline, headSha, body });
    const payload = { event: 'COMMENT', body, comments: inline };
    if (headSha) payload.commit_id = headSha;
    const res = await rustGh(bin, `/repos/${repo}/pulls/${prNumber}/reviews`, { method: 'POST', body: payload });
    return res?.id || null;
  }

  async function listInlineComments() {
    if (!repo || !prNumber || !token) return [];
    const bin = resolveGithubBin();
    if (!bin) return jsClient.listInlineComments();
    return rustList(bin, `/repos/${repo}/pulls/${prNumber}/comments?per_page=100`);
  }

  async function listIssueLabels() {
    if (!repo || !prNumber || !token) return [];
    const bin = resolveGithubBin();
    if (!bin) return jsClient.listIssueLabels();
    return rustList(bin, `/repos/${repo}/issues/${prNumber}/labels?per_page=100`);
  }

  async function readStickyReview() {
    if (!repo || !prNumber || !token) return null;
    const bin = resolveGithubBin();
    if (!bin) return jsClient.readStickyReview();
    const comments = await rustList(bin, `/repos/${repo}/issues/${prNumber}/comments?per_page=100`);
    return pickOwnSticky(comments, trustedAuthors, log);
  }

  function repoParts() {
    const [owner, name] = String(repo || '').split('/');
    if (!owner || !name) throw new Error('github.repo not configured');
    return { owner, name };
  }

  async function listUnresolvedReviewThreads() {
    const bin = resolveGithubBin();
    if (!bin) return jsClient.listUnresolvedReviewThreads();
    const { owner, name } = repoParts();
    const nodes = [];
    let after = null;
    const maxPages = 50;
    let done = false;
    for (let page = 0; page < maxPages; page += 1) {
      const data = await rustGraphql(bin, THREADS_QUERY, { owner, name, pr: prNumber, after });
      const conn = data?.repository?.pullRequest?.reviewThreads;
      nodes.push(...(conn?.nodes || []));
      if (!conn?.pageInfo?.hasNextPage) {
        done = true;
        break;
      }
      after = conn.pageInfo.endCursor;
    }
    if (!done) throw new Error(`reviewThreads pagination exceeded ${maxPages} pages`);
    return nodes
      .filter((t) => !t.isResolved)
      .map((t) => ({
        id: t.id,
        path: t.path,
        line: t.line,
        comments: (t.comments?.nodes || []).map((c) => ({ body: c.body, author: c.author?.login })),
      }));
  }

  async function replyReviewThread(threadId, body) {
    const bin = resolveGithubBin();
    if (!bin) return jsClient.replyReviewThread(threadId, body);
    await rustGraphql(bin, REPLY_MUTATION, { threadId, body });
  }

  async function resolveReviewThread(threadId) {
    const bin = resolveGithubBin();
    if (!bin) return jsClient.resolveReviewThread(threadId);
    await rustGraphql(bin, RESOLVE_MUTATION, { threadId });
  }

  async function listPullCommentReactions(commentId) {
    if (!repo || !token) return [];
    const bin = resolveGithubBin();
    if (!bin) return jsClient.listPullCommentReactions(commentId);
    return rustList(bin, `/repos/${repo}/pulls/comments/${commentId}/reactions?per_page=100`);
  }

  return {
    gh, loadDiff, readPriorFindings, readPriorStickyData, upsertSticky, deleteStaleInline, postReview,
    listInlineComments, listIssueLabels, readStickyReview, listPullCommentReactions,
    listUnresolvedReviewThreads, replyReviewThread, resolveReviewThread,
  };
}

// Graphql-запросы тредов — общие для Rust- и JS-путей (как в прежней реализации).
const THREADS_QUERY = `query($owner: String!, $name: String!, $pr: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $pr) {
      reviewThreads(first: 100, after: $after) {
        nodes {
          id
          isResolved
          path
          line
          comments(first: 10) {
            nodes { body author { login } }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}`;

const REPLY_MUTATION = `mutation($threadId: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, body: $body }) {
    comment { id }
  }
}`;

const RESOLVE_MUTATION = `mutation($threadId: ID!) {
  resolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } }
}`;

/**
 * Вызов Rust-бинаря: один op (request/list) целиком — HTTP + ретраи внутри.
 * spawn + ручной stdin: async execFile с input на Node 22 не закрывает stdin
 * ребёнка (см. lib/llm.mjs), поэтому promisify(execFile) не годится.
 */
function runRustGithub(bin, input, timeoutMs) {
  return new Promise((resolve, reject) => {
    const child = spawn(bin, [], { stdio: ['pipe', 'pipe', 'pipe'], env: process.env });
    let stdout = '', stderr = '';
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(Object.assign(new Error(`github binary timeout после ${timeoutMs}мс`), { killed: true }));
    }, timeoutMs);
    child.stdout.on('data', (d) => { stdout += d; });
    child.stderr.on('data', (d) => { stderr += d; });
    child.on('error', (e) => { clearTimeout(timer); reject(e); });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      if (code === 0) {
        try {
          resolve(JSON.parse(stdout));
        } catch (e) {
          reject(Object.assign(e, { stdout: stdout.slice(0, 2000) }));
        }
        return;
      }
      const info = parseErrorPayload(stderr);
      if (info) {
        reject(Object.assign(new Error(info.error), { status: info.status ?? undefined }));
        return;
      }
      reject(Object.assign(
        new Error(`github binary завершился code=${code} signal=${signal}: ${stderr.slice(0, 400)}`),
        { code, signal },
      ));
    });
    child.stdin.on('error', () => {});
    child.stdin.write(JSON.stringify(input));
    child.stdin.end();
  });
}

function parseErrorPayload(stderr) {
  try {
    const info = JSON.parse(String(stderr || '').trim());
    return info && typeof info.error === 'string' && 'status' in info ? info : null;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия прежней реализации (lib/github.mjs до
// Фазы 3, шага 4). Используется когда Rust-бинарь отсутствует ИЛИ fetch
// инжектирован/перехвачен (моки/DI); при битом бинаре НЕ подменяет его.
// ---------------------------------------------------------------------------

function createGithubClientJs(cfg, log, fetchImpl) {
  const { github: ghCfg, httpRetries } = cfg;
  const { api, token, repo, prNumber } = ghCfg;
  const trustedAuthors = ghCfg.trustedAuthors ?? parseTrustedAuthors();

  async function ghRaw(path, { method = 'GET', body, accept = 'application/vnd.github+json' } = {}) {
    const url = path.startsWith('http') ? path : `${api}${path}`;
    const opts = {
      method,
      headers: {
        Authorization: `Bearer ${token}`,
        Accept: accept,
        'X-GitHub-Api-Version': '2022-11-28',
        'User-Agent': cfg.profile?.toolName || getProfile().toolName,
        ...(body ? { 'Content-Type': 'application/json' } : {}),
      },
      body: body ? JSON.stringify(body) : undefined,
    };
    for (let attempt = 0; ; attempt++) {
      const ctrl = new AbortController();
      const t = setTimeout(() => ctrl.abort(), 30_000);
      try {
        const res = await fetchImpl(url, { ...opts, signal: ctrl.signal });
        const text = await res.text();
        if (!res.ok) {
          if (isTransient(res.status) && attempt < httpRetries) {
            const wait = backoffMs(attempt, res.headers.get('retry-after'));
            log(`GitHub ${method} ${path} → ${res.status}, повтор через ${wait}мс (${attempt + 1}/${httpRetries})`);
            await sleep(wait);
            continue;
          }
          throw Object.assign(new Error(`GitHub ${method} ${path} → ${res.status}: ${text.slice(0, 500)}`), { status: res.status });
        }
        return { text, link: res.headers.get('link') || '' };
      } catch (e) {
        const networky = e.status === undefined && /fetch failed|network|socket|ECONN|ETIMEDOUT|EAI_AGAIN|terminated|aborted/i.test(e.message || '');
        if (networky && attempt < httpRetries) {
          const wait = backoffMs(attempt);
          log(`GitHub ${method} ${path} сетевой сбой (${e.message}) → повтор через ${wait}мс (${attempt + 1}/${httpRetries})`);
          await sleep(wait);
          continue;
        }
        throw e;
      } finally { clearTimeout(t); }
    }
  }

  async function gh(path, opts = {}) {
    const { text } = await ghRaw(path, opts);
    const accept = opts.accept || 'application/vnd.github+json';
    return accept.includes('diff') ? text : (text ? JSON.parse(text) : {});
  }

  async function ghList(path) {
    let url = path, out = [];
    for (let guard = 0; url && guard < 50; guard++) {
      const { text, link } = await ghRaw(url);
      const page = text ? JSON.parse(text) : [];
      if (!Array.isArray(page)) return page;
      out = out.concat(page);
      url = /<([^>]+)>;\s*rel="next"/.exec(link)?.[1] || '';
    }
    return out;
  }

  async function loadDiff() {
    if (!repo || !prNumber || !token) return null;
    const pr = await gh(`/repos/${repo}/pulls/${prNumber}`);
    const headSha = pr.head?.sha || '';
    if (pr.draft) return { draft: true };
    const diff = await gh(`/repos/${repo}/pulls/${prNumber}`, { accept: 'application/vnd.github.v3.diff' });
    return { pr, headSha, diff };
  }

  async function readPriorStickyData() {
    if (!repo || !prNumber || !token) return { findings: [], runs: [], sha: '' };
    try {
      const comments = await ghList(`/repos/${repo}/issues/${prNumber}/comments?per_page=100`);
      const mine = pickOwnSticky(comments, trustedAuthors, log);
      // Берём ПОСЛЕДНЕЕ вхождение маркера: находка могла процитировать его пример в своём
      // тексте, и разбор «до первого» вернул бы чужой JSON (случай PR #531 — отчёт выглядел
      // пустым при шести находках).
      const raw = mine && extractStickyFooterRaw(mine.body);
      // Форма возврата одна во ВСЕХ ветках: на первом прогоне PR sticky ещё нет, и `[]`
      // здесь ронял пайплайн на `priorSticky.findings.length` (TypeError).
      if (raw == null) return { findings: [], runs: [], sha: '' };
      let data;
      try {
        data = JSON.parse(raw.trim());
      } catch {
        // Footer мог быть обрезан под лимит GitHub — пробуем ремонт JSON (P2-12),
        // иначе prior findings молча теряются.
        data = extractJsonObject(raw, { onRepaired: () => {} });
      }
      // Футер пишет наш же движок, но читаем мы его из чужого текста (комментарий на GitHub
      // мог быть отредактирован руками) — поэтому проверяем форму элементов, а не только массива.
      const isFinding = (f) => f && typeof f === 'object' && typeof f.file === 'string';
      const isRun = (r) => r && typeof r === 'object' && typeof r.sha === 'string';
      return {
        findings: Array.isArray(data.findings) ? data.findings.filter(isFinding).slice(0, 40) : [],
        // История прогонов для Δ-блока: sha + счётчики. Хранится в футере, чтобы sticky
        // оставался ОДИН (сравнивать прогоны по ленте комментариев — шум), но был виден
        // рост/спад набора находок.
        runs: Array.isArray(data.runs) ? data.runs.filter(isRun).slice(-5) : [],
        sha: typeof data.sha === 'string' ? data.sha : '',
      };
    } catch { return { findings: [], runs: [], sha: '' }; }
  }

  /** Совместимость: прежние вызывающие ждут массив находок. */
  async function readPriorFindings() {
    return (await readPriorStickyData()).findings;
  }

  async function upsertSticky(body) {
    const comments = await ghList(`/repos/${repo}/issues/${prNumber}/comments?per_page=100`);
    const mine = pickOwnSticky(comments, trustedAuthors, log);
    if (mine) {
      await gh(`/repos/${repo}/issues/comments/${mine.id}`, { method: 'PATCH', body: { body } });
      return 'updated';
    }
    await gh(`/repos/${repo}/issues/${prNumber}/comments`, { method: 'POST', body: { body } });
    return 'created';
  }

  async function deleteStaleInline({ exceptReviewId = null } = {}) {
    let removed = 0, kept = 0, foreign = 0;
    try {
      const comments = await ghList(`/repos/${repo}/pulls/${prNumber}/comments?per_page=100`);
      const hasReplies = new Set(comments.map((c) => c.in_reply_to_id).filter(Boolean));
      for (const c of comments) {
        if (isForeignInline(c, trustedAuthors)) { foreign++; continue; }
        if (!hasMarker(c.body, MARKERS.inline)) continue;
        // Только что запощенный review (post-then-delete: при сбое постинга старые не теряем).
        if (exceptReviewId && c.pull_request_review_id === exceptReviewId) continue;
        // Тред с ответами не трогаем — удаление корня рвёт обсуждение (разрулит auto-triage).
        if (hasReplies.has(c.id)) { kept++; continue; }
        await gh(`/repos/${repo}/pulls/comments/${c.id}`, { method: 'DELETE' });
        removed++;
      }
    } catch (e) { log('очистка старых инлайнов не удалась (не критично):', e.message); }
    if (kept) log(`старых инлайн-тредов с ответами оставлено: ${kept}`);
    if (foreign) log(`инлайнов с маркером от недоверенных авторов не тронуто: ${foreign}`);
    return removed;
  }

  async function postReview({ inline, headSha, body = reviewSummaryBody(inline.length) }) {
    const payload = { event: 'COMMENT', body, comments: inline };
    if (headSha) payload.commit_id = headSha;
    const res = await gh(`/repos/${repo}/pulls/${prNumber}/reviews`, { method: 'POST', body: payload });
    return res?.id || null;
  }

  async function listInlineComments() {
    if (!repo || !prNumber || !token) return [];
    return ghList(`/repos/${repo}/pulls/${prNumber}/comments?per_page=100`);
  }

  async function listIssueLabels() {
    if (!repo || !prNumber || !token) return [];
    return ghList(`/repos/${repo}/issues/${prNumber}/labels?per_page=100`);
  }

  async function readStickyReview() {
    if (!repo || !prNumber || !token) return null;
    const comments = await ghList(`/repos/${repo}/issues/${prNumber}/comments?per_page=100`);
    return pickOwnSticky(comments, trustedAuthors, log);
  }

  async function graphql(query, variables = {}) {
    const { text } = await ghRaw('/graphql', { method: 'POST', body: { query, variables } });
    const parsed = text ? JSON.parse(text) : {};
    if (parsed.errors?.length) {
      throw new Error(parsed.errors.map((e) => e.message).join('; '));
    }
    return parsed.data;
  }

  function repoParts() {
    const [owner, name] = String(repo || '').split('/');
    if (!owner || !name) throw new Error('github.repo not configured');
    return { owner, name };
  }

  async function listUnresolvedReviewThreads() {
    const { owner, name } = repoParts();
    const nodes = [];
    let after = null;
    const maxPages = 50;
    let done = false;
    for (let page = 0; page < maxPages; page += 1) {
      const data = await graphql(
        THREADS_QUERY,
        { owner, name, pr: prNumber, after },
      );
      const conn = data.repository?.pullRequest?.reviewThreads;
      nodes.push(...(conn?.nodes || []));
      if (!conn?.pageInfo?.hasNextPage) {
        done = true;
        break;
      }
      after = conn.pageInfo.endCursor;
    }
    if (!done) throw new Error(`reviewThreads pagination exceeded ${maxPages} pages`);
    return nodes
      .filter((t) => !t.isResolved)
      .map((t) => ({
        id: t.id,
        path: t.path,
        line: t.line,
        comments: (t.comments?.nodes || []).map((c) => ({ body: c.body, author: c.author?.login })),
      }));
  }

  async function replyReviewThread(threadId, body) {
    await graphql(REPLY_MUTATION, { threadId, body });
  }

  async function resolveReviewThread(threadId) {
    await graphql(RESOLVE_MUTATION, { threadId });
  }

  async function listPullCommentReactions(commentId) {
    if (!repo || !token) return [];
    return ghList(`/repos/${repo}/pulls/comments/${commentId}/reactions?per_page=100`);
  }

  return {
    gh, loadDiff, readPriorFindings, readPriorStickyData, upsertSticky, deleteStaleInline, postReview,
    listInlineComments, listIssueLabels, readStickyReview, listPullCommentReactions,
    listUnresolvedReviewThreads, replyReviewThread, resolveReviewThread,
  };
}
