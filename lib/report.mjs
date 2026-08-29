import { SEV, MARKERS } from './constants.mjs';
import { extractReasoningEntries } from './export.mjs';
import { publicErrorLabel } from './util.mjs';
import { findingFingerprint } from './findings.mjs';

const REASONING_ADVISOR_MAX = 1800;
const REASONING_SECTION_MAX = 10_000;
const STICKY_REASONING_EXCERPT = 400;
// Сколько advisory-находок показываем в sticky «вне строк диффа» (остальные — в artifact).
const OUTSIDE_MAX = 10;

function reasoningTitle({ hasRefs, hasClaims, hasConsensus }) {
  if (hasRefs && hasClaims) return 'MoA reasoning (advisors → aggregator)';
  if (hasRefs) return 'MoA advisor notes';
  if (hasClaims) return 'Jury (advisor → aggregator)';
  if (hasConsensus) return 'Model reasoning (consensus)';
  return 'Review reasoning';
}

/** Excerpt для sticky footer JSON (без полного prose). */
export function buildStickyReasoningFooter(reviews, reviewMode, { excerptLen = STICKY_REASONING_EXCERPT } = {}) {
  return extractReasoningEntries(reviews, reviewMode, { excerptLen })
    .filter((e) => e.excerpt || e.error);
}

/** Collapsible MoA advisor prose / consensus summaries + jury (advisorClaims). */
export function buildReasoningSection(reviews, allFindings, { enabled = true, reviewMode = 'consensus' } = {}) {
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

  const title = reasoningTitle({
    hasRefs: refs.length > 0,
    hasClaims: claimsFindings.length > 0,
    hasConsensus: consensusRows.length > 0 && !refs.length,
  });
  const text = `\n<details><summary>🔗 ${title}</summary>\n\n${body.trim()}\n\n</details>\n`;
  return { text, truncated };
}

