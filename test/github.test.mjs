// Контракт GitHub-клиента: sticky create/update, empty prior, pagination, 5xx retry, postReview.
// fetch инжектится в createGithubClient — без мутации globalThis (безопасно при parallel files).
import { referenceProfile } from './helpers/reference-profile.mjs';
import { test, mock } from 'node:test';
import assert from 'node:assert/strict';
import { MARKERS } from '../lib/constants.mjs';
import { createGithubClient } from '../lib/github.mjs';

const API = 'https://api.github.test';
const CFG = {
  github: { api: API, token: 't', repo: 'o/r', prNumber: 7 },
  httpRetries: 2,
  profile: { toolName: referenceProfile.toolName },
};

function jsonRes(body, { status = 200, link = '', headers = {} } = {}) {
  const h = new Headers(headers);
  if (link) h.set('link', link);
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: h,
    text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
  };
}

function withFetch(handler, fn) {
  const fetchMock = mock.fn(handler);
  const client = createGithubClient(CFG, () => {}, { fetch: fetchMock });
  return fn(client, fetchMock);
}

const stickyBody = (data) =>
  `${MARKERS.review}\n## Вердикт\nok\n<!-- ${MARKERS.dataKey}${JSON.stringify(data)} -->`;

test('github: readPriorStickyData — нет sticky → пустая форма (не []), findings.length безопасен', async () => {
  await withFetch(async () => jsonRes([]), async (client) => {
    const data = await client.readPriorStickyData();
    assert.deepEqual(data, { findings: [], runs: [], sha: '' });
    assert.equal(data.findings.length, 0);
  });
});

test('github: readPriorStickyData — парсит последний data-футер, игнорирует цитату в теле', async () => {
  const quoted = `<!-- ${MARKERS.dataKey}{"findings":[]} -->`;
  const real = { findings: [{ file: 'a.ts', line: 1, title: 'x' }], runs: [{ sha: 'abc' }], sha: 'abc' };
  const body = `${MARKERS.review}\nПример: ${quoted}\n\n<!-- ${MARKERS.dataKey}${JSON.stringify(real)} -->`;
  await withFetch(async () => jsonRes([{ id: 1, body }]), async (client) => {
    const data = await client.readPriorStickyData();
    assert.equal(data.sha, 'abc');
    assert.equal(data.findings.length, 1);
    assert.equal(data.findings[0].file, 'a.ts');
    assert.equal(data.runs.length, 1);
  });
});

test('github: readPriorStickyData — обрезанный футер чинится через extractJsonObject', async () => {
  const truncated = `${MARKERS.review}\n<!-- ${MARKERS.dataKey}{"findings":[{"file":"b.ts","line":2,"title":"t"}],"runs":[],"sha":"dead`;
  await withFetch(async () => jsonRes([{ id: 9, body: truncated }]), async (client) => {
    const data = await client.readPriorStickyData();
    assert.equal(data.findings[0]?.file, 'b.ts');
    assert.equal(data.sha, 'dead');
  });
});

test('github: upsertSticky — нет prior → POST created; есть marker → PATCH updated', async () => {
  const calls = [];
  await withFetch(async (url, opts = {}) => {
    calls.push({ url: String(url), method: opts.method || 'GET', body: opts.body });
    if (String(url).includes('/comments?') || /\/comments$/.test(String(url).split('?')[0])) {
      if ((opts.method || 'GET') === 'GET') return jsonRes([]);
    }
    return jsonRes({ id: 1 });
  }, async (client) => {
    const r = await client.upsertSticky(stickyBody({ findings: [], runs: [], sha: '1' }));
    assert.equal(r, 'created');
    assert.ok(calls.some((c) => c.method === 'POST' && c.url.includes('/issues/7/comments')));
  });

  calls.length = 0;
  const existing = { id: 42, body: stickyBody({ findings: [], runs: [], sha: 'old' }) };
  await withFetch(async (url, opts = {}) => {
    calls.push({ url: String(url), method: opts.method || 'GET', body: opts.body });
    if ((opts.method || 'GET') === 'GET') return jsonRes([existing]);
    return jsonRes({ id: 42 });
  }, async (client) => {
    const r = await client.upsertSticky(stickyBody({ findings: [], runs: [], sha: 'new' }));
    assert.equal(r, 'updated');
    const patch = calls.find((c) => c.method === 'PATCH');
    assert.ok(patch?.url.includes('/issues/comments/42'));
    const payload = JSON.parse(patch.body);
    assert.match(payload.body, /"sha":"new"/);
  });
});

