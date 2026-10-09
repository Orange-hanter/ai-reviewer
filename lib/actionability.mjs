/**
 * Фаза 1, шаг 5: lib/actionability.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/actionability (порт scoreActionability/routeFindings и,
 * заодно, classify/findingBlock из createFindingsFormatter — они дергают
 * routeFindings, поэтому живут в этом же бинаре; см. lib/findings.mjs).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Все потребители
 * (pipeline.mjs, lib/findings.mjs, тесты) вызывают функции СИНХРОННО, поэтому вместо
 * async execFile используется execFileSync: бинарь вызывается на каждую операцию
 * с JSON {"op", ...args} на stdin, ответ — JSON с результатом.
 *
 * Резолвинг бинаря (по аналогии с lib/diff.mjs, lib/json.mjs, lib/findings.mjs):
 *   AI_REVIEWER_ACTIONABILITY_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/actionability;
 *   AI_REVIEWER_ACTIONABILITY_FORCE_JS=1 — принудительно чистый JS (паритет/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm test/eval
 * идут раньше cargo build, см. .github/workflows/ci.yml), обёртка деградирует на чистый
 * JS fallback ниже — эталонную копию исходной реализации. Если бинарь ЕСТЬ, но упал
 * (non-zero exit) — ошибка пробрасывается, fallback не используется: молчаливая
 * подмена реализации при битом Rust-коде скрывала бы регрессии. Исключений нет:
 * layout.sensitive профиля компилируется в Rust крейтом fancy-regex (RFC 04,
 * решение 4), поэтому JS-синтаксис вроде lookahead больше не требует «мягкой»
 * деградации на чистый JS.
 *
 * Нюансы паритета Rust-порта (проверены parity-прогоном rust/parity/actionability.parity.mjs):
 *   - ctx.fileClass — функция в JS; обёртка разворачивает её в карту fileClasses
 *     (путь → класс) по файлам находок ДО вызова, Rust получает готовую карту;
 *   - layout.sensitive профиля передаётся исходником (sensitiveSource), компилируется
 *     fancy-regex с (?i) — как new RegExp(raw, 'i') в lib/profile.mjs; отличия
 *     семантики от JS RegExp разобраны в rust/src/fp_patterns.rs;
 *   - мутации (actionabilityScore/Reasons/Promoted) накладываются обёрткой на
 *     ИСХОДНЫЕ объекты находок по индексу (семантика разделяемых ссылок, как в
 *     findings: дальше по пайплайну ходит тот же массив объектов);
 *   - findingLocationKey и isInlineable остались чистым JS: однострочные шаблоны
 *     на горячих путях (pipeline мапит их по всем prior/prescan-находкам) — вызов
 *     процесса на находку свёл бы прогон в ноль, а отдельных потребителей у них нет.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { getProfile } from './profile.mjs';
import { BLAST_FINDING_SOURCE } from './constants.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_ACTIONABILITY_BIN = resolve(__dir, '../rust/target/debug/actionability');

function resolveActionabilityBin() {
  if (process.env.AI_REVIEWER_ACTIONABILITY_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_ACTIONABILITY_BIN || '').trim() || DEFAULT_ACTIONABILITY_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveActionabilityBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

/** Внутренний диспетчер для соседних обёрток: classify/findingBlock из
 * createFindingsFormatter (lib/findings.mjs) живут в этом бинаре. */
export function runActionabilityRust(op, args) {
  return runRust(op, args);
}

// ---------------------------------------------------------------------------
// Сериализация аргументов (общая для routeFindings и classify из lib/findings.mjs)
// ---------------------------------------------------------------------------

/** Map<string, DiffFileRecord> → {path: {validLines}} для Rust. */
export function filesToJson(files) {
  if (!files) return null;
  const out = {};
  for (const [path, rec] of files) {
    out[path] = { validLines: rec?.validLines ? [...rec.validLines] : [] };
  }
  return out;
}

/** ctx (priorKeys/prescanKeys Set'ы, fileClass-функция) → JSON для Rust.
 * fileClass разворачивается в карту по файлам находок ДО вызова. */
export function ctxToJson(findings, ctx = {}) {
  const fileClasses = {};
  if (typeof ctx.fileClass === 'function') {
    for (const f of findings || []) {
      if (f?.file) fileClasses[f.file] = ctx.fileClass(f.file);
    }
  }
  const sensitive = getProfile().layout.sensitive;
  return {
    priorKeys: ctx.priorKeys ? [...ctx.priorKeys] : [],
    prescanKeys: ctx.prescanKeys ? [...ctx.prescanKeys] : [],
    fileClasses,
    sensitiveSource: sensitive ? sensitive.source : null,
  };
}

