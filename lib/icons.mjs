/**
 * Пиктограммы вместо платформенных эмодзи в PR-комментариях (`REVIEW_ICON_BASE_URL`).
 *
 * Подмена делается на границе публикации, а не в рендере: report/actionability портированы
 * в Rust, и протягивать флаг через каждый форматтер значило бы дублировать его в обоих
 * портах. Здесь — одна функция над готовым Markdown, которая трогает только видимый текст:
 *   - HTML-комментарии пропускаются — в них маркеры и машинный футер `<ns>-data:{json}`,
 *     вставка `<img src="…">` внутрь JSON сломала бы carry-over находок;
 *   - код (fenced-блоки, включая ```suggestion, и инлайн-код) пропускается — там текст
 *     должен остаться буквальным.
 * Эмодзи остаётся в `alt`: где картинка не грузится (письма, мобильные уведомления),
 * смысл прежний. Логи, dry-run и Telegram по-прежнему печатают эмодзи.
 *
 * Сами SVG — публичный репозиторий Orange-hanter/ai-reviewer-icons (движок приватный, а
 * GitHub тянет картинки комментариев через прокси без авторизации). Базу задают с пином
 * на тег: опубликованные комментарии ссылаются на конкретный URL навсегда.
 */

import { reviewSummaryBody } from './github.mjs';

/** Эмодзи → имя файла в наборе (.github/icons/ — источник правды). */
export const ICON_FILES = Object.freeze({
  '🤖': 'bot',
  '⛔': 'gate-blocked',
  '✅': 'gate-open',
  '🔴': 'sev-critical',
  '🟠': 'sev-major',
  '🟡': 'sev-minor',
  '🔵': 'sev-nit',
  '🔄': 'delta',
  '🔎': 'verifier',
  '⚖️': 'jury',
  '🔧': 'prescan',
});

/** Высота в заголовке Markdown и в обычной строке — как в README (.github/icons/README.md). */
const HEIGHT_HEADING = 20;
const HEIGHT_TEXT = 16;

// ⚖ встречается и с вариационным селектором U+FE0F, и без него.
const EMOJI_SRC = '🤖|⛔|✅|🔴|🟠|🟡|🔵|🔄|🔎|⚖\\uFE0F?|🔧';
const EMOJI_RE = new RegExp(EMOJI_SRC, 'gu');
const HAS_EMOJI = new RegExp(EMOJI_SRC, 'u');

// Защищённые участки: HTML-комментарий, fenced-блок (``` или ~~~, закрытый той же длиной
// и символом), инлайн-код. Незакрытый fence не матчится — тогда его хвост обрабатывается
// как текст, что безопасно: худший исход — картинка вместо эмодзи внутри кода.
const PROTECTED_RE = /<!--[\s\S]*?-->|^ {0,3}(`{3,}|~{3,})[^\n]*\n[\s\S]*?^ {0,3}\1[ \t]*$|`[^`\n]+`/gm;

function imgTag(base, emoji, height) {
  const key = emoji.startsWith('⚖') ? '⚖️' : emoji;
  return `<img src="${base}/${ICON_FILES[key]}.svg" alt="${key}" height="${height}">`;
}

function iconizeText(text, base) {
  return text.replace(/^.*$/gm, (line) => {
    if (!HAS_EMOJI.test(line)) return line;
    const height = /^\s*(?:>\s*)*#{1,6}\s/.test(line) ? HEIGHT_HEADING : HEIGHT_TEXT;
    return line.replace(EMOJI_RE, (e) => imgTag(base, e, height));
  });
}

/**
 * Заменяет известные эмодзи видимого текста на `<img>` с базой `baseUrl`.
 * Пустая база → текст без изменений.
 */
export function iconize(markdown, baseUrl) {
  const base = String(baseUrl || '').trim().replace(/\/+$/, '');
  if (!base || !markdown) return markdown;
  let out = '';
  let last = 0;
  for (const m of markdown.matchAll(PROTECTED_RE)) {
    out += iconizeText(markdown.slice(last, m.index), base) + m[0];
    last = m.index + m[0].length;
  }
  return out + iconizeText(markdown.slice(last), base);
}

/**
 * Обёртка github-клиента: пиктограммы в sticky, в шапке review-сводки и в теле
 * inline-комментариев.
 * Остальные методы (auto-triage, чтение тредов, чистка) не трогаются — в частности,
 * ответы auto-triage опознаются по тексту маркера `🤖 auto-triage`.
 */
export function withIcons(github, baseUrl) {
  if (!String(baseUrl || '').trim() || !github) return github;
  return {
    ...github,
    upsertSticky: (body) => github.upsertSticky(iconize(body, baseUrl)),
    postReview: ({ inline = [], ...rest }) => github.postReview({
      ...rest,
      body: iconize(rest.body ?? reviewSummaryBody(inline.length), baseUrl),
      inline: inline.map((c) => ({ ...c, body: iconize(c.body, baseUrl) })),
    }),
  };
}
