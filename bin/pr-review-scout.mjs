#!/usr/bin/env node
// Офлайн-замер слоя косвенных признаков (RFC 08, этап 0): поднимаются ли размеченные
// дефектные строки в топ ранжирования спанов — БЕЗ единого вызова LLM.
//
// Запуск:
//   node bin/pr-review-scout.mjs --gold-dir gold             # офлайн, локальные диффы
//   node bin/pr-review-scout.mjs --gold-dir gold-public      # решающий замер (1-й прогон — сеть)
//   node bin/pr-review-scout.mjs --case <id> --briefs         # показать наряды кейса
//   node bin/pr-review-scout.mjs --gold-dir gold-public --json
//
// Метрики — effort-aware, как принято в JIT-предсказании дефектов (Kamei 2013, Yang 2016):
// recall на единицу потраченных усилий, а не F1. Эффорт — строки новой версии, которые
// пришлось бы осмотреть.
import { readdir, readFile } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { extractFileDiffSections, classifyFilePath } from '../lib/router.mjs';
import { collectSpans, collectCues, rankSpans } from '../lib/cues.mjs';
import { buildBriefs, formatBrief } from '../lib/brief.mjs';
import { ensureCaseDiff } from '../lib/gold-fetch.mjs';
import { DEFAULT_LOCATION_TOLERANCE } from '../lib/benchmark.mjs';

const argv = process.argv.slice(2);
const flag = (name) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : null;
};
const has = (name) => argv.includes(name);

const GOLD_DIR = resolve(
  process.cwd(),
  flag('--gold-dir') || process.env.REVIEW_GOLD_DIR || join(dirname(fileURLToPath(import.meta.url)), '../gold'),
);
const CACHE_DIR = resolve(process.cwd(), process.env.REVIEW_GOLD_CACHE || '.ai-review/gold-public');
const only = flag('--case');
const asJson = has('--json');
const showBriefs = has('--briefs');
const TOLERANCE = DEFAULT_LOCATION_TOLERANCE;

// Класс пути спрашивают тысячи раз, а под ним может быть спавн Rust-бинаря на вызов —
// мемоизируем ровно так же, как это делает pipeline.mjs:452.
const classCache = new Map();
const fileClass = (p) => {
  if (!classCache.has(p)) classCache.set(p, classifyFilePath(p));
  return classCache.get(p);
};

