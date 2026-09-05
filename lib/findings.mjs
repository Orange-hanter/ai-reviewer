/**
 * Фаза 1, шаг 3: lib/findings.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/findings (порт нормализации/кластеринга/gate/fingerprint).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Все потребители
 * (pipeline.mjs, lib/engine.mjs, lib/eval.mjs, lib/report.mjs, index.mjs, тесты) вызывают
 * функции СИНХРОННО, поэтому вместо async execFile используется execFileSync: бинарь
 * вызывается на каждую операцию с JSON {"op", ...args} на stdin, ответ — JSON с результатом.
 *
 * Резолвинг бинаря (по аналогии с lib/diff.mjs и lib/json.mjs):
 *   AI_REVIEWER_FINDINGS_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/findings;
 *   AI_REVIEWER_FINDINGS_FORCE_JS=1 — принудительно чистый JS (паритетные прогоны/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm test/eval
 * идут раньше cargo build, см. .github/workflows/ci.yml), обёртка деградирует на чистый
 * JS fallback ниже — эталонную копию исходной реализации. Если бинарь ЕСТЬ, но упал
 * (non-zero exit) — ошибка пробрасывается, fallback не используется: молчаливая
 * подмена реализации при битом Rust-коде скрывала бы регрессии.
 *
 * Мутации кластерного гейта (clusterFindings/resolveBlockers/applyPrescanBlockPolicy)
 * накладываются обёрткой на ИСХОДНЫЕ объекты находок по индексу: в JS мутации видны
 * всем владельцам разделяемой ссылки (findings ↔ clusters ↔ blockers — один набор объектов,
 * дальше по пайплайну ходит он же), и копия разъехалась бы с ним.
 *
 * Нюансы паритета Rust-порта (проверены parity-прогоном rust/parity/findings.parity.mjs):
 *   - FP-regex-матчинг (matchesKnownFalsePositive / filterFalsePositiveFindings) НЕ
 *     портирован и остаётся в JS: 2 из 51 курируемых паттернов в feedback/fp-patterns.jsonl
 *     используют lookahead (`(?=...)`), который regex-крейт Rust не поддерживает; кроме
 *     того, здесь живут кэширование паттернов и resetFpPatternsCache — публичный API
 *     lib/fp-patterns.mjs. resolveBlockers делает FP-фильтр в JS, а кластер/гейт — в Rust;
 *   - createFindingsFormatter/classify и findingBlock портированы в Фазе 1, шаге 5 —
 *     живут в Rust-бинаре actionability (classify дергает routeFindings, поэтому по
 *     связности — там; MARKERS.inline передаётся аргументом, как и раньше). Обёртка
 *     вызывает его через runActionabilityRust из lib/actionability.mjs, при недоступном
 *     бинаре — чистый JS fallback ниже;
 *   - normalizeConfidence: non-finite (NaN/Infinity) и undefined канонизируются в null
 *     ДО вызова (JSON их не несёт); экзотика JS Number(string) ("0x10", "1_000") не
 *     эмулируется полностью — см. rust/src/findings.rs;
 *   - строки с lone surrogates приходят в Rust как U+FFFD (String их не хранит);
 *   - null там, где JS коэрсит в 0 (gate.consensusMin/confidenceMin), на практике не
 *     встречается: конфиг гейта всегда полный (loadReviewConfig).
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { SEV, MARKERS, BLAST_FINDING_SOURCE } from './constants.mjs';
import { DEFAULT_FP_PATTERNS_PATH, getFpPatterns, matchFpPatterns } from './fp-patterns.mjs';
import { normalizeAdvisorClaims } from './specialists.mjs';
import { routeFindings, runActionabilityRust, filesToJson as actionabilityFilesToJson, ctxToJson as actionabilityCtxToJson, applyRouteMutations } from './actionability.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_FINDINGS_BIN = resolve(__dir, '../rust/target/debug/findings');

function resolveFindingsBin() {
  if (process.env.AI_REVIEWER_FINDINGS_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_FINDINGS_BIN || '').trim() || DEFAULT_FINDINGS_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveFindingsBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

/** undefined/NaN/Infinity → null: JSON их не несёт, семантика normalizeConfidence совпадает. */
function confidenceArg(raw) {
  if (raw === undefined || raw === null) return null;
  if (typeof raw === 'number' && !Number.isFinite(raw)) return null;
  return raw;
}

