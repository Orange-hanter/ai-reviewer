#!/usr/bin/env node
// Shadow eval ПОЛНОГО pipeline (с LLM) на gold-диффах с разметкой TP/FP (R-eval, backlog P0-3).
// В отличие от eval.mjs (fixtures, post-processing без LLM) — здесь вызываются модели.
// Запуск: OLLAMA_API_KEY=… npx pr-review-gold [--case <id>] [--jsonl out.jsonl]
// Каталог набора — REVIEW_GOLD_DIR; диффы публичных кейсов тянутся по пину в REVIEW_GOLD_CACHE.
// Blast-radius в gold-прогонах заглушен (детерминизм; на находки влияет слабо).
import { readdir, readFile, writeFile, mkdir, access } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadReviewConfig } from '../lib/config.mjs';
import { createReviewPipeline } from '../pipeline.mjs';
import { MARKERS } from '../lib/constants.mjs';
import { scoreGoldCase } from '../lib/benchmark.mjs';
import { ensureCaseDiff, localRepoPath } from '../lib/gold-fetch.mjs';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { buildGraphIndex } from '../lib/graphify.mjs';
import { PROFILE_FILENAMES, resetProfile } from '../lib/profile.mjs';

const GOLD_DIR = process.env.REVIEW_GOLD_DIR
  ? resolve(process.cwd(), process.env.REVIEW_GOLD_DIR)
  : join(dirname(fileURLToPath(import.meta.url)), '../gold');
// Кэш диффов публичных кейсов: чужой код не вендорится, тянется по пину (RFC 03).
const CACHE_DIR = resolve(process.cwd(), process.env.REVIEW_GOLD_CACHE || '.ai-review/gold-public');
const onlyIdx = process.argv.indexOf('--case');
const only = onlyIdx >= 0 ? process.argv[onlyIdx + 1] : null;
const jsonlIdx = process.argv.indexOf('--jsonl');
const jsonlPath = jsonlIdx >= 0 ? resolve(process.cwd(), process.argv[jsonlIdx + 1]) : null;
const tagIdx = process.argv.indexOf('--tag');
const onlyTag = tagIdx >= 0 ? process.argv[tagIdx + 1] : null;

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
  // Вне репозитория ai-reviewer: внутри gitignored `.ai-review/` graphify наследует чужой
  // .gitignore и пропускает почти весь код (граф в 44 узла вместо 8 724).
  const root = resolve(process.env.REVIEW_GOLD_TREES || join(tmpdir(), 'ai-reviewer-gold-trees'));
  const tree = join(root, `${meta.id}.${String(meta.source.sha).slice(0, 12)}`);
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

/**
 * Граф Graphify в дереве кейса: `REVIEW_GOLD_GRAPHIFY=<бинарь graphify>` строит его один раз
 * (`graphify update . --force`, только AST, без LLM) — как шаг CI потребителя на чекауте PR.
 * Без графа карточек символов нет, и сравнение REVIEW_SYMBOL_CARDS=0/1 вырождается.
 */
