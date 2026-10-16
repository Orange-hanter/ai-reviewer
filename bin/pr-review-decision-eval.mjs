#!/usr/bin/env node
// Офлайн-замер decision-моделей (RFC 21, этап 0). Тонкий сборщик: здесь только корпус и опрос
// моделей, потому что они зависят от непортированного JS (FP-регексы с lookahead, gold-fetch,
// chunk-routing, buildChunks-обёртка). Построители входа, метрики и отчёт — Rust-бинарь
// `decision` (rust/src/decision.rs, rust/src/decision_eval.rs), вызов модели — бинарь `llm`.
//
// Q1 (семейство FP находки): находки fixtures + находки gold-прогона из export JSON
//   (`pr-review-gold --jsonl … --export-dir …`); метки — из разметки, без ручной работы.
// Q2 (нужно ли чанку жюри): чанки диффов fixtures и gold; детерминированный вердикт —
//   chunkNeedsSpecialists; метка jury_required — чанк с размеченным дефектом.
// Для каждого элемента и модели: R повторов на исходном порядке вариантов (шум) и один вызов с
// K перестановками (смещение порядка).
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
import { createDecisionClient, resolveDecisionBin, resolveDecisionOpsBin, runDecisionOp } from '../lib/decision.mjs';

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

const bin = resolveDecisionBin();
const opsBin = resolveDecisionOpsBin();
if (!bin || !opsBin) {
  console.error('[decision-eval] нет Rust-бинарей llm/decision — cd rust && cargo build');
  process.exit(2);
}
const baseCfg = loadReviewConfig({ ...process.env, REVIEW_DECISION: 'shadow' }, { dryRun: true });
if (!baseCfg.decision.key) {
  console.error('[decision-eval] нет ключа: REVIEW_DECISION_API_KEY / OPENROUTER_API_KEY / LLM_API_KEY');
  process.exit(2);
}
const QUESTIONS = runDecisionOp('questions');
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

/** Находки одного диффа → элементы Q1 (вход строит Rust одним вызовом). */
function fpItems(source, corpus, labeled) {
  if (!labeled.length) return [];
  const regexIds = labeled.map(({ finding: f }) => matchFpPatterns(`${f.title} ${f.body}`, corpus.changedPaths, patterns));
  const built = runDecisionOp('fpStates', {
    items: labeled.map(({ finding }, i) => ({ finding, section: corpus.sections.get(finding.file) || '', regexPatternId: regexIds[i] })),
  });
  return labeled.map(({ finding, label, family }, i) => ({
    kind: 'fp', source, file: finding.file, line: finding.line, title: String(finding.title || '').slice(0, 100),
    state: built[i].state, label, regexPatternId: regexIds[i], regexFamily: built[i].regexFamily,
    family: family ?? (label === 'fp' ? built[i].regexFamily : label === 'real' ? 'real' : null),
  }));
}

