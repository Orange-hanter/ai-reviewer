#!/usr/bin/env node
// Shadow eval ПОЛНОГО pipeline (с LLM) на gold-диффах с разметкой TP/FP (R-eval, backlog P0-3).
// В отличие от eval.mjs (fixtures, post-processing без LLM) — здесь вызываются модели.
// Запуск: OLLAMA_API_KEY=… npx pr-review-gold [--case <id>] [--jsonl out.jsonl]
// Каталог набора — REVIEW_GOLD_DIR; диффы публичных кейсов тянутся по пину в REVIEW_GOLD_CACHE.
// Blast-radius в gold-прогонах заглушен (детерминизм; на находки влияет слабо). Исключение —
// REVIEW_GOLD_BLAST=1 у кейса со своим деревом: граф Graphify строится в дереве, blast считается
// по-настоящему (замер RFC 21, REVIEW_BLAST_VERIFY). Нужен GRAPHIFY_BIN.
import { readdir, readFile, writeFile, mkdir, access } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadReviewConfig } from '../lib/config.mjs';
import { createReviewPipeline } from '../pipeline.mjs';
import { MARKERS } from '../lib/constants.mjs';
import { scoreGoldCase } from '../lib/benchmark.mjs';
import { ensureCaseDiff, localRepoPath } from '../lib/gold-fetch.mjs';
import { spawn } from 'node:child_process';
import { buildGraphIndex, createGraphifyClient } from '../lib/graphify.mjs';
import { envFlag } from '../lib/util.mjs';

const GOLD_DIR = process.env.REVIEW_GOLD_DIR
  ? resolve(process.cwd(), process.env.REVIEW_GOLD_DIR)
  : join(dirname(fileURLToPath(import.meta.url)), '../gold');
// Кэш диффов публичных кейсов: чужой код не вендорится, тянется по пину (RFC 03).
const CACHE_DIR = resolve(process.cwd(), process.env.REVIEW_GOLD_CACHE || '.ai-review/gold-public');
const onlyIdx = process.argv.indexOf('--case');
const only = onlyIdx >= 0 ? process.argv[onlyIdx + 1] : null;
const jsonlIdx = process.argv.indexOf('--jsonl');
const jsonlPath = jsonlIdx >= 0 ? resolve(process.cwd(), process.argv[jsonlIdx + 1]) : null;

let graphIndexCache;
/** Индекс graph.json (REVIEW_GOLD_GRAPH или GRAPH) — один раз на прогон; нет файла → null. */
async function goldGraphIndex(cfg) {
  if (graphIndexCache !== undefined) return graphIndexCache;
  const path = resolve(process.cwd(), process.env.REVIEW_GOLD_GRAPH || cfg.graphify.graph);
  try {
    graphIndexCache = buildGraphIndex(JSON.parse(await readFile(path, 'utf8')));
  } catch (e) {
    console.error(`[gold] карточки символов без графа (${path}: ${e.code || e.message})`);
    graphIndexCache = null;
  }
  return graphIndexCache;
}

/** Граф Graphify внутри дерева кейса (`.graphify/graphify-out/graph.json`), если его построили. */
async function treeGraphIndex(tree) {
  try {
    return buildGraphIndex(JSON.parse(await readFile(join(tree, '.graphify/graphify-out/graph.json'), 'utf8')));
  } catch { return null; }
}

/**
 * Граф Graphify в дереве кейса для настоящего blast-radius (REVIEW_GOLD_BLAST=1). Строится один
 * раз: дерево неизменно для пина кейса, повторный прогон берёт готовый граф. null — нет
 * GRAPHIFY_BIN или сборка упала: кейс идёт с заглушкой, как без флага, и об этом пишется в stderr.
 */
