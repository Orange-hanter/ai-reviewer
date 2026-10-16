#!/usr/bin/env node
// Офлайн-замер decision-моделей (RFC 21, этап 0): пайплайн не запускается, модели опрашиваются
// на корпусе из fixtures/ и gold/ через тот же Rust-бинарь, что и shadow (режим decide).
//
// Q1 (семейство FP находки): находки fixtures + находки gold-прогона из export JSON
//   (`pr-review-gold --jsonl … --export-dir …`); метки — из разметки, без ручной работы.
// Q2 (нужно ли чанку жюри): чанки диффов fixtures и gold тем же buildChunks, что в пайплайне;
//   детерминированный вердикт — chunkNeedsSpecialists; метка jury_required — чанк с размеченным
//   дефектом (mustFindLocations).
//
// Для каждого элемента и модели: R повторов на исходном порядке вариантов (шум) и один вызов с
// K перестановками (смещение порядка). Шум и порядок разделены: у Decision-1 они одного масштаба.
//
// Запуск:
//   REVIEW_DECISION_API_KEY=… node bin/pr-review-decision-eval.mjs \
//     [--models a,b] [--repeats 5] [--permutations 2] [--fixtures fixtures] [--gold-dir gold] \
//     [--gold-jsonl .ai-review/gold.jsonl --export-dir .ai-review/gold-export] \
//     [--limit N] [--concurrency 4] [--json out.json] [--md out.md]
import { readdir, readFile, writeFile, mkdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join, resolve, dirname } from 'node:path';
import { loadReviewConfig } from '../lib/config.mjs';
import { parseDiff, buildChunks } from '../lib/diff.mjs';
import { extractFileDiffSections, classifyFilePath } from '../lib/router.mjs';
import { getFpPatterns, matchFpPatterns } from '../lib/fp-patterns.mjs';
import { chunkNeedsSpecialists } from '../lib/chunk-routing.mjs';
import { locationMatches } from '../lib/benchmark.mjs';
import { ensureCaseDiff } from '../lib/gold-fetch.mjs';
import { pkgPath } from '../lib/package-root.mjs';
import {
  FP_QUESTION, CHUNK_QUESTION, fpFamilyOfPattern, diffWindowFor, buildFpState, buildChunkState,
  createDecisionClient, resolveDecisionBin,
} from '../lib/decision.mjs';
import {
  auc, bootstrapCi, cohenKappa, spearman, quantile, repeatNoise, looTemperatureEce, argmaxOf,
} from '../lib/decision-metrics.mjs';

const argv = process.argv.slice(2);
const opt = (name, def = null) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : def;
};
const MODELS = opt('models', 'microsoft/microsoft-decision-1,openai/gpt-6-luna-decisions').split(',').map((s) => s.trim()).filter(Boolean);
const REPEATS = Math.max(2, Number(opt('repeats', 5)));
const PERMS = Math.max(2, Number(opt('permutations', 2)));
const LIMIT = opt('limit') ? Number(opt('limit')) : Infinity;
const CONCURRENCY = Math.max(1, Number(opt('concurrency', 4)));
const FIXTURES = resolve(process.cwd(), opt('fixtures', pkgPath('fixtures')));
const GOLD_DIR = resolve(process.cwd(), opt('gold-dir', pkgPath('gold')));
const GOLD_JSONL = opt('gold-jsonl') ? resolve(process.cwd(), opt('gold-jsonl')) : null;
const EXPORT_DIR = opt('export-dir') ? resolve(process.cwd(), opt('export-dir')) : null;
const CACHE_DIR = resolve(process.cwd(), process.env.REVIEW_GOLD_CACHE || '.ai-review/gold-public');

/** Fixtures, чьё FP снимается не регексом, а проверкой дерева репозитория (семейство 7). */
const FIXTURE_FAMILY = { 'fp-absent-path-outside-diff': 'f7_absent_path' };
const BLOCKING = new Set(['critical', 'major']);

const baseCfg = loadReviewConfig({ ...process.env, REVIEW_DECISION: 'shadow' }, { dryRun: true });
const fileClassCache = new Map();
const fileClass = (p) => {
  if (!fileClassCache.has(p)) fileClassCache.set(p, classifyFilePath(p));
  return fileClassCache.get(p);
};
const routingCtx = { fileClass, sensitive: baseCfg.profile?.layout?.sensitive || null };
const patterns = getFpPatterns();

function diffCorpus(diffText) {
  const { files, annotated } = parseDiff(diffText);
  return { changedPaths: [...files.keys()], annotated, sections: extractFileDiffSections(annotated) };
}

