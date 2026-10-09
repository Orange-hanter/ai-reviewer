/**
 * RFC 16: клиент GitLab (gitlab.com и self-managed, REST API v4) с тем же контрактом, что
 * createGithubClient. Пайплайн, auto-triage и feedback-harvest читают GitHub-форму
 * подмножества полей — клиент приводит к ней ответы GitLab, а не наоборот: так пайплайн
 * не знает, на каком хостинге работает (`lib/forge.mjs`).
 *
 * Соответствие сущностей:
 *   PR                    → merge request (iid); pr.html_url ← web_url;
 *   issue comment sticky  → MR note без позиции (system-заметки GitLab пропускаем);
 *   review + inline       → draft notes с position + bulk_publish (fallback: discussions);
 *   review thread         → discussion с resolvable-заметками;
 *   reaction +1/-1        → award emoji thumbsup/thumbsdown;
 *   label                 → поле labels MR.
 *
 * Только JS (RFC 16, Р1): оркестрация и так в JS, а Rust-транспорт GitHub зашивает его
 * заголовки — обобщать его до второго клиента значило бы гадать о контракте.
 *
 * Позиция инлайна. GitLab отклоняет discussion без полной позиции: три SHA из diff_refs,
 * old_path/new_path и new_line для добавленной строки или пара old_line+new_line для
 * контекстной. Пайплайн знает только путь и номер строки новой стороны, поэтому
 * недостающее клиент восстанавливает из того же диффа (`buildDiffLineMap`).
 */

import { sleep, isTransient, backoffMs } from './util.mjs';
import { MARKERS, hasMarker } from './constants.mjs';
import { getProfile } from './profile.mjs';
import { findStickyComment, parseStickyData } from './sticky.mjs';

const PAGE_GUARD = 50;

/**
 * Карта строк диффа для позиций GitLab: путь новой стороны → { oldPath, added, context }.
 * `context` — newLine → oldLine для неизменённых строк хунка.
 * @param {string} diff unified diff (формат `git diff`)
 */
export function buildDiffLineMap(diff) {
  const files = new Map();
  let cur = null;
  let oldLine = 0;
  let newLine = 0;
  let inHunk = false;
  const unq = (p) => p.replace(/^"(.*)"$/, '$1');
  for (const line of String(diff || '').split('\n')) {
    if (line.startsWith('diff --git ')) {
      const m = /^diff --git a\/(.+?) b\/(.+)$/.exec(line);
      cur = { oldPath: m ? unq(m[1]) : '', newPath: m ? unq(m[2]) : '', added: new Set(), context: new Map() };
      if (cur.newPath) files.set(cur.newPath, cur);
      inHunk = false;
      continue;
    }
    if (!cur) continue;
    if (!inHunk) {
      if (line.startsWith('rename from ')) cur.oldPath = line.slice('rename from '.length);
      else if (line.startsWith('--- a/')) cur.oldPath = unq(line.slice(6));
      else if (line.startsWith('+++ b/')) {
        const p = unq(line.slice(6));
        if (p !== cur.newPath) {
          files.delete(cur.newPath);
          cur.newPath = p;
          files.set(p, cur);
        }
      }
    }
    const h = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (h) {
      oldLine = Number(h[1]);
      newLine = Number(h[2]);
      inHunk = true;
      continue;
    }
    if (!inHunk) continue;
    if (line.startsWith('+')) cur.added.add(newLine++);
    else if (line.startsWith('-')) oldLine++;
    else if (line.startsWith(' ')) cur.context.set(newLine++, oldLine++);
    // `\ No newline at end of file` и пустой хвост номера строк не сдвигают.
  }
  return files;
}

/**
 * Позиция GitLab для инлайна на строке новой стороны. Строка вне диффа → null: GitLab
 * ответит 400, и лучше не тратить на неё запрос.
 */
export function gitlabPosition(lineMap, { path, line }, refs) {
  const base = {
    position_type: 'text',
    base_sha: refs.base_sha,
    start_sha: refs.start_sha,
    head_sha: refs.head_sha,
    new_path: path,
  };
  const rec = lineMap?.get(path);
  if (!rec) return lineMap ? null : { ...base, old_path: path, new_line: line };
  const oldPath = rec.oldPath || path;
  if (rec.added.has(line)) return { ...base, old_path: oldPath, new_line: line };
  if (rec.context.has(line)) return { ...base, old_path: oldPath, new_line: line, old_line: rec.context.get(line) };
  return null;
}

/**
 * Unified diff из ответа `GET …/merge_requests/:iid/diffs`. В ответе нет заголовков
 * `diff --git` и mode-строк — без них parseDiff не увидит добавленный файл
 * (collectAddedPaths) и rename-only секцию, поэтому собираем их из флагов записи.
 */
