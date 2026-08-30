/**
 * Actionability — маршрутизация находки: строчный комментарий или свод в sticky.
 *
 * Раньше маршрут задавала severity (`REVIEW_INLINE_ON=critical,major`), то есть ярлык модели.
 * Практика PR #520/#524 показала две вещи: ярлык нестабилен (одна тема на трёх прогонах одного
 * sha получала critical, major и minor), а среди minor попадаются дефекты гейт-уровня —
 * `-->` внутри футера обнулял машинный блок sticky, `sort` без `LC_ALL=C` возвращал петлю
 * пустых PR. Поэтому «блокирует ли merge» и «где показывать» разведены: блокировка осталась
 * по severity, а маршрут считается по детерминированным сигналам, которые пайплайн уже имеет.
 *
 * Сигналы намеренно не про формулировку находки: это то, что можно проверить без модели.
 */

import { getProfile } from './profile.mjs';
import { BLAST_FINDING_SOURCE } from './constants.mjs';

// Чувствительные зоны: цена пропущенного дефекта заметно выше средней. Базовый набор —
// общий для любого репозитория; свои зоны проект добавляет через профиль
// (`layout.sensitive`) — например инструменты собственного гейта, где minor-находки
// оказывались дефектами, ломавшими сам гейт.
const SENSITIVE_PATH = /(?:^|\/)(?:auth|rbac|permissions?|billing|pricing|payments?|money|migrations?)(?:\/|\.|$)|\.github\/workflows\//i;

function isSensitivePath(path) {
  if (SENSITIVE_PATH.test(path)) return true;
  const extra = getProfile().layout.sensitive;
  return Boolean(extra?.test(path));
}

export const ACTIONABILITY_SIGNALS = Object.freeze({
  prescan: { weight: 3, why: 'подтверждено детерминированным prescan (eslint/ast-grep) на той же строке' },
  verified: { weight: 3, why: 'verifier подтвердил находку' },
  repeated: { weight: 2, why: 'та же находка была в прошлом прогоне (не дисперсия моделей)' },
  jury: { weight: 2, why: 'жюри R5 подтвердило без возражений' },
  consensus: { weight: 2, why: 'находку дали независимо несколько моделей' },
  confidence: { weight: 1, why: 'модель уверена (confidence ≥ 0.8)' },
  sensitivePath: { weight: 1, why: 'файл из чувствительной зоны (авторизация, деньги, миграции, гейты)' },
  suggestion: { weight: 1, why: 'есть конкретный предлагаемый фикс' },
  logicFile: { weight: 1, why: 'код, а не документация' },
  blastCaller: { weight: 2, why: 'caller вне диффа подтверждён blast-radius (другой слой)' },
  outOfChunk: { weight: -4, why: 'файл не был виден этой части ревью — доказать нечем' },
  juryDowngraded: { weight: -2, why: 'жюри понизило или отклонило часть претензий' },
  docsOnly: { weight: -1, why: 'замечание к документации' },
});

/** Ключ «файл:строка» — общий для скоринга и для ctx-множеств вызывающего. */
export const findingLocationKey = (f) => `${String(f?.file || '').trim()}:${Number(f?.line) || 0}`;

/** Инлайн физически возможен только на строке, которая есть в диффе. */
export function isInlineable(finding, files) {
  const rec = finding?.file ? files?.get(finding.file) : null;
  const line = Number(finding?.line) || 0;
  return Boolean(rec && line > 0 && rec.validLines?.has(line));
}

/**
 * Скор находки и список сработавших сигналов (для объяснения в комментарии).
 * @param {object} finding
 * @param {{priorKeys?: Set<string>, prescanKeys?: Set<string>, fileClass?: (path: string) => string}} ctx
 */