function fpItem({ source, finding, corpus, label, family }) {
  const regexPatternId = matchFpPatterns(`${finding.title} ${finding.body}`, corpus.changedPaths, patterns);
  return {
    kind: 'fp', source, file: finding.file, line: finding.line, severity: finding.severity,
    title: String(finding.title || '').slice(0, 100),
    state: buildFpState(finding, diffWindowFor(corpus.sections.get(finding.file) || '', finding.line)),
    label, family: family ?? (label === 'fp' ? fpFamilyOfPattern(regexPatternId) : label === 'real' ? 'real' : null),
    regexPatternId, regexFamily: fpFamilyOfPattern(regexPatternId),
  };
}

function chunkItems(source, corpus, defectFiles = new Set()) {
  return buildChunks(corpus.annotated, new Map(), baseCfg.limits).map((chunk, i) => ({
    kind: 'chunk', source: `${source}#${i}`,
    state: buildChunkState(chunk, routingCtx),
    deterministic: chunkNeedsSpecialists(chunk, routingCtx) ? 'jury' : 'single',
    label: (chunk.paths || []).some((p) => defectFiles.has(p)) ? 'jury_required' : null,
    nPaths: (chunk.paths || []).length,
  }));
}

async function fixtureCorpus() {
  const items = [];
  if (!existsSync(FIXTURES)) return items;
  for (const id of (await readdir(FIXTURES)).sort()) {
    const dir = join(FIXTURES, id);
    if (!existsSync(join(dir, 'meta.json')) || !existsSync(join(dir, 'findings.json'))) continue;
    const meta = JSON.parse(await readFile(join(dir, 'meta.json'), 'utf8'));
    const findings = JSON.parse(await readFile(join(dir, 'findings.json'), 'utf8')).findings || [];
    const diff = existsSync(join(dir, 'diff.patch')) ? await readFile(join(dir, 'diff.patch'), 'utf8') : '';
    const corpus = diffCorpus(diff);
    const exp = meta.expected || {};
    for (const f of findings) {
      if (f.prescan) continue;
      // Метка только там, где разметка говорит «это FP»: fp-кейс, где находка обязана исчезнуть.
      const isFp = id.startsWith('fp-') && (exp.findingCount === 0 || (exp.noTitles || []).includes(f.title));
      items.push(fpItem({ source: `fixture:${id}`, finding: f, corpus, label: isFp ? 'fp' : null, family: isFp ? FIXTURE_FAMILY[id] : undefined }));
    }
    if (diff) items.push(...chunkItems(`fixture:${id}`, corpus));
  }
  return items;
}

async function exportFindings(id) {
  const dir = join(EXPORT_DIR, id);
  if (!existsSync(dir)) return null;
  const file = (await readdir(dir)).find((f) => f.endsWith('.json') && !f.endsWith('-swarm.json'));
  if (!file) return null;
  const payload = JSON.parse(await readFile(join(dir, file), 'utf8'));
  return payload.llmFailed ? null : payload.findings || [];
}

async function goldCorpus() {
  const items = [];
  if (!existsSync(GOLD_DIR)) return items;
  const records = new Map();
  if (GOLD_JSONL && existsSync(GOLD_JSONL)) {
    for (const line of (await readFile(GOLD_JSONL, 'utf8')).split('\n').filter(Boolean)) {
      const r = JSON.parse(line);
      records.set(r.id, r);
    }
  }
  for (const id of (await readdir(GOLD_DIR)).sort()) {
    const dir = join(GOLD_DIR, id);
    if (!existsSync(join(dir, 'meta.json'))) continue;
    const meta = JSON.parse(await readFile(join(dir, 'meta.json'), 'utf8'));
    let diffText;
    try {
      diffText = await readFile((await ensureCaseDiff(dir, meta, { cacheDir: CACHE_DIR })).path, 'utf8');
    } catch (e) {
      console.error(`[decision-eval] gold ${id}: дифф недоступен (${e.message}) — пропуск`);
      continue;
    }
    const corpus = diffCorpus(diffText);
    const expect = meta.expect || {};
    const locs = expect.mustFindLocations || [];
    items.push(...chunkItems(`gold:${id}`, corpus, new Set(locs.map((l) => l.file))));
    if (!EXPORT_DIR || (records.size && !records.has(id))) continue;
    const findings = await exportFindings(id);
    if (!findings) continue;
    for (const f of findings) {
      if (f.prescan) continue;
      const real = locs.some((loc) => locationMatches([f], loc));
      const fp = (expect.mustNotFindTitles || []).some((t) => String(f.title || '').toLowerCase().includes(String(t).toLowerCase()))
        || (meta.type === 'clean' && BLOCKING.has(f.severity));
      const label = real && !fp ? 'real' : fp && !real ? 'fp' : null;
      items.push(fpItem({ source: `gold:${id}`, finding: f, corpus, label }));
    }
  }
  return items;
}

