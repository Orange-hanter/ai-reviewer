#!/usr/bin/env node
// Пробник моделей Ollama Cloud: latency/поведение reasoning_effort, JSON-mode.
// Запуск: OLLAMA_API_KEY=... npx pr-review-probe-models [model1,model2,...]
const MODELS = (process.argv[2] || 'kimi-k2.7-code,glm-5.2,deepseek-v4-pro,deepseek-v4-flash,minimax-m2.7,minimax-m3').split(',');
const BASE = (process.env.OLLAMA_BASE_URL || 'https://ollama.com/v1').replace(/\/$/, '');
const KEY = process.env.OLLAMA_API_KEY;

const PROMPT = [
  { role: 'system', content: 'You are a code reviewer. Answer with a single JSON object.' },
  { role: 'user', content: 'Код: `const x = arr[arr.length];`\nОпредели проблему. Ответь СТРОГО JSON: {"bug": boolean, "desc": string (на русском, ≤10 слов)}' },
];

async function probe(model, { effort, jsonMode }) {
  const t0 = Date.now();
  try {
    const res = await fetch(`${BASE}/chat/completions`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${KEY}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({
        model, messages: PROMPT, temperature: 0.4, max_tokens: 2048,
        reasoning_effort: effort, stream: false,
        ...(jsonMode ? { response_format: { type: 'json_object' } } : {}),
      }),
    });
    const ms = Date.now() - t0;
    const text = await res.text();
    if (!res.ok) return { model, effort, jsonMode, ms, error: `HTTP ${res.status}: ${text.slice(0, 120)}` };
    const j = JSON.parse(text);
    const content = j.choices?.[0]?.message?.content || '';
    const usage = j.usage || {};
    let jsonOk = false;
    try { JSON.parse(content.replace(/```(?:json)?/g, '').trim()); jsonOk = true; } catch { /* prose */ }
    return {
      model, effort, jsonMode, ms, jsonOk,
      tokens: `${usage.prompt_tokens ?? '?'}→${usage.completion_tokens ?? '?'}`,
      content: content.replace(/\s+/g, ' ').slice(0, 90),
    };
  } catch (e) {
    return { model, effort, jsonMode, ms: Date.now() - t0, error: String(e.message || e).slice(0, 120) };
  }
}

console.log('model'.padEnd(20), 'effort'.padEnd(7), 'jm', 'ms'.padStart(7), 'json', 'tokens', ' content');
for (const model of MODELS) {
  for (const effort of ['low', 'none']) {
    for (const jsonMode of [true, false]) {
      const r = await probe(model, { effort, jsonMode });
      if (r.error) {
        console.log(model.padEnd(20), effort.padEnd(7), jsonMode ? 'y ' : 'n ', String(r.ms).padStart(7), '—', r.error);
      } else {
        console.log(
          model.padEnd(20), effort.padEnd(7), jsonMode ? 'y ' : 'n ',
          String(r.ms).padStart(7), r.jsonOk ? ' ok ' : 'FAIL', r.tokens.padEnd(10), r.content,
        );
      }
    }
  }
}
