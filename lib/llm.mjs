/**
 * Фаза 3, шаг 1: lib/llm.mjs — асинхронная обёртка над Rust-бинарём
 * rust/target/debug/llm (порт chatRequest: OpenAI-compatible /chat/completions,
 * ручной SSE-стриминг, ретраи с backoff, бюджет таймаута, fail-open по
 * stream_options/include_usage). Это первый async/сетевой модуль миграции:
 * бинарь делает весь HTTP-вызов сам (reqwest), обёртка зовёт его через
 * execFile (async), публичное API (chatJson/chatText) осталось прежним.
 *
 * Резолвинг бинаря (по аналогии с lib/json.mjs):
 *   AI_REVIEWER_LLM_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/llm;
 *   AI_REVIEWER_LLM_FORCE_JS=1 — принудительно чистый JS (паритетные прогоны/отладка).
 *
 * Дополнительное условие деградации на JS: globalThis.fetch перехвачен с момента
 * загрузки модуля. Тесты (test/pr-review.engine.test.mjs) и потребители фейкают
 * сеть через DI именно в fetch — Rust-бинарь эту подмену не видит, поэтому при
 * перехваченном fetch работает эталонный JS, сохраняющий injectable-точку.
 *
 * Что намеренно осталось в JS (бинарь — одноразовый процесс на запрос):
 *   - адаптивные семафоры (глобальный + per-model): сквозное состояние между
 *     запросами клиента, внутрипроцессные семафоры бинаря были бы бессмысленны;
 *     бинарь сообщает о 429 флагом `throttled`, обёртка режет лимиты (AIMD);
 *   - JSON-mode graceful fallback (effort=none, больше токенов, снятие
 *     response_format) поверх extractJsonObject (lib/json.mjs → json_repair);
 *   - costTracker (учёт usage) и observability.
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
import { sleep, isTransient, backoffMs, createAdaptiveSemaphore, redactSecretsForLog } from './util.mjs';
import { extractJsonObject } from './json.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_LLM_BIN = resolve(__dir, '../rust/target/debug/llm');
// Нативный fetch на момент загрузки модуля: перехват = мок/DI → эталонный JS.
const NATIVE_FETCH = typeof globalThis.fetch === 'function' ? globalThis.fetch : null;

function resolveLlmBin() {
  if (process.env.AI_REVIEWER_LLM_FORCE_JS === '1') return null;
  // fetch перехвачен (тесты/DI) — Rust-бинарь подмену не видит, сохраняем эталонный JS.
  if (!NATIVE_FETCH || globalThis.fetch !== NATIVE_FETCH) return null;
  const bin = (process.env.AI_REVIEWER_LLM_BIN || '').trim() || DEFAULT_LLM_BIN;
  return existsSync(bin) ? bin : null;
}

/**
 * Вызов Rust-бинаря: один chatRequest целиком (HTTP + SSE + ретраи внутри).
 * spawn + ручной stdin: async execFile с input на Node 22 не закрывает stdin
 * ребёнка (проверено: зависает даже на `cat`), поэтому promisify(execFile) не годится.
 */
function runRustChat(bin, input, { execTimeoutMs }) {
  return new Promise((resolve, reject) => {
    const child = spawn(bin, [], { stdio: ['pipe', 'pipe', 'pipe'], env: process.env });
    let stdout = '', stderr = '';
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(Object.assign(new Error(`llm binary timeout после ${execTimeoutMs}мс`), { killed: true }));
    }, execTimeoutMs);
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
        resolve({ __error: info });
        return;
      }
      reject(Object.assign(
        new Error(`llm binary завершился code=${code} signal=${signal}: ${stderr.slice(0, 400)}`),
        { code, signal },
      ));
    });
    child.stdin.on('error', () => {});
    child.stdin.write(input);
    child.stdin.end();
  });
}

function parseErrorPayload(stderr) {
  try {
    const info = JSON.parse(String(stderr || '').trim());
    return info && typeof info.error === 'string' ? info : null;
  } catch {
    return null;
  }
}