/** Map<number,string> (rec.text из parseDiff) → JSON-объект. */
function lineTextToJson(text) {
  if (!text) return null;
  return Object.fromEntries(text);
}

/** Map<string, DiffFileRecord> → {path: {text}} для fingerprint-операций. */
function filesToTextJson(files) {
  if (!files) return null;
  const out = {};
  for (const [path, rec] of files) {
    out[path] = { text: lineTextToJson(rec?.text) };
  }
  return out;
}

/** Map<string, DiffFileRecord> → {path: {validLines, text}} для carry-over-инвалидации. */
function filesToFilterJson(files) {
  const out = {};
  for (const [path, rec] of files) {
    out[path] = {
      validLines: rec?.validLines ? [...rec.validLines] : [],
      text: lineTextToJson(rec?.text),
    };
  }
  return out;
}

/** Наложить мутации гейта из Rust на исходные объекты находок (семантика разделяемых ссылок). */
function applyMutations(findings, mutations) {
  for (const m of mutations || []) {
    const f = findings[m.index];
    f.confidence = m.confidence;
    f.consensus = m.consensus;
    f.clusterModels = m.clusterModels;
    f.confidenceBlock = m.confidenceBlock;
    f.consensusBlock = m.consensusBlock;
  }
}

/**
 * @typedef {Object} Finding — единая структура находки; поля нарастают по стадиям пайплайна.
 *
 * Базовые (normalizeFinding, из ответа модели или prescan):
 * @property {string} file
 * @property {number} line
 * @property {'critical'|'major'|'minor'|'nit'} severity
 * @property {string} title
 * @property {string} body
 * @property {string} [suggestion]
 * @property {number} confidence — 0..1 (отсутствующий/битый → 0.5, см. normalizeConfidence)
 * @property {string} model — лейбл модели/агрегатора
 * @property {Array}  [advisorClaims] — jury-разбор advisor'ов (MoA specialists)
 *
 * Prescan (eslint / ast-grep):
 * @property {boolean} [prescan]
 * @property {boolean} [prescanBlock] — rule входит в blockRules
 *
 * Гейт (clusterFindings, мутация):
 * @property {boolean} [consensus] — ≥2 модели в кластере
 * @property {string[]} [clusterModels]
 * @property {boolean} [confidenceBlock]
 * @property {boolean} [consensusBlock] — итоговое решение блокировки merge
 *
 * Маршрутизация (routeFindings, мутация — у всех прошедших маршрутизацию):
 * @property {number}   [actionabilityScore] — сумма весов сигналов доказуемости (у всех)
 * @property {string[]} [actionabilityReasons] — какие сигналы сработали (у всех)
 * @property {boolean}  [actionabilityPromoted] — advisory, поднятая в инлайн по доказуемости:
 *   только она печатает объяснение «почему в строке» и не резолвится авто-триажем
 * @property {boolean}  [outOfChunk] — файл не был виден чанку (движок понизил находку до nit)
 * @property {number}   [dupCount] — сколько чанков дали ту же находку (дедуп внутри модели)
 *
 * Verifier Pass B (applyVerifierVerdicts, мутация):
 * @property {'verified'|'rejected'|'downgraded'|'unverified'|'overflow'} [verifierStatus]
 * @property {string} [verifierReason]
 *
 * Blast-radius (emitBlastRadiusFindings):
 * @property {boolean} [blastCaller] — actionability-сигнал (weight 2); сам по себе merge не смягчает без source
 * @property {boolean} [blastCrossCommunity]
 * @property {string}  [blastEdgeConfidence]
 * @property {typeof BLAST_FINDING_SOURCE} [source] — детерминированный эмиттер; soft-gate смотрит на него
 */

/**
 * Отсутствующий/битый confidence → **0.5** (не 1.0!): иначе модель обходит
 * confidence-гейт (≥0.8 для блока), просто не отдав поле — canvas P1, бэклог #4.
 */
export function normalizeConfidence(raw) {
  const r = runRust('normalizeConfidence', { raw: confidenceArg(raw) });
  if (r !== null) return r;
  return normalizeConfidenceJs(raw);
}

/** Нормализация одной находки из JSON модели (включая confidence 0–1).
 * @returns {Finding|null} */
export function normalizeFinding(f, model, { requireQuote = false } = {}) {
  const r = runRust('normalizeFinding', { finding: f ?? null, model, requireQuote: !!requireQuote });
  if (r !== null) return r; // null (не находка) приходит как JSON null
  return normalizeFindingJs(f, model, { requireQuote });
}

