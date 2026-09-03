import { createHash } from 'node:crypto';
import { SEV, MARKERS, BLAST_FINDING_SOURCE } from './constants.mjs';
import { DEFAULT_FP_PATTERNS_PATH, getFpPatterns, matchFpPatterns } from './fp-patterns.mjs';
import { normalizeAdvisorClaims } from './specialists.mjs';
import { routeFindings } from './actionability.mjs';

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
  if (raw == null || raw === '') return 0.5;
  const n = Number(raw);
  if (!Number.isFinite(n)) return 0.5;
  return Math.min(1, Math.max(0, n));
}

/** Нормализация одной находки из JSON модели (включая confidence 0–1).
 * @returns {Finding|null} */
export function normalizeFinding(f, model, { requireQuote = false } = {}) {
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
    confidence: normalizeConfidence(f.confidence),
    advisorClaims: normalizeAdvisorClaims(f.advisorClaims),
    model,
  };
}

/** Ключ дубля: та же модель, тот же файл/строка/severity и та же по смыслу формулировка. */
export function sameFindingKey(f) {
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

/**
 * Схлопывает повторы ОДНОЙ модели: чанки ревьюятся независимо, и общая тема (например, файл,
 * видимый из нескольких частей) приходит N раз — в sticky это выглядело как N разных замечаний.
 * Между моделями не дедупим: там повтор — сигнал консенсуса для clusterFindings.
 */
export function dedupeSameModelFindings(findings, log = () => {}) {
  const seen = new Map();
  for (const f of findings || []) {
    const key = sameFindingKey(f);
    const prev = seen.get(key);
    // Копия, а не мутация входа: вызывающий (aggregateModel) отдаёт массив, который живёт
    // в результатах чанков, и дописывать в него служебное поле нельзя.
    if (prev) prev.dupCount = (prev.dupCount || 1) + 1;
    else seen.set(key, { ...f });
  }
  const kept = [...seen.values()];
  const dropped = (findings?.length || 0) - kept.length;
  if (dropped > 0) log(`дедуп повторов внутри модели: ${dropped} (одна тема из нескольких чанков)`);
  return kept;
}

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

/**
 * Blast soft-gate policy (вызывается из clusterFindings): чистый blast-кластер
 * блокирует merge только после verifier. Fail-open, если verifier не бежал.
 * blastCaller без source — не soft (иначе LLM-находка с actionability-флагом ослабит гейт).
 */
export function softConsensusForBlastCluster(blockTier) {
  if (!blockTier?.length) return null;
  if (!blockTier.every((f) => f.source === BLAST_FINDING_SOURCE)) return null;
  return blockTier.some((f) => f.verifierStatus === 'verified');
}

/** Группирует находки по file|line-bucket; мутирует элементы (consensus, consensusBlock). */
export function clusterFindings(allFindings, gate) {
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
      ? Math.max(...blockCandidates.map((f) => normalizeConfidence(f.confidence)))
      : 1;
    const confidenceBlock = !blockCandidates.length || maxConf >= confidenceMin;
    let consensusBlock = tierBlock && confidenceBlock;
    if (strict && consensusBlock) {
      consensusBlock = group.some((f) =>
        blockOn.includes(f.severity) && (f.prescan || f.verifierStatus === 'verified'));
    } else if (consensusBlock) {
      const blockTier = group.filter((f) => blockOn.includes(f.severity));
      const soft = softConsensusForBlastCluster(blockTier);
      if (soft !== null) consensusBlock = soft;
    }
    for (const f of group) {
      f.confidence = normalizeConfidence(f.confidence);
      f.consensus = models.size >= 2;
      f.clusterModels = [...models];
      f.confidenceBlock = confidenceBlock;
      f.consensusBlock = consensusBlock;
    }
  }
  return [...clusters.values()];
}

/**
 * Отпечаток строки, на которую показывает находка: код, а не координата.
 * Пробелы схлопнуты — переформатирование отступов не должно выглядеть как починка.
 *
 * Берём ТОЛЬКО собственную строку, без окрестности: с окном соседей починка одной строки
 * стирала отпечатки соседних находок, и они снимались вместе с исправленной, хотя живы.
 * Плата за узкое окно — коллизии на типовых строках (`}`), они разрешаются выбором
 * ближайшего совпадения к прежнему номеру строки.
 *
 * Возвращает '' если строки нет (файл вне диффа, строка вне hunks) или она пустая.
 */
export function findingFingerprint(rec, line) {
  const n = Number(line) || 0;
  const raw = n > 0 ? rec?.text?.get(n) : undefined;
  const body = String(raw ?? '').trim().replace(/\s+/g, ' ');
  if (!body) return '';
  return createHash('sha256').update(body).digest('hex').slice(0, 12);
}

/**
 * Проставляет `fp` — отпечаток кода под находкой — во ВСЕ находки прогона.
 *
 * Раньше отпечаток считался только внутри `buildSummary`, в локальный список для футера
 * sticky. В артефакт экспорта уходил тот же массив находок, но уже без `fp`, и потребитель
 * (cod-doc ingest) дедуплицировал их по заголовку: переформулированная находка на той же
 * строке считалась новой. Отпечаток нужен обоим потребителям — считаем его один раз здесь,
 * до ветвления «футер / экспорт».
 *
 * Мутация намеренная: дальше по пайплайну ходит тот же массив объектов (кластеры, гейт,
 * экспорт, triage) — копия разъехалась бы с ним. Пустой отпечаток (файл вне диффа, строка
 * вне hunks) не пишется: `fp: ''` в артефакте читался бы как «отпечаток есть».
 */
