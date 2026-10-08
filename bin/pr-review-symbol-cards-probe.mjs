#!/usr/bin/env node
// Офлайн-пробник карточек символов (RFC 14, аудит §2): какой набор карточек получила бы модель
// на реальной истории репозитория — БЕЗ единого вызова LLM. Тем же кодом, что пайплайн:
// parseDiff → секции → окна ±margin → buildSymbolCards.
//
// Запуск (из корня репозитория-потребителя, где лежит граф Graphify):
//   node bin/pr-review-symbol-cards-probe.mjs                       # 40 последних коммитов
//   node bin/pr-review-symbol-cards-probe.mjs --repo ../app --graph ../app/.graphify/graphify-out/graph.json
//   node bin/pr-review-symbol-cards-probe.mjs --commits 100 --sample 30
//   node bin/pr-review-symbol-cards-probe.mjs --json > probe.json
//
// Что меряется: сколько вызванных имён становится карточками и почему остальные отсеяны;
// доля карточек, найденных через импорт вызывающего файла; доля карточек без сигнатуры
// (заголовок без `(` у функции — `export const X =`). Выборка карточек — для ручной разметки
// «вводит ли в заблуждение»: эту долю автоматически не посчитать.
//
// Оговорка: файлы коммита читаются в версии коммита, остальные — из ТЕКУЩЕГО дерева (граф
// построен по нему). Чем старше коммит, тем больше карточек отсеется как `stale_graph` /
// `not_a_declaration`; для оценки точности это консервативно (отсев, а не ложные карточки).
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

import { parseDiff } from '../lib/diff.mjs';
import { extractFileDiffSections } from '../lib/router.mjs';
import { buildGraphIndex } from '../lib/graphify.mjs';
import { windowChunkFileContents } from '../lib/context-window.mjs';
import { buildSymbolCards, createLineReader, isSymbolCardPath } from '../lib/symbol-cards.mjs';

function arg(name, def) {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : def;
}

const repo = resolve(arg('--repo', process.cwd()));
const graphPath = resolve(repo, arg('--graph', '.graphify/graphify-out/graph.json'));
const commits = Number(arg('--commits', 40));
const sampleSize = Number(arg('--sample', 20));
const margin = Number(arg('--margin', 40));
const asJson = process.argv.includes('--json');

const git = (...a) => execFileSync('git', ['-C', repo, ...a], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });

let graph;
try {
  graph = JSON.parse(readFileSync(graphPath, 'utf8'));
} catch (e) {
  console.error(`[probe] нет графа ${graphPath}: ${e.code || e.message}`);
  process.exit(1);
}
const graphIndex = buildGraphIndex(graph);
const readLines = createLineReader(repo);

const shas = git('log', '--no-merges', `-n${commits}`, '--format=%H').trim().split('\n').filter(Boolean);
const totals = { commits: shas.length, files: 0, candidates: 0, resolved: 0, cards: 0, via_import: 0 };
const skip = {};
const allCards = [];
for (const sha of shas) {
  const diff = git('show', '--format=', '--unified=3', '--no-color', sha);
  const { files, annotated } = parseDiff(diff);
  const paths = [...files.keys()].filter(isSymbolCardPath);
  if (!paths.length) continue;
  totals.files += paths.length;
  const sections = extractFileDiffSections(annotated);
  // Файлы коммита — в ЕГО версии (как дерево PR в CI): номера строк диффа совпадают с файлом,
  // маска литералов и импорты верны. Остальное — из текущего дерева, согласованного с графом.
  const inCommit = new Set(files.keys());
  const atSha = new Map();
  const readAt = (p) => {
    if (!inCommit.has(p)) return readLines(p);
    if (!atSha.has(p)) {
      try { atSha.set(p, git('show', `${sha}:${p}`).split('\n')); } catch { atSha.set(p, null); }
    }
    return atSha.get(p);
  };
  // контекст — как в пайплайне: окна вокруг изменённых строк по текущему тексту файла
  const contentMap = new Map();
  for (const p of paths) {
    const lines = readAt(p);
    if (lines) contentMap.set(p, `### ${p}\n\`\`\`\n${lines.join('\n')}\n\`\`\``);
  }
  const windows = windowChunkFileContents(contentMap, files, { margin, minChars: 8000 }).map;
  const r = buildSymbolCards(paths, { graphIndex, sections, windows, readLines: readAt, root: repo });
  for (const k of ['candidates', 'resolved', 'cards', 'via_import']) totals[k] += r.metrics[k] || 0;
  for (const [k, v] of Object.entries(r.metrics.skip_reasons)) skip[k] = (skip[k] || 0) + v;
  for (const [caller, cards] of r.byPath) for (const c of cards) allCards.push({ sha: sha.slice(0, 8), caller, ...c });
}

// Карточка «без сигнатуры»: функция/константа, в заголовке которой нет ни `(`, ни `class`.
const noSignature = allCards.filter((c) => !/[(]/.test(c.declaration) && !/(?:^|\s)class\s/.test(c.declaration));
// Детерминированная выборка: равномерно по списку, без случайности — повторяемо.
const step = Math.max(1, Math.floor(allCards.length / Math.max(1, sampleSize)));
const sample = allCards.filter((_, i) => i % step === 0).slice(0, sampleSize);

const report = {
  repo,
  graph: graphPath,
  totals,
  share: {
    cards_per_candidate: totals.candidates ? +(totals.cards / totals.candidates).toFixed(3) : 0,
    via_import: totals.cards ? +(totals.via_import / totals.cards).toFixed(3) : 0,
    no_signature: allCards.length ? +(noSignature.length / allCards.length).toFixed(3) : 0,
  },
  skip_reasons: Object.fromEntries(Object.entries(skip).sort((a, b) => b[1] - a[1])),
  sample: sample.map((c) => ({ sha: c.sha, caller: c.caller, name: c.name, at: `${c.path}:L${c.line}${c.endLine && c.endLine !== c.line ? `–L${c.endLine}` : ''}`, declaration: c.declaration })),
};

if (asJson) {
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
} else {
  console.log(`[probe] ${repo} · граф ${graphPath}`);
  console.log(`коммитов ${totals.commits} · JS/TS-файлов ${totals.files} · кандидатов ${totals.candidates} → карточек ${totals.cards} (${(report.share.cards_per_candidate * 100).toFixed(1)}%)`);
  console.log(`через импорт: ${(report.share.via_import * 100).toFixed(1)}% · без сигнатуры: ${(report.share.no_signature * 100).toFixed(1)}%`);
  console.log(`отсев: ${Object.entries(report.skip_reasons).map(([k, v]) => `${k} ${v}`).join(' · ')}`);
  console.log(`\nвыборка для ручной разметки (${report.sample.length}):`);
  for (const c of report.sample) console.log(`  ${c.sha} ${c.caller} → ${c.name} @ ${c.at}\n      ${c.declaration.split('\n').join('\n      ')}`);
}

