// Контракт GitLab-клиента (RFC 16): дифф, sticky, позиции инлайна, post-then-delete,
// треды, реакции; плюс резолвинг хостинга в конфиге. fetch инжектится в клиент — без
// мутации globalThis (безопасно при parallel files).
import { referenceProfile } from './helpers/reference-profile.mjs';
import { test, mock } from 'node:test';
import assert from 'node:assert/strict';
import { MARKERS } from '../lib/constants.mjs';
import {
  createGitlabClient, buildDiffLineMap, gitlabPosition, synthesizeUnifiedDiff,
} from '../lib/gitlab.mjs';
import { createForgeClient, forgeOf } from '../lib/forge.mjs';
import { resolveForge } from '../lib/config.mjs';
import { resolveTrustedAuthors, DEFAULT_TRUSTED_AUTHORS } from '../lib/feedback.mjs';
import { parseDiff, collectAddedPaths } from '../lib/diff.mjs';
import { buildCodeQuality, CODEQUALITY_FILE } from '../lib/codequality.mjs';
import { writeReviewExports } from '../lib/export.mjs';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const API = 'https://gitlab.test/api/v4';
const MR = '/projects/g%2Fp/merge_requests/7';
const CFG = {
  forge: { kind: 'gitlab', api: API, token: 't', repo: 'g/p', prNumber: 7, webUrl: 'https://gitlab.test/g/p' },
  httpRetries: 2,
  profile: { toolName: referenceProfile.toolName },
};
const REFS = { base_sha: 'b'.repeat(40), start_sha: 's'.repeat(40), head_sha: 'h'.repeat(40) };

function res(body, { status = 200, link = '', headers = {} } = {}) {
  const h = new Headers(headers);
  if (link) h.set('link', link);
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: h,
    text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
  };
}

/**
 * Фейковый GitLab: маршруты `METHOD path-без-query` → (call) => ответ. Неописанный
 * маршрут — 404, как у настоящего API на отсутствующий эндпоинт.
 */
function fakeGitlab(routes) {
  routes = { 'GET /user': () => res({ username: 'project_1_bot_x' }), ...routes };
  const calls = [];
  const fetchMock = mock.fn(async (url, opts = {}) => {
    const u = new URL(String(url));
    const path = u.pathname.replace('/api/v4', '');
    const method = opts.method || 'GET';
    const call = { method, path, query: u.searchParams, body: opts.body ? JSON.parse(opts.body) : null, headers: opts.headers };
    calls.push(call);
    const h = routes[`${method} ${path}`];
    return h ? h(call) : res({ message: '404 Not Found' }, { status: 404 });
  });
  const client = createGitlabClient(CFG, () => {}, { fetch: fetchMock });
  return { client, calls, fetchMock };
}

const DIFF = [
  'diff --git a/src/old.js b/src/new.js',
  'similarity index 90%',
  'rename from src/old.js',
  'rename to src/new.js',
  '--- a/src/old.js',
  '+++ b/src/new.js',
  '@@ -10,4 +10,5 @@ function f() {',
  ' const a = 1;',
  '-const b = 2;',
  '+const b = 3;',
  '+const c = 4;',
  ' return a;',
  ' }',
  'diff --git a/added.txt b/added.txt',
  'new file mode 100644',
  '--- /dev/null',
  '+++ b/added.txt',
  '@@ -0,0 +1,2 @@',
  '+one',
  '+two',
  '',
].join('\n');

const note = (id, body, extra = {}) => ({ id, body, system: false, author: { username: 'project_1_bot_x' }, ...extra });
const diffNote = (id, body, extra = {}) => note(id, body, {
  type: 'DiffNote', resolvable: true, resolved: false,
  position: { new_path: 'src/new.js', new_line: 11, old_path: 'src/old.js' }, ...extra,
});

