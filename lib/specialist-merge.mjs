/**
 * Слияние ролей в один вызов, когда все они назначены на одну модель (RFC 04, решение 4).
 *
 * Три специалиста на одной модели с одинаковым контекстом — это не три мнения, а одно,
 * оплаченное трижды (ровно случай локального прогона: security=correctness=architecture=
 * одна модель). Здесь решается, применимо ли слияние, и как разобрать слитый ответ обратно
 * в те же `references`, которые ждут жюри, `mergeSpecialistReferences` и refutation.
 *
 * Чистый JS: оркестрация engine в Rust не портирована, паритетной копии нет.
 */

import { MERGED_ROLE_HEADING } from './prompt.mjs';

/**
 * Применимо ли слияние. `always` — даже при разных моделях (тогда берётся первая: осознанный
 * выбор оператора). `auto` — только когда модель у всех ролей одна и ролей больше одной.
 */
export function shouldMergeSpecialists(assignments, mode = 'auto') {
  const list = Array.isArray(assignments) ? assignments.filter((a) => a?.role && a?.model) : [];
  if (list.length < 2 || mode === 'never') return false;
  if (mode === 'always') return true;
  return new Set(list.map((a) => a.model)).size === 1;
}

const escapeRe = (s) => String(s).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/**
 * Разбирает слитый ответ на секции ролей.
 *
 * Возвращает `{ ok, sections }`. `ok: false` — модель не выдержала формат; вызывающий обязан
 * НЕ раздавать один и тот же текст всем ролям: три claims, выведенные из одного абзаца, —
 * это подделка консенсуса, которую refutation примет за подтверждение.
 */
export function splitMergedSpecialistText(text, roles) {
  const src = String(text ?? '');
  const list = (Array.isArray(roles) ? roles : []).filter(Boolean);
  const sections = new Map();
  if (!src.trim() || !list.length) return { ok: false, sections };

  // Позиция заголовка каждой роли: заголовок обязан стоять в начале строки, иначе упоминание
  // «### ROLE: security» внутри цитаты кода расколет чужую секцию.
  const marks = [];
  for (const role of list) {
    const re = new RegExp(`^[ \\t]*${escapeRe(MERGED_ROLE_HEADING(role))}[ \\t]*$`, 'im');
    const m = re.exec(src);
    if (m) marks.push({ role, start: m.index, end: m.index + m[0].length });
  }
  if (marks.length !== list.length) return { ok: false, sections };

  marks.sort((a, b) => a.start - b.start);
  marks.forEach((mark, i) => {
    const to = i + 1 < marks.length ? marks[i + 1].start : src.length;
    sections.set(mark.role, src.slice(mark.end, to).trim());
  });
  return { ok: true, sections };
}

/**
 * Слитый ответ → `references` в том же виде, что даёт опрос ролей по отдельности.
 * При неудачном разборе роль остаётся одна (первая), с полным текстом: жюри увидит одного
 * советника вместо трёх — честно и без выдуманного консенсуса.
 */
export function mergedTextToReferences(text, assignments, { onFallback } = {}) {
  const list = assignments.filter((a) => a?.role && a?.model);
  const { ok, sections } = splitMergedSpecialistText(text, list.map((a) => a.role));
  if (!ok) {
    onFallback?.();
    const first = list[0];
    return [{ role: first.role, model: first.model, text: String(text ?? ''), mergedFallback: true }];
  }
  return list.map(({ role, model }) => ({ role, model, text: sections.get(role) || '', merged: true }));
}