/** Ключ дубля: та же модель, тот же файл/строка/severity и та же по смыслу формулировка. */
export function sameFindingKey(f) {
  const r = runRust('sameFindingKey', { finding: f ?? null });
  if (r !== null) return r;
  return sameFindingKeyJs(f);
}

/**
 * Схлопывает повторы ОДНОЙ модели: чанки ревьюятся независимо, и общая тема (например, файл,
 * видимый из нескольких частей) приходит N раз — в sticky это выглядело как N разных замечаний.
 * Между моделями не дедупим: там повтор — сигнал консенсуса для clusterFindings.
 *
 * Возвращает КОПИИ (не мутация входа): вызывающий (aggregateModel) отдаёт массив, который живёт
 * в результатах чанков, и дописывать в него служебное поле нельзя.
 */
export function dedupeSameModelFindings(findings, log = () => {}) {
  const r = runRust('dedupeSameModelFindings', { findings: findings || [] });
  if (r !== null) {
    const dropped = (findings?.length || 0) - r.kept.length;
    if (dropped > 0) log(`дедуп повторов внутри модели: ${dropped} (одна тема из нескольких чанков)`);
    return r.kept;
  }
  return dedupeSameModelFindingsJs(findings, log);
}

// ---------------------------------------------------------------------------
// FP-фильтр — НЕ портирован в Rust (JS lookahead в паттернах), остаётся чистым JS.
// ---------------------------------------------------------------------------

export function matchesKnownFalsePositive(text, changedPaths, opts = {}) {
  const patternsFile = opts.patternsFile || DEFAULT_FP_PATTERNS_PATH;
  const patterns = opts.patterns || getFpPatterns(patternsFile);
  return Boolean(matchFpPatterns(text, changedPaths, patterns));
}

export function filterFalsePositiveFindings(findings, changedPaths, log = () => {}, fpOpts = {}) {
  return findings.filter((f) => {
    const text = `${f.title} ${f.body}`;
    if (matchesKnownFalsePositive(text, changedPaths, fpOpts)) {
      log(`filtered false finding: ${f.title}`);
      return false;
    }
    return true;
  });
}

/** Blast soft-gate policy (вызывается из clusterFindings): чистый blast-кластер
 * блокирует merge только после verifier. Fail-open, если verifier не бежал.
 * blastCaller без source — не soft (иначе LLM-находка с actionability-флагом ослабит гейт). */
export function softConsensusForBlastCluster(blockTier) {
  const r = runRust('softConsensusForBlastCluster', { blockTier: blockTier || [] });
  if (r !== null) return r; // null приходит как JSON null
  return softConsensusForBlastClusterJs(blockTier);
}

/** Группирует находки по file|line-bucket; мутирует элементы (consensus, consensusBlock). */
export function clusterFindings(allFindings, gate) {
  const r = runRust('clusterFindings', { findings: allFindings || [], gate: gate || {} });
  if (r !== null) {
    applyMutations(allFindings, r.mutations);
    return r.clusters.map((group) => group.map((i) => allFindings[i]));
  }
  return clusterFindingsJs(allFindings, gate);
}

/** Отпечаток строки, на которую показывает находка: код, а не координата.
 * Пробелы схлопнуты — переформатирование отступов не должно выглядеть как починка.
 *
 * Берём ТОЛЬКО собственную строку, без окрестности: с окном соседей починка одной строки
 * стирала отпечатки соседних находок, и они снимались вместе с исправленной, хотя живы.
 * Плата за узкое окно — коллизии на типовых строках (`}`), они разрешаются выбором
 * ближайшего совпадения к прежнему номеру строки.
 *
 * Возвращает '' если строки нет (файл вне диффа, строка вне hunks) или она пустая. */
export function findingFingerprint(rec, line) {
  const r = runRust('findingFingerprint', { text: lineTextToJson(rec?.text), line: line ?? null });
  if (r !== null) return r;
  return findingFingerprintJs(rec, line);
}

/**
 * Проставляет `fp` — отпечаток кода под находкой — во ВСЕ находки прогона.
 *
 * Мутация намеренная: дальше по пайплайну ходит тот же массив объектов (кластеры, гейт,
 * экспорт, triage) — копия разъехалась бы с ним. Пустой отпечаток (файл вне диффа, строка
 * вне hunks) не пишется: `fp: ''` в артефакте читался бы как «отпечаток есть».
 */