test('gitlab: buildDiffLineMap — добавленные, контекстные с old_line, старый путь rename', () => {
  const map = buildDiffLineMap(DIFF);
  const rec = map.get('src/new.js');
  assert.equal(rec.oldPath, 'src/old.js');
  assert.deepEqual([...rec.added], [11, 12]);
  assert.deepEqual([...rec.context.entries()], [[10, 10], [13, 12], [14, 13]]);
  assert.deepEqual([...map.get('added.txt').added], [1, 2]);
});

test('gitlab: gitlabPosition — new_line для добавленной, пара строк для контекста, null вне диффа', () => {
  const map = buildDiffLineMap(DIFF);
  const added = gitlabPosition(map, { path: 'src/new.js', line: 11 }, REFS);
  assert.deepEqual(added, {
    position_type: 'text', ...REFS, new_path: 'src/new.js', old_path: 'src/old.js', new_line: 11,
  });
  const ctx = gitlabPosition(map, { path: 'src/new.js', line: 13 }, REFS);
  assert.equal(ctx.old_line, 12);
  assert.equal(ctx.new_line, 13);
  assert.equal(gitlabPosition(map, { path: 'src/new.js', line: 40 }, REFS), null);
  assert.equal(gitlabPosition(map, { path: 'nope.js', line: 1 }, REFS), null);
  // Без диффа — только new_line (верно для добавленных строк, остальное отклонит GitLab).
  assert.equal(gitlabPosition(null, { path: 'x.js', line: 3 }, REFS).new_line, 3);
});

test('gitlab: synthesizeUnifiedDiff — заголовки, которые нужны parseDiff и collectAddedPaths', () => {
  const diff = synthesizeUnifiedDiff([
    { old_path: 'a.js', new_path: 'a.js', diff: '@@ -1 +1 @@\n-x\n+y\n' },
    { old_path: 'n.js', new_path: 'n.js', new_file: true, b_mode: '100644', diff: '@@ -0,0 +1 @@\n+z\n' },
    { old_path: 'o.js', new_path: 'r.js', renamed_file: true, diff: '' },
    { old_path: 'd.js', new_path: 'd.js', deleted_file: true, a_mode: '100644', diff: '@@ -1 +0,0 @@\n-q\n' },
  ]);
  assert.match(diff, /^diff --git a\/n\.js b\/n\.js\nnew file mode 100644\n--- \/dev\/null\n\+\+\+ b\/n\.js$/m);
  assert.match(diff, /^rename from o\.js\nrename to r\.js$/m);
  assert.match(diff, /^--- a\/d\.js\n\+\+\+ \/dev\/null$/m);
  assert.deepEqual([...collectAddedPaths(diff)], ['n.js']);
  const { files } = parseDiff(diff);
  assert.ok(files.get('a.js')?.validLines.has(1));
  assert.ok(files.get('n.js')?.validLines.has(1));
});

test('gitlab: loadDiff — raw_diffs, pr в GitHub-форме, head из diff_refs', async () => {
  const { client } = fakeGitlab({
    [`GET ${MR}`]: () => res({ iid: 7, title: 'T', web_url: 'https://gitlab.test/g/p/-/merge_requests/7', draft: false, diff_refs: REFS, sha: 'x' }),
    [`GET ${MR}/raw_diffs`]: () => res(DIFF),
  });
  const loaded = await client.loadDiff();
  assert.deepEqual(loaded.pr, { number: 7, title: 'T', html_url: 'https://gitlab.test/g/p/-/merge_requests/7', draft: false });
  assert.equal(loaded.headSha, REFS.head_sha);
  assert.equal(loaded.diff, DIFF);
});

