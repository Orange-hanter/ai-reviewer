/**
 * Наряды исполнителям ревью — RFC 15, этап формирования задачи.
 *
 * Наряд — это адресная задача: где смотреть (файл и диапазон строк), почему участок поднят
 * (признаки с уликами), какую гипотезу проверять, сколько контекста приложить и какая роль
 * этим занимается. Экспериментальное основание — Gonçalves, Baum & Bacchelli (EMSE 2022):
 * явная стратегия «что искать, где, когда» повышает результативность ревью у людей и
 * снижает когнитивную нагрузку; та же форма задачи предъявляется модели.
 *
 * Наряд НЕ является находкой и не участвует в гейте: он не имеет severity, не попадает в
 * `resolveBlockers` и не может ничего заблокировать. Обратное сделало бы признак уликой,
 * а признак ошибается по построению.
 *
 * Бюджет контекста — перераспределение, а не увеличение: сумма окон по умолчанию равна
 * тому, что пайплайн тратит сегодня (`REVIEW_CONTEXT_MARGIN` на каждый файл), и
 * раскладывается по спанам пропорционально плотности признаков. Ни один спан не остаётся
 * без окна: нижняя граница `minMargin` — защита от тихой потери recall.
 *
 * Модуль чистый, JS-only, в parity не входит.
 */

import { HYPOTHESIS_ROLE } from './cues.mjs';

/** Порядок ролей фиксирован: наряд обязан быть воспроизводим между прогонами. */
const ROLE_ORDER = ['security', 'correctness', 'architecture'];

/**
 * Роли по гипотезам. Пустой список — не «никто не смотрит», а «спец-гипотезы нет»:
 * такой спан достаётся общему проходу без адресной роли.
 */
export function rolesForHypotheses(hypotheses) {
  const set = new Set();
  for (const h of hypotheses || []) {
    const role = HYPOTHESIS_ROLE[h];
    if (role) set.add(role);
  }
  return ROLE_ORDER.filter((r) => set.has(r));
}

/**
 * Ставка спана: может ли находка отсюда в принципе блокировать merge. Отражает уже
 * действующее правило движка (`chunkNeedsSpecialists` в lib/chunk-routing.mjs): чанк без
 * исполняемого файла идёт без жюри, и его находки гейт не блокирует. Наряд не меняет это
 * правило, а называет его вслух — чтобы исполнитель знал цену своей ошибки.
 */
function stakesFor(span, { fileClass, sensitive } = {}) {
  if (sensitive?.test?.(span.file)) return 'blocking';
  const cls = typeof fileClass === 'function' ? fileClass(span.file) : null;
  return cls === 'logic' ? 'blocking' : 'advisory';
}

/**
 * Раздача бюджета контекста по спанам: сначала каждому нижняя граница, остаток —
 * пропорционально плотности признаков, с потолком.
 *
 * Добавленный файл margin не получает вовсе: его содержимое целиком лежит в диффе
 * (решение 1 [RFC 04](../proposals/04-token-economy.md), `collectAddedPaths`), и
 * освобождённые строки уходят остальным.
 */
function allocateMargins(ranked, { baseMargin, minMargin, maxMargin, budget }) {
  const need = ranked.filter((s) => !s.addedFile);
  const out = new Map();
  for (const s of ranked) out.set(s, s.addedFile ? 0 : minMargin);
  let left = budget - need.length * minMargin;
  if (left <= 0 || !need.length) return out;

  const totalDensity = need.reduce((sum, s) => sum + Math.max(0, s.density || 0), 0);
  if (totalDensity <= 0) {
    // Признаков нет ни у кого — раздаём поровну: это ровно сегодняшнее равномерное внимание.
    const even = Math.min(maxMargin - minMargin, Math.floor(left / need.length));
    for (const s of need) out.set(s, minMargin + even);
    return out;
  }
  for (const s of need) {
    const share = Math.max(0, s.density || 0) / totalDensity;
    const extra = Math.min(maxMargin - minMargin, Math.floor(left * share));
    out.set(s, minMargin + extra);
  }
  // Недораздача из-за потолка и округления возвращается верхушке ранжирования.
  let rest = budget - [...out.values()].reduce((a, b) => a + b, 0);
  for (const s of need) {
    if (rest <= 0) break;
    const cur = out.get(s);
    const room = maxMargin - cur;
    if (room <= 0) continue;
    const give = Math.min(room, rest);
    out.set(s, cur + give);
    rest -= give;
  }
  return out;
}