export function attachFingerprints(findings, files) {
  if (!findings) return findings;
  const r = runRust('attachFingerprints', { findings, files: filesToTextJson(files) });
  if (r !== null) {
    for (const { index, fp } of r) findings[index].fp = fp;
    return findings;
  }
  return attachFingerprintsJs(findings, files);
}

/**
 * Инвалидация prior findings (carry-over) по СОДЕРЖИМОМУ, а не по позиции.
 *
 * В футере sticky у находки лежит `fp` — отпечаток кода, на который она показывала.
 * На следующем прогоне:
 *   - отпечаток нашёлся в файле (пусть на другой строке) → код не тронут, находку несём
 *     дальше, обновив номер строки;
 *   - не нашёлся → код в этом месте изменён (или файл вышел из PR) → находка снимается;
 *   - у находки нет `fp` (старый футер) → работает прежнее правило ±3, чтобы первый прогон
 *     после выката не потерял carry-over целиком.
 */
export function filterStalePriorFindings(priors, files, log = () => {}) {
  const r = runRust('filterStalePriorFindings', { priors: priors || [], files: filesToFilterJson(files) });
  if (r !== null) {
    const kept = r.kept.map(({ index, line }) => {
      const p = priors[index];
      return line === (Number(p.line) || 0) ? p : { ...p, line };
    });
    // Причины снятия считаются раздельно: «файл вышел из PR» и «код в этом месте изменён» —
    // разные истории, и по одному общему счётчику не понять, работает ли инвалидация.
    const reasons = [
      r.codeChanged && `код изменён: ${r.codeChanged}`,
      r.outOfDiff && `файл вне диффа: ${r.outOfDiff}`,
      r.staleLegacy && `правило ±3 (без fingerprint): ${r.staleLegacy}`,
    ].filter(Boolean);
    if (reasons.length) log(`prior findings: снято ${reasons.join(', ')}`);
    if (r.legacy) log(`prior findings: ${r.legacy} из старого sticky без fingerprint — сравнение по ±3 строки`);
    return kept;
  }
  return filterStalePriorFindingsJs(priors, files, log);
}

/**
 * Снимает prior findings, починенные в диффе между прогонами (adjacent commit на PR).
 *
 * @deprecated Имя для adjacent-commit семантики. Канон — {@link filterStalePriorFindings}
 * (полная fingerprint-инвалидация). Алиас сохранён для публичного API.
 */
export function filterFindingsFixedInDiff(priors, files, log = () => {}) {
  return filterStalePriorFindings(priors, files, log);
}

/** Prescan: блокируют только rule из blockRules; REVIEW_PRESCAN_BLOCK=0 → advisory. */
export function applyPrescanBlockPolicy(clusters, prescanCfg) {
  if (!prescanCfg || prescanCfg.enabled === false) return;
  const findings = clusters.flat();
  const r = runRust('applyPrescanBlockPolicy', {
    findings,
    clusters: clusters.map((g) => g.map((f) => findings.indexOf(f))),
    prescan: prescanCfg,
  });
  if (r !== null) {
    for (const m of r) findings[m.index].consensusBlock = m.consensusBlock;
    return;
  }
  applyPrescanBlockPolicyJs(clusters, prescanCfg);
}

/** Filter → cluster → блокеры (pipeline + eval harness).
 * FP-фильтр — в JS (regex-паритет), кластер/гейт/блокеры — в Rust. */
export function resolveBlockers(allFindings, changedPaths, gate, log = () => {}, opts = {}) {
  const { skipFpFilter = false, prescan: prescanCfg } = opts;
  const findings = skipFpFilter
    ? allFindings
    : filterFalsePositiveFindings(allFindings, changedPaths, log, opts);
  const r = runRust('resolveBlockers', { findings, gate: gate || {}, prescan: prescanCfg ?? null });
  if (r !== null) {
    applyMutations(findings, r.mutations);
    for (const m of r.prescanMutations || []) findings[m.index].consensusBlock = m.consensusBlock;
    const clusters = r.clusters.map((group) => group.map((i) => findings[i]));
    const blockers = r.blockers.map((i) => findings[i]);
    return { findings, clusters, blockers, wouldBlock: r.wouldBlock };
  }
  const clusters = clusterFindingsJs(findings, gate);
  applyPrescanBlockPolicyJs(clusters, prescanCfg);
  const blockers = clusters
    .filter((g) => g.some((f) => f.consensusBlock))
    .map((g) => g.find((f) => f.consensusBlock) || g[0]);
  return { findings, clusters, blockers, wouldBlock: blockers.length > 0 };
}