const normPath = (s) => String(s || '').trim().replace(/^\.\//, '');

/** Спан покрывает размеченную локацию: тот же файл, пересечение диапазонов в пределах допуска. */
function covers(span, loc) {
  if (normPath(span.file) !== normPath(loc.file)) return false;
  const range = Array.isArray(loc.lines) ? loc.lines : [loc.line, loc.line];
  const from = Number(range[0]);
  const to = Number(range[1] ?? range[0]);
  // Локация без строк — «где-то в этом файле».
  if (!Number.isFinite(from)) return true;
  return Math.max(from, span.startLine - TOLERANCE) <= Math.min(to, span.endLine + TOLERANCE);
}

/**
 * Кривая «эффорт → recall» для заданного порядка спанов.
 * @returns {{acc20:number, ifa:number|null, popt:number|null, hits:number, points:[number,number][]}}
 */
function curve(order, locations, totalEffort) {
  const found = new Set();
  const points = [[0, 0]];
  let spent = 0;
  let ifa = null;
  let seen = 0;
  // Сколько локаций взято к моменту, когда осмотрено ровно N спанов. Единица эффорта здесь
  // — наряд, а не строка: наряд и есть то, что стоит денег на этапах 1–2 (окно контекста
  // и вызов роли), и стоит он примерно одинаково независимо от длины хунка.
  const byRank = [];
  for (const span of order) {
    seen += 1;
    spent += span.effort;
    let hitHere = false;
    locations.forEach((loc, i) => {
      if (!found.has(i) && covers(span, loc)) { found.add(i); hitHere = true; }
    });
    if (hitHere && ifa === null) ifa = seen - 1;
    byRank.push(found.size / locations.length);
    points.push([spent / totalEffort, found.size / locations.length]);
  }
  const at = (x) => {
    let y = 0;
    for (const [px, py] of points) { if (px <= x) y = py; else break; }
    return y;
  };
  const topN = (n) => (byRank.length ? byRank[Math.min(n, byRank.length) - 1] : 0);
  // Цена первых трёх нарядов в долях всего диффа: без неё Recall@3 сравнивает стратегии,
  // которые читают разный объём кода, и «взять три самых больших хунка» выглядит победой.
  const costTop3 = order.slice(0, 3).reduce((a, x) => a + x.effort, 0) / totalEffort;
  return { acc20: at(0.2), top3: topN(3), top5: topN(5), costTop3, ifa, hits: found.size, points };
}

/** Площадь под кривой (трапеции) — для P_opt. */
function area(points) {
  let a = 0;
  for (let i = 1; i < points.length; i += 1) {
    const [x0, y0] = points[i - 1];
    const [x1, y1] = points[i];
    a += (x1 - x0) * (y0 + y1) / 2;
  }
  const last = points[points.length - 1];
  if (last[0] < 1) a += (1 - last[0]) * last[1];
  return a;
}

/**
 * P_opt: насколько порядок близок к оптимальному в единицах площади под кривой
 * cost-effectiveness. 1 — оптимум, 0 — худший возможный порядок.
 */
function popt(order, locations, totalEffort) {
  const covering = order.filter((s) => locations.some((l) => covers(s, l)));
  const rest = order.filter((s) => !covering.includes(s));
  const best = [...covering].sort((a, b) => a.effort - b.effort).concat(rest);
  const worst = [...rest].sort((a, b) => b.effort - a.effort)
    .concat([...covering].sort((a, b) => b.effort - a.effort));
  const aModel = area(curve(order, locations, totalEffort).points);
  const aBest = area(curve(best, locations, totalEffort).points);
  const aWorst = area(curve(worst, locations, totalEffort).points);
  if (aBest - aWorst <= 0) return null;
  return (aModel - aWorst) / (aBest - aWorst);
}

/** Детерминированный PRNG: замер обязан воспроизводиться. */
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function shuffled(list, rnd) {
  const a = [...list];
  for (let i = a.length - 1; i > 0; i -= 1) {
    const j = Math.floor(rnd() * (i + 1));
    [a[i], a[j]] = [a[j], a[i]];
  }
  return a;
}

/** Порядки-претенденты. `diff-order` — сегодняшнее равномерное внимание движка. */
function strategies(spans) {
  const byDiff = [...spans].sort((a, b) => a.index - b.index);
  return {
    'cues:e=0.0': rankSpans(spans, { effortExponent: 0 }),
    'cues:e=0.5': rankSpans(spans, { effortExponent: 0.5 }),
    'cues:e=1.0': rankSpans(spans, { effortExponent: 1 }),
    'diff-order': byDiff,
    'size-asc': [...spans].sort((a, b) => a.effort - b.effort || a.index - b.index),
    'size-desc': [...spans].sort((a, b) => b.effort - a.effort || a.index - b.index),
  };
}

async function runCase(id) {
  const dir = join(GOLD_DIR, id);
  const meta = JSON.parse(await readFile(join(dir, 'meta.json'), 'utf8'));
  const patch = await ensureCaseDiff(dir, meta, { cacheDir: CACHE_DIR });
  const diff = await readFile(patch.path, 'utf8');

  const sections = extractFileDiffSections(diff);
  const spans = collectCues(collectSpans(sections), { fileClass });
  const locations = meta.expect?.mustFindLocations || [];
  const totalEffort = spans.reduce((s, x) => s + x.effort, 0) || 1;
  // Потолок: локации, которых не касается ни один спан. Их не возьмёт никакое ранжирование —
  // это дефект разметки или доставки диффа, и он обязан быть виден отдельно от качества.
  const reachable = locations.filter((l) => spans.some((s) => covers(s, l)));

  const out = {
    id,
    type: meta.type || 'tp',
    files: sections.size,
    spans: spans.length,
    effort: totalEffort,
    locations: locations.length,
    reachable: reachable.length,
    diffSource: patch.source,
    strategies: {},
  };
  if (reachable.length) {
    for (const [name, order] of Object.entries(strategies(spans))) {
      const c = curve(order, reachable, totalEffort);
      out.strategies[name] = {
        acc20: c.acc20,
        top3: c.top3,
        top5: c.top5,
        costTop3: c.costTop3,
        ifa: c.ifa,
        popt: popt(order, reachable, totalEffort),
      };
    }
    // Случайный порядок — среднее по 20 перестановкам с фиксированным зерном.
    const rnd = mulberry32(0x5eed);
    const runs = Array.from({ length: 20 }, () => {
      const order = shuffled(spans, rnd);
      const c = curve(order, reachable, totalEffort);
      return {
        acc20: c.acc20, top3: c.top3, top5: c.top5, costTop3: c.costTop3,
        ifa: c.ifa ?? spans.length, popt: popt(order, reachable, totalEffort) ?? 0,
      };
    });
    const mean = (k) => runs.reduce((s, r) => s + r[k], 0) / runs.length;
    out.strategies.random = {
      acc20: mean('acc20'), top3: mean('top3'), top5: mean('top5'),
      costTop3: mean('costTop3'), ifa: mean('ifa'), popt: mean('popt'),
    };
  }
  if (showBriefs) {
    out.briefs = buildBriefs(rankSpans(spans, { effortExponent: 0.5 }), { fileClass });
  }
  return out;
}

function median(xs) {
  const a = xs.filter((x) => Number.isFinite(x)).sort((x, y) => x - y);
  if (!a.length) return null;
  const m = Math.floor(a.length / 2);
  return a.length % 2 ? a[m] : (a[m - 1] + a[m]) / 2;
}

function pct(x) {
  return x == null ? '  —  ' : `${(x * 100).toFixed(1).padStart(5)}%`;
}

function report(results) {
  const scored = results.filter((r) => r.reachable > 0);
  const lines = [];
  lines.push(`# Разведка по косвенным признакам — ${GOLD_DIR}`);
  lines.push('');
  lines.push('| кейс | спанов | строк | локаций | Recall@3 (cues) | Recall@3 (diff) | IFA | P_opt |');
  lines.push('|---|---:|---:|---:|---:|---:|---:|---:|');
  for (const r of results) {
    const s = r.strategies['cues:e=0.5'];
    const d = r.strategies['diff-order'];
    lines.push(`| \`${r.id}\` | ${r.spans} | ${r.effort} | ${r.locations}`
      + ` | ${s ? pct(s.top3) : '  —  '} | ${d ? pct(d.top3) : '  —  '}`
      + ` | ${s?.ifa ?? '—'} | ${s?.popt != null ? s.popt.toFixed(2) : '—'} |`);
  }
  lines.push('');

  const names = ['cues:e=0.0', 'cues:e=0.5', 'cues:e=1.0', 'diff-order', 'size-asc', 'size-desc', 'random'];
  lines.push(`## Сводка по ${scored.length} кейс(ам) с достижимыми локациями`);
  lines.push('');
  lines.push('| стратегия | Recall@3 | цена топ-3 | Recall@5 | ACC@20%LOC | IFA (медиана) | P_opt (среднее) |');
  lines.push('|---|---:|---:|---:|---:|---:|---:|');
  for (const name of names) {
    // micro — по локациям (кейс с тремя локациями весит втрое), macro — по кейсам.
    const micro = { top3: 0, top5: 0, acc20: 0, costTop3: 0 };
    let total = 0;
    const ifas = [];
    const popts = [];
    for (const r of scored) {
      const s = r.strategies[name];
      if (!s) continue;
      // micro — по локациям: кейс с пятью размеченными строками весит впятеро.
      micro.top3 += s.top3 * r.reachable;
      micro.top5 += s.top5 * r.reachable;
      micro.acc20 += s.acc20 * r.reachable;
      micro.costTop3 += (s.costTop3 ?? 0) * r.reachable;
      total += r.reachable;
      if (s.ifa != null) ifas.push(s.ifa);
      if (s.popt != null) popts.push(s.popt);
    }
    lines.push(`| ${name} | ${pct(total ? micro.top3 / total : null)}`
      + ` | ${pct(total ? micro.costTop3 / total : null)}`
      + ` | ${pct(total ? micro.top5 / total : null)}`
      + ` | ${pct(total ? micro.acc20 / total : null)}`
      + ` | ${median(ifas)?.toFixed(1) ?? '—'}`
      + ` | ${popts.length ? (popts.reduce((a, b) => a + b, 0) / popts.length).toFixed(3) : '—'} |`);
  }
  const unreachable = results.reduce((s, r) => s + (r.locations - r.reachable), 0);
  if (unreachable) {
    lines.push('');
    lines.push(`⚠️ Недостижимых локаций (не покрыты ни одним спаном): **${unreachable}**`
      + ' — их не возьмёт никакое ранжирование, это разметка или доставка диффа, а не качество слоя.');
  }
  return lines.join('\n');
}

async function main() {
  const ids = only
    ? [only]
    : (await readdir(GOLD_DIR, { withFileTypes: true }))
      .filter((d) => d.isDirectory())
      .map((d) => d.name)
      .sort();

  const results = [];
  for (const id of ids) {
    try {
      results.push(await runCase(id));
    } catch (e) {
      if (!asJson) console.error(`💥 ${id}: ${e.message}`);
    }
  }

  if (asJson) {
    console.log(JSON.stringify({ goldDir: GOLD_DIR, tolerance: TOLERANCE, results }, null, 2));
    return;
  }
  console.log(report(results));
  if (showBriefs) {
    for (const r of results) {
      if (!r.briefs?.length) continue;
      console.log(`\n## Наряды — ${r.id} (${r.briefs.length}, показаны первые 15)\n`);
      for (const b of r.briefs.slice(0, 15)) console.log(formatBrief(b));
    }
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