/**
 * Наряды по ранжированным спанам.
 *
 * @param {object[]} ranked — из `rankSpans`
 * @param {object} opts
 * @param {number} [opts.baseMargin=40] — сегодняшний `REVIEW_CONTEXT_MARGIN`
 * @param {number} [opts.minMargin=8] — никому не меньше: защита от тихой потери recall
 * @param {number} [opts.maxMargin] — потолок, по умолчанию 2×baseMargin
 * @param {number} [opts.budget] — суммарный бюджет строк контекста; по умолчанию —
 *   ровно сегодняшняя трата (baseMargin на каждый спан, требующий контекста)
 * @param {(path:string)=>string} [opts.fileClass]
 * @param {RegExp} [opts.sensitive]
 * @param {number} [opts.maxCues=6]
 * @returns {object[]} наряды в порядке ранжирования
 */
export function buildBriefs(ranked, opts = {}) {
  const baseMargin = Number.isFinite(opts.baseMargin) ? opts.baseMargin : 40;
  const minMargin = Number.isFinite(opts.minMargin) ? opts.minMargin : 8;
  const maxMargin = Number.isFinite(opts.maxMargin) ? opts.maxMargin : baseMargin * 2;
  const list = ranked || [];
  const budget = Number.isFinite(opts.budget)
    ? opts.budget
    : baseMargin * list.filter((s) => !s.addedFile).length;
  const margins = allocateMargins(list, { baseMargin, minMargin, maxMargin, budget });
  const maxCues = Number.isFinite(opts.maxCues) ? opts.maxCues : 6;

  return list.map((span, i) => ({
    id: `${span.file}#${span.startLine}-${span.endLine}`,
    rank: i + 1,
    file: span.file,
    startLine: span.startLine,
    endLine: span.endLine,
    score: span.score,
    density: Number((span.density ?? 0).toFixed(4)),
    effort: span.effort,
    // Сначала содержательные признаки по убыванию веса, затем множители поверхности:
    // исполнитель читает «что найдено», а не «на сколько это поделено».
    cues: [...(span.cues || [])]
      .sort((a, b) => (
        (a.kind === 'surface' ? 1 : 0) - (b.kind === 'surface' ? 1 : 0)
        || (b.weight ?? 0) - (a.weight ?? 0)
      ))
      .slice(0, maxCues),
    hypotheses: span.hypotheses || [],
    roles: rolesForHypotheses(span.hypotheses),
    contextMargin: margins.get(span) ?? minMargin,
    stakes: stakesFor(span, opts),
  }));
}

/** Одна строка наряда для человека и для лога. */
export function formatBrief(brief) {
  const cues = brief.cues
    .map((c) => (c.kind === 'surface' ? `${c.key}×${c.factor}` : `${c.key}${c.weight > 0 ? `+${c.weight}` : c.weight}`))
    .join(' ');
  const roles = brief.roles.length ? brief.roles.join(',') : '—';
  return `#${String(brief.rank).padStart(3)} ${brief.file}:${brief.startLine}-${brief.endLine}`
    + ` · score ${brief.score} · ±${brief.contextMargin} · ${roles} · ${brief.stakes}`
    + (cues ? `\n      ${cues}` : '')
    + (brief.hypotheses.length ? `\n      гипотезы: ${brief.hypotheses.join(', ')}` : '');
}