/** Sticky для полного LLM-аутеджа: отдельный, но единый formatter fail-open/fail-closed путей. */
export function buildLlmUnavailableSummary({
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

/**
 * Ключи находки между прогонами. Их ДВА, и это не дублирование: они отвечают на разные вопросы,
 * а раньше отвечали одним `fp` — отсюда дефект.
 *
 * `fp` — это `findingFingerprint(rec, line)`, отпечаток СТРОКИ КОДА, а не находки. Две разные
 * находки на одной строке («SQL-инъекция» и «нет обработки ошибки») получают ОДИН `fp`. Пока
 * оба вопроса решались этим ключом, вторая находка на уже помеченной строке считалась той же
 * самой: Δ-блок не показывал её в «новых», а `kept` вылезал за размер прошлого набора
 * (`diffFindings([A,B], [A])` давал kept 2 при |prior| = 1) и завышал стабильность — тем
 * сильнее, чем больше находок село на строку.
 *
 * - `finding` — «та же ли это претензия»: отпечаток кода + нормализованный заголовок. Нужен
 *   СПИСКАМ «новые» / «закрыты»: читателю важно увидеть реально новую находку, даже если она
 *   села на строку, уже помеченную в прошлый раз.
 * - `point` — «то же ли это место в коде»: только отпечаток. Нужен МЕТРИКЕ стабильности:
 *   сколько находок село на строку, месту безразлично.
 *
 * Без отпечатка (старый sticky, строка вне hunks) оба ключа откатываются на прежний
 * `file:line:нормализованный заголовок` — лучшего признака места там просто нет. Плата: такие
 * находки метрика считает разными точками, даже если они на одной строке. Это осознанно
 * консервативно (завышает разнообразие, а не стабильность) и касается лишь находок вне hunks.
 */
export function findingKeys(f) {
  const title = String(f?.title || '').toLowerCase().replace(/[`*_"«»]/g, '').replace(/\s+/g, ' ').trim();
  const loc = `loc:${f?.file || ''}:${Number(f?.line) || 0}:${title}`;
  const fp = f?.fp ? `fp:${f.fp}` : null;
  return { fp, loc, finding: fp ? `${fp}|${title}` : loc, point: fp || loc };
}

/**
 * Совпадают ли находки разных прогонов. Отпечаток КОДА главнее координаты: если он есть у
 * обеих сторон, решают отпечаток и формулировка — иначе находка на той же строке с тем же
 * заголовком, но по изменившемуся коду, маскировалась бы под «осталась», и Δ-блок молчал бы
 * о починке. По loc сверяем лишь когда отпечатка нет хотя бы у одной стороны (старый sticky,
 * строка вне hunks) — иначе такие находки все выглядели бы новыми.
 */
function sameFinding(a, b) {
  const ka = findingKeys(a);
  const kb = findingKeys(b);
  if (ka.fp && kb.fp) return ka.finding === kb.finding;
  return ka.loc === kb.loc;
}

/** То же место в коде? Заголовок здесь не участвует — см. `findingKeys`. */
function samePoint(a, b) {
  const ka = findingKeys(a);
  const kb = findingKeys(b);
  if (ka.fp && kb.fp) return ka.fp === kb.fp;
  return ka.loc === kb.loc;
}

/** Сильнейший ключ находки: отпечаток кода с формулировкой, иначе координата с заголовком. */
export const findingIdentity = (f) => findingKeys(f).finding;

/**
 * Что изменилось с прошлого прогона. Считается по идентичности находок, а не по счётчикам:
 * «было 6, стало 6» может скрывать полную смену набора — ровно то, что показал замер
 * дисперсии MoA (из ~14 находок между прогонами совпадали 5).
 */
export function diffFindings(current, prior) {
  const cur = current || [];
  const prev = prior || [];
  const added = cur.filter((f) => !prev.some((p) => sameFinding(f, p)));
  const gone = prev.filter((p) => !cur.some((f) => sameFinding(f, p)));
  const kept = cur.filter((f) => prev.some((p) => sameFinding(f, p)));
  return { added, gone, kept };
}

/**
 * Пересечение и разности двух прогонов по ТОЧКАМ КОДА (см. `findingKeys`): каждое место
 * учитывается один раз, сколько бы находок на него ни село.
 *
 * Обе стороны дедуплицированы, и каждое совпадение «съедает» ровно одну точку прошлого набора,
 * поэтому `kept ≤ min(|current|, |prior|)` по построению — арифметика метрики не может
 * разъехаться так, как разъезжалась на общем ключе.
 */
export function findingPointDelta(current, prior) {
  const uniq = (list) => {
    const seen = new Map();
    for (const f of list || []) {
      const key = findingKeys(f).point;
      if (!seen.has(key)) seen.set(key, f);
    }
    return [...seen.values()];
  };
  const cur = uniq(current);
  const prev = uniq(prior);
  const used = new Set();
  let kept = 0;
  for (const f of cur) {
    // Совпадение по отпечатку, а при его отсутствии — по координате (тот же откат, что у
    // списков): иначе прогон рядом с выкатом `fp` показал бы 0% вместо честного сравнения.
    // В смешанном случае (у одной стороны отпечаток есть, у другой нет) сверяется координата
    // представителя точки, а в неё входит заголовок — то есть исход зависит от того, какая из
    // находок на строке стала представителем. Мириться с этим можно: смешанный случай бывает
    // только на стыке со старым sticky и живёт один прогон.
    const hit = prev.findIndex((p, i) => !used.has(i) && samePoint(f, p));
    if (hit >= 0) { used.add(hit); kept++; }
  }
  return { kept, added: cur.length - kept, gone: prev.length - kept, current: cur.length, prior: prev.length };
}

/**
 * Стабильность между прогонами — Jaccard по ТОЧКАМ КОДА: `kept / (kept + added + gone)`,
 * то есть пересечение к объединению множеств задетых мест.
 *
 * Почему по точкам, а не по находкам (НЕ «упрощать» обратно): замер восьми прогонов на
 * неизменном диффе (PR #533, 2026-08-09) показал, что при опознании находки по точному
 * совпадению заголовка Jaccard соседних прогонов равен 0.000 в обеих сериях — модели каждый
 * раз переформулируют один и тот же дефект («Неразрешённые маркеры Git-конфликта в
 * коммитируемом файле» ↔ «Неразрешённый git-merge конфликт в документе»). Метрика на
 * формулировках была бы нулевым шумом и ничего не измеряла; точка кода — то, что прогоны
 * реально могут воспроизвести. Списки Δ-блока при этом опознают находку ВМЕСТЕ с заголовком:
 * им нужна претензия, а не место (см. `findingKeys`).
 *
 * Мера СИММЕТРИЧНА намеренно, и «упрощать» её до односторонней `kept / (kept + added)`
 * («доля доживших») тоже нельзя: та слепа ровно к тому, ради чего метрика заводится. Прогон,
 * нашедший ОДНУ находку, и ту старую, потеряв прочие 13, получил бы у неё 1.00 — идеальную
 * стабильность при полной потере recall. Jaccard в том же сценарии даёт 0.07.
 *
 * `null` вместо числа — только когда сравнивать не с чем:
 *  - база сравнения неизвестна (`priorKnown === false`: первый прогон PR, где нет ни
 *    prior-находок, ни истории прогонов) — 0 читался бы как «наборы не пересеклись», хотя
 *    пересекаться было нечему;
 *  - знаменатель нулевой (оба набора пусты) — делить не на что.
 * Пустой ТЕКУЩИЙ набор при непустом прошлом — честный 0, а не null: объединение непусто,
 * пересечение пусто, не выжило ничего. Округление до сотых — чтобы читалось целыми процентами.
 */
export function findingsStability(current, prior, { priorKnown = true } = {}) {
  const { kept, added, gone } = findingPointDelta(current, prior);
  const union = kept + added + gone;
  if (!priorKnown || !union) return null;
  return Math.round((kept / union) * 100) / 100;
}

/** Доля → проценты для отчётов; `—` там, где метрика не определена. */
const formatStability = (v) => (typeof v === 'number' && Number.isFinite(v) ? `${Math.round(v * 100)}%` : '—');

/**
 * Воспроизводимость ВЕРДИКТА на одном коммите: сколько прогонов по этому sha заблокировали
 * merge из скольких. Стабильность набора (Jaccard выше) меряет шум в СОВЕТАХ, эта метрика —
 * шум в РЕШЕНИИ: замер на #533 (дифф не менялся, 6 прогонов подряд) дал блокер в 4 прогонах из
 * 6, причём severity мигрировала critical↔major. Один и тот же код то блокирует merge, то нет —
 * это дороже дисперсии advisory, и по счётчикам находок такое не видно вовсе.
 *
 * Считаем ТОЛЬКО по записям того же sha: на другом коммите сменился дифф, разные вердикты там
 * законны и недетерминизма не доказывают.
 *
 * `null` — когда сравнивать не с чем, и эта ветка отделена намеренно:
 *  - на этом sha известен один прогон (обычный случай: первый прогон коммита) — «1 из 1» не
 *    отличает воспроизводимый вердикт от неизмеренного;
 *  - записи истории сделаны до ввода поля `blocked` — вердикт неизвестен, а не «не блокировал»:
 *    считать отсутствие поля за `false` значило бы придумывать воспроизводимость задним числом.
 * Глубина ограничена капом истории (последние 5 записей футера + текущий прогон).
 */
export function verdictReproducibility(runs, sha) {
  const short = String(sha || '').slice(0, 7);
  if (!short) return null;
  const sameSha = (runs || []).filter((r) => r?.sha === short && typeof r?.blocked === 'boolean');
  if (sameSha.length < 2) return null;
  const blocked = sameSha.filter((r) => r.blocked).length;
  return { total: sameSha.length, blocked, flaky: blocked > 0 && blocked < sameSha.length };
}

/** Вердикт прогона в истории: ⛔/✅, а `—` — записи до ввода поля (вердикт неизвестен). */
const verdictMark = (blocked) => (blocked === true ? '⛔' : blocked === false ? '✅' : '—');

/** Строка Δ-блока по одной находке: severity, путь, заголовок — коротко. */
const deltaLine = (f) => {
  // Путь — в backticks, заголовок — с экранированием markdown: и то и другое приходит из
  // ответа модели и не должно ломать разметку отчёта.
  const safeTitle = String(f.title || 'замечание').replace(/[\\`*_[\]<>|]/g, '\\$&').slice(0, 110);
  const loc = f.file ? ` \`${String(f.file).replace(/`/g, '')}${f.line ? `:${f.line}` : ''}\`` : '';
  return `- ${SEV[f.severity] || '⚪'}${loc} ${safeTitle}`;
};