export function synthesizeUnifiedDiff(entries) {
  const out = [];
  for (const e of entries || []) {
    const a = e.old_path || e.new_path;
    const b = e.new_path || e.old_path;
    out.push(`diff --git a/${a} b/${b}`);
    if (e.new_file) out.push(`new file mode ${e.b_mode || '100644'}`);
    else if (e.deleted_file) out.push(`deleted file mode ${e.a_mode || '100644'}`);
    else if (e.a_mode && e.b_mode && e.a_mode !== e.b_mode) out.push(`old mode ${e.a_mode}`, `new mode ${e.b_mode}`);
    if (e.renamed_file) out.push(`rename from ${a}`, `rename to ${b}`);
    const body = String(e.diff || '').replace(/\n$/, '');
    if (!body) continue;
    if (!body.startsWith('--- ')) {
      out.push(`--- ${e.new_file ? '/dev/null' : `a/${a}`}`, `+++ ${e.deleted_file ? '/dev/null' : `b/${b}`}`);
    }
    out.push(body);
  }
  return out.length ? `${out.join('\n')}\n` : '';
}

/** Заметка GitLab → GitHub-форма комментария, которую читают feedback и triage. */
function toComment(note, extra = {}) {
  return {
    id: note.id,
    body: note.body || '',
    // У автора заметки GitLab нет надёжного признака бота (в отличие от user.type GitHub):
    // доверенность решается только allowlist'ом логинов (lib/feedback.mjs).
    user: { login: note.author?.username || '' },
    ...extra,
  };
}

function isDiffNote(note) {
  return note?.type === 'DiffNote' || Boolean(note?.position);
}

