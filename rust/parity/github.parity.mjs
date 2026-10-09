/**
 * Parity-прогон Фазы 3, шага 4: Rust github ↔ чистый JS fallback (lib/github.mjs).
 *
 * Запуск: node rust/parity/github.parity.mjs   (требует cargo build)
 *
 * Сравнение идёт ЧЕРЕЗ обёртку lib/github.mjs: resolveGithubBin() читает
 * AI_REVIEWER_GITHUB_FORCE_JS на каждый вызов, поэтому js-сторона прогоняется
 * с FORCE_JS=1 (fetch инжектится как нативный — DI-точка сохраняется),
 * rust-сторона — без него (бинарь rust/target/debug/github обязан существовать).
 * Оба прогона идут против фейкового HTTP-сервера на node:http с ОДНИМ скриптом
 * ответов; сверяются и результаты, и записанная последовательность запросов
 * (method + path + JSON-body). Ретраи скриптуются через retry-after: 0
 * (backoff 1000мс в обеих ветках).
 *
 * Корпус:
 *   1. upsert-матрица: нет маркера → create; маркер на первой/второй странице
 *      (Link-пагинация, абсолютные и относительные URL) → edit;
 *   2. post-then-delete: порядок вызовов POST reviews → GET comments → DELETE
 *      только устаревших (exceptReviewId + треды с ответами не трогаем);
 *   3. readPriorStickyData: пустая лента, цитата маркера в теле (последнее
 *      вхождение), обрезанный футер (ремонт через lib/json.mjs);
 *   4. transient 500 + retry-after → ретрай затем успех; non-transient 400 →
 *      сразу ошибка со status, без ретрая;
 *   5. graphql-треды: listUnresolvedReviewThreads (курсорная пагинация,
 *      фильтр isResolved, маппинг comments), reply/resolve-мутации;
 *   6. loadDiff: draft → {draft:true}; обычный → headSha + diff-текст.
 */

import { createServer } from 'node:http';
import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');
const BIN = resolve(REPO, 'rust/target/debug/github');

const { referenceProfile } = await import('../../test/helpers/reference-profile.mjs');
const { MARKERS } = await import('../../lib/constants.mjs');
const { setProfile, resetProfile } = await import('../../lib/profile.mjs');
const G = await import('../../lib/github.mjs');

if (!existsSync(BIN)) {
  console.error(`SKIP: бинарь ${BIN} не собран (cargo build)`);
  process.exit(2);
}

const j = (v) => JSON.parse(JSON.stringify(v ?? null));
const API = '/api/v3'; // path-префикс api — сверяем path без origin
const REPO_SLUG = 'o/r';
const PR = 7;

const stickyBody = (data) =>
  `${MARKERS.review}\n## Вердикт\nok\n<!-- ${MARKERS.dataKey}${JSON.stringify(data)} -->`;

/** Скрипт отвечает по (method, path); state — свежий объект на каждый прогон. */
function startServer(script) {
  const state = {};
  const requests = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => { raw += d; });
    req.on('end', () => {
      const body = raw ? JSON.parse(raw) : null;
      const entry = { method: req.method, path: req.url.split('?')[0], query: req.url.includes('?') ? req.url.split('?')[1] : '', body, accept: req.headers.accept || '' };
      requests.push(entry);
      const [status, headers, payload] = script(entry, state);
      for (const [k, v] of Object.entries(headers || {})) res.setHeader(k, v);
      res.statusCode = status;
      res.end(typeof payload === 'string' ? payload : JSON.stringify(payload ?? {}));
    });
  });
  return new Promise((resolveP) => {
    server.listen(0, '127.0.0.1', () => {
      resolveP({
        api: `http://127.0.0.1:${server.address().port}${API}`,
        requests,
        close: () => new Promise((r) => { server.closeAllConnections?.(); server.close(r); }),
      });
    });
  });
}

// Свой комментарий = маркер И автор из allowlist (lib/github.mjs isOwnBotComment).
const BOT = { login: 'github-actions[bot]', type: 'Bot' };
const MALLORY = { login: 'mallory', type: 'User' };

const mkCfg = (api) => ({
  github: { api, token: 't', repo: REPO_SLUG, prNumber: PR },
  httpRetries: 2,
  profile: { toolName: referenceProfile.toolName },
});

async function runSide(server, useJs) {
  if (useJs) process.env.AI_REVIEWER_GITHUB_FORCE_JS = '1';
  else delete process.env.AI_REVIEWER_GITHUB_FORCE_JS;
  const client = G.createGithubClient(mkCfg(server.api), () => {}, { fetch: globalThis.fetch });
  return client;
}