/**
 * Δ-блок + история прогонов. Один sticky остаётся единственным (лента из отчётов на каждый
 * прогон делает тред нечитаемым и ломает поиск «последнего» для carry-over и гейта), но
 * теперь по нему видно движение: что появилось, что ушло, как менялись счётчики.
 */
export function buildDeltaSection(current, prior, priorRuns, priorSha, currentRun = null) {
  if (!prior?.length && !priorRuns?.length) return '';
  const { added, gone, kept } = diffFindings(current, prior);
  // Сюда доходим только когда прошлый прогон известен (guard выше), поэтому priorKnown=true.
  // Стабильность печатаем прямо в заголовке Δ-блока: в таблице ниже она появится лишь на
  // следующем прогоне (туда пишется история), а дисперсию нужно видеть на том прогоне, где её посчитали.
  const stability = findingsStability(current, prior);
  const stabilityNote = stability == null ? '' : `, стабильность ${formatStability(stability)}`;
  const shaNote = priorSha ? ` (прошлый прогон \`${priorSha}\`)` : '';
  let md = `\n> 🔄 **С прошлого прогона${shaNote}:** +${added.length} новых, −${gone.length} закрыто, ${kept.length} без изменений${stabilityNote}.\n`;
  if (added.length || gone.length) {
    const parts = [];
    if (added.length) parts.push(`**Новые (${added.length})**\n${added.slice(0, 10).map(deltaLine).join('\n')}`);
    if (gone.length) parts.push(`**Закрыты или сняты (${gone.length})**\n${gone.slice(0, 10).map(deltaLine).join('\n')}`);
    md += `\n<details><summary>🔄 Что изменилось</summary>\n\n${parts.join('\n\n')}\n\n</details>\n`;
  }
  if (priorRuns?.length) {
    const rows = priorRuns.slice(-5).map((r) => {
      const c = r.counts || {};
      // Записи прогонов до ввода метрики (и прогоны без базы сравнения) стабильности не несут:
      // рисуем `—`, а не 0% — ноль здесь читался бы как «наборы вообще не пересеклись».
      return `| \`${r.sha || '—'}\` | ${verdictMark(r.blocked)} | ${c.critical ?? 0} | ${c.major ?? 0} | ${c.minor ?? 0} | ${c.nit ?? 0} | +${r.added ?? 0} / −${r.gone ?? 0} | ${formatStability(r.stability)} |`;
    });
    // Текущий прогон в таблицу не попадает (он ляжет в футер для следующего), но в подсчёт
    // воспроизводимости входит: его вердикт — такой же замер, а читателю показывают именно его.
    const repro = currentRun ? verdictReproducibility([...priorRuns, currentRun], currentRun.sha) : null;
    const reproLine = repro && (repro.flaky
      ? `\n\n⛔ **Вердикт на \`${currentRun.sha}\` невоспроизводим: ${repro.blocked} из ${repro.total} прогонов на этом коммите блокировали merge.** `
        + 'Дифф между ними не менялся — значит, недетерминирован сам гейт, а не код.'
      : `\n\n${repro.blocked ? '⛔' : '✅'} Вердикт на \`${currentRun.sha}\` воспроизводим: `
        + `${repro.blocked ? `блокировали все ${repro.total}` : `ни один из ${repro.total}`} прогонов на этом коммите.`);
    // Флаг недетерминизма дублируется в <summary>: таблица свёрнута, а «гейт решает монеткой» —
    // ровно то, что нельзя пропустить, не раскрывая блок.
    const summaryLine = repro?.flaky
      ? `📈 История прогонов · ⛔ вердикт нестабилен (${repro.blocked} из ${repro.total} на этом коммите)`
      : '📈 История прогонов';
    md += `\n<details><summary>${summaryLine}</summary>\n\n`
      + `| sha | гейт | 🔴 | 🟠 | 🟡 | 🔵 | Δ | стаб. |\n|---|---|---|---|---|---|---|---|\n${rows.join('\n')}`
      + `${reproLine || ''}\n\n</details>\n`;
  }
  return md;
}