export function createFindingsFormatter(cfg) {
  const inlineOn = cfg.inlineOn?.length ? cfg.inlineOn : ['critical', 'major'];

  function findingBlock(f) {
    const r = runActionabilityRust('findingBlock', { finding: f });
    if (r !== null) return r;
    return findingBlockJs(f);
  }

  /**
   * Маршрут каждой находки считает `routeFindings`; проверка «строка есть в диффе» живёт
   * там же (`isInlineable`) — единственное место, где решается физическая возможность
   * инлайна, чтобы гард не разъехался между двумя копиями.
   * Блокеры — всегда в строку, advisory —
   * если доказуемость набрала порог (см. lib/actionability.mjs). `ctx` приносит сигналы,
   * которых нет в самой находке: повтор с прошлого прогона, попадание prescan, класс файла.
   * Портировано в Rust-бинарь actionability (Фаза 1, шаг 5): route + группировка + рендер
   * в одном процессе; MARKERS.inline активного профиля передаётся аргументом.
   */
  function classify(findings, files, ctx = {}) {
    const r = runActionabilityRust('classifyFindings', {
      findings,
      files: actionabilityFilesToJson(files),
      cfg: {
        inlineOn,
        threshold: cfg.actionability?.threshold,
        maxAdvisoryInline: cfg.actionability?.maxAdvisoryInline,
        strict: cfg.gate?.strict,
      },
      ctx: actionabilityCtxToJson(findings, ctx),
      markerInline: MARKERS.inline,
    });
    if (r !== null) {
      applyRouteMutations(findings, r.mutations);
      return { inline: r.inline, outside: r.outside.map((i) => findings[i]) };
    }
    return classifyJs(findings, files, ctx, cfg);
  }

  return { classify, findingBlock };
}

// ---------------------------------------------------------------------------
// Чистый JS fallback createFindingsFormatter (эталонная копия до Фазы 1, шага 5)
// ---------------------------------------------------------------------------

function findingBlockJs(f) {
  const head = `${SEV[f.severity] || '⚪'} **${f.severity}: ${f.title || 'замечание'}**`;
  // Advisory, поднятая в строку по доказуемости, объясняет, почему она здесь: иначе
  // «nit в инлайне» читается как шум, а не как «это проверено и локализовано».
  const why = f.actionabilityPromoted && f.actionabilityReasons?.length
    ? `\n\n<sub>📌 в строке, потому что: ${f.actionabilityReasons.join('; ')}. Не блокирует merge.</sub>`
    : '';
  const sug = f.suggestion ? `\n\n\`\`\`suggestion\n${f.suggestion}\n\`\`\`` : '';
  return `${head}\n\n${f.body || ''}${sug}${why}`;
}

function classifyJs(findings, files, ctx = {}, cfg) {
  const groups = new Map();
  const outside = [];
  const inlineOn = cfg.inlineOn?.length ? cfg.inlineOn : ['critical', 'major'];
  const routed = routeFindings(findings, files, {
    inlineOn,
    threshold: cfg.actionability?.threshold,
    maxAdvisoryInline: cfg.actionability?.maxAdvisoryInline,
    strict: cfg.gate?.strict,
    ctx,
  });
  for (const { finding: f, route } of routed) {
    if (route === 'inline') {
      const k = `${f.file}:${Number(f.line) || 0}`;
      if (!groups.has(k)) groups.set(k, []);
      groups.get(k).push(f);
    } else {
      outside.push(f);
    }
  }
  const inline = [...groups.values()].map((group) => ({
    path: group[0].file, line: group[0].line, side: 'RIGHT', body: fmtInlineGroup(group),
  }));
  return { inline, outside };

  function fmtInlineGroup(group) {
    const blocks = group.map(findingBlockJs).join('\n\n---\n\n');
    return `${MARKERS.inline}\n${blocks}`;
  }
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/findings.mjs до Фазы 1).
// Используется только когда Rust-бинарь отсутствует; при битом бинаре НЕ подменяет его.
// ---------------------------------------------------------------------------

function normalizeConfidenceJs(raw) {
  if (raw == null || raw === '') return 0.5;
  const n = Number(raw);
  if (!Number.isFinite(n)) return 0.5;
  return Math.min(1, Math.max(0, n));
}