export function scoreActionability(finding, ctx = {}) {
  const reasons = [];
  const add = (key) => reasons.push({ key, ...ACTIONABILITY_SIGNALS[key] });

  const claims = Array.isArray(finding.advisorClaims) ? finding.advisorClaims : [];
  // Ключ строится ровно так же, как ctx-множества у вызывающего (pipeline): один хелпер на
  // обе стороны, иначе рассинхрон нормализации молча гасит сигналы prescan/повтора.
  const locKey = findingLocationKey(finding);

  if (ctx.prescanKeys?.has(locKey)) add('prescan');
  if (finding.verifierStatus === 'verified') add('verified');
  else if (finding.verifierStatus === 'rejected' || finding.verifierStatus === 'downgraded') add('juryDowngraded');
  if (ctx.priorKeys?.has(locKey)) add('repeated');
  if (claims.length && claims.every((c) => c.status === 'verified')) add('jury');
  else if (claims.some((c) => c.status === 'rejected' || c.status === 'downgraded')) add('juryDowngraded');
  if (finding.consensus) add('consensus');
  if ((finding.confidence ?? 0) >= 0.8) add('confidence');
  if (finding.file && isSensitivePath(finding.file)) add('sensitivePath');
  if (finding.suggestion) add('suggestion');
  if (finding.blastCaller || finding.source === BLAST_FINDING_SOURCE) add('blastCaller');

  const cls = ctx.fileClass && finding.file ? ctx.fileClass(finding.file) : null;
  if (cls === 'logic') add('logicFile');
  if (cls === 'docs') add('docsOnly');
  if (finding.outOfChunk) add('outOfChunk');

  const score = reasons.reduce((sum, r) => sum + r.weight, 0);
  return { score, reasons };
}

/**
 * Решение по каждой находке: `inline` (в строку) или `sticky` (в свод).
 * Блокеры идут в строку всегда — там их читают. Advisory попадает в строку, если её
 * доказуемость набрала порог; лимит `maxAdvisoryInline` не даёт залить PR замечаниями,
 * причём в него отбираются самые доказуемые, а не первые попавшиеся.
 */
export function routeFindings(findings, files, cfg = {}) {
  const { inlineOn = ['critical', 'major'], threshold = 4, maxAdvisoryInline = 5, ctx = {}, strict = false } = cfg;
  const tiers = new Set(inlineOn);
  const effectiveThreshold = strict ? Number.POSITIVE_INFINITY : threshold;
  // Находка без severity (битый ответ модели, прошедший ремонт JSON) — не «не-блокер»,
  // а неизвестность: в строку по доказуемости её не поднимаем.
  const knownSeverity = (f) => ['critical', 'major', 'minor', 'nit'].includes(f?.severity);
  const scored = (findings || []).map((f) => {
    const { score, reasons } = scoreActionability(f, ctx);
    return { finding: f, score, reasons, inlineable: isInlineable(f, files) };
  });

  const advisory = scored
    .filter((s) => s.inlineable && knownSeverity(s.finding) && !tiers.has(s.finding.severity) && s.score >= effectiveThreshold)
    .sort((a, b) => b.score - a.score)
    .slice(0, maxAdvisoryInline);
  const promoted = new Set(advisory.map((s) => s.finding));

  for (const s of scored) {
    const blockInline = tiers.has(s.finding.severity) && s.inlineable
      && (!strict || s.finding.prescan || s.finding.verifierStatus === 'verified');
    s.route = blockInline || promoted.has(s.finding) ? 'inline' : 'sticky';
    s.promoted = promoted.has(s.finding);
    // Пометка пишется В САМУ находку намеренно: её читают и рендер инлайна (объяснение
    // «почему в строке»), и auto-triage (такие нити не резолвим молча). Возвращать копию
    // нельзя — дальше по пайплайну ходит тот же массив объектов (кластеры, гейт, экспорт).
    if (s.promoted) {
      s.finding.actionabilityScore = s.score;
      s.finding.actionabilityReasons = s.reasons.map((r) => r.why);
    }
  }
  return scored;
}
