/**
 * Раскладка сообщений под префиксный кэш провайдера (RFC 04, решение 5).
 *
 * Как было: роль лежит в `system` (первое сообщение), а жюри дописывает мнения советников
 * в НАЧАЛО последнего user-сообщения. Четыре вызова по одному чанку расходятся с первого
 * токена и не делят ни байта — префиксный кэш невозможен по построению, хотя 95% объёма
 * у них общие (дифф и контекст файлов).
 *
 * Как здесь: `[system: общая рамка] [user: контекст чанка] [user: инструкция роли]`.
 * Первые два сообщения побайтово равны у всех ролей чанка, роль приезжает последней —
 * заодно ближе к концу окна, где модели держат инструкции лучше.
 *
 * Собирается из уже готовых сообщений, а не из кусков промпта: тексты ролей продолжает
 * строить lib/prompt.mjs (и его Rust-порт), здесь только перестановка. Поэтому у модуля
 * нет паритетной копии в Rust и разойтись сторонам нечем.
 *
 * Выгода зависит от провайдера: OpenAI-совместимые кэшируют общий префикс сами, Anthropic
 * требует явных cache_control-маркеров. Отсюда дефолт `legacy` и пробник
 * `bin/pr-review-probe-cache.mjs`, который меряет выигрыш на конкретном endpoint.
 */

export const PROMPT_LAYOUTS = Object.freeze(['legacy', 'shared-prefix']);

/**
 * Общая рамка. Намеренно короткая и без правил проекта: правила уже едут внутри инструкции
 * роли, а всё, что попадёт сюда, оплачивается в каждом вызове независимо от кэша.
 */
export const SHARED_SYSTEM = `You take part in a strict code review of a pull request.
The next message contains the full context of this part of the PR: the changed-file map, the
deterministic pre-scan, the blast-radius, findings from the previous run, file contents and the
annotated diff. The message after it defines YOUR role and the exact output format for it.
Read the context first, then follow the role instructions literally.`;

const lastUser = (messages) => {
  for (let i = messages.length - 1; i >= 0; i--) if (messages[i]?.role === 'user') return messages[i].content ?? '';
  return null;
};

const layoutOf = (context, instruction) => [
  { role: 'system', content: SHARED_SYSTEM },
  { role: 'user', content: context },
  { role: 'user', content: instruction },
];

/**
 * Роль-советник (specialist / advisor): её сообщения — `[system: правила роли, user: контекст]`.
 * Возвращает null, если форма не та (значит промпт перестроили) — вызывающий обязан остаться
 * на прежней раскладке, а не собирать сообщения наугад.
 */
export function sharedPrefixAdvisor(roleMessages) {
  const list = Array.isArray(roleMessages) ? roleMessages : [];
  // Позиция system не фиксируется: сборщик промпта волен ставить его первым (так делает
  // lib/prompt.mjs) или последним. Важно, что инструкция роли — это ровно system-сообщение,
  // а контекст — последний user.
  const system = list.find((m) => m?.role === 'system')?.content ?? null;
  const context = lastUser(list);
  if (!system || context == null) return null;
  return layoutOf(context, system);
}

/**
 * Жюри-агрегатор: `buildSpecialistAggregatorMessages`/`buildAggregatorMessages` возвращают
 * базовые сообщения, где к последнему user приписаны СПЕРЕДИ мнения советников и схема
 * ответа. Разбираем обратно: контекст — это базовый user, инструкция — правила агрегатора
 * (его system) плюс приписанное.
 *
 * Срез проверяется через endsWith: если приписали не спереди, форма изменилась — null,
 * и вызывающий остаётся на прежней раскладке. Тихо собрать «почти правильный» промпт хуже,
 * чем не экономить.
 */
export function sharedPrefixAggregator(baseMessages, aggMessages) {
  const rules = baseMessages?.[0]?.role === 'system' ? baseMessages[0].content : null;
  const baseUser = lastUser(baseMessages || []);
  const aggUser = lastUser(aggMessages || []);
  if (!rules || baseUser == null || aggUser == null) return null;
  if (!aggUser.endsWith(baseUser)) return null;
  const prepended = aggUser.slice(0, aggUser.length - baseUser.length);
  return layoutOf(baseUser, `${rules}\n\n${prepended}`.trimEnd());
}

/** Длина общего префикса в символах — то, что провайдер сможет отдать из кэша. */
export function sharedPrefixChars(messages) {
  if (!Array.isArray(messages) || messages.length < 3) return 0;
  return String(messages[0]?.content ?? '').length + String(messages[1]?.content ?? '').length;
}