export function attachFingerprints(findings, files) {
  for (const f of findings || []) {
    const fp = findingFingerprint(files?.get(f.file), f.line);
    if (fp) f.fp = fp;
  }
  return findings;
}

/** Все отпечатки файла: fp → номера строк (их может быть несколько — типовые строки). */
function fileFingerprints(rec) {
  const map = new Map();
  if (!rec?.text) return map;
  for (const line of rec.text.keys()) {
    const fp = findingFingerprint(rec, line);
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
const nearestLine = (lines, line) => (lines?.length
  ? lines.reduce((best, cur) => (Math.abs(cur - line) < Math.abs(best - line) ? cur : best))
  : 0);

/**
 * Инвалидация prior findings (carry-over) по СОДЕРЖИМОМУ, а не по позиции.
 *
 * Раньше находка считалась живой, если её файл в диффе и рядом (±3 строки) есть изменённая
 * строка. Правило инвертировало смысл: чем точнее чинили помеченную строку, тем надёжнее
 * находка проходила фильтр как «актуальная» и возвращалась прогон за прогоном (PR #520).
 *
 * Теперь в футере sticky у находки лежит `fp` — отпечаток кода, на который она показывала.
 * На следующем прогоне:
 *   - отпечаток нашёлся в файле (пусть на другой строке) → код не тронут, находку несём
 *     дальше, обновив номер строки;
 *   - не нашёлся → код в этом месте изменён (или файл вышел из PR) → находка снимается;
 *   - у находки нет `fp` (старый футер) → работает прежнее правило ±3, чтобы первый прогон
 *     после выката не потерял carry-over целиком.
 */
export function filterStalePriorFindings(priors, files, log = () => {}) {
  const kept = [];
  const fpCache = new Map();
  // Причины снятия считаем раздельно: «файл вышел из PR» и «код в этом месте изменён» —
  // разные истории, и по одному общему счётчику не понять, работает ли инвалидация.
  const outOfDiff = [];
  const codeChanged = [];
  const staleLegacy = [];
  let legacy = 0;
  for (const f of priors || []) {
    const rec = f.file ? files.get(f.file) : null;
    const line = Number(f.line) || 0;
    if (!rec) { outOfDiff.push(f); continue; }
    if (f.fp) {
      if (!fpCache.has(f.file)) fpCache.set(f.file, fileFingerprints(rec));
      const hits = fpCache.get(f.file).get(f.fp);
      if (!hits?.length) { codeChanged.push(f); continue; }
      const at = nearestLine(hits, line);
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
  for (const group of clusters) {
    for (const f of group) {
      if (!f.prescan) continue;
      if (prescanCfg.blockEnabled === false || !f.prescanBlock) {
        f.consensusBlock = false;
      }
    }
  }
}

/** Filter → cluster → блокеры (pipeline + eval harness). */
export function resolveBlockers(allFindings, changedPaths, gate, log = () => {}, opts = {}) {
  const { skipFpFilter = false, prescan: prescanCfg, patternsFile } = opts;
  const fpOpts = patternsFile ? { patternsFile } : {};
  const findings = skipFpFilter
    ? allFindings
    : filterFalsePositiveFindings(allFindings, changedPaths, log, fpOpts);
  const clusters = clusterFindings(findings, gate);
  applyPrescanBlockPolicy(clusters, prescanCfg);
  const blockers = clusters
    .filter((g) => g.some((f) => f.consensusBlock))
    .map((g) => g.find((f) => f.consensusBlock) || g[0]);
  return { findings, clusters, blockers, wouldBlock: blockers.length > 0 };
}

export function createFindingsFormatter(cfg) {
  const inlineOn = cfg.inlineOn?.length ? cfg.inlineOn : ['critical', 'major'];

  function findingBlock(f) {
    const head = `${SEV[f.severity] || '⚪'} **${f.severity}: ${f.title || 'замечание'}**`;
    // Advisory, поднятая в строку по доказуемости, объясняет, почему она здесь: иначе
    // «nit в инлайне» читается как шум, а не как «это проверено и локализовано».
    const why = f.actionabilityPromoted && f.actionabilityReasons?.length
      ? `\n\n<sub>📌 в строке, потому что: ${f.actionabilityReasons.join('; ')}. Не блокирует merge.</sub>`
      : '';
    const sug = f.suggestion ? `\n\n\`\`\`suggestion\n${f.suggestion}\n\`\`\`` : '';
    return `${head}\n\n${f.body || ''}${sug}${why}`;
  }

  function fmtInlineGroup(group) {
    const blocks = group.map(findingBlock).join('\n\n---\n\n');
    return `${MARKERS.inline}\n${blocks}`;
  }

  /**
   * Маршрут каждой находки считает `routeFindings`; проверка «строка есть в диффе» живёт
   * там же (`isInlineable`) — единственное место, где решается физическая возможность
   * инлайна, чтобы гард не разъехался между двумя копиями.
   * Блокеры — всегда в строку, advisory —
   * если доказуемость набрала порог (см. lib/actionability.mjs). `ctx` приносит сигналы,
   * которых нет в самой находке: повтор с прошлого прогона, попадание prescan, класс файла.
   */
  function classify(findings, files, ctx = {}) {
    const groups = new Map();
    const outside = [];
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
  }

  return { classify, findingBlock };
}