test('gitlab: loadDiff — нет raw_diffs (404) → сборка из /diffs; черновик → { draft }', async () => {
  const { client } = fakeGitlab({
    [`GET ${MR}`]: () => res({ iid: 7, title: 'T', web_url: 'u', diff_refs: REFS }),
    [`GET ${MR}/diffs`]: () => res([{ old_path: 'a.js', new_path: 'a.js', diff: '@@ -1 +1 @@\n-x\n+y\n' }]),
  });
  const loaded = await client.loadDiff();
  assert.match(loaded.diff, /^diff --git a\/a\.js b\/a\.js\n--- a\/a\.js\n\+\+\+ b\/a\.js\n@@/);

  const draft = fakeGitlab({ [`GET ${MR}`]: () => res({ iid: 7, draft: true, diff_refs: REFS }) });
  assert.deepEqual(await draft.client.loadDiff(), { draft: true });
});

test('gitlab: sticky — читается из обычных заметок, DiffNote и system пропускаются; PUT/POST', async () => {
  const data = { findings: [{ file: 'a.ts', line: 1, title: 'x' }], runs: [{ sha: 'abc' }], sha: 'abc' };
  const sticky = `${MARKERS.review}\nok\n<!-- ${MARKERS.dataKey}${JSON.stringify(data)} -->`;
  const notes = [
    note(1, sticky),
    // Последняя по времени, но чужая: подменить футер (перенос находок) она не должна.
    note(9, `${MARKERS.review}\n<!-- ${MARKERS.dataKey}{"sha":"forged"} -->`, { author: { username: 'dev' } }),
    note(2, `${MARKERS.review} системная`, { system: true }),
    diffNote(3, `${MARKERS.review} в строке`),
  ];
  const withSticky = fakeGitlab({
    [`GET ${MR}/notes`]: () => res(notes),
    [`PUT ${MR}/notes/1`]: () => res({ id: 1 }),
  });
  const prior = await withSticky.client.readPriorStickyData();
  assert.equal(prior.sha, 'abc');
  assert.equal(prior.findings.length, 1);
  assert.equal(await withSticky.client.upsertSticky('new'), 'updated');
  assert.deepEqual(withSticky.calls.at(-1).body, { body: 'new' });

  const empty = fakeGitlab({
    [`GET ${MR}/notes`]: () => res([]),
    [`POST ${MR}/notes`]: () => res({ id: 5 }),
  });
  assert.deepEqual(await empty.client.readPriorStickyData(), { findings: [], runs: [], sha: '' });
  assert.equal(await empty.client.upsertSticky('b'), 'created');
});

test('gitlab: postReview — draft notes + один bulk_publish; 400 и строка вне диффа не роняют остальные', async () => {
  const posted = [];
  const { client, calls } = fakeGitlab({
    [`GET ${MR}/discussions`]: () => res([]),
    [`GET ${MR}`]: () => res({ iid: 7, diff_refs: REFS }),
    [`GET ${MR}/draft_notes`]: () => res([{ id: 90 }]),
    [`DELETE ${MR}/draft_notes/90`]: () => res(''),
    [`POST ${MR}/draft_notes`]: (c) => {
      if (c.body.position.new_line === 12) return res({ message: 'line_code invalid' }, { status: 400 });
      posted.push(c.body);
      return res({ id: 100 + posted.length });
    },
    [`POST ${MR}/draft_notes/bulk_publish`]: () => res('', { status: 204 }),
  });
  const token = await client.postReview({
    inline: [
      { path: 'src/new.js', line: 11, body: 'a' },
      { path: 'src/new.js', line: 12, body: 'b' },
      { path: 'src/new.js', line: 13, body: 'c' },
      { path: 'src/new.js', line: 99, body: 'вне диффа' },
    ],
    headSha: REFS.head_sha,
    diff: DIFF,
  });
  assert.equal(token.posted, 2);
  assert.equal(token.rejected, 2);
  assert.deepEqual(posted.map((p) => [p.note, p.position.new_line, p.position.old_line]), [['a', 11, undefined], ['c', 13, 12]]);
  assert.equal(posted[0].position.old_path, 'src/old.js');
  assert.ok(calls.some((c) => c.method === 'DELETE' && c.path.endsWith('/draft_notes/90')), 'старые черновики удалены до постинга');
  assert.equal(calls.filter((c) => c.path.endsWith('/bulk_publish')).length, 1);
});