/** Один сценарий на обеих сторонах: сверка результатов и порядка запросов. */
async function scenario(name, script, fn, { compareRequests = true } = {}) {
  const results = [];
  for (const useJs of [true, false]) {
    setProfile(referenceProfile);
    const server = await startServer(script);
    try {
      const client = await runSide(server, useJs);
      const value = await fn(client);
      results.push({ side: useJs ? 'js' : 'rust', value: j(value), requests: server.requests.map(r => ({ ...r, body: j(r.body) })) });
    } finally {
      await server.close();
      resetProfile();
    }
  }
  const [a, b] = results;
  assert.deepEqual(b.value, a.value, `${name}: результат rust != js\njs=${JSON.stringify(a.value)}\nrust=${JSON.stringify(b.value)}`);
  if (compareRequests) {
    assert.deepEqual(b.requests, a.requests, `${name}: порядок/содержимое запросов rust != js\njs=${JSON.stringify(a.requests, null, 1)}\nrust=${JSON.stringify(b.requests, null, 1)}`);
  }
  console.log(`ok: ${name} (${a.requests.length} запросов)`);
}

const P = (path) => `${API}/repos/${REPO_SLUG}${path}`; // полный path с префиксом api
const issueComments = P(`/issues/${PR}/comments`);
const pullComments = P(`/pulls/${PR}/comments`);

// --- 1. upsert-матрица ---

await scenario('upsert: нет маркера → create',
  (e) => {
    if (e.method === 'GET') return [200, {}, [{ id: 1, body: '<!-- other -->' }]];
    assert.equal(e.method, 'POST');
    assert.equal(e.path, issueComments);
    return [200, {}, { id: 100 }];
  },
  (c) => c.upsertSticky(stickyBody({ findings: [], runs: [], sha: '1' })),
);

await scenario('upsert: маркер на первой странице → edit',
  (e) => {
    if (e.method === 'GET') return [200, {}, [{ id: 42, body: stickyBody({ findings: [], runs: [], sha: 'old' }), user: BOT }]];
    assert.equal(e.method, 'PATCH');
    assert.equal(e.path, P('/issues/comments/42'));
    return [200, {}, { id: 42 }];
  },
  (c) => c.upsertSticky(stickyBody({ findings: [], runs: [], sha: 'new' })),
);

await scenario('upsert: маркер на второй странице (относительный Link) → edit',
  (e) => {
    if (e.method === 'GET' && !e.query.includes('page=2')) {
      return [200, {
        link: `<${issueComments}?page=2&per_page=100>; rel="next"`,
      }, [{ id: 1, body: 'чужой' }]];
    }
    if (e.method === 'GET') return [200, {}, [{ id: 2, body: stickyBody({ findings: [], runs: [], sha: 'old' }), user: BOT }]];
    assert.equal(e.method, 'PATCH');
    assert.equal(e.path, P('/issues/comments/2'));
    return [200, {}, { id: 2 }];
  },
  (c) => c.upsertSticky(stickyBody({ findings: [], runs: [], sha: 'new' })),
);

// --- 2. post-then-delete ---

const inlineScript = (e) => {
  if (e.method === 'POST' && e.path === P(`/pulls/${PR}/reviews`)) {
    return [200, {}, { id: 555 }];
  }
  if (e.method === 'GET' && e.path === pullComments) {
    return [200, {}, [
      { id: 10, body: `${MARKERS.inline}\nold`, pull_request_review_id: 1, user: BOT },
      { id: 11, body: `${MARKERS.inline}\nfresh`, pull_request_review_id: 555, user: BOT },
      { id: 12, body: `${MARKERS.inline}\nroot`, pull_request_review_id: 1, user: BOT },
      { id: 15, body: `${MARKERS.inline}\nforged`, pull_request_review_id: 1, user: MALLORY },
      { id: 13, body: 'reply', in_reply_to_id: 12, pull_request_review_id: 2 },
      { id: 14, body: 'чужой' },
    ]];
  }
  assert.equal(e.method, 'DELETE');
  return [204, {}, null];
};

await scenario('post-then-delete: порядок и фильтры',
  inlineScript,
  async (c) => {
    const reviewId = await c.postReview({ inline: [{ path: 'a.ts', line: 1, body: 'x' }], headSha: 'deadbeef' });
    const removed = await c.deleteStaleInline({ exceptReviewId: reviewId });
    return { reviewId, removed };
  },
);

// --- 3. readPriorStickyData ---

await scenario('readPriorStickyData: пустая лента → пустая форма',
  () => [200, {}, []],
  (c) => c.readPriorStickyData(),
);