/** Фабрика клиента OpenAI-compatible LLM — JSON для aggregator, prose для MoA-advisors. */
export function createLlmClient(cfg, log, { costTracker } = {}) {
  const { ollama: o, moa, httpRetries } = cfg;

  const globalSem = createAdaptiveSemaphore(o.providerConcurrency || 12);
  const modelSems = new Map();
  const semFor = (model) => {
    if (!modelSems.has(model)) modelSems.set(model, createAdaptiveSemaphore(o.providerPerModel || 6));
    return modelSems.get(model);
  };

  function recordUsage(usage, model, callType) {
    if (!usage) return;
    costTracker?.record?.(usage, model, callType);
  }

  /** 429 → AIMD-троттлинг семафоров (единая точка для Rust- и JS-путей). */
  function throttleFor(model) {
    const g = globalSem, m = semFor(model);
    g.throttle();
    m.throttle();
    log(`[${model}] 429 → снижение параллелизма (global: ${JSON.stringify(g.stats())}, model: ${JSON.stringify(m.stats())})`);
  }

  async function chatRequest(messages, { model, maxTokens, temperature, effort, jsonMode, callType = 'chat' }) {
    const timeoutMs = o.requestTimeoutMs || 240_000;
    const g = globalSem, m = semFor(model);
    let gHeld = false, mHeld = false;
    try {
      await g.acquire();
      gHeld = true;
      await m.acquire();
      mHeld = true;
    } catch (e) {
      if (mHeld) m.release();
      if (gHeld) g.release();
      throw e;
    }
    try {
      const bin = resolveLlmBin();
      if (bin) {
        const out = await runRustChat(bin, JSON.stringify({
          url: `${o.base}/chat/completions`,
          key: o.key,
          model,
          messages,
          maxTokens: maxTokens ?? null,
          temperature: temperature ?? null,
          effort: effort ?? null,
          jsonMode: !!jsonMode,
          seed: o.seed ?? null,
          httpRetries,
          timeoutMs,
          includeUsage: true,
        }), { execTimeoutMs: Math.max(timeoutMs * 2, 30_000) });
        if (out.__error) {
          if (out.__error.throttled) throttleFor(model);
          throw Object.assign(new Error(redactSecretsForLog(out.__error.error, 400)), {
            status: out.__error.status ?? undefined,
            empty: !!out.__error.empty,
          });
        }
        if (out.throttled) throttleFor(model);
        recordUsage(out.usage, model, callType);
        return { content: out.content, usage: out.usage };
      }
      const out = await chatRequestJs(messages, { model, maxTokens, temperature, effort, jsonMode, timeoutMs });
      recordUsage(out.usage, model, callType);
      return out;
    } finally { m.release(); g.release(); }
  }

  async function chatJsonOnce(messages, { model, maxTokens, effort, temperature, jsonMode, callType }) {
    const { content, usage } = await chatRequest(messages, { model, maxTokens, temperature, effort, jsonMode, callType });
    let repaired = null;
    const result = extractJsonObject(content, {
      onRepaired: (kind) => {
        repaired = kind;
        log(
          kind === 'escapes'
            ? 'JSON: исправлены невалидные escape-последовательности в ответе модели'
            : 'JSON восстановлен из оборванного ответа модели (усечение)',
        );
      },
    });
    if (repaired && result && typeof result === 'object') {
      Object.defineProperty(result, '__repaired', { value: repaired, enumerable: false });
    }
    if (usage && result && typeof result === 'object') {
      Object.defineProperty(result, '__usage', { value: usage, enumerable: false });
    }
    return result;
  }

  async function chatJson(messages, model, { temperature = o.temperature, callType = 'review_json' } = {}) {
    const jm = o.jsonMode;
    try {
      return await chatJsonOnce(messages, { maxTokens: 24000, effort: 'low', model, jsonMode: jm, temperature, callType });
    } catch (e) {
      if (e.empty) {
        log(`[${model}] content пуст на effort=low → повтор effort=none:`, e.message);
        return chatJsonOnce(messages, { maxTokens: 24000, effort: 'none', model, jsonMode: jm, temperature, callType });
      }
      if (e.truncated) {
        log(`[${model}] JSON оборван → повтор effort=none + больше токенов:`, e.message);
        return chatJsonOnce(messages, { maxTokens: 32000, effort: 'none', model, jsonMode: jm, temperature, callType });
      }
      if (e.badEscape || /Bad escaped|Unexpected (?:token|end).*JSON/i.test(e.message || '')) {
        log(`[${model}] битые escape → повтор effort=none + больше токенов (без JSON-mode):`, e.message);
        return chatJsonOnce(messages, { maxTokens: 32000, effort: 'none', model, jsonMode: false, temperature, callType });
      }
      if (jm && (e.status === 400 || /response_format/i.test(e.message || ''))) {
        log(`[${model}] ошибка, возможно из-за JSON-mode → повтор без него:`, e.message);
        return chatJsonOnce(messages, { maxTokens: 24000, effort: 'low', jsonMode: false, model, temperature, callType });
      }
      throw e;
    }
  }

  async function chatText(messages, model, { maxTokens = moa.refMaxTokens, temperature = moa.refTemp, callType = 'advisor' } = {}) {
    try {
      const { content } = await chatRequest(messages, {
        model, maxTokens, temperature, effort: o.effort || 'low', jsonMode: false, callType,
      });
      return content;
    } catch (e) {
      if (e.empty) {
        log(`[${model}] advisor content пуст на effort=low → effort=none`);
        const { content } = await chatRequest(messages, {
          model, maxTokens, temperature, effort: 'none', jsonMode: false, callType,
        });
        return content;
      }
      throw e;
    }
  }

  // -------------------------------------------------------------------------
  // Чистый JS fallback — эталонная копия прежней реализации (lib/llm.mjs до
  // Фазы 3). Используется когда Rust-бинарь отсутствует ИЛИ fetch перехвачен
  // (моки/DI); при битом бинаре НЕ подменяет его. Семафоры держит диспетчер
  // chatRequest выше (в прежней реализации они брались здесь же — эквивалентно).
  // -------------------------------------------------------------------------

  async function chatRequestJs(messages, { model, maxTokens, temperature, effort, jsonMode, timeoutMs }) {
    // stream_options.include_usage — не везде; при 400 снимаем и повторяем (fail-open),
    // без расхода слота httpRetries.
    let includeUsage = true;
    const t0 = Date.now();
    const budgetLeft = () => Date.now() - t0 < timeoutMs;
    for (let attempt = 0; ; ) {
      const payload = JSON.stringify({
        model, messages, temperature, stream: true, max_tokens: maxTokens,
        reasoning_effort: effort, seed: o.seed,
        ...(includeUsage ? { stream_options: { include_usage: true } } : {}),
        ...(jsonMode ? { response_format: { type: 'json_object' } } : {}),
      });
      const ctrl = new AbortController();
      const t = setTimeout(() => ctrl.abort(), timeoutMs);
      try {
        const res = await fetch(`${o.base}/chat/completions`, {
          method: 'POST',
          headers: { Authorization: `Bearer ${o.key}`, 'Content-Type': 'application/json' },
          body: payload,
          signal: ctrl.signal,
        });
        if (!res.ok) {
          const body = await res.text().catch(() => '');
          if (res.status === 429) throttleFor(model);
          if (res.status === 400 && includeUsage
            && /stream_options|include_usage/i.test(body)) {
            includeUsage = false;
            log(`[${model}] LLM 400 (stream_options/include_usage) → повтор без него: ${redactSecretsForLog(body, 120)}`);
            await sleep(backoffMs(0, null));
            continue;
          }
          if (isTransient(res.status) && attempt < httpRetries && budgetLeft()) {
            const wait = backoffMs(attempt, res.headers.get('retry-after'));
            log(`[${model}] LLM ${res.status} → повтор через ${wait}мс (${attempt + 1}/${httpRetries})`);
            await sleep(wait);
            attempt++;
            continue;
          }
          throw Object.assign(new Error(`LLM ${res.status}: ${redactSecretsForLog(body, 400)}`), { status: res.status });
        }
        return await readLlmSseContentJs(res);
      } catch (e) {
        const networky = e.status === undefined && !e.empty && !e.truncated &&
          /fetch failed|network|socket|ECONN|ETIMEDOUT|EAI_AGAIN|terminated|aborted/i.test(e.message || '');
        if (networky && attempt < httpRetries && budgetLeft()) {
          const wait = backoffMs(attempt);
          log(`[${model}] сетевой сбой LLM (${redactSecretsForLog(e?.message ?? e, 120)}) → повтор через ${wait}мс (${attempt + 1}/${httpRetries})`);
          await sleep(wait);
          attempt++;
          continue;
        }
        throw e;
      } finally { clearTimeout(t); }
    }
  }

  return { chatJson, chatText };
}