function chunkItems(source, corpus, defectFiles = new Set()) {
  const chunks = buildChunks(corpus.annotated, new Map(), baseCfg.limits);
  if (!chunks.length) return [];
  const classes = {};
  for (const c of chunks) for (const p of c.paths || []) classes[p] ??= fileClass(p);
  const states = runDecisionOp('chunkStates', { chunks: chunks.map((c) => ({ paths: c.paths || [], annotated: c.annotated || '' })), classes });
  return chunks.map((chunk, i) => ({
    kind: 'chunk', source: `${source}#${i}`, state: states[i],
    deterministic: chunkNeedsSpecialists(chunk, routingCtx) ? 'jury' : 'single',
    label: (chunk.paths || []).some((p) => defectFiles.has(p)) ? 'jury_required' : null,
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
    // Метка только там, где разметка говорит «это FP»: fp-кейс, где находка обязана исчезнуть.
    const labeled = findings.filter((f) => !f.prescan).map((f) => {
      const isFp = id.startsWith('fp-') && (exp.findingCount === 0 || (exp.noTitles || []).includes(f.title));
      return { finding: f, label: isFp ? 'fp' : null, family: isFp ? FIXTURE_FAMILY[id] : undefined };
    });
    items.push(...fpItems(`fixture:${id}`, corpus, labeled));
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
  const records = new Set();
  if (GOLD_JSONL && existsSync(GOLD_JSONL)) {
    for (const line of (await readFile(GOLD_JSONL, 'utf8')).split('\n').filter(Boolean)) records.add(JSON.parse(line).id);
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
    const labeled = findings.filter((f) => !f.prescan).map((f) => {
      const real = locs.some((loc) => locationMatches([f], loc));
      const fp = (expect.mustNotFindTitles || []).some((t) => String(f.title || '').toLowerCase().includes(String(t).toLowerCase()))
        || (meta.type === 'clean' && BLOCKING.has(f.severity));
      return { finding: f, label: real && !fp ? 'real' : fp && !real ? 'fp' : null };
    });
    items.push(...fpItems(`gold:${id}`, corpus, labeled));
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
  const question = item.kind === 'fp' ? QUESTIONS.fp : QUESTIONS.chunk;
  const repeats = [];
  const errors = [];
  for (let r = 0; r < REPEATS; r++) {
    const res = await client.one.decide(item.state, question);
    if (res.error) errors.push(res.error); else repeats.push(res);
  }
  const perm = await client.perm.decide(item.state, question);
  if (perm.error) errors.push(perm.error);
  return {
    kind: item.kind, source: item.source, file: item.file, line: item.line, title: item.title,
    label: item.label, family: item.family, regexPatternId: item.regexPatternId, regexFamily: item.regexFamily,
    deterministic: item.deterministic,
    perm: perm.error ? null : perm.answer,
    repeats: repeats.map((r) => r.answer.probabilities),
    latency: [...repeats.map((r) => r.latencyMs), perm.latencyMs].filter(Number.isFinite),
    cost: [...repeats, perm].reduce((s, r) => s + (Number.isFinite(r?.usage?.cost) ? r.usage.cost : 0), 0),
    errors,
  };
}

const all = [...await fixtureCorpus(), ...await goldCorpus()];
const items = Number.isFinite(LIMIT) ? all.slice(0, LIMIT) : all;
console.error(`[decision-eval] элементов: ${items.length} (Q1 ${items.filter((i) => i.kind === 'fp').length}, Q2 ${items.filter((i) => i.kind === 'chunk').length}), моделей ${MODELS.length}`);

const rows = {};
for (const model of MODELS) {
  const mk = (permutations) => createDecisionClient(
    { decision: { ...baseCfg.decision, model, permutations, maxCalls: Infinity, concurrency: CONCURRENCY } },
    { bin },
  );
  const client = { one: mk(1), perm: mk(PERMS) };
  const t0 = Date.now();
  rows[model] = await mapLimit(items, CONCURRENCY, (item) => probeItem(client, item));
  console.error(`[decision-eval] ${model}: ${((Date.now() - t0) / 1000).toFixed(0)} с`);
}

const report = runDecisionOp('report', {
  date: new Date().toISOString().slice(0, 10),
  params: { repeats: REPEATS, permutations: PERMS, timeoutMs: baseCfg.decision.timeoutMs },
  corpus: {
    fp: items.filter((i) => i.kind === 'fp').length,
    fpLabeled: items.filter((i) => i.kind === 'fp' && i.label).length,
    chunk: items.filter((i) => i.kind === 'chunk').length,
    chunkLabeled: items.filter((i) => i.kind === 'chunk' && i.label).length,
    goldFindings: items.some((i) => i.kind === 'fp' && i.source.startsWith('gold:')),
  },
  models: MODELS,
  rows,
}, opsBin);

for (const [flag, body] of [['json', `${JSON.stringify({ ...report, rows }, null, 2)}\n`], ['md', `${report.markdown}\n`]]) {
  if (!opt(flag)) continue;
  await mkdir(dirname(resolve(opt(flag))), { recursive: true });
  await writeFile(resolve(opt(flag)), body, 'utf8');
}
console.log(report.markdown);