async function ensureTreeGraph(tree) {
  const bin = process.env.REVIEW_GOLD_GRAPHIFY;
  const out = join(tree, '.graphify/graphify-out');
  if (!bin) return;
  try { await access(join(out, 'graph.json')); return; } catch { /* построить */ }
  await new Promise((ok, fail) => {
    const p = spawn(bin, ['update', '.', '--force'], {
      cwd: tree,
      env: { ...process.env, GRAPHIFY_OUT: out, GRAPHIFY_NO_TIPS: '1', GRAPHIFY_MAX_WORKERS: '4' },
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    let err = '';
    p.stderr.on('data', (d) => { err += d; });
    p.on('close', (code) => (code === 0 ? ok() : fail(new Error(`graphify ${code}: ${err.slice(-200)}`))));
    p.on('error', fail);
  });
}

/**
 * Профиль проекта кейса — как в его CI: `.ai-reviewer.json` в корне или
 * `ops/ai-review/profile.json`. Нет файла — дефолтный профиль, а не профиль прошлого кейса:
 * профиль process-wide, и иначе кейсы разных проектов ревьюились бы с чужими правилами.
 */
async function treeProfile(tree) {
  for (const name of [...PROFILE_FILENAMES, 'ops/ai-review/profile.json']) {
    try { await access(join(tree, name)); return join(tree, name); } catch { /* следующее */ }
  }
  resetProfile();
  return null;
}

async function runCase(id) {
  const dir = join(GOLD_DIR, id);
  const meta = JSON.parse(await readFile(join(dir, 'meta.json'), 'utf8'));
  const diff = await ensureCaseDiff(dir, meta, { cacheDir: CACHE_DIR });
  const tree = await caseTree(meta);
  if (tree) await ensureTreeGraph(tree);
  const profile = tree ? await treeProfile(tree) : null;
  const env = {
    ...process.env,
    ...(tree ? { REVIEW_PROFILE: profile || '' } : {}),
    // REVIEW_GOLD_OBSERVABILITY_DIR — JSONL прогона (токены, plan.symbolCards) для сравнения
    // конфигураций; по умолчанию выключено, как и было.
    ...(process.env.REVIEW_GOLD_OBSERVABILITY_DIR
      ? { REVIEW_OBSERVABILITY: '1', REVIEW_OBSERVABILITY_DIR: resolve(process.cwd(), process.env.REVIEW_GOLD_OBSERVABILITY_DIR) }
      : { REVIEW_OBSERVABILITY: '0' }),
    REVIEW_EXPORT: '0',
    REVIEW_AUTO_TRIAGE: '0',
  };
  const captured = [];
  const origLog = console.log;
  const t0 = Date.now();
  const cwd0 = process.cwd();
  let code;
  try {
    // В дерево — до конфига: автопоиск профиля и относительные пути профиля идут от cwd,
    // а в корне ai-reviewer лежит свой .ai-reviewer.json.
    if (tree) process.chdir(tree);
    const cfg = loadReviewConfig(env, { dryRun: true, diffFile: diff.path, repo: '', prNumber: 0 });
    // Граф для карточек: у кейса со своим деревом — только граф внутри дерева (если его там
    // построили), общий REVIEW_GOLD_GRAPH относится к другому коду.
    const graphIndex = !cfg.limits.symbolCards ? null
      : tree ? await treeGraphIndex(tree) : await goldGraphIndex(cfg);
    console.log = (...a) => { captured.push(a.join(' ')); };
    const pipeline = createReviewPipeline(cfg, {
      // Blast-radius заглушен, но при карточках символов (RFC 14) индекс графа нужен — без
      // него сравнение REVIEW_SYMBOL_CARDS=0/1 дало бы два одинаковых прогона.
      graphify: { buildBlastRadius: async () => ({ blocks: [], metrics: null, graphIndex }) },
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
    profile: profile ? profile.slice(tree.length + 1) : null,
    findings: footerFindings(captured.join('\n')),
    wallMs: Date.now() - t0,
    diffSource: diff.source,
    tree: Boolean(tree),
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
// --tag contract — подмножество кейсов по тегу разметки (meta.tags).
if (onlyTag) {
  const tagged = [];
  for (const id of ids) {
    const meta = JSON.parse(await readFile(join(GOLD_DIR, id, 'meta.json'), 'utf8').catch(() => '{}'));
    if (meta.tags?.includes(onlyTag)) tagged.push(id);
  }
  ids.splice(0, ids.length, ...tagged);
}
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
    const { meta, code, findings, wallMs, diffSource, profile } = await runCase(id);
    const wouldBlock = code === 1;
    const score = scoreGoldCase(meta, { wouldBlock, findings });
    const errors = score.precisionHints;
    const stat = errors.length ? '❌' : '✅';
    console.log(`${stat} wouldBlock=${code === 1} findings=${findings.length} wall=${(wallMs / 1000).toFixed(0)}с`);
    records.push({ id, meta, findings, wouldBlock, code, wallMs, diffSource, profile });
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