export function buildSummary(cfg, input) {
  const {
    reviews, allFindings, outside, blastBlocks, blastMetrics = null, files, pr, headSha, note,
    blockerClusters, verifierStats, refutationStats,
    priorFindings = [], priorRuns = [], priorSha = '',
  } = input;
  const { gate, reviewMode, models, moa, verifier, specialists } = cfg;
  const { blockOn, blockSingle, consensusMin } = gate;

  const counts = { critical: 0, major: 0, minor: 0, nit: 0 };
  for (const f of allFindings) if (counts[f.severity] != null) counts[f.severity]++;
  const total = allFindings.length;
  const blocking = blockerClusters.length;
  const verdictRu = blocking ? '🔴 нужны правки' : (total ? '💬 есть замечания' : '✅ можно мёржить');
  const consensusTier = blockOn.filter((s) => !blockSingle.includes(s));
  const ruleNote = reviewMode === 'moa'
    ? `${blockOn.join('/')} — от MoA-aggregator; confidence ≥${gate.confidenceMin}`
    : `${blockSingle.join('/')} — от любой модели` +
      (consensusTier.length ? `; ${consensusTier.join('/')} — по консенсусу ≥${consensusMin} модел.` : '') +
      `; confidence ≥${gate.confidenceMin} для блока`;

  let md = `${MARKERS.review}\n## 🤖 Авто код-ревью — ${verdictRu}\n\n`;
  if (blocking) {
    md += `> ⛔ **Merge заблокирован:** ${blocking} замечани${blocking === 1 ? 'е' : 'й'}. ` +
      `Правило блокировки: ${ruleNote}. Исправьте (или ослабьте \`REVIEW_BLOCK_ON\`/\`REVIEW_BLOCK_SINGLE\`/\`REVIEW_CONSENSUS\`). ` +
      `Инфра-сбои merge не блокируют.\n\n`;
  } else if (allFindings.some((f) => blockOn.includes(f.severity)) && reviewMode !== 'moa') {
    md += `> ✅ Блокеров нет: ${consensusTier.join('/') || blockOn.join('/')} есть лишь у одной модели — показаны как ` +
      `замечания (для блока нужен консенсус ≥${consensusMin}); ${blockSingle.join('/')} не найдено.\n\n`;
  } else if (allFindings.some((f) => blockOn.includes(f.severity)) && reviewMode === 'moa') {
    md += `> ✅ Merge не блокируется: aggregator не подтвердил ${blockOn.join('/')} как блокеры.\n\n`;
  }

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
  // Отпечатки считаются ОДИН раз: они нужны и Δ-блоку, и футеру.
  const slim = allFindings.map((f) => {
    const fp = findingFingerprint(files.get(f.file), f.line);
    return {
      file: f.file, line: f.line, severity: f.severity, title: f.title, model: f.model,
      ...(f.dupCount > 1 ? { dupCount: f.dupCount } : {}),
      ...(fp ? { fp } : {}),
    };
  });
  const delta = diffFindings(slim, priorFindings);
  // Метрика считается по другому ключу, чем списки Δ-блока (см. `findingKeys`), поэтому её
  // слагаемые берутся отдельным проходом, а не из `delta`.
  const points = findingPointDelta(slim, priorFindings);
  // Признак «прошлый прогон известен» — тот же, по которому рисуется Δ-блок: находки прошлого
  // прогона ИЛИ хотя бы его запись в истории. Иначе первый прогон PR получал бы «стабильность 0%»
  // (пересечение пусто) — а пересекаться там было не с чем.
  const stability = findingsStability(slim, priorFindings, {
    priorKnown: Boolean(priorFindings?.length || priorRuns?.length),
  });
  // Запись прогона собирается ДО Δ-блока: воспроизводимость вердикта считается по истории
  // вместе с текущим прогоном, а в футер уходит ровно та же запись — одна структура, не две.
  // `blocked` пишется всегда булевым: пропуск поля означал бы «вердикт неизвестен» (записи до
  // ввода метрики), и «не блокировал» слилось бы с «не измеряли».
  const currentRun = {
    sha: String(headSha).slice(0, 7),
    counts,
    // added/gone в записи прогона — ПО СПИСКАМ Δ-блока: читатель истории сверяет их с тем,
    // что видит в «Что изменилось». Слагаемые метрики (точки кода) уходят в событие.
    added: delta.added.length,
    gone: delta.gone.length,
    // Неопределённую стабильность ключом не пишем: отсутствие поля читается как «—» и у
    // прогонов до ввода метрики, и у прогонов без базы сравнения — одна ветка вместо двух.
    ...(stability == null ? {} : { stability }),
    blocked: blocking > 0,
  };
  md += buildDeltaSection(slim, priorFindings, priorRuns, priorSha, currentRun);
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
    // Top-K по severity — хвост не засоряет sticky (бэклог #6), полный список в artifact.
    const rank = { critical: 0, major: 1, minor: 2, nit: 3 };
    const sorted = [...outside].sort((a, b) => (rank[a.severity] ?? 9) - (rank[b.severity] ?? 9));
    const shown = sorted.slice(0, OUTSIDE_MAX);
    md += `\n### Замечания вне строк диффа\n`;
    for (const f of shown) {
      const loc = f.file ? ` (\`${f.file}\`${f.line ? `:${f.line}` : ''})` : '';
      // ×N — сколько чанков увидели ту же тему (дедуп внутри модели): видно, что это одна
      // находка из нескольких частей, а не несколько независимых.
      const dup = f.dupCount > 1 ? ` ×${f.dupCount}` : '';
      md += `- ${SEV[f.severity] || '⚪'} **${f.title || 'замечание'}**¹${dup}${loc} — ${f.body || ''}\n`;
    }
    const tail = sorted.slice(OUTSIDE_MAX);
    if (tail.length) {
      // Хвост раньше уезжал только в artifact, который никто не открывает — а среди minor
      // попадались дефекты гейт-уровня. Теперь он свёрнут, но в отчёте.
      const rows = tail.map((f) => {
        const loc = f.file ? ` (\`${f.file}\`${f.line ? `:${f.line}` : ''})` : '';
        const dup = f.dupCount > 1 ? ` ×${f.dupCount}` : '';
        return `- ${SEV[f.severity] || '⚪'} **${f.title || 'замечание'}**${dup}${loc}`;
      });
      md += `\n<details><summary>…и ещё ${tail.length} advisory</summary>\n\n${rows.join('\n')}\n\n`
        + `<sub>Полные тела — в artifact \`pr-review-export\` (JSON/SARIF).</sub>\n\n</details>\n`;
    }
  }
  // Снятые FP-паттернами находки — видимая секция, а не молчаливое удаление:
  // ошибочно широкий паттерн иначе незаметно проглотил бы верное замечание.
  if (input.fpRemoved?.length) {
    const rows = input.fpRemoved.slice(0, 10).map((f) => {
      const loc = f.file ? ` (\`${f.file}${f.line ? `:${f.line}` : ''}\`)` : '';
      return `- ${SEV[f.severity] || '⚪'} **${f.title || 'замечание'}** _(${f.model})_${loc}`;
    }).join('\n');
    md += `\n<details><summary>🗑 Отфильтровано как известные FP (${input.fpRemoved.length})</summary>\n\n${rows}\n\n`;
    md += '> _Скрыто FP-паттернами из feedback (`ops/pr-review/feedback/fp-patterns.jsonl`). Если среди них есть верное замечание — это ошибка паттерна, поправьте его._\n\n</details>\n';
  }
  {
    const m = blastMetrics && typeof blastMetrics === 'object' ? blastMetrics : null;
    const resolved = m ? `${m.symbols_resolved ?? '—'}/${m.symbols_total ?? 0}` : `${blastBlocks.length ? '—' : '0'}/—`;
    const fresh = m?.graph_status || (m?.graph_fresh === true ? 'fresh' : m?.graph_fresh === false ? 'stale' : 'unknown');
    const skip = m?.skip_reason && m.skip_reason !== 'none' ? ` · skip: ${m.skip_reason}` : '';
    const summary = `📡 Blast-radius (Graphify) · ${blastBlocks.length} блоков · symbols ${resolved} · graph ${fresh}${skip}`;
    if (blastBlocks.length) {
      md += `\n<details><summary>${summary}</summary>\n\n${blastBlocks.join('\n')}\n\n`
        + `<sub>AST code graph only (no semantic reindex in CI). Callers вне диффа — сигнал регрессии для LLM/verifier.</sub>\n\n</details>\n`;
    } else {
      md += `\n<details><summary>${summary}</summary>\n\n`
        + `_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. `
        + `Смотри artifact \`pr-review-graphify\` и \`blast_radius.skip_reasons\` в observability._\n\n</details>\n`;
    }
  }

  const reasoningEnabled = cfg.export?.reasoningInSticky !== false;
  const { text: reasoningMd, truncated: reasoningTruncated } = buildReasoningSection(reviews, allFindings, {
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

  // fp — отпечаток кода, на который показывает находка. По нему следующий прогон решает,
  // жива ли она: код на месте → несём дальше, изменился → снимаем. Без него carry-over
  // держался на «рядом есть изменённая строка», то есть точная починка находку сохраняла.
  const reasoning = reasoningEnabled ? buildStickyReasoningFooter(reviews, reviewMode) : [];
  // История прогонов — в футере, а не в ленте комментариев: sticky остаётся один, но
  // сравнение прогонов возможно. Кап 5 — дальше футер начинает есть лимит sticky.
  const runs = [...(priorRuns || []), currentRun].slice(-5);
  const footerData = {
    sha: String(headSha).slice(0, 7),
    findings: slim,
    runs,
    ...(reasoning.length ? { reasoning } : {}),
  };
  // Внутри JSON может оказаться `-->` или `<!--` — например, находка процитировала regex
  // разбора этого самого футера. Тогда читатель (ленивый `dataRe`) обрежет футер на чужой
  // последовательности, JSON не распарсится, и carry-over со статус-инструментом увидят
  // пустоту. `>`/`<` — валидные JSON-escape: после parse строка та же, но в сыром
  // тексте последовательностей-маркеров больше нет.
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
  // delta отдаём наружу числами: пайплайну он нужен не для разметки, а чтобы залогировать
  // дисперсию в observability — иначе тренд стабильности пришлось бы считать по sticky.
  // kept/added/gone здесь — ПО ТОЧКАМ КОДА, то есть ровно слагаемые `stability`: агрегат
  // (`lib/metrics.mjs`) печатает их рядом со средним, и из по-находочных счётчиков метрика
  // не пересчитывалась бы. Размеры списков Δ-блока лежат отдельным полем `findings`.
  // verdict — из той же записи прогона, что ушла в футер: если бы пайплайн считал факт
  // блокировки сам, отчёт и метрика могли бы разойтись ровно там, где это важнее всего.
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
    // Кроме факта блокировки этого прогона отдаём агрегат по коммиту: в sticky он виден,
    // но sticky перезаписывается, а artifact прогона — нет. Без агрегата из артефактов
    // нельзя восстановить, был ли гейт недетерминирован на этом sha.
    verdict: {
      sha: currentRun.sha,
      blocked: currentRun.blocked,
      reproducibility: verdictReproducibility([...(priorRuns || []), currentRun], currentRun.sha),
    },
  };
}

/** Sticky для skip/light (R4) — без LLM, merge не блокируется. */
export function buildRouterSummary(cfg, { route, pr, headSha, changedPaths }) {
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
