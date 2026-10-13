/**
 * Фаза 3, шаг 5: lib/report.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/report (рендер sticky-комментария: вердикт, Δ-блок,
 * история прогонов, reasoning-секция, машинный футер).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Потребители
 * (pipeline.mjs, тесты, gold-прогоны) вызывают функции СИНХРОННО, поэтому вместо
 * async execFile используется execFileSync: бинарь вызывается на каждую операцию
 * с JSON {"op", ...args} на stdin, ответ — JSON с результатом.
 *
 * Резолвинг бинаря (по аналогии с lib/findings.mjs):
 *   AI_REVIEWER_REPORT_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/report;
 *   AI_REVIEWER_REPORT_FORCE_JS=1 — принудительно чистый JS (паритет/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm
 * test/eval идут раньше cargo build, см. .github/workflows/ci.yml), обёртка
 * деградирует на чистый JS fallback ниже — эталонную копию исходной реализации.
 * Если бинарь ЕСТЬ, но упал (non-zero exit) — ошибка пробрасывается, fallback
 * не используется: молчаливая подмена при битом Rust-коде скрывала бы регрессии.
 *
 * КРИТИЧНО для побайтовости: маркеры и regex-парсинг футера живут в
 * lib/constants.mjs (MARKERS — getter'ы активного профиля). Обёртка читает
 * MARKERS.review/dataKey на каждый вызов и передаёт бинарю аргументом (как
 * MARKERS.inline в findings): sticky upsert находит собственные комментарии
 * по маркеру, футер парсится следующим прогоном — рассинхрон маркеров ломает
 * carry-over. SEV продублирован в Rust (op `sev` — страж рассинхрона, гоняется
 * parity-прогоном). Футер внутри buildSummary сериализуется js_json_stringify
 * с семантикой V8 (порядок ключей, escape, числа) + replace `-->`/`<!--` —
 * побайтовый паритет проверен rust/parity/report.parity.mjs.
 *
 * Нюансы паритета Rust-порта:
 *   - `files` (Map<string, DiffFileRecord>) не круглится через JSON: обёртка
 *     передаёт fileCount + карту текстов файлов; запасной fp вычисляется в
 *     Rust тем же finding_fingerprint (crate::findings);
 *   - `blockerClusters` нужен buildSummary только по длине: обёртка передаёт
 *     число (обращение к input.blockerClusters.length сохранено — при undefined
 *     падает как в JS);
 *   - обрезки строк/бюджеты идут по UTF-16 code units; js_slice_utf16 не рвёт
 *     суррогатную пару (JS дал бы lone surrogate — в String недопустим);
 *   - toFixed стоимости: Rust decimal rounding, расхождение с V8 только на
 *     точных бинарных тайях (0.125) — реальными суммами не достигается.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { SEV, MARKERS } from './constants.mjs';
import { extractReasoningEntries } from './export.mjs';
import { publicErrorLabel } from './util.mjs';
import { findingFingerprint } from './findings.mjs';
import { formatCostStickyLine } from './cost.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_REPORT_BIN = resolve(__dir, '../rust/target/debug/report');

function resolveReportBin() {
  if (process.env.AI_REVIEWER_REPORT_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_REPORT_BIN || '').trim() || DEFAULT_REPORT_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveReportBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

// ---------------------------------------------------------------------------
// Сериализация аргументов
// ---------------------------------------------------------------------------

/** Error-объекты не круглятся через JSON (JSON.stringify(Error) === '{}' —
 *  у Error нет enumerable-полей): заменяем на {message}, что эквивалентно
 *  тому, что publicErrorLabel читает через err.message. */
function normalizeErrors(value) {
  if (value instanceof Error) return { message: value.message };
  if (Array.isArray(value)) return value.map(normalizeErrors);
  if (value && typeof value === 'object') {
    const out = {};
    for (const [k, v] of Object.entries(value)) out[k] = normalizeErrors(v);
    return out;
  }
  return value;
}

/** Map<number,string> (rec.text из parseDiff) → JSON-объект. */
function lineTextToJson(text) {
  if (!text) return null;
  return Object.fromEntries(text);
}

/** Map<string, DiffFileRecord> → {path: {line: text}} для запасного fp в Rust. */
function filesToTextJson(files) {
  const out = {};
  for (const [path, rec] of files) {
    out[path] = lineTextToJson(rec?.text);
  }
  return out;
}