async function ensureTreeGraph(tree, id) {
  const out = join(tree, '.graphify/graphify-out');
  const graph = join(out, 'graph.json');
  try { await access(graph); return graph; } catch { /* построить */ }
  const bin = process.env.GRAPHIFY_BIN;
  if (!bin) {
    console.error(`[gold] ${id}: REVIEW_GOLD_BLAST без GRAPHIFY_BIN — blast заглушен`);
    return null;
  }
  const code = await new Promise((ok) => {
    const p = spawn(resolve(cwdAtStart, bin), ['update', tree, '--force'], {
      cwd: tree,
      env: { ...process.env, GRAPHIFY_OUT: out, GRAPHIFY_NO_TIPS: '1' },
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    let err = '';
    p.stderr.on('data', (d) => { err += d; });
    p.on('close', (c) => {
      if (c !== 0) console.error(`[gold] ${id}: graphify update ${c}: ${err.slice(-300)}`);
      ok(c);
    });
    p.on('error', (e) => { console.error(`[gold] ${id}: graphify: ${e.message}`); ok(1); });
  });
  if (code !== 0) return null;
  try { await access(graph); return graph; } catch { return null; }
}

const cwdAtStart = process.cwd();

function footerFindings(text) {
  const m = MARKERS.dataRe.exec(text);
  if (!m) return [];
  try { return JSON.parse(m[1].trim()).findings || []; } catch { return []; }
}

/**
 * Дерево проекта на коммите кейса — для кейсов из своих репозиториев (`source.local`).
 * Без него пайплайн читает файлы PR из каталога ai-reviewer: окон контекста нет, карточкам
 * нечего читать, и прогон меряет не тот вход, что получил бы живой PR (аудит RFC 14, 0.3).
 * `git archive`, а не `git worktree add`: в клонах пользователя не остаётся записей о worktree.
 * REVIEW_GOLD_CHECKOUT=0 — по-старому, без дерева.
 */
async function caseTree(meta) {
  if (!meta?.source?.local || process.env.REVIEW_GOLD_CHECKOUT === '0') return null;
  const repo = localRepoPath(meta);
  const tree = join(CACHE_DIR, 'trees', `${meta.id}.${String(meta.source.sha).slice(0, 12)}`);
  const done = join(tree, '.gold-tree-ok');
  try { await access(done); return tree; } catch { /* развернуть */ }
  await mkdir(tree, { recursive: true });
  await new Promise((ok, fail) => {
    const archive = spawn('git', ['-C', repo, 'archive', '--format=tar', meta.source.sha]);
    const tar = spawn('tar', ['-x', '-C', tree]);
    archive.stdout.pipe(tar.stdin);
    let err = '';
    archive.stderr.on('data', (d) => { err += d; });
    tar.on('close', (code) => (code === 0 ? ok() : fail(new Error(`tar ${code}: ${err.slice(0, 200)}`))));
    archive.on('error', fail);
    tar.on('error', fail);
  });
  await writeFile(done, `${meta.source.sha}\n`, 'utf8');
  return tree;
}

async function runCase(id) {
  const dir = join(GOLD_DIR, id);
  const meta = JSON.parse(await readFile(join(dir, 'meta.json'), 'utf8'));
  const diff = await ensureCaseDiff(dir, meta, { cacheDir: CACHE_DIR });
  const tree = await caseTree(meta);
  const env = {
    ...process.env,
    // REVIEW_GOLD_OBSERVABILITY_DIR — JSONL прогона (токены, plan.symbolCards) для сравнения
    // конфигураций; по умолчанию выключено, как и было.
    ...(process.env.REVIEW_GOLD_OBSERVABILITY_DIR
      ? { REVIEW_OBSERVABILITY: '1', REVIEW_OBSERVABILITY_DIR: resolve(process.cwd(), process.env.REVIEW_GOLD_OBSERVABILITY_DIR) }
      : { REVIEW_OBSERVABILITY: '0' }),
    REVIEW_EXPORT: '0',
    REVIEW_AUTO_TRIAGE: '0',
  };
  const cfg = loadReviewConfig(env, { dryRun: true, diffFile: diff.path, repo: '', prNumber: 0 });
  // Граф для карточек: у кейса со своим деревом — только граф внутри дерева (если его там
  // построили), общий REVIEW_GOLD_GRAPH относится к другому коду.
  const graphIndex = !cfg.limits.symbolCards ? null
    : tree ? await treeGraphIndex(tree) : await goldGraphIndex(cfg);
  // Настоящий blast-radius — только у кейса со своим деревом: без дерева граф был бы чужим кодом.
  let graphify = { buildBlastRadius: async () => ({ blocks: [], metrics: null, graphIndex }) };
  let blastLive = false;
  if (tree && envFlag('REVIEW_GOLD_BLAST', false, process.env)) {
    const graph = await ensureTreeGraph(tree, meta.id);
    if (graph) {
      const g = { ...cfg.graphify, bin: resolve(cwdAtStart, process.env.GRAPHIFY_BIN), graph, report: join(dirname(graph), 'GRAPH_REPORT.md') };
      graphify = createGraphifyClient({ ...cfg, graphify: g });
      blastLive = true;
    }
  }
  const captured = [];
  const origLog = console.log;
  console.log = (...a) => { captured.push(a.join(' ')); };
  const t0 = Date.now();
  const cwd0 = process.cwd();
  let code;
  try {
    if (tree) process.chdir(tree);
    const pipeline = createReviewPipeline(cfg, {
      // Blast-radius заглушен (кроме REVIEW_GOLD_BLAST), но при карточках символов (RFC 14)
      // индекс графа нужен — без него сравнение REVIEW_SYMBOL_CARDS=0/1 дало бы два одинаковых прогона.
      graphify,
      die: (m) => { throw new Error(`die: ${m}`); },
    });
    code = await pipeline.run();
  } finally {
    console.log = origLog;
    if (tree) process.chdir(cwd0);
  }
  return {
    meta,
    code,
    findings: footerFindings(captured.join('\n')),
    wallMs: Date.now() - t0,
    diffSource: diff.source,
    tree: Boolean(tree),
    blastLive,
  };
}

if (!process.env.LLM_API_KEY && !process.env.OLLAMA_API_KEY) {
  console.error('[gold] нет LLM_API_KEY / OLLAMA_API_KEY');
  process.exit(1);
}

const ids = (await readdir(GOLD_DIR, { withFileTypes: true }))
  .filter((e) => e.isDirectory())
  .map((e) => e.name)
  .filter((id) => !only || id === only)
  .sort();
if (!ids.length) {
  console.error(`[gold] кейсы не найдены${only ? `: ${only}` : ''}`);
  process.exit(1);
}

let failed = 0;
// Записи прогона в формате, который читает `scoreFromRecord` в pr-review-benchmark:
// без этого раннер и скорер остаются несвязанными половинами замера.
const records = [];
for (const id of ids) {
  process.stdout.write(`[gold] ${id} … `);
  try {
    const { meta, code, findings, wallMs, diffSource, blastLive } = await runCase(id);
    const wouldBlock = code === 1;
    const score = scoreGoldCase(meta, { wouldBlock, findings });
    const errors = score.precisionHints;
    const stat = errors.length ? '❌' : '✅';
    console.log(`${stat} wouldBlock=${code === 1} findings=${findings.length} wall=${(wallMs / 1000).toFixed(0)}с${blastLive ? " blast=live" : ""}`);
    records.push({ id, meta, findings, wouldBlock, code, wallMs, diffSource, blastLive });
    for (const e of errors) {
      console.log(`   ✗ ${e}`);
      failed++;
    }
  } catch (e) {
    console.log(`💥 ${e.message}`);
    records.push({ id, error: e.message });
    failed++;
  }
}
if (jsonlPath) {
  // Каталог создаём здесь: прогон стоит десятки минут, и терять его на отсутствующем
  // `.ai-review/` в самом конце — недопустимо.
  await mkdir(dirname(jsonlPath), { recursive: true });
  await writeFile(jsonlPath, `${records.map((r) => JSON.stringify(r)).join('\n')}\n`, 'utf8');
  console.log(`[gold] записи прогона → ${jsonlPath}`);
}
console.log(failed ? `⛔ gold: ${failed} несовпадение(й)` : '✅ gold: все кейсы совпали с разметкой');
process.exit(failed ? 1 : 0);