function normalizeFindingJs(f, model, { requireQuote = false } = {}) {
  if (!f || typeof f !== 'object') return null;
  const quote = typeof f.quote === 'string' ? f.quote.trim() : '';
  const severity = ['critical', 'major', 'minor', 'nit'].includes(f.severity) ? f.severity : 'minor';
  if (requireQuote && !quote && ['critical', 'major'].includes(severity)) return null;
  return {
    file: typeof f.file === 'string' ? f.file : '',
    line: Number(f.line) || 0,
    severity,
    title: typeof f.title === 'string' ? f.title : 'замечание',
    body: typeof f.body === 'string' ? f.body : '',
    suggestion: typeof f.suggestion === 'string' ? f.suggestion : undefined,
    quote: quote || undefined,
    confidence: normalizeConfidenceJs(f.confidence),
    // `?? undefined`: normalizeAdvisorClaims — сама обёртка над бинарём specialists, и «claims нет»
    // она отдаёт как JSON `null`, когда бинарь собран, и как `undefined`, когда нет. Разница видна
    // снаружи: JSON.stringify роняет `undefined`, но несёт `null`, поэтому находка уезжала бы в
    // sticky-футер и в версионированный артефакт экспорта то с полем `advisorClaims: null`, то без
    // поля вовсе — в зависимости от того, гоняли ли `cargo build`. Абсентность — единственное
    // значение, которое умеет отдать Rust-порт (Value без undefined) и которого ждут читатели:
    // typedef объявляет поле опциональным массивом, а strict-refutation, actionability и export
    // различают «жюри высказалось» и «жюри не было» по наличию массива, а не по null.
    advisorClaims: normalizeAdvisorClaims(f.advisorClaims) ?? undefined,
    model,
  };
}