export function createGitlabClient(cfg, log, { fetch: fetchImpl = globalThis.fetch } = {}) {
  const forgeCfg = cfg.forge || cfg.gitlab || {};
  const { api, token, repo, prNumber } = forgeCfg;
  const httpRetries = cfg.httpRetries ?? 2;
  const project = encodeURIComponent(String(repo || ''));
  const mrPath = `/projects/${project}/merge_requests/${prNumber}`;
  let diffRefs = null;
  let selfLoginCache;

  async function glRaw(path, { method = 'GET', body } = {}) {
    const url = path.startsWith('http') ? path : `${api}${path}`;
    const opts = {
      method,
      headers: {
        'PRIVATE-TOKEN': token,
        Accept: 'application/json',
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
            log(`GitLab ${method} ${path} → ${res.status}, повтор через ${wait}мс (${attempt + 1}/${httpRetries})`);
            await sleep(wait);
            continue;
          }
          throw Object.assign(new Error(`GitLab ${method} ${path} → ${res.status}: ${text.slice(0, 500)}`), { status: res.status });
        }
        return { text, link: res.headers.get('link') || '' };
      } catch (e) {
        const networky = e.status === undefined && /fetch failed|network|socket|ECONN|ETIMEDOUT|EAI_AGAIN|terminated|aborted/i.test(e.message || '');
        if (networky && attempt < httpRetries) {
          const wait = backoffMs(attempt);
          log(`GitLab ${method} ${path} сетевой сбой (${e.message}) → повтор через ${wait}мс (${attempt + 1}/${httpRetries})`);
          await sleep(wait);
          continue;
        }
        throw e;
      } finally { clearTimeout(t); }
    }
  }

  async function gl(path, opts = {}) {
    const { text } = await glRaw(path, opts);
    return text ? JSON.parse(text) : {};
  }

  /** Offset-пагинация GitLab отдаёт тот же `Link: <…>; rel="next"`, что и GitHub. */
  async function glList(path) {
    let url = path;
    let out = [];
    for (let guard = 0; url && guard < PAGE_GUARD; guard++) {
      const { text, link } = await glRaw(url);
      const page = text ? JSON.parse(text) : [];
      if (!Array.isArray(page)) return page;
      out = out.concat(page);
      url = /<([^>]+)>;\s*rel="next"/.exec(link)?.[1] || '';
    }
    return out;
  }

  const withPage = (path) => `${path}${path.includes('?') ? '&' : '?'}per_page=100`;
  const configured = () => Boolean(repo && prNumber && token);

  async function loadMr() {
    const mr = await gl(mrPath);
    if (mr.diff_refs) diffRefs = mr.diff_refs;
    return mr;
  }

  async function refs() {
    if (!diffRefs) await loadMr();
    if (!diffRefs?.head_sha) throw new Error('GitLab не вернул diff_refs merge request');
    return diffRefs;
  }

  async function loadMrDiff() {
    // raw_diffs отдаёт формат `git diff` как есть; есть не во всех версиях GitLab, и формат
    // проверяем, а не доверяем — иначе parseDiff молча получит пустой набор файлов.
    try {
      const { text } = await glRaw(`${mrPath}/raw_diffs`);
      if (/^diff --git /m.test(text)) return text;
    } catch (e) {
      if (e.status !== 404 && e.status !== 400) throw e;
    }
    const entries = await glList(withPage(`${mrPath}/diffs`));
    const lost = entries.filter((e) => !e.diff && (e.too_large || e.collapsed)).map((e) => e.new_path);
    if (lost.length) log(`GitLab не отдал дифф ${lost.length} файл(ов) (too_large/collapsed): ${lost.slice(0, 5).join(', ')}`);
    return synthesizeUnifiedDiff(entries);
  }

  async function loadDiff() {
    if (!configured()) return null;
    const mr = await loadMr();
    if (mr.draft || mr.work_in_progress) return { draft: true };
    const headSha = mr.diff_refs?.head_sha || mr.sha || '';
    const diff = await loadMrDiff();
    const pr = { number: mr.iid, title: mr.title || '', html_url: mr.web_url || '', draft: false };
    return { pr, headSha, diff };
  }

  async function listNotes() {
    return glList(withPage(`${mrPath}/notes?sort=asc&order_by=created_at`));
  }

  /** Обычные заметки MR (sticky живёт здесь): без позиции и не системные. */
  async function listGeneralComments() {
    return (await listNotes()).filter((n) => !n.system && !isDiffNote(n)).map((n) => toComment(n));
  }

  async function readPriorStickyData() {
    if (!configured()) return { findings: [], runs: [], sha: '' };
    try {
      return parseStickyData(findStickyComment(await listGeneralComments())?.body);
    } catch { return { findings: [], runs: [], sha: '' }; }
  }

  async function readPriorFindings() {
    return (await readPriorStickyData()).findings;
  }

  async function readStickyReview() {
    if (!configured()) return null;
    return findStickyComment(await listGeneralComments());
  }

  async function upsertSticky(body) {
    const mine = findStickyComment(await listGeneralComments());
    if (mine) {
      await gl(`${mrPath}/notes/${mine.id}`, { method: 'PUT', body: { body } });
      return 'updated';
    }
    await gl(`${mrPath}/notes`, { method: 'POST', body: { body } });
    return 'created';
  }

  async function listDiscussions() {
    return glList(withPage(`${mrPath}/discussions`));
  }

  /** Инлайны движка: корневая DiffNote дискуссии с маркером инлайна. */
  async function listOwnInlineDiscussions() {
    return (await listDiscussions()).filter((d) => {
      const root = d.notes?.[0];
      return root && !root.system && isDiffNote(root) && hasMarker(root.body, MARKERS.inline);
    });
  }

  async function deleteStaleInline({ exceptReviewId = null } = {}) {
    let removed = 0;
    let kept = 0;
    try {
      // ReviewToken GitLab — снимок инлайнов, существовавших ДО постинга: удаляем только их.
      // У GitLab нет id «одного review», которым GitHub отделяет только что запощенное.
      const stale = exceptReviewId?.staleNoteIds || null;
      for (const d of await listOwnInlineDiscussions()) {
        const root = d.notes[0];
        if (stale && !stale.has(root.id)) continue;
        // Тред с ответами не трогаем — удаление корня рвёт обсуждение (разрулит auto-triage).
        if (d.notes.filter((n) => !n.system).length > 1) { kept++; continue; }
        await gl(`${mrPath}/notes/${root.id}`, { method: 'DELETE' });
        removed++;
      }
    } catch (e) { log('очистка старых инлайнов не удалась (не критично):', e.message); }
    if (kept) log(`старых инлайн-тредов с ответами оставлено: ${kept}`);
    return removed;
  }

  async function deleteOwnDrafts() {
    // bulk_publish публикует ВСЕ черновики пользователя: черновики, оставшиеся от упавшего
    // прогона, иначе всплыли бы вместе с новыми.
    const drafts = await glList(withPage(`${mrPath}/draft_notes`));
    for (const dn of drafts) await gl(`${mrPath}/draft_notes/${dn.id}`, { method: 'DELETE' });
  }

  async function postViaDrafts(items) {
    await deleteOwnDrafts();
    let posted = 0;
    let rejected = 0;
    for (const { note, position } of items) {
      try {
        await gl(`${mrPath}/draft_notes`, { method: 'POST', body: { note, position } });
        posted++;
      } catch (e) {
        if (e.status !== 400) throw e;
        rejected++;
      }
    }
    if (posted) await glRaw(`${mrPath}/draft_notes/bulk_publish`, { method: 'POST' });
    return { posted, rejected };
  }

  async function postViaDiscussions(items) {
    let posted = 0;
    let rejected = 0;
    for (const { note, position } of items) {
      try {
        await gl(`${mrPath}/discussions`, { method: 'POST', body: { body: note, position } });
        posted++;
      } catch (e) {
        if (e.status !== 400) throw e;
        rejected++;
      }
    }
    return { posted, rejected };
  }

  /**
   * @param {{ inline: {path:string, line:number, body:string}[], headSha?: string, diff?: string }} p
   * @returns {Promise<{ staleNoteIds: Set<number>, posted: number, rejected: number }>} ReviewToken
   */
  async function postReview({ inline, diff }) {
    const staleNoteIds = new Set((await listOwnInlineDiscussions()).map((d) => d.notes[0].id));
    const r = await refs();
    const lineMap = diff ? buildDiffLineMap(diff) : null;
    const items = [];
    let unplaced = 0;
    for (const c of inline) {
      const position = gitlabPosition(lineMap, c, r);
      if (!position) { unplaced++; continue; }
      items.push({ note: c.body, position });
    }
    let res;
    try {
      res = await postViaDrafts(items);
    } catch (e) {
      // Draft notes нет в старых self-managed (404) или они запрещены токену (403).
      if (e.status !== 404 && e.status !== 403) throw e;
      log(`GitLab draft notes недоступны (${e.status}) — инлайны по одному, без атомарной публикации`);
      res = await postViaDiscussions(items);
    }
    const rejected = res.rejected + unplaced;
    if (rejected) log(`GitLab отклонил позицию у ${rejected} инлайн(ов) — они не опубликованы`);
    return { staleNoteIds, posted: res.posted, rejected };
  }

  async function listInlineComments() {
    if (!configured()) return [];
    const out = [];
    for (const d of await listDiscussions()) {
      const notes = (d.notes || []).filter((n) => !n.system && isDiffNote(n));
      const rootId = notes[0]?.id;
      for (const n of notes) {
        out.push(toComment(n, {
          path: n.position?.new_path || n.position?.old_path || '',
          line: n.position?.new_line ?? n.position?.old_line ?? null,
          ...(n.id !== rootId ? { in_reply_to_id: rootId } : {}),
        }));
      }
    }
    return out;
  }

  async function listIssueLabels() {
    if (!configured()) return [];
    const mr = await loadMr();
    return (mr.labels || []).map((l) => ({ name: typeof l === 'string' ? l : l.name }));
  }

  async function listPullCommentReactions(noteId) {
    if (!repo || !token) return [];
    const awards = await glList(withPage(`${mrPath}/notes/${noteId}/award_emoji`));
    const content = (name) => (name === 'thumbsup' ? '+1' : name === 'thumbsdown' ? '-1' : name);
    return awards.map((a) => ({ content: content(a.name), user: { login: a.user?.username || '' } }));
  }

  async function listUnresolvedReviewThreads() {
    const out = [];
    for (const d of await listDiscussions()) {
      const notes = (d.notes || []).filter((n) => !n.system);
      const resolvable = notes.filter((n) => n.resolvable);
      if (!resolvable.length || resolvable.every((n) => n.resolved)) continue;
      const root = notes[0];
      out.push({
        id: d.id,
        path: root.position?.new_path || root.position?.old_path || '',
        line: root.position?.new_line ?? root.position?.old_line ?? null,
        comments: notes.map((n) => ({ body: n.body, author: n.author?.username })),
      });
    }
    return out;
  }

  async function replyReviewThread(threadId, body) {
    await gl(`${mrPath}/discussions/${threadId}/notes`, { method: 'POST', body: { body } });
  }

  async function resolveReviewThread(threadId) {
    await gl(`${mrPath}/discussions/${threadId}?resolved=true`, { method: 'PUT' });
  }

  /**
   * Логин владельца токена. На GitLab бот — обычно project/group access token с логином
   * вида `project_<id>_bot_<hash>`: дефолтный allowlist (`github-actions[bot]`) его не
   * знает, и без этого auto-triage и harvest пропускали бы все инлайны движка.
   */
  async function selfLogin() {
    if (selfLoginCache === undefined) {
      const me = await gl('/user');
      selfLoginCache = me?.username || '';
    }
    return selfLoginCache;
  }

  /** Недавние MR проекта в GitHub-форме, нужной selectFeedbackPulls (feedback-batch). */
  async function listRecentChanges({ limit = 100 } = {}) {
    const mrs = await gl(`/projects/${project}/merge_requests?state=all&order_by=updated_at&sort=desc&per_page=${Math.min(limit, 100)}`);
    return (Array.isArray(mrs) ? mrs : []).map((m) => ({
      number: m.iid,
      updated_at: m.updated_at,
      user: { login: m.author?.username || '' },
    }));
  }

  return {
    kind: 'gitlab',
    gl, loadDiff, readPriorFindings, readPriorStickyData, upsertSticky, deleteStaleInline, postReview,
    listInlineComments, listIssueLabels, readStickyReview, listPullCommentReactions,
    listUnresolvedReviewThreads, replyReviewThread, resolveReviewThread, selfLogin, listRecentChanges,
  };
}