// ---------------------------------------------------------------------------
// SSE-парсинг чистого JS fallback'а (эталон readLlmSseContent).
// ---------------------------------------------------------------------------

/** Читает OpenAI-compatible SSE `/chat/completions`; usage — из финального chunk при наличии. */
async function readLlmSseContentJs(res) {
  let content = '', reasoningLen = 0, finish = null, buf = '';
  let usage = null;
  const dec = new TextDecoder();
  const takeLine = (line) => {
    line = line.trim();
    if (!line.startsWith('data:')) return false;
    const data = line.slice(5).trim();
    if (data === '[DONE]') return true;
    let j; try { j = JSON.parse(data); } catch { return false; }
    const d = j.choices?.[0];
    if (d?.delta?.content) content += d.delta.content;
    if (d?.delta?.reasoning) reasoningLen += d.delta.reasoning.length;
    if (d?.finish_reason) finish = d.finish_reason;
    if (j.usage && typeof j.usage === 'object') usage = j.usage;
    return false;
  };
  let done = false;
  for await (const chunk of res.body) {
    buf += dec.decode(chunk, { stream: true });
    let nl;
    while ((nl = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, nl); buf = buf.slice(nl + 1);
      if (takeLine(line)) { done = true; buf = ''; break; }
    }
    if (done) break;
  }
  if (!done && buf.trim()) takeLine(buf);
  if (!content.trim()) {
    throw Object.assign(new Error(`пустой content (finish=${finish}, reasoning=${reasoningLen}симв)`), { empty: true });
  }
  return { content: content.trim(), usage: normalizeUsageJs(usage) };
}

function normalizeUsageJs(raw) {
  if (!raw || typeof raw !== 'object') return null;
  const prompt = Number(raw.prompt_tokens);
  const completion = Number(raw.completion_tokens);
  const total = Number(raw.total_tokens);
  if (![prompt, completion, total].some((n) => Number.isFinite(n) && n >= 0)) return null;
  return {
    prompt_tokens: Number.isFinite(prompt) && prompt >= 0 ? prompt : 0,
    completion_tokens: Number.isFinite(completion) && completion >= 0 ? completion : 0,
    total_tokens: Number.isFinite(total) && total >= 0
      ? total
      : (Number.isFinite(prompt) ? prompt : 0) + (Number.isFinite(completion) ? completion : 0),
  };
}