async function mapLimit(list, n, fn) {
  const out = new Array(list.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(n, list.length) }, async () => {
    while (next < list.length) {
      const i = next++;
      out[i] = await fn(list[i], i);
    }
  }));
  return out;
}

/** Один элемент × одна модель: R повторов (K=1) + один вызов с K перестановками. */
async function probeItem(client, item) {
  const question = item.kind === 'fp' ? FP_QUESTION : CHUNK_QUESTION;
  const repeats = [];
  const errors = [];
  for (let r = 0; r < REPEATS; r++) {
    const res = await client.one.decide(item.state, question);
    if (res.error) errors.push(res.error); else repeats.push(res);
  }
  const perm = await client.perm.decide(item.state, question);
  if (perm.error) errors.push(perm.error);
  return {
    repeats: repeats.map((r) => r.answer.probabilities),
    perm: perm.error ? null : perm.answer,
    latency: [...repeats.map((r) => r.latencyMs), perm.latencyMs].filter(Number.isFinite),
    cost: [...repeats, perm].reduce((s, r) => s + (Number.isFinite(r?.usage?.cost) ? r.usage.cost : 0), 0),
    errors,
  };
}

const pRealOf = (dist) => (dist ? dist.real ?? null : null);
const fmt = (x, d = 3) => (x == null || !Number.isFinite(x) ? '—' : x.toFixed(d));
const fmtCi = (ci) => (ci ? `[${fmt(ci[0], 2)}, ${fmt(ci[1], 2)}]` : '—');

function summarize(model, rows) {
  const fp = rows.filter((r) => r.item.kind === 'fp');
  const ch = rows.filter((r) => r.item.kind === 'chunk');
  const calls = rows.length * (REPEATS + 1);
  const errs = rows.reduce((s, r) => s + r.res.errors.length, 0);
  const noise = rows.map((r) => repeatNoise(r.res.repeats)).filter(Boolean);
  // Порядок сверх шума: TV между перестановками против TV между повторами того же элемента.
  const orderTv = rows.map((r) => r.res.perm?.tvMax).filter(Number.isFinite);
  const flips = rows.filter((r) => r.res.perm).map((r) => Boolean(r.res.perm.flip));

  const labeled = fp.filter((r) => r.item.label && r.res.perm);
  const scores = labeled.map((r) => pRealOf(r.res.perm.probabilities));
  const ys = labeled.map((r) => r.item.label === 'real');
  const aucVal = auc(scores, ys);
  const aucCi = bootstrapCi(labeled.map((r, i) => ({ s: scores[i], y: ys[i] })), (xs) => auc(xs.map((x) => x.s), xs.map((x) => x.y)));
  const famLabeled = fp.filter((r) => r.item.family && r.res.perm);
  const famAcc = famLabeled.length
    ? famLabeled.filter((r) => r.res.perm.argmax === r.item.family).length / famLabeled.length : null;
  const regexRows = fp.filter((r) => r.item.regexFamily && r.res.perm);
  const regexKappa = cohenKappa(regexRows.map((r) => r.item.regexFamily), regexRows.map((r) => r.res.perm.argmax));
  const regexFpRate = regexRows.length
    ? regexRows.filter((r) => r.res.perm.argmax !== 'real').length / regexRows.length : null;

  const chOk = ch.filter((r) => r.res.perm);
  const agree = chOk.length ? chOk.filter((r) => r.res.perm.argmax === r.item.deterministic).length / chOk.length : null;
  const missedJury = chOk.filter((r) => r.item.label === 'jury_required' && r.res.perm.argmax === 'single');
  const k3 = missedJury.filter((r) => (r.res.perm.probabilities.single ?? 0) >= 0.7);
  const savings = chOk.filter((r) => r.item.deterministic === 'jury' && r.res.perm.argmax === 'single').length;

  const lat = rows.flatMap((r) => r.res.latency);
  const cal = labeled.length >= 4 ? looTemperatureEce(scores, ys) : null;
  return {
    model, items: rows.length, fpItems: fp.length, chunkItems: ch.length, calls, errorRate: calls ? errs / calls : null,
    noise: {
      argmaxChangeRate: noise.length ? noise.filter((n) => n.argmaxChanged).length / noise.length : null,
      tvMedian: quantile(noise.map((n) => n.tvMax), 0.5),
    },
    order: { flipRate: flips.length ? flips.filter(Boolean).length / flips.length : null, tvMedian: quantile(orderTv, 0.5) },
    q1: {
      labeled: labeled.length, positives: ys.filter(Boolean).length, auc: aucVal, aucCi,
      familyLabeled: famLabeled.length, familyAccuracy: famAcc,
      regexRows: regexRows.length, regexKappa, regexFpRate,
      calibration: cal,
    },
    q2: { chunks: chOk.length, agreeWithDeterministic: agree, jurySaved: savings, missedJuryRequired: missedJury.length, k3: k3.length },
    latency: { p50: quantile(lat, 0.5), p95: quantile(lat, 0.95) },
    costUsd: rows.reduce((s, r) => s + r.res.cost, 0),
  };
}

