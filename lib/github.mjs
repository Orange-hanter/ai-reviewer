import { sleep, isTransient, backoffMs } from './util.mjs';
import { MARKERS, hasMarker, extractStickyFooterRaw } from './constants.mjs';
import { extractJsonObject } from './json.mjs';
import { getProfile } from './profile.mjs';

export function createGithubClient(cfg, log, { fetch: fetchImpl = globalThis.fetch } = {}) {
  const { github: ghCfg, httpRetries } = cfg;
  const { api, token, repo, prNumber } = ghCfg;

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
      const mine = [...comments].reverse().find((c) => hasMarker(c.body, MARKERS.review));
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
    const mine = [...comments].reverse().find((c) => hasMarker(c.body, MARKERS.review));
    if (mine) {
      await gh(`/repos/${repo}/issues/comments/${mine.id}`, { method: 'PATCH', body: { body } });
      return 'updated';
    }
    await gh(`/repos/${repo}/issues/${prNumber}/comments`, { method: 'POST', body: { body } });
    return 'created';
  }

  async function deleteStaleInline({ exceptReviewId = null } = {}) {
    let removed = 0, kept = 0;
    try {
      const comments = await ghList(`/repos/${repo}/pulls/${prNumber}/comments?per_page=100`);
      const hasReplies = new Set(comments.map((c) => c.in_reply_to_id).filter(Boolean));
      for (const c of comments) {
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
    return removed;
  }

  async function postReview({ inline, headSha }) {
    const body = inline.length
      ? '🤖 Авто-ревью: инлайн-замечания ниже, общий вывод — в закреплённом комментарии.'
      : '🤖 Авто-ревью: инлайн-замечаний нет, общий вывод — в закреплённом комментарии.';
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
    return [...comments].reverse().find((c) => hasMarker(c.body, MARKERS.review)) || null;
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
        `query($owner: String!, $name: String!, $pr: Int!, $after: String) {
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
        }`,
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
    await graphql(
      `mutation($threadId: ID!, $body: String!) {
        addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, body: $body }) {
          comment { id }
        }
      }`,
      { threadId, body },
    );
  }

  async function resolveReviewThread(threadId) {
    await graphql(
      `mutation($threadId: ID!) {
        resolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } }
      }`,
      { threadId },
    );
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