test('gitlab: postReview — draft notes недоступны (404) → discussions по одному', async () => {
  const { client, calls } = fakeGitlab({
    [`GET ${MR}/discussions`]: () => res([]),
    [`GET ${MR}`]: () => res({ iid: 7, diff_refs: REFS }),
    [`POST ${MR}/discussions`]: () => res({ id: 'd1' }),
  });
  const token = await client.postReview({ inline: [{ path: 'added.txt', line: 1, body: 'x' }], diff: DIFF });
  assert.equal(token.posted, 1);
  const post = calls.find((c) => c.method === 'POST' && c.path === `${MR}/discussions`);
  assert.deepEqual(post.body.position.new_path, 'added.txt');
  assert.equal(post.body.body, 'x');
});

test('gitlab: post-then-delete — удаляются только инлайны, бывшие до постинга, тред с ответом остаётся', async () => {
  let discussions = [
    { id: 'old', notes: [diffNote(1, `${MARKERS.inline}\nстарый`)] },
    { id: 'replied', notes: [diffNote(2, `${MARKERS.inline}\nс ответом`), note(3, 'не согласен')] },
    { id: 'human', notes: [diffNote(4, 'человек')] },
    // Маркер поставил человек: токен с ролью Maintainer мог бы удалить заметку — не должен.
    { id: 'forged', notes: [diffNote(5, `${MARKERS.inline}\nподделка`, { author: { username: 'dev' } })] },
  ];
  const deleted = [];
  const { client } = fakeGitlab({
    [`GET ${MR}/discussions`]: () => res(discussions),
    [`GET ${MR}`]: () => res({ iid: 7, diff_refs: REFS }),
    [`GET ${MR}/draft_notes`]: () => res([]),
    [`POST ${MR}/draft_notes`]: () => res({ id: 50 }),
    [`POST ${MR}/draft_notes/bulk_publish`]: () => {
      discussions = [...discussions, { id: 'new', notes: [diffNote(60, `${MARKERS.inline}\nновый`)] }];
      return res('', { status: 204 });
    },
    [`DELETE ${MR}/notes/1`]: () => { deleted.push(1); return res(''); },
    [`DELETE ${MR}/notes/60`]: () => { deleted.push(60); return res(''); },
  });
  const token = await client.postReview({ inline: [{ path: 'src/new.js', line: 11, body: `${MARKERS.inline}\nновый` }], diff: DIFF });
  const removed = await client.deleteStaleInline({ exceptReviewId: token });
  assert.equal(removed, 1);
  assert.deepEqual(deleted, [1]);
  // Без токена (cleanup-only ветка пайплайна) — все инлайны движка без ответов.
  assert.equal(await client.deleteStaleInline(), 2);
  assert.deepEqual(deleted, [1, 1, 60]);
});

test('gitlab: треды — нерешённые resolvable-дискуссии, ответ и resolve через REST', async () => {
  const { client, calls } = fakeGitlab({
    [`GET ${MR}/discussions`]: () => res([
      { id: 'open', notes: [diffNote(1, `${MARKERS.inline}\nнаходка`), note(2, 'ответ', { resolvable: true, resolved: false })] },
      { id: 'done', notes: [diffNote(3, 'x', { resolved: true })] },
      { id: 'plain', notes: [note(4, 'обычная заметка')] },
    ]),
    [`POST ${MR}/discussions/open/notes`]: () => res({ id: 9 }),
    [`PUT ${MR}/discussions/open`]: () => res({ id: 'open' }),
  });
  const threads = await client.listUnresolvedReviewThreads();
  assert.deepEqual(threads, [{
    id: 'open', path: 'src/new.js', line: 11,
    comments: [
      { body: `${MARKERS.inline}\nнаходка`, author: 'project_1_bot_x' },
      { body: 'ответ', author: 'project_1_bot_x' },
    ],
  }]);
  await client.replyReviewThread('open', 'r');
  await client.resolveReviewThread('open');
  const put = calls.find((c) => c.method === 'PUT');
  assert.equal(put.query.get('resolved'), 'true');
});