/** slim-конфиг для Rust (gate/модели/MoA/verifier/specialists/export). */
function summaryCfgJson(cfg) {
  return {
    gate: {
      blockOn: cfg.gate.blockOn,
      blockSingle: cfg.gate.blockSingle,
      consensusMin: cfg.gate.consensusMin,
      confidenceMin: cfg.gate.confidenceMin,
    },
    reviewMode: cfg.reviewMode,
    models: { list: cfg.models.list },
    moa: { aggregator: cfg.moa.aggregator, references: cfg.moa.references },
    verifier: { enabled: cfg.verifier.enabled, model: cfg.verifier.model },
    specialists: { enabled: cfg.specialists.enabled, assignments: cfg.specialists.assignments },
    export: { reasoningInSticky: cfg.export?.reasoningInSticky },
  };
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

const REASONING_ADVISOR_MAX = 1800;
const REASONING_SECTION_MAX = 10_000;
const STICKY_REASONING_EXCERPT = 400;
// Сколько advisory-находок показываем в sticky «вне строк диффа» (остальные — в artifact).
const OUTSIDE_MAX = 10;

/** Excerpt для sticky footer JSON (без полного prose). */
export function buildStickyReasoningFooter(reviews, reviewMode, { excerptLen = STICKY_REASONING_EXCERPT } = {}) {
  const r = runRust('buildStickyReasoningFooter', { reviews: normalizeErrors(reviews) ?? null, reviewMode, excerptLen });
  if (r !== null) return r;
  return buildStickyReasoningFooterJs(reviews, reviewMode, { excerptLen });
}

/** Collapsible MoA advisor prose / consensus summaries + jury (advisorClaims). */
export function buildReasoningSection(reviews, allFindings, { enabled = true, reviewMode = 'consensus' } = {}) {
  const r = runRust('buildReasoningSection', { reviews: normalizeErrors(reviews), allFindings: allFindings ?? null, enabled, reviewMode });
  if (r !== null) return r;
  return buildReasoningSectionJs(reviews, allFindings, { enabled, reviewMode });
}

/** Sticky для полного LLM-аутеджа: отдельный, но единый formatter fail-open/fail-closed путей. */
export function buildLlmUnavailableSummary(args) {
  const r = runRust('buildLlmUnavailableSummary', {
    llmRequired: Boolean(args?.llmRequired),
    whyPublic: args?.whyPublic ?? null,
    models: args?.models ?? null,
    prescanBlockers: args?.prescanBlockers ?? null,
    prescanAdvisoryFindings: args?.prescanAdvisoryFindings ?? null,
    reviewMarker: MARKERS.review,
  });
  if (r !== null) return r;
  return buildLlmUnavailableSummaryJs(args);
}

export function findingKeys(f) {
  const r = runRust('findingKeys', { finding: f ?? null });
  if (r !== null) return r;
  return findingKeysJs(f);
}

export const findingIdentity = (f) => findingKeys(f).finding;

export function diffFindings(current, prior) {
  const r = runRust('diffFindings', { current: current ?? null, prior: prior ?? null });
  if (r !== null) return r;
  return diffFindingsJs(current, prior);
}

export function findingPointDelta(current, prior) {
  const r = runRust('findingPointDelta', { current: current ?? null, prior: prior ?? null });
  if (r !== null) return r;
  return findingPointDeltaJs(current, prior);
}

export function findingsStability(current, prior, { priorKnown = true } = {}) {
  const r = runRust('findingsStability', { current: current ?? null, prior: prior ?? null, priorKnown });
  if (r !== null) return r;
  return findingsStabilityJs(current, prior, { priorKnown });
}

export function verdictReproducibility(runs, sha) {
  const r = runRust('verdictReproducibility', { runs: runs ?? null, sha: sha ?? null });
  if (r !== null) return r;
  return verdictReproducibilityJs(runs, sha);
}

export function buildDeltaSection(current, prior, priorRuns, priorSha, currentRun = null) {
  const r = runRust('buildDeltaSection', {
    current: current ?? null,
    prior: prior ?? null,
    priorRuns: priorRuns ?? null,
    priorSha: priorSha ?? null,
    currentRun,
  });
  if (r !== null) return r;
  return buildDeltaSectionJs(current, prior, priorRuns, priorSha, currentRun);
}

export function buildWalkthroughSection(reviews) {
  const r = runRust('buildWalkthroughSection', { reviews: normalizeErrors(reviews) });
  if (r !== null) return r;
  return buildWalkthroughSectionJs(reviews);
}

export function buildSummary(cfg, input) {
  // blocking = blockerClusters.length — обращение сохранено: при undefined
  // падает как в JS-оригинале, до вызова бинаря.
  const blocking = input.blockerClusters.length;
  const r = runRust('buildSummary', {
    cfg: summaryCfgJson(cfg),
    input: {
      reviews: normalizeErrors(input.reviews),
      allFindings: input.allFindings ?? null,
      outside: input.outside ?? null,
      blastBlocks: input.blastBlocks ?? null,
      blastMetrics: input.blastMetrics ?? null,
      fileCount: input.files.size,
      headSha: input.headSha ?? null,
      note: input.note ?? null,
      verifierStats: input.verifierStats ?? null,
      refutationStats: input.refutationStats ?? null,
      prescanResult: input.prescanResult ?? null,
      fpRemoved: input.fpRemoved ?? null,
      verifierUnavailable: input.verifierUnavailable ?? null,
      coverageGap: input.coverageGap ?? null,
      priorFindings: input.priorFindings ?? null,
      priorRuns: input.priorRuns ?? null,
      priorSha: input.priorSha ?? null,
      costSnapshot: input.costSnapshot ?? null,
    },
    fileTexts: filesToTextJson(input.files),
    blocking,
    reviewMarker: MARKERS.review,
    dataKey: MARKERS.dataKey,
  });
  if (r !== null) return r;
  return buildSummaryJs(cfg, input);
}

/** Sticky для skip/light (R4) — без LLM, merge не блокируется. */
export function buildRouterSummary(cfg, { route, pr, headSha, changedPaths }) {
  const r = runRust('buildRouterSummary', {
    route: route ?? null,
    headSha: headSha ?? null,
    changedPaths: changedPaths ?? null,
    routerEnabled: Boolean(cfg.router?.enabled),
    reviewMarker: MARKERS.review,
  });
  if (r !== null) return r;
  return buildRouterSummaryJs(cfg, { route, pr, headSha, changedPaths });
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/report.mjs
// до Фазы 3, шага 5). Используется только когда Rust-бинарь отсутствует;
// при битом бинаре НЕ подменяет его.
// ---------------------------------------------------------------------------

function reasoningTitleJs({ hasRefs, hasClaims, hasConsensus }) {
  if (hasRefs && hasClaims) return 'MoA reasoning (advisors → aggregator)';
  if (hasRefs) return 'MoA advisor notes';
  if (hasClaims) return 'Jury (advisor → aggregator)';
  if (hasConsensus) return 'Model reasoning (consensus)';
  return 'Review reasoning';
}

function buildStickyReasoningFooterJs(reviews, reviewMode, { excerptLen = STICKY_REASONING_EXCERPT } = {}) {
  return extractReasoningEntries(reviews, reviewMode, { excerptLen })
    .filter((e) => e.excerpt || e.error);
}

function buildReasoningSectionJs(reviews, allFindings, { enabled = true, reviewMode = 'consensus' } = {}) {
  if (!enabled) return { text: '', truncated: false };

  const refs = (reviews || []).flatMap((r) => r.references || []).filter((ref) => ref.text || ref.error);
  const claimsFindings = (allFindings || []).filter((f) => f.advisorClaims?.length);
  const consensusRows = reviewMode !== 'moa'
    ? (reviews || []).filter((r) => !r.error && r.summary)
    : [];

  if (!refs.length && !claimsFindings.length && !consensusRows.length) {
    return { text: '', truncated: false };
  }

  let body = '';
  let budget = REASONING_SECTION_MAX;
  let truncated = false;

  for (const ref of refs) {
    const label = ref.role ? `${ref.role} · ${ref.model}` : ref.model;
    let chunk = '';
    if (ref.error) {
      chunk = `⚠️ ${publicErrorLabel(ref.error)}`;
    } else {
      const raw = String(ref.text || '');
      chunk = raw.slice(0, REASONING_ADVISOR_MAX);
      if (raw.length > REASONING_ADVISOR_MAX) {
        chunk += '\n\n…(обрезано)';
        truncated = true;
      }
    }
    const block = `#### ${label}${ref.chunkCount > 1 ? ` (${ref.chunkCount} chunks)` : ''}\n\n${chunk}\n\n`;
    if (block.length > budget) {
      truncated = true;
      break;
    }
    body += block;
    budget -= block.length;
  }

  for (const r of consensusRows) {
    const raw = String(r.summary || '');
    let chunk = raw.slice(0, REASONING_ADVISOR_MAX);
    if (raw.length > REASONING_ADVISOR_MAX) {
      chunk += '\n\n…(обрезано)';
      truncated = true;
    }
    const block = `#### ${r.model}\n\n${chunk}\n\n`;
    if (block.length > budget) {
      truncated = true;
      break;
    }
    body += block;
    budget -= block.length;
  }

  if (claimsFindings.length && budget > 200) {
    body += '#### Jury (advisor → aggregator)\n\n';
    for (const f of claimsFindings.slice(0, 12)) {
      const loc = f.file ? `\`${f.file}${f.line ? `:${f.line}` : ''}\`` : '—';
      const lines = (f.advisorClaims || []).map((c) =>
        `- **${c.role}** ${c.status}${c.reason ? `: ${c.reason.slice(0, 160)}` : ''}`,
      ).join('\n');
      const block = `- ${loc} «${(f.title || '').slice(0, 80)}»\n${lines}\n`;
      if (block.length > budget) {
        truncated = true;
        break;
      }
      body += block;
      budget -= block.length;
    }
  }

  if (!body.trim()) return { text: '', truncated: false };

  if (truncated) {
    body += '\n> _Reasoning обрезан по лимиту секции (~10k симв.). Полный JSON artifact — без prose body._\n';
  }

  const title = reasoningTitleJs({
    hasRefs: refs.length > 0,
    hasClaims: claimsFindings.length > 0,
    hasConsensus: consensusRows.length > 0 && !refs.length,
  });
  const text = `\n<details><summary>🔗 ${title}</summary>\n\n${body.trim()}\n\n</details>\n`;
  return { text, truncated };
}

function buildLlmUnavailableSummaryJs({
  llmRequired, whyPublic, models, prescanBlockers = [], prescanAdvisoryFindings = [],
}) {
  const code = (value) => {
    const raw = String(value || '—');
    const delimiter = '`'.repeat(Math.max(1, ...(raw.match(/`+/g) || []).map((s) => s.length)) + 1);
    return `${delimiter}${raw}${delimiter}`;
  };
  const text = (value, fallback) => String(value || fallback).replace(/[\\`*_[\]()#+.!|~<>]/g, '\\$&');
  const formatFindings = (findings) => findings.map((f) => {
    const line = Number.isFinite(Number(f.line)) && Number(f.line) > 0 ? `:${Number(f.line)}` : '';
    return `- ${code(`${String(f.file || '—')}${line}`)} **${text(f.severity, 'minor')}** ${text(f.title, 'замечание')}`;
  }).join('\n');
  const blockerSection = prescanBlockers.length
    ? `\n\n### ⛔ Prescan-находки (${prescanBlockers.length}) — требуют исправления независимо от LLM\n${formatFindings(prescanBlockers)}`
    : '';
  const advisorySection = prescanAdvisoryFindings.length
    ? `\n\n### 🔧 Prescan-находки (advisory, ${prescanAdvisoryFindings.length})\n${formatFindings(prescanAdvisoryFindings)}`
    : '';
  const prescanSection = `${blockerSection}${advisorySection}`;
  const modelList = models.join(', ');

  const md = llmRequired
    ? `${MARKERS.review}\n## 🤖 Авто код-ревью\n\n⛔ ИИ-ревью недоступно (${whyPublic}).${prescanSection}\n\n<sub>Модели: ${modelList}. Без LLM-ревью гейт не может подтвердить PR — merge заблокирован (fail-closed, \`REVIEW_LLM_REQUIRED=1\`).${prescanBlockers.length ? ' Prescan-находки также блокируют merge.' : ''} Перезапустите job позже; осознанное ослабление — \`REVIEW_LLM_REQUIRED=0\`. Детали — в логе CI job.</sub>`
    : `${MARKERS.review}\n## 🤖 Авто код-ревью\n\n${prescanBlockers.length ? '⛔' : '⚠️'} ИИ-ревью временно недоступно (${whyPublic}).${prescanSection}\n\n<sub>Модели: ${modelList}.${prescanBlockers.length ? ' Prescan-находки блокируют merge.' : ' Это не блокирует merge.'} Детали — в логе CI job.</sub>`;
  const LIMIT = 65_000;
  return md.length > LIMIT
    ? `${md.slice(0, LIMIT - 100)}\n\n> …(отчёт обрезан под лимит GitHub)\n`
    : md;
}

function findingKeysJs(f) {
  const title = String(f?.title || '').toLowerCase().replace(/[`*_"«»]/g, '').replace(/\s+/g, ' ').trim();
  const loc = `loc:${f?.file || ''}:${Number(f?.line) || 0}:${title}`;
  const fp = f?.fp ? `fp:${f.fp}` : null;
  return { fp, loc, finding: fp ? `${fp}|${title}` : loc, point: fp || loc };
}

function diffFindingsJs(current, prior) {
  const cur = current || [];
  const prev = prior || [];
  const added = cur.filter((f) => !prev.some((p) => sameFindingJs(f, p)));
  const gone = prev.filter((p) => !cur.some((f) => sameFindingJs(f, p)));
  const kept = cur.filter((f) => prev.some((p) => sameFindingJs(f, p)));
  return { added, gone, kept };
}

function sameFindingJs(a, b) {
  const ka = findingKeysJs(a);
  const kb = findingKeysJs(b);
  if (ka.fp && kb.fp) return ka.finding === kb.finding;
  return ka.loc === kb.loc;
}

function samePointJs(a, b) {
  const ka = findingKeysJs(a);
  const kb = findingKeysJs(b);
  if (ka.fp && kb.fp) return ka.fp === kb.fp;
  return ka.loc === kb.loc;
}

function findingPointDeltaJs(current, prior) {
  const uniq = (list) => {
    const seen = new Map();
    for (const f of list || []) {
      const key = findingKeysJs(f).point;
      if (!seen.has(key)) seen.set(key, f);
    }
    return [...seen.values()];
  };
  const cur = uniq(current);
  const prev = uniq(prior);
  const used = new Set();
  let kept = 0;
  for (const f of cur) {
    const hit = prev.findIndex((p, i) => !used.has(i) && samePointJs(f, p));
    if (hit >= 0) { used.add(hit); kept++; }
  }
  return { kept, added: cur.length - kept, gone: prev.length - kept, current: cur.length, prior: prev.length };
}

function findingsStabilityJs(current, prior, { priorKnown = true } = {}) {
  const { kept, added, gone } = findingPointDeltaJs(current, prior);
  const union = kept + added + gone;
  if (!priorKnown || !union) return null;
  return Math.round((kept / union) * 100) / 100;
}

function verdictReproducibilityJs(runs, sha) {
  const short = String(sha || '').slice(0, 7);
  if (!short) return null;
  const sameSha = (runs || []).filter((r) => r?.sha === short && typeof r?.blocked === 'boolean');
  if (sameSha.length < 2) return null;
  const blocked = sameSha.filter((r) => r.blocked).length;
  return { total: sameSha.length, blocked, flaky: blocked > 0 && blocked < sameSha.length };
}

const formatStabilityJs = (v) => (typeof v === 'number' && Number.isFinite(v) ? `${Math.round(v * 100)}%` : '—');

/**
 * Текст модели в Markdown sticky. Заголовок и имя модели — инлайн: экранируем разметку, чтобы
 * `**`/`[]()`/`<details>` из ответа модели не ломали строку и не вставляли ссылки. Тело —
 * блок: модель легитимно пишет списки и код, поэтому гасим только `<` (иначе текст закроет
 * `<details>` или откроет HTML-комментарий рядом с футером). Путь — в backticks, и backtick
 * из него убираем, иначе он закроет code span.
 */
const escapeMdInline = (s) => String(s).replace(/[\\`*_[\]<>|]/g, '\\$&');
const escapeMdBlock = (s) => String(s).replace(/</g, '&lt;');
const codePath = (s) => String(s).replace(/`/g, '');

const deltaLineJs = (f) => {
  const safeTitle = escapeMdInline(f.title || 'замечание').slice(0, 110);
  const loc = f.file ? ` \`${String(f.file).replace(/`/g, '')}${f.line ? `:${f.line}` : ''}\`` : '';
  return `- ${SEV[f.severity] || '⚪'}${loc} ${safeTitle}`;
};

function buildDeltaSectionJs(current, prior, priorRuns, priorSha, currentRun = null) {
  if (!prior?.length && !priorRuns?.length) return '';
  const { added, gone, kept } = diffFindingsJs(current, prior);
  // Сюда доходим только когда прошлый прогон известен (guard выше), поэтому priorKnown=true.
  const stability = findingsStabilityJs(current, prior);
  const stabilityNote = stability == null ? '' : `, стабильность ${formatStabilityJs(stability)}`;
  const shaNote = priorSha ? ` (прошлый прогон \`${priorSha}\`)` : '';
  let md = `\n> 🔄 **С прошлого прогона${shaNote}:** +${added.length} новых, −${gone.length} закрыто, ${kept.length} без изменений${stabilityNote}.\n`;
  if (added.length || gone.length) {
    const parts = [];
    if (added.length) parts.push(`**Новые (${added.length})**\n${added.slice(0, 10).map(deltaLineJs).join('\n')}`);
    if (gone.length) parts.push(`**Закрыты или сняты (${gone.length})**\n${gone.slice(0, 10).map(deltaLineJs).join('\n')}`);
    md += `\n<details><summary>🔄 Что изменилось</summary>\n\n${parts.join('\n\n')}\n\n</details>\n`;
  }
  if (priorRuns?.length) {
    const rows = priorRuns.slice(-5).map((r) => {
      const c = r.counts || {};
      return `| \`${r.sha || '—'}\` | ${verdictMarkJs(r.blocked)} | ${c.critical ?? 0} | ${c.major ?? 0} | ${c.minor ?? 0} | ${c.nit ?? 0} | +${r.added ?? 0} / −${r.gone ?? 0} | ${formatStabilityJs(r.stability)} |`;
    });
    const repro = currentRun ? verdictReproducibilityJs([...priorRuns, currentRun], currentRun.sha) : null;
    const reproLine = repro && (repro.flaky
      ? `\n\n⛔ **Вердикт на \`${currentRun.sha}\` невоспроизводим: ${repro.blocked} из ${repro.total} прогонов на этом коммите блокировали merge.** `
        + 'Дифф между ними не менялся — значит, недетерминирован сам гейт, а не код.'
      : `\n\n${repro.blocked ? '⛔' : '✅'} Вердикт на \`${currentRun.sha}\` воспроизводим: `
        + `${repro.blocked ? `блокировали все ${repro.total}` : `ни один из ${repro.total}`} прогонов на этом коммите.`);
    const summaryLine = repro?.flaky
      ? `📈 История прогонов · ⛔ вердикт нестабилен (${repro.blocked} из ${repro.total} на этом коммите)`
      : '📈 История прогонов';
    md += `\n<details><summary>${summaryLine}</summary>\n\n`
      + `| sha | гейт | 🔴 | 🟠 | 🟡 | 🔵 | Δ | стаб. |\n|---|---|---|---|---|---|---|---|\n${rows.join('\n')}`
      + `${reproLine || ''}\n\n</details>\n`;
  }
  return md;
}

const verdictMarkJs = (blocked) => (blocked === true ? '⛔' : blocked === false ? '✅' : '—');

function buildWalkthroughSectionJs(reviews) {
  const ok = (reviews || []).filter((r) => !r.error && String(r.summary || '').trim());
  if (!ok.length) return '';
  const best = ok.reduce((a, b) =>
    String(b.summary).trim().length > String(a.summary).trim().length ? b : a);
  const summary = String(best.summary).trim().replace(/\s+/g, ' ').slice(0, 600);
  if (!summary) return '';
  return `### Walkthrough\n\n${summary}\n\n`;
}

/** Баннер «verifier недоступен» (RFC 18): находки уровня блока не проверены вторым судьёй. */
function verifierUnavailableBannerJs({ reason, unverified, blocked }) {
  const n = `${unverified} замечани${unverified === 1 ? 'е' : 'й'} уровня блока`;
  return blocked
    ? `> ⛔ **Merge заблокирован: verifier недоступен** (${reason}) — ${n} не проверены вторым судьёй `
      + `и показаны как minor. Перезапустите ревью. Пропускать merge при сбое verifier — \`REVIEW_LLM_REQUIRED=0\`.\n\n`
    : `> ⚠️ **Verifier недоступен** (${reason}) — ${n} не проверены и понижены до minor; `
      + `merge не блокируется (\`REVIEW_LLM_REQUIRED=0\`).\n\n`;
}

/** Баннер «ревью неполное» (RFC 19): части PR не отревьюены из-за сбоя модели. Без путей:
 * имя файла задаёт автор PR, пути — в логе и событии coverage_gap. */
function coverageGapBannerJs({ failed, total, blocked }) {
  const n = `${failed} из ${total} частей PR не отревьюены из-за сбоя модели`;
  return blocked
    ? `> ⛔ **Merge заблокирован: ревью неполное** — ${n}, находок по ним нет. Перезапустите ревью. `
      + `Пропускать merge при сбое — \`REVIEW_LLM_REQUIRED=0\`.\n\n`
    : `> ⚠️ **Ревью неполное** — ${n}; merge не блокируется (\`REVIEW_LLM_REQUIRED=0\`).\n\n`;
}

function buildSummaryJs(cfg, input) {
  const {
    reviews, allFindings, outside, blastBlocks, blastMetrics = null, files, pr, headSha, note,
    blockerClusters, verifierStats, refutationStats,
    priorFindings = [], priorRuns = [], priorSha = '',
    costSnapshot = null, verifierUnavailable = null, coverageGap = null,
  } = input;
  const { gate, reviewMode, models, moa, verifier, specialists } = cfg;
  const { blockOn, blockSingle, consensusMin } = gate;

  const counts = { critical: 0, major: 0, minor: 0, nit: 0 };
  for (const f of allFindings) if (counts[f.severity] != null) counts[f.severity]++;
  const total = allFindings.length;
  const blocking = blockerClusters.length;
  // Verifier недоступен (RFC 18): блок без подтверждённых блокеров — ревью не завершено.
  const degraded = Boolean(verifierUnavailable?.blocked || coverageGap?.blocked);
  const verdictRu = blocking ? '🔴 нужны правки'
    : degraded ? '⛔ ревью не завершено'
      : (total ? '💬 есть замечания' : '✅ можно мёржить');
  const consensusTier = blockOn.filter((s) => !blockSingle.includes(s));
  const ruleNote = reviewMode === 'moa'
    ? `${blockOn.join('/')} — от MoA-aggregator; confidence ≥${gate.confidenceMin}`
    : `${blockSingle.join('/')} — от любой модели`
      + (consensusTier.length ? `; ${consensusTier.join('/')} — по консенсусу ≥${consensusMin} модел.` : '')
      + `; confidence ≥${gate.confidenceMin} для блока`;

  let md = `${MARKERS.review}\n## 🤖 Авто код-ревью — ${verdictRu}\n\n`;
  if (blocking) {
    md += `> ⛔ **Merge заблокирован:** ${blocking} замечани${blocking === 1 ? 'е' : 'й'}. `
      + `Правило блокировки: ${ruleNote}. Исправьте (или ослабьте \`REVIEW_BLOCK_ON\`/\`REVIEW_BLOCK_SINGLE\`/\`REVIEW_CONSENSUS\`).\n\n`;
  } else if (allFindings.some((f) => blockOn.includes(f.severity)) && reviewMode !== 'moa') {
    md += `> ✅ Блокеров нет: ${consensusTier.join('/') || blockOn.join('/')} есть лишь у одной модели — показаны как `
      + `замечания (для блока нужен консенсус ≥${consensusMin}); ${blockSingle.join('/')} не найдено.\n\n`;
  } else if (allFindings.some((f) => blockOn.includes(f.severity)) && reviewMode === 'moa') {
    md += `> ✅ Merge не блокируется: aggregator не подтвердил ${blockOn.join('/')} как блокеры.\n\n`;
  }
  if (verifierUnavailable) md += verifierUnavailableBannerJs(verifierUnavailable);
  if (coverageGap) md += coverageGapBannerJs(coverageGap);

  md += buildWalkthroughSectionJs(reviews);

  md += `### Оценки моделей\n`;
  for (const r of reviews) {
    const fbNote = r.fallbackFor ? ` _(резерв вместо ${r.fallbackFor})_` : '';
    if (r.error) { md += `- **${r.model}**${fbNote} — ⚠️ недоступна (${publicErrorLabel(r.error)})\n`; continue; }
    const c = { critical: 0, major: 0, minor: 0, nit: 0 };
    for (const f of r.findings) if (c[f.severity] != null) c[f.severity]++;
    md += `- **${r.model}**${fbNote} — ${r.findings.length} (${SEV.critical}${c.critical} ${SEV.major}${c.major} ${SEV.minor}${c.minor} ${SEV.nit}${c.nit}): ${r.summary || ''}\n`;
    if (r.references?.length) {
      for (const ref of r.references) {
        const label = ref.role ? `${ref.role}/${ref.model}` : ref.model;
        if (ref.error) md += `  - specialist **${label}** — ⚠️ ${publicErrorLabel(ref.error)}\n`;
        else md += `  - specialist **${label}** — ok (${(ref.text || '').length} симв.)\n`;
      }
    }
  }

  md += `\n**Итого:** ${total} замечаний — ${SEV.critical}${counts.critical} ${SEV.major}${counts.major} ${SEV.minor}${counts.minor} ${SEV.nit}${counts.nit}. Файлов в диффе: ${files.size}.\n`;
  const costLine = formatCostStickyLine(costSnapshot);
  if (costLine) md += `\n> ${costLine}\n`;
  const slim = allFindings.map((f) => {
    const fp = f.fp || findingFingerprint(files.get(f.file), f.line);
    return {
      file: f.file, line: f.line, severity: f.severity, title: f.title, model: f.model,
      ...(f.dupCount > 1 ? { dupCount: f.dupCount } : {}),
      ...(fp ? { fp } : {}),
    };
  });
  const delta = diffFindingsJs(slim, priorFindings);
  const points = findingPointDeltaJs(slim, priorFindings);
  const stability = findingsStabilityJs(slim, priorFindings, {
    priorKnown: Boolean(priorFindings?.length || priorRuns?.length),
  });
  const currentRun = {
    sha: String(headSha).slice(0, 7),
    counts,
    added: delta.added.length,
    gone: delta.gone.length,
    ...(stability == null ? {} : { stability }),
    blocked: blocking > 0 || degraded,
  };
  md += buildDeltaSectionJs(slim, priorFindings, priorRuns, priorSha, currentRun);
  if (verifierStats && verifier.enabled && (verifierStats.rejected || verifierStats.downgraded || verifierStats.verified || verifierStats.unverified)) {
    const unv = verifierStats.unverified ? `, без подтверждения → minor ${verifierStats.unverified}` : '';
    md += `\n> 🔎 Verifier (Pass B, ${verifier.model}): подтверждено ${verifierStats.verified}, отклонено FP ${verifierStats.rejected}, понижено ${verifierStats.downgraded}${unv}.\n`;
  }
  if (refutationStats && specialists.enabled && (refutationStats.dropped || refutationStats.downgraded)) {
    const nc = refutationStats.downgradedNoClaims ? `, noClaims→minor ${refutationStats.downgradedNoClaims}` : '';
    md += `\n> ⚖️ Refutation (R5 jury): dropped ${refutationStats.dropped}, downgraded ${refutationStats.downgraded}${nc}, kept ${refutationStats.kept}.\n`;
  }
  if (input.prescanResult?.findings?.length) {
    const n = input.prescanResult.findings.length;
    const bt = input.prescanResult.byTool;
    const tools = bt
      ? `eslint ${bt.eslint ?? 0} + ast-grep ${bt.astgrep ?? 0}`
      : `${n}`;
    md += `\n> 🔧 Prescan (R6 eslint + ast-grep): ${tools} детерминированных finding(s) на ${input.prescanResult.scanned} файлах (LLM dedup).\n`;
  }
  if (note) md += `\n> ⚠️ ${note}\n`;

  if (outside.length) {
    const rank = { critical: 0, major: 1, minor: 2, nit: 3 };
    const sorted = [...outside].sort((a, b) => (rank[a.severity] ?? 9) - (rank[b.severity] ?? 9));
    const shown = sorted.slice(0, OUTSIDE_MAX);
    md += `\n### Замечания вне строк диффа\n`;
    for (const f of shown) {
      const loc = f.file ? ` (\`${codePath(f.file)}\`${f.line ? `:${f.line}` : ''})` : '';
      const dup = f.dupCount > 1 ? ` ×${f.dupCount}` : '';
      md += `- ${SEV[f.severity] || '⚪'} **${escapeMdInline(f.title || 'замечание')}**¹${dup}${loc} — ${escapeMdBlock(f.body || '')}\n`;
    }
    const tail = sorted.slice(OUTSIDE_MAX);
    if (tail.length) {
      const rows = tail.map((f) => {
        const loc = f.file ? ` (\`${codePath(f.file)}\`${f.line ? `:${f.line}` : ''})` : '';
        const dup = f.dupCount > 1 ? ` ×${f.dupCount}` : '';
        return `- ${SEV[f.severity] || '⚪'} **${escapeMdInline(f.title || 'замечание')}**${dup}${loc}`;
      });
      md += `\n<details><summary>…и ещё ${tail.length} advisory</summary>\n\n${rows.join('\n')}\n\n`
        + `<sub>Полные тела — в artifact \`pr-review-export\` (JSON/SARIF).</sub>\n\n</details>\n`;
    }
  }
  if (input.fpRemoved?.length) {
    const rows = input.fpRemoved.slice(0, 10).map((f) => {
      const loc = f.file ? ` (\`${codePath(f.file)}${f.line ? `:${f.line}` : ''}\`)` : '';
      return `- ${SEV[f.severity] || '⚪'} **${escapeMdInline(f.title || 'замечание')}** _(${escapeMdInline(f.model)})_${loc}`;
    }).join('\n');
    md += `\n<details><summary>🗑 Отфильтровано как известные FP (${input.fpRemoved.length})</summary>\n\n${rows}\n\n`;
    md += '> _Скрыто FP-паттернами из feedback (`ops/pr-review/feedback/fp-patterns.jsonl`) или сверкой с деревом репозитория («файла нет» о файле вне PR; путь — в логе прогона). Если среди них есть верное замечание — это ошибка фильтра, поправьте его._\n\n</details>\n';
  }
  {
    const m = blastMetrics && typeof blastMetrics === 'object' ? blastMetrics : null;
    const resolved = m ? `${m.symbols_resolved ?? '—'}/${m.symbols_total ?? 0}` : `${blastBlocks.length ? '—' : '0'}/—`;
    const fresh = m?.graph_status || (m?.graph_fresh === true ? 'fresh' : m?.graph_fresh === false ? 'stale' : 'unknown');
    const skip = m?.skip_reason && m.skip_reason !== 'none' ? ` · skip: ${m.skip_reason}` : '';
    const summary = `📡 Blast-radius (Graphify) · ${blastBlocks.length} блоков · symbols ${resolved} · graph ${fresh}${skip}`;
    if (blastBlocks.length) {
      md += `\n<details><summary>${summary}</summary>\n\n${blastBlocks.join('\n')}\n\n`
        + `<sub>AST code graph: caller @ path [relation][EXTRACTED|INFERRED]; cross-community — сильнее слой-эвристик. Callers вне диффа — сигнал регрессии для LLM/verifier.</sub>\n\n</details>\n`;
    } else {
      md += `\n<details><summary>${summary}</summary>\n\n`
        + `_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. `
        + `Смотри artifact \`pr-review-graphify\` и \`blast_radius.skip_reasons\` в observability._\n\n</details>\n`;
    }
  }

  const reasoningEnabled = cfg.export?.reasoningInSticky !== false;
  const { text: reasoningMd, truncated: reasoningTruncated } = buildReasoningSectionJs(reviews, allFindings, {
    enabled: reasoningEnabled,
    reviewMode,
  });
  md += reasoningMd;

  const gateNote = blockOn.length ? `блокирует merge: ${ruleNote}` : 'ревью советующее';
  const modelFoot = reviewMode === 'moa'
    ? (specialists.enabled
      ? `MoA specialists: ${specialists.assignments.map((a) => `${a.role}=${a.model}`).join(', ')} · aggregator: ${moa.aggregator}`
      : `MoA advisors: ${moa.references.join(', ')} · aggregator: ${moa.aggregator}`)
    : `Модели: ${models.list.join(', ')}`;
  md += `\n<sub>¹ ${modelFoot} · commit ${String(headSha).slice(0, 7) || '—'} · ${gateNote}; инфра-сбои merge не блокируют.</sub>`;

  const reasoning = reasoningEnabled ? buildStickyReasoningFooterJs(reviews, reviewMode) : [];
  const runs = [...(priorRuns || []), currentRun].slice(-5);
  const footerData = {
    sha: String(headSha).slice(0, 7),
    findings: slim,
    runs,
    ...(reasoning.length ? { reasoning } : {}),
  };
  const footerJson = JSON.stringify(footerData).replace(/-->/g, '--\\u003e').replace(/<!--/g, '\\u003c!--');
  const footer = `\n<!-- ${MARKERS.dataKey}${footerJson} -->`;
  const LIMIT = 65_000;
  if (md.length + footer.length > LIMIT) {
    md = md.slice(0, Math.max(0, LIMIT - footer.length - 120)) + '\n\n> …(отчёт обрезан под лимит GitHub)\n';
    if (reasoningMd) {
      md += '\n> _Секция reasoning могла быть частично обрезана — см. artifact `pr-review-export`._\n';
    }
  } else if (reasoningTruncated) {
    md += '\n> _Reasoning частично обрезан в sticky — полные excerpts в JSON artifact._\n';
  }
  return {
    md: md + footer,
    counts,
    total,
    delta: {
      kept: points.kept,
      added: points.added,
      gone: points.gone,
      stability,
      findings: { added: delta.added.length, gone: delta.gone.length, kept: delta.kept.length },
    },
    verdict: {
      sha: currentRun.sha,
      blocked: currentRun.blocked,
      reproducibility: verdictReproducibilityJs([...(priorRuns || []), currentRun], currentRun.sha),
    },
  };
}

function buildRouterSummaryJs(cfg, { route, pr, headSha, changedPaths }) {
  const labels = { skip: '⏭ пропуск LLM', light: '📄 docs/config-only' };
  const title = labels[route.route] || route.route;
  let md = `${MARKERS.review}\n## 🤖 Авто код-ревью — ${title}\n\n`;
  md += `> ✅ **Merge не блокируется:** роутер классифицировал PR как \`${route.route}\` (${route.reason}). `;
  md += 'ИИ-ревью кода не запускалось — экономия токенов на lockfile/docs/rename-only.\n\n';
  md += `### Изменённые файлы (${changedPaths.length})\n`;
  for (const f of route.files) md += `- \`${f.path}\` _(${f.class})_\n`;
  md += `\n<sub>Router R4 · commit ${String(headSha).slice(0, 7) || '—'} · REVIEW_ROUTER=${cfg.router.enabled ? '1' : '0'}</sub>`;
  return md;
}
