import { sleep, isTransient, backoffMs, createAdaptiveSemaphore } from './util.mjs';
import { extractJsonObject } from './json.mjs';

async function readOllamaSseContent(res) {
  let content = '', reasoningLen = 0, finish = null, buf = '';
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
  return content.trim();
}

/** Фабрика клиента Ollama Cloud — JSON для aggregator, prose для MoA-advisors. */
export function createOllamaClient(cfg, log) {
  const { ollama: o, moa, httpRetries } = cfg;
  // Rate-limit control: глобальный + per-model семафоры с AIMD по 429 (бэклог #5).
  const globalSem = createAdaptiveSemaphore(o.providerConcurrency || 12);
  const modelSems = new Map();
  const semFor = (model) => {
    if (!modelSems.has(model)) modelSems.set(model, createAdaptiveSemaphore(o.providerPerModel || 6));
    return modelSems.get(model);
  };

  async function chatRequest(messages, { model, maxTokens, temperature, effort, jsonMode }) {
    const payload = JSON.stringify({
      model, messages, temperature, stream: true, max_tokens: maxTokens,
      reasoning_effort: effort, seed: o.seed,
      ...(jsonMode ? { response_format: { type: 'json_object' } } : {}),
    });
    // Таймаут попытки из конфига (было 900с хардкод: httpRetries × 15 мин взрывали 25-мин CI job).
    // Ретрай — только пока не израсходован бюджет одного таймаута: после полного висения
    // повторять бессмысленно, худший случай логического вызова ≈ 2 × timeoutMs.
    const timeoutMs = o.requestTimeoutMs || 240_000;
    const t0 = Date.now();
    const budgetLeft = () => Date.now() - t0 < timeoutMs;
    for (let attempt = 0; ; attempt++) {
      // Семафоры до fetch (таймаут стартует после получения слота — ожидание в очереди не горит).
      // Ownership-флаги: при сбое второго acquire первый слот не утекает.
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
          if (res.status === 429) {
            g.throttle();
            m.throttle();
            log(`[${model}] 429 → снижение параллелизма (global: ${JSON.stringify(g.stats())}, model: ${JSON.stringify(m.stats())})`);
          }
          if (isTransient(res.status) && attempt < httpRetries && budgetLeft()) {
            const wait = backoffMs(attempt, res.headers.get('retry-after'));
            log(`[${model}] Ollama ${res.status} → повтор через ${wait}мс (${attempt + 1}/${httpRetries})`);
            await sleep(wait);
            continue;
          }
          throw Object.assign(new Error(`Ollama ${res.status}: ${body.slice(0, 400)}`), { status: res.status });
        }
        return await readOllamaSseContent(res);
      } catch (e) {
        const networky = e.status === undefined && !e.empty && !e.truncated &&
          /fetch failed|network|socket|ECONN|ETIMEDOUT|EAI_AGAIN|terminated|aborted/i.test(e.message || '');
        if (networky && attempt < httpRetries && budgetLeft()) {
          const wait = backoffMs(attempt);
          log(`[${model}] сетевой сбой (${e.message}) → повтор через ${wait}мс (${attempt + 1}/${httpRetries})`);
          await sleep(wait);
          continue;
        }
        throw e;
      } finally { clearTimeout(t); m.release(); g.release(); }
    }
  }

  async function chatJsonOnce(messages, { model, maxTokens, effort, temperature, jsonMode }) {
    const content = await chatRequest(messages, { model, maxTokens, temperature, effort, jsonMode });
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
    // Неперечислимый маркер для observability (в JSON-вывод не попадает).
    if (repaired && result && typeof result === 'object') {
      Object.defineProperty(result, '__repaired', { value: repaired, enumerable: false });
    }
    return result;
  }

  async function chatJson(messages, model, { temperature = o.temperature } = {}) {
    const jm = o.jsonMode;
    try {
      return await chatJsonOnce(messages, { maxTokens: 24000, effort: 'low', model, jsonMode: jm, temperature });
    } catch (e) {
      if (e.empty) {
        log(`[${model}] content пуст на effort=low → повтор effort=none:`, e.message);
        return chatJsonOnce(messages, { maxTokens: 24000, effort: 'none', model, jsonMode: jm, temperature });
      }
      if (e.truncated) {
        log(`[${model}] JSON оборван → повтор effort=none + больше токенов:`, e.message);
        return chatJsonOnce(messages, { maxTokens: 32000, effort: 'none', model, jsonMode: jm, temperature });
      }
      if (e.badEscape || /Bad escaped|Unexpected (?:token|end).*JSON/i.test(e.message || '')) {
        log(`[${model}] битые escape → повтор effort=none + больше токенов (без JSON-mode):`, e.message);
        return chatJsonOnce(messages, { maxTokens: 32000, effort: 'none', model, jsonMode: false, temperature });
      }
      if (jm && (e.status === 400 || /response_format/i.test(e.message || ''))) {
        log(`[${model}] ошибка, возможно из-за JSON-mode → повтор без него:`, e.message);
        return chatJsonOnce(messages, { maxTokens: 24000, effort: 'low', model, jsonMode: false, temperature });
      }
      throw e;
    }
  }

  async function chatText(messages, model, { maxTokens = moa.refMaxTokens, temperature = moa.refTemp } = {}) {
    try {
      return await chatRequest(messages, { model, maxTokens, temperature, effort: o.effort || 'low', jsonMode: false });
    } catch (e) {
      if (e.empty) {
        log(`[${model}] advisor content пуст на effort=low → effort=none`);
        return chatRequest(messages, { model, maxTokens, temperature, effort: 'none', jsonMode: false });
      }
      throw e;
    }
  }

  return { chatJson, chatText };
}
