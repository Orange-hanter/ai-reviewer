/**
 * Разбор скрытого футера sticky (`<!-- <ns>-data:{…} -->`) — общий для всех клиентов
 * хостинга (RFC 16): GitHub и GitLab хранят sticky по-разному (issue comment / MR note),
 * но футер пишет один и тот же рендер, и читаться он обязан одинаково — иначе перенос
 * находок и Δ-блок разъедутся между хостингами.
 */

import { MARKERS, hasMarker, extractStickyFooterRaw } from './constants.mjs';
import { extractJsonObject } from './json.mjs';

export const EMPTY_STICKY_DATA = Object.freeze({ findings: [], runs: [], sha: '' });

/** Последний комментарий с маркером sticky (находка могла процитировать маркер — PR #531). */
export function findStickyComment(comments) {
  return [...(comments || [])].reverse().find((c) => hasMarker(c.body, MARKERS.review)) || null;
}

/**
 * Данные футера из тела sticky. Форма возврата одна во всех ветках: на первом прогоне
 * sticky ещё нет, и `[]` вместо объекта ронял пайплайн на `priorSticky.findings.length`.
 */
export function parseStickyData(body) {
  const raw = body && extractStickyFooterRaw(body);
  if (raw == null) return { findings: [], runs: [], sha: '' };
  let data;
  try {
    data = JSON.parse(raw.trim());
  } catch {
    // Футер мог быть обрезан под лимит комментария — пробуем ремонт JSON (P2-12),
    // иначе prior findings молча теряются.
    data = extractJsonObject(raw, { onRepaired: () => {} });
  }
  // Футер пишет наш же движок, но читаем мы его из чужого текста (комментарий мог быть
  // отредактирован руками) — поэтому проверяем форму элементов, а не только массива.
  const isFinding = (f) => f && typeof f === 'object' && typeof f.file === 'string';
  const isRun = (r) => r && typeof r === 'object' && typeof r.sha === 'string';
  return {
    findings: Array.isArray(data?.findings) ? data.findings.filter(isFinding).slice(0, 40) : [],
    // История прогонов для Δ-блока: sha + счётчики. Хранится в футере, чтобы sticky
    // оставался ОДИН, но был виден рост/спад набора находок.
    runs: Array.isArray(data?.runs) ? data.runs.filter(isRun).slice(-5) : [],
    sha: typeof data?.sha === 'string' ? data.sha : '',
  };
}