test('gitlab: feedback-форма — инлайны с in_reply_to_id, лейблы, award emoji → +1/-1', async () => {
  const { client } = fakeGitlab({
    [`GET ${MR}/discussions`]: () => res([
      { id: 'd', notes: [diffNote(1, `${MARKERS.inline}\nx`), diffNote(2, 'reply')] },
    ]),
    [`GET ${MR}`]: () => res({ iid: 7, labels: ['ai-review-fp', 'other'], diff_refs: REFS }),
    [`GET ${MR}/notes/1/award_emoji`]: () => res([
      { name: 'thumbsdown', user: { username: 'dev' } },
      { name: 'rocket', user: { username: 'dev2' } },
    ]),
  });
  const inline = await client.listInlineComments();
  assert.equal(inline[0].path, 'src/new.js');
  assert.equal(inline[0].line, 11);
  assert.equal(inline[0].user.login, 'project_1_bot_x');
  assert.equal(inline[1].in_reply_to_id, 1);
  assert.deepEqual(await client.listIssueLabels(), [{ name: 'ai-review-fp' }, { name: 'other' }]);
  assert.deepEqual(await client.listPullCommentReactions(1), [
    { content: '-1', user: { login: 'dev' } },
    { content: 'rocket', user: { login: 'dev2' } },
  ]);
});

test('gitlab: пагинация по Link и ретрай 5xx; токен в PRIVATE-TOKEN', async () => {
  let attempts = 0;
  const { client, calls } = fakeGitlab({
    [`GET ${MR}/notes`]: (c) => {
      if (!c.query.get('page')) {
        attempts++;
        if (attempts === 1) return res('boom', { status: 502 });
        return res([note(1, 'a')], { link: `<${API}${MR}/notes?page=2&per_page=100>; rel="next"` });
      }
      return res([note(2, `${MARKERS.review}\nsticky`)]);
    },
  });
  const sticky = await client.readStickyReview();
  assert.equal(sticky.id, 2);
  assert.equal(attempts, 2);
  assert.equal(calls[0].headers['PRIVATE-TOKEN'], 't');
});

test('gitlab: ссылка пагинации на чужой origin не запрашивается (токен не уходит)', async () => {
  const { client, calls } = fakeGitlab({
    [`GET ${MR}/notes`]: () => res([note(1, 'a')], { link: '<https://evil.test/api/v4/x?page=2>; rel="next"' }),
  });
  await client.readStickyReview();
  assert.ok(calls.every((c) => !String(c.path).startsWith('/x')));
  assert.equal(calls.filter((c) => c.path === `${MR}/notes`).length, 1);
});

test('gitlab: владелец токена не определён → инлайны не чистятся', async () => {
  const { client, calls } = fakeGitlab({
    'GET /user': () => res({ message: '401' }, { status: 401 }),
    [`GET ${MR}/discussions`]: () => res([{ id: 'old', notes: [diffNote(1, `${MARKERS.inline}\nстарый`)] }]),
  });
  assert.equal(await client.deleteStaleInline(), 0);
  assert.ok(!calls.some((c) => c.method === 'DELETE'));
});

