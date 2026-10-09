#!/usr/bin/env node
// Пробник префиксного кэша: даёт ли провайдер скидку за повторно присланный общий префикс.
// Отвечает на единственный вопрос — стоит ли включать REVIEW_PROMPT_LAYOUT=shared-prefix
// (RFC 04, решение 5) на ЭТОМ endpoint. Без замера включать нечего: OpenAI-совместимые
// провайдеры кэшируют префикс сами, Anthropic требует явных cache_control-маркеров, а через
// шим поверх claude -p кэш нам вообще не подчиняется.
//
// Запуск: OLLAMA_API_KEY=... npx pr-review-probe-cache [model] [--size 20000]
//
// Метод: три вызова подряд одной моделью.
//   1) большой общий префикс + хвост A  — прогревает кэш;
//   2) тот же префикс + хвост B         — если кэш есть, латентность падает, а провайдер
//                                         может показать это в usage;
//   3) ДРУГОЙ префикс + хвост B         — контроль: столько стоит вызов без кэша.
// Кэш есть, если (2) заметно дешевле (3) при одинаковом размере входа.

const MODEL = (process.argv[2] && !process.argv[2].startsWith('--')) ? process.argv[2] : 'kimi-k2.7-code';
const sizeArg = process.argv.indexOf('--size');
const SIZE = sizeArg > 0 ? Number(process.argv[sizeArg + 1]) || 20_000 : 20_000;
const BASE = (process.env.LLM_BASE_URL || process.env.OLLAMA_BASE_URL || 'https://ollama.com/v1').replace(/\/$/, '');
const KEY = process.env.LLM_API_KEY || process.env.OLLAMA_API_KEY;

/** Синтетический «контекст чанка»: детерминированный, но не сжимаемый в одну строку. */
function filler(seed, size) {
  const out = [];
  for (let i = 0; out.join('\n').length < size; i++) {
    out.push(`  ${String(i).padStart(4)}: const value_${seed}_${i} = compute(${i}, "${seed}");`);
  }
  return out.join('\n');
}

const prefixA = `## Diff (RIGHT-side line numbers)\n\`\`\`diff\n${filler('alpha', SIZE)}\n\`\`\``;
const prefixB = `## Diff (RIGHT-side line numbers)\n\`\`\`diff\n${filler('bravo', SIZE)}\n\`\`\``;

const call = async (prefix, tail) => {
  const t0 = Date.now();
  const res = await fetch(`${BASE}/chat/completions`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${KEY}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      model: MODEL,
      messages: [
        { role: 'system', content: 'You take part in a code review. Answer in one short sentence.' },
        { role: 'user', content: prefix },
        { role: 'user', content: tail },
      ],
      temperature: 0, max_tokens: 64, stream: false,
    }),
  });
  const ms = Date.now() - t0;
  const text = await res.text();
  if (!res.ok) throw new Error(`HTTP ${res.status}: ${text.slice(0, 200)}`);
  const j = JSON.parse(text);
  const u = j.usage || {};
  // Поле кэша называется по-разному у разных провайдеров — ищем любое похожее.
  const cached = u.prompt_tokens_details?.cached_tokens ?? u.cached_tokens
    ?? u.prompt_cache_hit_tokens ?? u.cache_read_input_tokens ?? null;
  return { ms, prompt: u.prompt_tokens ?? null, completion: u.completion_tokens ?? null, cached };
};

if (!KEY) {
  console.error('нужен LLM_API_KEY или OLLAMA_API_KEY');
  process.exit(1);
}

const line = (name, r) => `${name.padEnd(28)} ${String(r.ms).padStart(6)}мс  prompt=${r.prompt ?? '?'}`
  + `${r.cached != null ? `  cached=${r.cached}` : ''}`;

try {
  console.log(`модель ${MODEL} · endpoint ${BASE} · префикс ~${Math.round(prefixA.length / 3.5 / 1000)}k токенов\n`);
  const warm = await call(prefixA, 'Роль: security. Одним предложением — что здесь опасно?');
  console.log(line('1. прогрев (префикс A)', warm));
  const hit = await call(prefixA, 'Роль: correctness. Одним предложением — где здесь ошибка?');
  console.log(line('2. тот же префикс A', hit));
  const miss = await call(prefixB, 'Роль: correctness. Одним предложением — где здесь ошибка?');
  console.log(line('3. другой префикс B', miss));

  console.log('');
  if (hit.cached != null && hit.cached > 0) {
    console.log(`✅ Провайдер сообщает о кэше: ${hit.cached} токен(ов) префикса прочитаны из него.`);
    console.log('   REVIEW_PROMPT_LAYOUT=shared-prefix даст экономию на каждом чанке.');
  } else if (miss.ms > 0 && hit.ms / miss.ms < 0.7) {
    console.log(`⏱ Повтор префикса быстрее контроля в ${(miss.ms / hit.ms).toFixed(1)}× — похоже на кэш,`);
    console.log('   хотя провайдер о нём не сообщает. Стоит проверить счётом, а не латентностью.');
  } else {
    console.log('➖ Признаков префиксного кэша нет: повтор префикса не дешевле и не быстрее контроля.');
    console.log('   Включать REVIEW_PROMPT_LAYOUT=shared-prefix ради экономии на этом endpoint незачем.');
  }
  console.log('\nЛатентность — шумный показатель: при сомнениях прогоните пробник несколько раз.');
} catch (e) {
  console.error(`пробник упал: ${e.message}`);
  process.exit(1);
}