/** Наложить мутации маршрутизации на исходные объекты находок (семантика JS-оригинала). */
export function applyRouteMutations(findings, mutations) {
  for (const m of mutations || []) {
    const f = findings[m.index];
    f.actionabilityScore = m.score;
    f.actionabilityReasons = m.reasons;
    f.actionabilityPromoted = m.promoted || undefined;
  }
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

// Чувствительные зоны: базовый набор — общий для любого репозитория; свои зоны
// проект добавляет через профиль (`layout.sensitive`).
const SENSITIVE_PATH = /(?:^|\/)(?:auth|rbac|permissions?|billing|pricing|payments?|money|migrations?)(?:\/|\.|$)|\.github\/workflows\//i;

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

/** Ключ «файл:строка» — общий для скоринга и для ctx-множеств вызывающего.
 * Чистый JS намеренно: горячий путь (pipeline мапит его по всем prior/prescan). */
export const findingLocationKey = (f) => `${String(f?.file || '').trim()}:${Number(f?.line) || 0}`;

/** Инлайн физически возможен только на строке, которая есть в диффе.
 * Чистый JS намеренно: работает с Map/Set в памяти, отдельных потребителей нет. */
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
  const findings = [finding];
  const r = runRust('scoreActionability', { finding, ctx: ctxToJson(findings, ctx) });
  if (r !== null) return r;
  return scoreActionabilityJs(finding, ctx);
}

/**
 * Решение по каждой находке: `inline` (в строку) или `sticky` (в свод).
 * Блокеры идут в строку всегда — там их читают. Advisory попадает в строку, если её
 * доказуемость набрала порог; лимит `maxAdvisoryInline` не даёт залить PR замечаниями,
 * причём в него отбираются самые доказуемые, а не первые попавшиеся.
 */
export function routeFindings(findings, files, cfg = {}) {
  const list = findings || [];
  const r = runRust('routeFindings', {
    findings: list,
    files: filesToJson(files),
    cfg: {
      inlineOn: cfg.inlineOn,
      threshold: cfg.threshold,
      maxAdvisoryInline: cfg.maxAdvisoryInline,
      strict: cfg.strict,
    },
    ctx: ctxToJson(list, cfg.ctx),
  });
  if (r !== null) {
    applyRouteMutations(list, r.mutations);
    return r.scored.map((s) => ({
      finding: list[s.index],
      score: s.score,
      reasons: s.reasons,
      inlineable: s.inlineable,
      route: s.route,
      promoted: s.promoted,
    }));
  }
  return routeFindingsJs(findings, files, cfg);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/actionability.mjs
// до Фазы 1). Используется только когда Rust-бинарь отсутствует; при битом бинаре
// НЕ подменяет его (единственное исключение — см. шапку модуля).
// ---------------------------------------------------------------------------

function isSensitivePathJs(path) {
  if (SENSITIVE_PATH.test(path)) return true;
  const extra = getProfile().layout.sensitive;
  return Boolean(extra?.test(path));
}

function scoreActionabilityJs(finding, ctx = {}) {
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
  // RFC 06: один вердикт жюри — один штраф. Если refutation уже понизила severity
  // (`refutationStatus === 'downgraded'`), второй раз за тот же факт не вычитаем: иначе одна
  // понижённая претензия из пяти держит потолок в 1 балл при пороге маршрута 4. Сигнал
  // остаётся для случая, когда severity не менялась: часть claims отклонена, но есть
  // verified — decideRefutation дал `keep`.
  else if (finding.refutationStatus !== 'downgraded'
    && claims.some((c) => c.status === 'rejected' || c.status === 'downgraded')) add('juryDowngraded');
  if (finding.consensus) add('consensus');
  if ((finding.confidence ?? 0) >= 0.8) add('confidence');
  if (finding.file && isSensitivePathJs(finding.file)) add('sensitivePath');
  if (finding.suggestion) add('suggestion');
  if (finding.blastCaller || finding.source === BLAST_FINDING_SOURCE) add('blastCaller');

  const cls = ctx.fileClass && finding.file ? ctx.fileClass(finding.file) : null;
  if (cls === 'logic') add('logicFile');
  if (cls === 'docs') add('docsOnly');
  if (finding.outOfChunk) add('outOfChunk');

  const score = reasons.reduce((sum, r) => sum + r.weight, 0);
  return { score, reasons };
}

function routeFindingsJs(findings, files, cfg = {}) {
  const { inlineOn = ['critical', 'major'], threshold = 4, maxAdvisoryInline = 5, ctx = {}, strict = false } = cfg;
  const tiers = new Set(inlineOn);
  const effectiveThreshold = strict ? Number.POSITIVE_INFINITY : threshold;
  // Находка без severity (битый ответ модели, прошедший ремонт JSON) — не «не-блокер»,
  // а неизвестность: в строку по доказуемости её не поднимаем.
  const knownSeverity = (f) => ['critical', 'major', 'minor', 'nit'].includes(f?.severity);
  const scored = (findings || []).map((f) => {
    const { score, reasons } = scoreActionabilityJs(f, ctx);
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
    //
    // Скор и сигналы пишутся КАЖДОЙ находке: они уходят в артефакт экспорта, где потребитель
    // считает по ним стабильность набора между прогонами. Раньше скор стоял только у поднятых
    // и в артефакте читался как «остальные не оценены». Роль «поднята в строку» несёт
    // отдельный флаг. Присваивание безусловное: routeFindings вызывается за прогон дважды
    // (verifier-стадия и classify) с разными лимитами — прошлая пометка не должна пережить
    // пересчёт.
    s.finding.actionabilityScore = s.score;
    s.finding.actionabilityReasons = s.reasons.map((r) => r.why);
    s.finding.actionabilityPromoted = s.promoted || undefined;
  }
  return scored;
}