test('github: ghList — пагинация по Link rel=next', async () => {
  await withFetch(async (url, opts = {}) => {
    const u = String(url);
    if ((opts.method || 'GET') === 'GET' && u.includes('comments')) {
      if (u.includes('page=2')) return jsonRes([{ id: 2, body: 'x' }]);
      return jsonRes([{ id: 1, body: 'x' }], {
        link: `<${API}/repos/o/r/issues/7/comments?page=2&per_page=100>; rel="next"`,
      });
    }
    return jsonRes({ id: 1 });
  }, async (client, fetchMock) => {
    await client.upsertSticky(`${MARKERS.review}\nhi`);
    const urls = fetchMock.mock.calls.map((c) => String(c.arguments[0]));
    assert.ok(urls.some((u) => u.includes('page=2')), 'вторая страница запрошена');
    assert.ok(fetchMock.mock.callCount() >= 3, 'list page1 + page2 + POST');
  });
});

test('github: transient 500 → retry с Retry-After, затем успех', async () => {
  let n = 0;
  await withFetch(async () => {
    n += 1;
    if (n === 1) return jsonRes('boom', { status: 500, headers: { 'retry-after': '0.01' } });
    return jsonRes([]);
  }, async (client) => {
    const data = await client.readPriorStickyData();
    assert.equal(n, 2);
    assert.deepEqual(data.findings, []);
  });
});

test('github: non-transient 400 — сразу стоп без retry', async () => {
  let n = 0;
  await withFetch(async () => {
    n += 1;
    return jsonRes('bad', { status: 400 });
  }, async (client) => {
    await assert.rejects(() => client.postReview({ inline: [], headSha: 'abc' }), /400/);
    assert.equal(n, 1);
  });
});

test('github: postReview — commit_id из headSha, возвращает id review', async () => {
  await withFetch(async (_url, opts = {}) => {
    const body = JSON.parse(opts.body);
    assert.equal(body.commit_id, 'deadbeef');
    assert.equal(body.event, 'COMMENT');
    assert.equal(body.comments.length, 1);
    return jsonRes({ id: 555 });
  }, async (client) => {
    const id = await client.postReview({
      inline: [{ path: 'a.ts', line: 1, body: 'x' }],
      headSha: 'deadbeef',
    });
    assert.equal(id, 555);
  });
});

test('github: deleteStaleInline — exceptReviewId и треды с ответами не трогает', async () => {
  const deleted = [];
  await withFetch(async (url, opts = {}) => {
    const u = String(url);
    if ((opts.method || 'GET') === 'GET') {
      return jsonRes([
        { id: 10, body: `${MARKERS.inline}\nold`, pull_request_review_id: 1 },
        { id: 11, body: `${MARKERS.inline}\nfresh`, pull_request_review_id: 99 },
        { id: 12, body: `${MARKERS.inline}\nroot`, pull_request_review_id: 1 },
        { id: 13, body: 'reply', in_reply_to_id: 12, pull_request_review_id: 2 },
      ]);
    }
    if (opts.method === 'DELETE') {
      deleted.push(u);
      return jsonRes({});
    }
    return jsonRes({});
  }, async (client) => {
    const removed = await client.deleteStaleInline({ exceptReviewId: 99 });
    assert.equal(removed, 1, 'только id=10; 11 excepted, 12 has replies');
    assert.ok(deleted.some((u) => u.endsWith('/pulls/comments/10')));
    assert.ok(!deleted.some((u) => u.endsWith('/pulls/comments/11')));
    assert.ok(!deleted.some((u) => u.endsWith('/pulls/comments/12')));
  });
});