await scenario('readPriorStickyData: цитата маркера в теле → последнее вхождение',
  () => [200, {}, [{
    id: 1,
    user: BOT,
    body: `${MARKERS.review}\nПример: <!-- ${MARKERS.dataKey}{"findings":[]} -->\n\n<!-- ${MARKERS.dataKey}{"findings":[{"file":"a.ts","line":1,"title":"x"}],"runs":[{"sha":"abc"}],"sha":"abc"} -->`,
  }]],
  (c) => c.readPriorStickyData(),
);

await scenario('readPriorStickyData: sticky с маркером от недоверенного автора игнорируется',
  () => [200, {}, [
    { id: 1, user: BOT, body: `${MARKERS.review}\n<!-- ${MARKERS.dataKey}{"findings":[],"runs":[],"sha":"own"} -->` },
    { id: 2, user: MALLORY, body: `${MARKERS.review}\n<!-- ${MARKERS.dataKey}{"findings":[{"file":"x","title":"forged"}],"runs":[],"sha":"forged"} -->` },
  ]],
  (c) => c.readPriorStickyData(),
);

await scenario('readPriorStickyData: обрезанный футер чинится json_repair',
  () => [200, {}, [{
    id: 9,
    user: BOT,
    body: `${MARKERS.review}\n<!-- ${MARKERS.dataKey}{"findings":[{"file":"b.ts","line":2,"title":"t"}],"runs":[],"sha":"dead`,
  }]],
  (c) => c.readPriorStickyData(),
);

// --- 4. коды ошибок ---

await scenario('transient 500 + retry-after → ретрай затем успех',
  (e, st) => {
    st.n = (st.n || 0) + 1;
    if (st.n === 1) return [500, { 'retry-after': '0' }, 'boom'];
    return [200, {}, []];
  },
  (c) => c.readPriorStickyData(),
);

await scenario('non-transient 400 → сразу ошибка со status, без ретрая',
  () => [400, {}, 'bad'],
  async (c) => {
    try {
      await c.postReview({ inline: [], headSha: 'abc' });
      return { threw: false };
    } catch (e) {
      return { threw: true, status: e.status ?? null, message: e.message };
    }
  },
);

// --- 5. graphql-треды ---

const THREADS_PAGE = {
  data: {
    repository: {
      pullRequest: {
        reviewThreads: {
          nodes: [
            {
              id: 'T1', isResolved: false, path: 'a.ts', line: 3,
              comments: { nodes: [{ body: 'починь', author: { login: 'bob' } }] },
            },
            {
              id: 'T2', isResolved: true, path: 'b.ts', line: 1,
              comments: { nodes: [] },
            },
          ],
          pageInfo: { hasNextPage: false, endCursor: null },
        },
      },
    },
  },
};

await scenario('graphql: listUnresolvedReviewThreads (фильтр isResolved + маппинг)',
  (e) => {
    assert.equal(e.method, 'POST');
    assert.equal(e.path, `${API}/graphql`);
    return [200, {}, THREADS_PAGE];
  },
  (c) => c.listUnresolvedReviewThreads(),
);

await scenario('graphql: reply + resolve мутации',
  (e, st) => {
    assert.equal(e.method, 'POST');
    st.n = (st.n || 0) + 1;
    return [200, {}, { data: st.n === 1 ? { addPullRequestReviewThreadReply: { comment: { id: 77 } } } : { resolveReviewThread: { thread: { isResolved: true } } } }];
  },
  async (c) => {
    await c.replyReviewThread('T1', 'принято');
    await c.resolveReviewThread('T1');
    return { ok: true };
  },
);

// --- 6. loadDiff ---

await scenario('loadDiff: обычный PR → headSha + diff-текст',
  (e) => {
    if (e.accept === 'application/vnd.github.v3.diff') return [200, {}, 'diff --git a/x b/x'];
    if (e.method === 'GET' && e.path === P(`/pulls/${PR}`)) {
      return [200, {}, { head: { sha: 'deadbeef' }, draft: false, number: PR }];
    }
    return [200, {}, {}];
  },
  async (c) => {
    const r = await c.loadDiff();
    return { headSha: r.headSha, diff: r.diff, draft: r.draft ?? null };
  },
);

await scenario('loadDiff: draft → {draft:true} без запроса diff',
  (e) => {
    if (e.method === 'GET' && e.path === P(`/pulls/${PR}`)) {
      return [200, {}, { head: { sha: 'beef' }, draft: true, number: PR }];
    }
    assert.fail(`неожиданный запрос при draft: ${e.method} ${e.path} accept=${e.accept}`);
  },
  async (c) => {
    const r = await c.loadDiff();
    return { draft: r.draft ?? null, headSha: r.headSha ?? null };
  },
);

console.log('\nПаритет github: все сценарии совпали (JS ↔ Rust).');