test('forge: resolveForge — GITLAB_CI, явный REVIEW_FORGE, GitHub по умолчанию', () => {
  const gl = resolveForge({
    GITLAB_CI: 'true', CI_API_V4_URL: 'https://gl.example/api/v4/', CI_PROJECT_ID: '42',
    CI_MERGE_REQUEST_IID: '7', CI_PROJECT_URL: 'https://gl.example/g/p', AI_REVIEW_GITLAB_TOKEN: 'x',
  }, {});
  assert.deepEqual({ ...gl }, {
    kind: 'gitlab', api: 'https://gl.example/api/v4', token: 'x', repo: '42', prNumber: 7, webUrl: 'https://gl.example/g/p',
  });
  assert.equal(resolveForge({ GITLAB_CI: 'true', REVIEW_FORGE: 'github' }, {}).kind, 'github');
  assert.equal(resolveForge({ REVIEW_FORGE: 'GitLab', CI_PROJECT_PATH: 'g/p' }, {}).webUrl, 'https://gitlab.com/g/p');
  const gh = resolveForge({ GITHUB_REPOSITORY: 'o/r', PR_NUMBER: '3' }, {}, { api: 'https://api.github.com', token: 't' });
  assert.deepEqual({ ...gh }, {
    kind: 'github', api: 'https://api.github.com', token: 't', repo: 'o/r', prNumber: 3, webUrl: 'https://github.com/o/r',
  });
});

test('forge: forgeOf — конфиг без forge (до RFC 16) читается как GitHub; клиент по kind', () => {
  const legacy = { github: { api: 'a', token: 't', repo: 'o/r', prNumber: 2 } };
  assert.equal(forgeOf(legacy).kind, 'github');
  assert.equal(forgeOf(legacy).webUrl, 'https://github.com/o/r');
  assert.equal(createForgeClient(CFG, () => {}, { fetch: async () => res([]) }).kind, 'gitlab');
});

test('feedback: resolveTrustedAuthors — дефолт заменяется владельцем токена, явный allowlist нет', async () => {
  const client = { selfLogin: async () => 'project_1_bot_x' };
  assert.deepEqual(await resolveTrustedAuthors(DEFAULT_TRUSTED_AUTHORS, client), ['project_1_bot_x']);
  assert.deepEqual(await resolveTrustedAuthors(undefined, client), ['project_1_bot_x']);
  assert.deepEqual(await resolveTrustedAuthors('me,you', client), ['me', 'you']);
  assert.deepEqual(await resolveTrustedAuthors(DEFAULT_TRUSTED_AUTHORS, {}), [DEFAULT_TRUSTED_AUTHORS]);
  assert.deepEqual(await resolveTrustedAuthors(DEFAULT_TRUSTED_AUTHORS, { selfLogin: async () => { throw new Error('401'); } }), [DEFAULT_TRUSTED_AUTHORS]);
});

test('codequality: формат отчёта GitLab, отпечаток не зависит от строки', () => {
  const report = buildCodeQuality({
    findings: [
      { file: 'a.js', line: 3, severity: 'critical', title: 'утечка' },
      { file: 'a.js', line: 0, severity: 'nit', title: 'стиль' },
      { file: '', line: 1, severity: 'major', title: 'без файла' },
    ],
  }, { checkPrefix: 'p' });
  assert.equal(report.length, 2);
  assert.deepEqual(report[0].location, { path: 'a.js', lines: { begin: 3 } });
  assert.equal(report[0].severity, 'critical');
  assert.equal(report[0].check_name, 'p/critical');
  assert.equal(report[1].severity, 'info');
  assert.equal(report[1].location.lines.begin, 1);
  const moved = buildCodeQuality({ findings: [{ file: 'a.js', line: 30, severity: 'critical', title: 'утечка' }] });
  assert.equal(moved[0].fingerprint, report[0].fingerprint);
});

test('codequality: writeReviewExports пишет отчёт только на GitLab', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'cq-'));
  try {
    const payload = { findings: [{ file: 'a.js', line: 1, severity: 'major', title: 't' }], blockers: [] };
    const exp = { enabled: true, dir, json: false, sarif: false };
    const gl = await writeReviewExports({ ...CFG, export: exp }, payload);
    assert.deepEqual(gl.written, [join(dir, CODEQUALITY_FILE)]);
    assert.equal(JSON.parse(await readFile(join(dir, CODEQUALITY_FILE), 'utf8')).length, 1);
    const gh = await writeReviewExports({ github: { repo: 'o/r' }, export: { ...exp, dir: join(dir, 'gh') } }, payload);
    assert.deepEqual(gh.written, []);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