function sameFindingKeyJs(f) {
  const title = String(f?.title || '')
    .toLowerCase()
    .replace(/[`*_"«»]/g, '')
    .replace(/\s+/g, ' ')
    .replace(/[.,;:!?\s]+$/, '')
    .trim();
  // Путь НЕ приводим к нижнему регистру: ключ должен совпадать по семантике с clusterFindings,
  // где регистр значим (`a.ts` и `A.ts` — разные файлы на case-sensitive ФС).
  return `${f?.model || ''}|${String(f?.file || '').trim()}|${Number(f?.line) || 0}|${f?.severity || ''}|${title}`;
}

function dedupeSameModelFindingsJs(findings, log = () => {}) {
  const seen = new Map();
  for (const f of findings || []) {
    const key = sameFindingKeyJs(f);
    const prev = seen.get(key);
    if (prev) prev.dupCount = (prev.dupCount || 1) + 1;
    else seen.set(key, { ...f });
  }
  const kept = [...seen.values()];
  const dropped = (findings?.length || 0) - kept.length;
  if (dropped > 0) log(`дедуп повторов внутри модели: ${dropped} (одна тема из нескольких чанков)`);
  return kept;
}

function softConsensusForBlastClusterJs(blockTier) {
  if (!blockTier?.length) return null;
  if (!blockTier.every((f) => f.source === BLAST_FINDING_SOURCE)) return null;
  return blockTier.some((f) => f.verifierStatus === 'verified');
}

function clusterFindingsJs(allFindings, gate) {
  const { blockOn, blockSingle, consensusMin, confidenceMin = 0.8, strict = false } = gate;
  const keyOf = (f) => `${f.file}|${f.line > 0 ? Math.floor((f.line - 1) / 3) : 'out'}`;
  const clusters = new Map();
  for (const f of allFindings) {
    const k = keyOf(f);
    if (!clusters.has(k)) clusters.set(k, []);
    clusters.get(k).push(f);
  }
  for (const group of clusters.values()) {
    const models = new Set(group.map((f) => f.model));
    const singleModels = new Set(group.filter((f) => blockSingle.includes(f.severity)).map((f) => f.model));
    const consensusModels = new Set(
      group.filter((f) => blockOn.includes(f.severity) && !blockSingle.includes(f.severity)).map((f) => f.model),
    );
    const tierBlock = singleModels.size >= 1 || consensusModels.size >= consensusMin;
    const blockCandidates = group.filter((f) => blockOn.includes(f.severity));
    const maxConf = blockCandidates.length
      ? Math.max(...blockCandidates.map((f) => normalizeConfidenceJs(f.confidence)))
      : 1;
    const confidenceBlock = !blockCandidates.length || maxConf >= confidenceMin;
    let consensusBlock = tierBlock && confidenceBlock;
    if (strict && consensusBlock) {
      consensusBlock = group.some((f) =>
        blockOn.includes(f.severity) && (f.prescan || f.verifierStatus === 'verified'));
    } else if (consensusBlock) {
      const blockTier = group.filter((f) => blockOn.includes(f.severity));
      const soft = softConsensusForBlastClusterJs(blockTier);
      if (soft !== null) consensusBlock = soft;
    }
    for (const f of group) {
      f.confidence = normalizeConfidenceJs(f.confidence);
      f.consensus = models.size >= 2;
      f.clusterModels = [...models];
      f.confidenceBlock = confidenceBlock;
      f.consensusBlock = consensusBlock;
    }
  }
  return [...clusters.values()];
}

function findingFingerprintJs(rec, line) {
  const n = Number(line) || 0;
  const raw = n > 0 ? rec?.text?.get(n) : undefined;
  const body = String(raw ?? '').trim().replace(/\s+/g, ' ');
  if (!body) return '';
  return createHash('sha256').update(body).digest('hex').slice(0, 12);
}

function attachFingerprintsJs(findings, files) {
  for (const f of findings || []) {
    const fp = findingFingerprintJs(files?.get(f.file), f.line);
    if (fp) f.fp = fp;
  }
  return findings;
}

/** Все отпечатки файла: fp → номера строк (их может быть несколько — типовые строки). */
function fileFingerprintsJs(rec) {
  const map = new Map();
  if (!rec?.text) return map;
  for (const line of rec.text.keys()) {
    const fp = findingFingerprintJs(rec, line);
    if (!fp) continue;
    if (map.has(fp)) map.get(fp).push(line);
    else map.set(fp, [line]);
  }
  return map;
}

/**
 * Из совпавших строк берём ближайшую к прежней позиции находки; при равном расстоянии —
 * меньший номер. Пустой список возвращает 0: `reduce` без начального значения бросил бы
 * TypeError, а вызывающий может измениться и перестать проверять длину заранее.
 */
const nearestLineJs = (lines, line) => (lines?.length
  ? lines.reduce((best, cur) => (Math.abs(cur - line) < Math.abs(best - line) ? cur : best))
  : 0);

function filterStalePriorFindingsJs(priors, files, log = () => {}) {
  const kept = [];
  const fpCache = new Map();
  const outOfDiff = [];
  const codeChanged = [];
  const staleLegacy = [];
  let legacy = 0;
  for (const f of priors || []) {
    const rec = f.file ? files.get(f.file) : null;
    const line = Number(f.line) || 0;
    if (!rec) { outOfDiff.push(f); continue; }
    if (f.fp) {
      if (!fpCache.has(f.file)) fpCache.set(f.file, fileFingerprintsJs(rec));
      const hits = fpCache.get(f.file).get(f.fp);
      if (!hits?.length) { codeChanged.push(f); continue; }
      const at = nearestLineJs(hits, line);
      kept.push(at === line ? f : { ...f, line: at });
      continue;
    }
    legacy++;
    let fresh = false;
    if (line > 0) {
      for (const vl of rec.validLines) {
        if (Math.abs(vl - line) <= 3) { fresh = true; break; }
      }
    }
    if (fresh) kept.push(f);
    else staleLegacy.push(f);
  }
  const reasons = [
    codeChanged.length && `код изменён: ${codeChanged.length}`,
    outOfDiff.length && `файл вне диффа: ${outOfDiff.length}`,
    staleLegacy.length && `правило ±3 (без fingerprint): ${staleLegacy.length}`,
  ].filter(Boolean);
  if (reasons.length) log(`prior findings: снято ${reasons.join(', ')}`);
  if (legacy) log(`prior findings: ${legacy} из старого sticky без fingerprint — сравнение по ±3 строки`);
  return kept;
}

function applyPrescanBlockPolicyJs(clusters, prescanCfg) {
  if (!prescanCfg || prescanCfg.enabled === false) return;
  for (const group of clusters) {
    for (const f of group) {
      if (!f.prescan) continue;
      if (prescanCfg.blockEnabled === false || !f.prescanBlock) {
        f.consensusBlock = false;
      }
    }
  }
}