function crossModel(byModel) {
  const out = [];
  for (let i = 0; i < MODELS.length; i++) {
    for (let j = i + 1; j < MODELS.length; j++) {
      const a = byModel[MODELS[i]];
      const b = byModel[MODELS[j]];
      const pairs = a.map((r, k) => [r, b[k]]).filter(([x, y]) => x.res.perm && y.res.perm);
      const fpPairs = pairs.filter(([x]) => x.item.kind === 'fp');
      out.push({
        a: MODELS[i], b: MODELS[j], n: pairs.length,
        kappaArgmax: cohenKappa(pairs.map(([x]) => x.res.perm.argmax), pairs.map(([, y]) => y.res.perm.argmax)),
        spearmanPReal: spearman(fpPairs.map(([x]) => pRealOf(x.res.perm.probabilities)), fpPairs.map(([, y]) => pRealOf(y.res.perm.probabilities))),
      });
    }
  }
  return out;
}

function verdict(s) {
  const q = s.q1;
  const checks = [
    ['ошибки API < 5%', s.errorRate != null && s.errorRate < 0.05],
    ['смена argmax за повторы ≤ 10%', s.noise.argmaxChangeRate != null && s.noise.argmaxChangeRate <= 0.1],
    ['flip перестановок ≤ 15%', s.order.flipRate != null && s.order.flipRate <= 0.15],
    ['ДИ AUC > 0.5 или κ с регексом ≥ 0.6', (q.aucCi && q.aucCi[0] > 0.5) || (q.regexKappa != null && q.regexKappa >= 0.6)],
    ['p95 латентности < таймаута', s.latency.p95 != null && s.latency.p95 < baseCfg.decision.timeoutMs],
  ];
  return checks;
}

function markdown(report) {
  const L = [];
  L.push(`# Офлайн-замер decision-моделей — ${report.date}`, '');
  L.push(`Корпус: ${report.corpus.fp} находок (Q1, размечено ${report.corpus.fpLabeled}), ${report.corpus.chunk} чанков (Q2, с меткой jury_required ${report.corpus.chunkLabeled}). `
    + `Повторов ${REPEATS}, перестановок ${PERMS}. Gold-находки: ${report.corpus.goldFindings ? 'есть' : 'нет'}.`, '');
  L.push('| Метрика | ' + report.models.map((s) => `\`${s.model}\``).join(' | ') + ' |');
  L.push('|---|' + report.models.map(() => '---|').join(''));
  const row = (name, fn) => L.push(`| ${name} | ${report.models.map(fn).join(' | ')} |`);
  row('Ошибки API', (s) => fmt(s.errorRate, 3));
  row('Шум: смена argmax за повторы', (s) => fmt(s.noise.argmaxChangeRate, 3));
  row('Шум: медиана TV повторов', (s) => fmt(s.noise.tvMedian, 3));
  row('Порядок: доля flip', (s) => fmt(s.order.flipRate, 3));
  row('Порядок: медиана TV перестановок', (s) => fmt(s.order.tvMedian, 3));
  row('Q1: AUC p(real), размечено (полож.)', (s) => `${fmt(s.q1.auc, 2)} ${fmtCi(s.q1.aucCi)} n=${s.q1.labeled} (${s.q1.positives})`);
  row('Q1: точность семейства (справочно)', (s) => `${fmt(s.q1.familyAccuracy, 2)} n=${s.q1.familyLabeled}`);
  row('Q1: κ с регекс-семейством', (s) => `${fmt(s.q1.regexKappa, 2)} n=${s.q1.regexRows}`);
  row('Q1: регекс-FP, которые модель тоже считает FP', (s) => fmt(s.q1.regexFpRate, 2));
  row('Q1: ECE до / после LOO-температуры (разведка)', (s) => (s.q1.calibration ? `${fmt(s.q1.calibration.before, 3)} / ${fmt(s.q1.calibration.after, 3)} (T=${fmt(s.q1.calibration.tAll, 2)})` : '—'));
  row('Q2: согласие с chunkNeedsSpecialists', (s) => `${fmt(s.q2.agreeWithDeterministic, 2)} n=${s.q2.chunks}`);
  row('Q2: jury → single (потенц. экономия)', (s) => String(s.q2.jurySaved));
  row('Q2: jury_required → single / при p≥0.7 (K3)', (s) => `${s.q2.missedJuryRequired} / ${s.q2.k3}`);
  row('Латентность p50 / p95, мс', (s) => `${fmt(s.latency.p50, 0)} / ${fmt(s.latency.p95, 0)}`);
  row('Цена замера, $', (s) => fmt(s.costUsd, 4));
  L.push('');
  for (const c of report.cross) L.push(`κ(argmax) \`${c.a}\` × \`${c.b}\`: ${fmt(c.kappaArgmax, 2)} (n=${c.n}); Спирмен p(real): ${fmt(c.spearmanPReal, 2)}`);
  L.push('', '## Критерии этапа 0 → 1', '');
  for (const s of report.models) {
    L.push(`**${s.model}**`);
    for (const [name, ok] of verdict(s)) L.push(`- ${ok ? '✅' : '❌'} ${name}`);
    L.push('');
  }
  return L.join('\n');
}

