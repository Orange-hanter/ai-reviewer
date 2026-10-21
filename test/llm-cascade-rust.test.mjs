/**
 * Каскад ретраев chatJson/chatText через Rust-бинарь (rust/src/llm/cascade.rs) — сквозь
 * JS-обёртку lib/llm.mjs и настоящий HTTP. Сервер ведёт себя как модель OpenRouter с
 * обязательным reasoning: effort=none → 400 «Reasoning is mandatory»
 * (docs/probe-openrouter-2026-10-14.md, п. 1).
 *
 * Без собранного бинаря тест пропускается: JS-копия каскада deprecated и этого правила
 * не знает (proposals/20-rust-primary.md). fetch здесь не перехватывается — иначе
 * обёртка сама уйдёт на JS-ветку.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { existsSync } from 'node:fs';
import { createLlmClient } from '../lib/llm.mjs';
import { defaultRustBin } from '../lib/rust-bin.mjs';

const BIN = defaultRustBin('llm');
const MANDATORY = '{"error":{"message":"Reasoning is mandatory for this endpoint and cannot be disabled."}}';

const sse = (text) => `data: ${JSON.stringify({ choices: [{ delta: { content: text } }] })}\n\n`
  + `data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } })}\n\n`
  + 'data: [DONE]\n\n';
const sseEmpty = () => `data: ${JSON.stringify({ choices: [{ delta: { reasoning: 'думаю' }, finish_reason: 'length' }] })}\n\ndata: [DONE]\n\n`;

/** Сервер-сценарий: на каждый запрос — следующий ответ; тела запросов копятся в seen. */
async function scriptedServer(responses) {
  const seen = [];
  const server = createServer((req, res) => {
    let body = '';
    req.on('data', (d) => { body += d; });
    req.on('end', () => {
      seen.push(JSON.parse(body));
      const [status, payload] = responses.shift() || [500, 'сценарий исчерпан'];
      res.writeHead(status, { 'content-type': status === 200 ? 'text/event-stream' : 'application/json' });
      res.end(payload);
    });
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return { seen, base: `http://127.0.0.1:${server.address().port}`, close: () => server.close() };
}

function client(base, logs, records, ollama = {}) {
  return createLlmClient({
    ollama: { base, key: 'k', jsonMode: true, seed: 7, requestTimeoutMs: 20_000, ...ollama },
    moa: { refMaxTokens: 6000, refTemp: 0.3 },
    httpRetries: 0,
  }, (...a) => logs.push(a.join(' ')), { costTracker: { record: (u) => records.push(u) } });
}

const skip = !existsSync(BIN) || process.env.AI_REVIEWER_LLM_FORCE_JS === '1'
  ? 'нет бинаря llm (cargo build или архив релиза в rust/bin) — нечего проверять' : false;

test('chatJson: пустой ответ → none → 400 mandatory → low с +8000 токенов', { skip }, async () => {
  const srv = await scriptedServer([[200, sseEmpty()], [400, MANDATORY], [200, sse('{"findings":[]}')]]);
  const logs = [], records = [];
  try {
    const out = await client(srv.base, logs, records).chatJson([{ role: 'user', content: 'x' }], 'gemini');
    assert.deepEqual(out, { findings: [] });
    assert.deepEqual(srv.seen.map((b) => [b.reasoning_effort, b.max_tokens, !!b.response_format]), [
      ['low', 24000, true], ['none', 24000, true], ['low', 32000, true],
    ]);
    assert.equal(records.length, 1, 'usage только у дошедшего ответа');
    assert.ok(logs.some((l) => /reasoning обязателен/.test(l)), logs.join('\n'));
  } finally { srv.close(); }
});

test('chatText: пустой ответ → none → 400 mandatory → low', { skip }, async () => {
  const srv = await scriptedServer([[200, sseEmpty()], [400, MANDATORY], [200, sse('замечаний нет')]]);
  const usages = [];
  try {
    const text = await client(srv.base, [], []).chatText([{ role: 'user', content: 'x' }], 'glm', {
      onUsage: (u) => usages.push(u),
    });
    assert.equal(text, 'замечаний нет');
    assert.deepEqual(srv.seen.map((b) => [b.reasoning_effort, b.max_tokens]), [
      ['low', 6000], ['none', 6000], ['low', 14000],
    ]);
    assert.equal(usages.length, 1);
  } finally { srv.close(); }
});

test('chatText: пустой ответ с usage учитывается в цене, причина — в логе', { skip }, async () => {
  const emptyPaid = `data: ${JSON.stringify({ choices: [{ delta: { reasoning: 'думаю' }, finish_reason: 'length' }] })}\n\n`
    + `data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 16000, completion_tokens: 6000, total_tokens: 22000 } })}\n\n`
    + 'data: [DONE]\n\n';
  const srv = await scriptedServer([[200, emptyPaid], [200, sse('ответ')]]);
  const logs = [];
  const usages = [];
  try {
    const text = await client(srv.base, logs, []).chatText([{ role: 'user', content: 'x' }], 'deepseek', {
      onUsage: (u) => usages.push(u),
    });
    assert.equal(text, 'ответ');
    assert.equal(usages.length, 2, 'холостой вызов тоже оплачен');
    assert.equal(usages[0].completion_tokens, 6000);
    assert.ok(logs.some((l) => /content пуст на effort=low \(пустой content \(finish=length/.test(l)), logs.join('\n'));
  } finally { srv.close(); }
});

test('chatJson: сбой каскада отдаёт флаги и не теряет usage неразобранного ответа', { skip }, async () => {
  const srv = await scriptedServer([[200, sse('просто текст без JSON')]]);
  const records = [];
  try {
    await assert.rejects(
      client(srv.base, [], records).chatJson([{ role: 'user', content: 'x' }], 'm'),
      (e) => /нет JSON/.test(e.message) && e.truncated === false && e.badEscape === false,
    );
    assert.equal(srv.seen.length, 1);
    assert.equal(records.length, 1);
  } finally { srv.close(); }
});

test('квота провайдера: без ретраев, остальные вызовы клиента отказывают без сети', { skip }, async () => {
  const weekly = '{"error":{"message":"you (x) have reached your weekly usage limit, upgrade for higher limits"}}';
  const srv = await scriptedServer([[429, weekly], [200, sse('не должен понадобиться')]]);
  const logs = [];
  try {
    const c = client(srv.base, logs, []);
    await assert.rejects(
      c.chatJson([{ role: 'user', content: 'x' }], 'm1'),
      (e) => e.quota === true && /квота провайдера исчерпана/.test(e.message),
    );
    await assert.rejects(
      c.chatText([{ role: 'user', content: 'x' }], 'm2'),
      (e) => e.quota === true,
    );
    assert.equal(srv.seen.length, 1, 'второй вызов не ходил в сеть, первый — без ретраев');
    assert.ok(logs.some((l) => /квота провайдера LLM исчерпана/.test(l)), logs.join('\n'));
    assert.ok(!logs.some((l) => /429 → снижение параллелизма/.test(l)), 'квота не режет семафоры');
  } finally { srv.close(); }
});

test('LLM_MODEL_SETTINGS: effort, jsonEffort, maxTokens и provider — свои у каждой модели', { skip }, async () => {
  const srv = await scriptedServer([[200, sse('текст')], [200, sse('{"findings":[]}')], [200, sse('текст')]]);
  const logs = [];
  try {
    const c = client(srv.base, logs, [], {
      modelSettings: {
        'deepseek/deepseek-v4.1-flash': { effort: 'none', maxTokens: 4000, provider: { ignore: ['OpenInference'] } },
        'z-ai/glm-5.3-flash': { effort: 'low', jsonEffort: 'medium' },
      },
      modelSettingsErrors: ['LLM_MODEL_SETTINGS[x]: неизвестное поле «efort»'],
    });
    await c.chatText([{ role: 'user', content: 'x' }], 'deepseek/deepseek-v4.1-flash');
    await c.chatJson([{ role: 'user', content: 'x' }], 'z-ai/glm-5.3-flash');
    await c.chatText([{ role: 'user', content: 'x' }], 'other/model');
    const [ds, glm, other] = srv.seen;
    assert.equal(ds.reasoning_effort, 'none');
    assert.equal(ds.max_tokens, 4000);
    assert.deepEqual(ds.provider, { ignore: ['OpenInference'] });
    assert.equal(glm.reasoning_effort, 'medium', 'jsonEffort главнее effort для JSON-вызовов');
    assert.equal(glm.provider, undefined);
    assert.equal(other.reasoning_effort, 'low', 'модель без настроек — прежнее поведение');
    assert.equal(other.max_tokens, 6000);
    assert.ok(logs.some((l) => /неизвестное поле «efort»/.test(l)), 'ошибки разбора видны в логе');
  } finally { srv.close(); }
});

test('лимит :free-модели глушит только её, платная модель того же ключа работает', { skip }, async () => {
  const freeLimit = '{"error":{"message":"Rate limit exceeded: free-models-per-day. Add 10 credits to unlock 1000 free model requests per day","code":429}}';
  const srv = await scriptedServer([[429, freeLimit], [200, sse('{"verdicts":[]}')]]);
  const logs = [];
  try {
    const c = client(srv.base, logs, []);
    await assert.rejects(c.chatJson([{ role: 'user', content: 'x' }], 'minimax/minimax-m3.1-flash:free'), (e) => e.quota === true);
    await assert.rejects(c.chatJson([{ role: 'user', content: 'x' }], 'minimax/minimax-m3.1-flash:free'), (e) => e.quota === true);
    assert.deepEqual(await c.chatJson([{ role: 'user', content: 'x' }], 'deepseek/pro'), { verdicts: [] });
    assert.equal(srv.seen.length, 2, 'второй вызов :free — без сети, платная модель — в сеть');
    assert.equal(srv.seen[1].model, 'deepseek/pro');
    assert.ok(logs.some((l) => /лимит модели исчерпан/.test(l)), logs.join('\n'));
  } finally { srv.close(); }
});