const bin = resolveDecisionBin();
if (!bin) {
  console.error('[decision-eval] нет Rust-бинаря llm — cd rust && cargo build');
  process.exit(2);
}
if (!baseCfg.decision.key) {
  console.error('[decision-eval] нет ключа: REVIEW_DECISION_API_KEY / OPENROUTER_API_KEY / LLM_API_KEY');
  process.exit(2);
}

const all = [...await fixtureCorpus(), ...await goldCorpus()];
const items = Number.isFinite(LIMIT) ? all.slice(0, LIMIT) : all;
const goldFindings = items.some((it) => it.kind === 'fp' && it.source.startsWith('gold:'));
console.error(`[decision-eval] элементов: ${items.length} (Q1 ${items.filter((i) => i.kind === 'fp').length}, Q2 ${items.filter((i) => i.kind === 'chunk').length}), моделей ${MODELS.length}`);

const byModel = {};
for (const model of MODELS) {
  const mk = (permutations) => createDecisionClient(
    { decision: { ...baseCfg.decision, model, permutations, maxCalls: Infinity, concurrency: CONCURRENCY } },
    { bin },
  );
  const client = { one: mk(1), perm: mk(PERMS) };
  const t0 = Date.now();
  byModel[model] = await mapLimit(items, CONCURRENCY, async (item) => ({ item, res: await probeItem(client, item) }));
  console.error(`[decision-eval] ${model}: ${((Date.now() - t0) / 1000).toFixed(0)} с`);
}

const report = {
  date: new Date().toISOString().slice(0, 10),
  params: { repeats: REPEATS, permutations: PERMS, models: MODELS },
  corpus: {
    fp: items.filter((i) => i.kind === 'fp').length,
    fpLabeled: items.filter((i) => i.kind === 'fp' && i.label).length,
    chunk: items.filter((i) => i.kind === 'chunk').length,
    chunkLabeled: items.filter((i) => i.kind === 'chunk' && i.label).length,
    goldFindings,
  },
  models: MODELS.map((m) => summarize(m, byModel[m])),
  cross: crossModel(byModel),
  rows: Object.fromEntries(MODELS.map((m) => [m, byModel[m].map(({ item, res }) => ({
    kind: item.kind, source: item.source, file: item.file, line: item.line, title: item.title,
    label: item.label, family: item.family, regexPatternId: item.regexPatternId, deterministic: item.deterministic,
    argmax: res.perm?.argmax ?? null, pReal: pRealOf(res.perm?.probabilities), flip: res.perm?.flip ?? null,
    tvMax: res.perm?.tvMax ?? null, repeatsArgmax: res.repeats.map(argmaxOf), errors: res.errors,
  }))])),
};

const md = markdown(report);
if (opt('json')) {
  await mkdir(dirname(resolve(opt('json'))), { recursive: true });
  await writeFile(resolve(opt('json')), `${JSON.stringify(report, null, 2)}\n`, 'utf8');
}
if (opt('md')) {
  await mkdir(dirname(resolve(opt('md'))), { recursive: true });
  await writeFile(resolve(opt('md')), `${md}\n`, 'utf8');
}
console.log(md);
