#!/usr/bin/env node
/**
 * Добыча gold-кейсов из ИСТОРИИ СВОИХ репозиториев (локальный клон, без API хостинга).
 *
 * Публичный сборщик (`pr-review-gold-harvest.mjs`) ищет фиксы, которые сами ссылаются на
 * PR-виновника фразой «regression introduced in #N». В своих репозиториях такие фразы редки,
 * зато есть conventional-коммиты и `git blame`. Метод — SZZ по локальной истории:
 *
 *   1. единица = коммит первого родителя интеграционной ветки (merge PR/MR или squash);
 *   2. фикс = единица с заголовком `fix…` (или ветка `fix/…` у merge);
 *   3. строки, которые фикс удалил, — `git blame` на родителе фикса → коммит-виновник → его
 *      единица (PR);
 *   4. локация дефекта в диффе виновника — по ТЕКСТУ удалённой строки среди добавленных
 *      виновником (как у публичного сборщика: дрейф файла между PR разметку не сдвигает).
 *
 * Clean-контроль — давние единицы, чьи строки ни один фикс не тронул.
 * Метка `contract` — фикс поменял вызов функции, объявленной в ДРУГОМ файле: класс, ради
 * которого делались карточки символов (RFC 14).
 *
 * Код в репозиторий ai-reviewer не попадает: кейс — только `meta.json` с пином
 * `source.local` (имя репо) + `sha`/`base`; дифф строится из локального клона
 * (`REVIEW_GOLD_REPOS="имя=путь,…"`, см. lib/gold-fetch.mjs).
 *
 * Запуск:
 *   node bin/pr-review-gold-harvest-local.mjs --repo ../Orakul --name orakul --branch origin/main --since 2026-06-22
 *   … --emit gold-private        # записать кандидатов
 *
 * Скрипт готовит кандидатов; отбор и чтение кейса глазами — ручной шаг поверх.
 */
import { execFileSync } from 'node:child_process';
import { mkdir, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';

const FLAGS = new Set(['--repo', '--name', '--branch', '--since', '--emit', '--clean', '--tp-max']);
// Неизвестный флаг — ошибка: «--emit gold-private» одним аргументом иначе молча даёт прогон без записи.
for (let i = 2; i < process.argv.length; i += 2) {
  if (!FLAGS.has(process.argv[i])) {
    console.error(`неизвестный аргумент «${process.argv[i]}»; флаги: ${[...FLAGS].join(' ')}`);
    process.exit(2);
  }
}

function arg(name, def) {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : def;
}
const REPO = resolve(arg('--repo', process.cwd()));
const NAME = arg('--name', 'repo');
const BRANCH = arg('--branch', 'origin/main');
const SINCE = arg('--since', '2026-01-01');
const EMIT = arg('--emit', null);
const CLEAN_WANT = Number(arg('--clean', 4));
const TP_MAX = Number(arg('--tp-max', 25));

// Пороги — как у публичного сборщика (RFC 03, «Оценка»): кейс влезает в прогон, дефект локализован.
const LIMITS = { buggyLines: 800, buggyFiles: 15, fixRemoved: 150, maxAgeDays: 120 };
const MIN_SIGNIFICANT_LINE = 12;
const IMPORT_LINE = /^\s*(?:import\b|from\s|#include\b|use\s|require\s*\(|export\s+\*\s+from)/;
const TEST_PATH = /(?:^|\/)(?:tests?|__tests__|spec|e2e|fixtures?|__snapshots__)\/|[._-](?:test|spec)\.[a-z]+$|_test\.go$/i;
const CODE_PATH = /\.(?:[cm]?[jt]sx?|vue|svelte|py|go|rs|rb|java|kt|sql|sh|ya?ml)$/i;
const RANGE_GAP = 2;
const CLEAN_MIN_AGE_DAYS = 45;
const CLEAN_MIN_LINES = 30;

const git = (...args) => execFileSync('git', ['-C', REPO, ...args], { encoding: 'utf8', maxBuffer: 512 * 1024 * 1024 });

// ── единицы истории ──
function loadUnits() {
  const raw = git('log', '--first-parent', `--since=${SINCE}`, '--format=%H%x00%P%x00%cI%x00%s%x00%b%x1e', BRANCH);
  const units = [];
  for (const rec of raw.split('\x1e')) {
    const [sha, parents, date, subject, body = ''] = rec.replace(/^\n/, '').split('\x00');
    if (!sha) continue;
    const ps = parents.split(' ').filter(Boolean);
    if (!ps.length) continue;
    const isMerge = ps.length > 1;
    let title = subject;
    let ref = null;
    let branch = '';
    const gh = /^Merge pull request #(\d+) from \S+?\/(\S+)/.exec(subject);
    const gl = /^Merge branch '([^']+)' into '/.exec(subject);
    if (gh) { ref = `#${gh[1]}`; branch = gh[2]; title = body.split('\n').find((l) => l.trim()) || subject; }
    else if (gl) {
      branch = gl[1];
      title = body.split('\n').find((l) => l.trim() && !/^See merge request/.test(l)) || subject;
      const mr = /See merge request \S+!(\d+)/.exec(body);
      if (mr) ref = `!${mr[1]}`;
    } else {
      const sq = /\(#(\d+)\)\s*$/.exec(subject);
      if (sq) ref = `#${sq[1]}`;
    }
    units.push({ sha, base: ps[0], isMerge, date: new Date(date), subject, title: title.trim(), ref, branch });
  }
  units.reverse(); // старые → новые
  return units;
}

function isFix(u) {
  return /^(?:fix|hotfix)(?:\(|:|!)/i.test(u.title) || /^(?:fix|hotfix|bugfix)\//i.test(u.branch);
}

// ── парсинг патчей ──
function parsePatch(patch) {
  // файл → { added: [{line,text}], removed: [{line,text}] }
  const files = new Map();
  let cur = null; let oldLine = 0; let newLine = 0;
  for (const raw of patch.split('\n')) {
    if (raw.startsWith('diff --git ')) { cur = null; continue; }
    if (raw.startsWith('+++ ')) {
      const m = /^\+\+\+ b\/(.+)$/.exec(raw);
      cur = m ? m[1] : null;
      if (cur && !files.has(cur)) files.set(cur, { added: [], removed: [] });
      continue;
    }
    if (raw.startsWith('--- ')) {
      const m = /^--- a\/(.+)$/.exec(raw);
      if (m) { // удалённый файл: +++ /dev/null — путь берём из ---
        cur = m[1];
        if (!files.has(cur)) files.set(cur, { added: [], removed: [] });
      }
      continue;
    }
    const h = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
    if (h) { oldLine = Number(h[1]); newLine = Number(h[2]); continue; }
    if (!cur) continue;
    const rec = files.get(cur);
    if (raw.startsWith('+')) { rec.added.push({ line: newLine, text: raw.slice(1) }); newLine++; }
    else if (raw.startsWith('-')) { rec.removed.push({ line: oldLine, text: raw.slice(1) }); oldLine++; }
    else if (raw.startsWith('\\')) { /* no newline */ }
    else { oldLine++; newLine++; }
  }
  return files;
}

const diffCache = new Map();
function unitDiff(u, context = 3) {
  const key = `${u.sha}:${context}`;
  if (!diffCache.has(key)) diffCache.set(key, parsePatch(git('diff', `-U${context}`, '--no-color', '--no-renames', u.base, u.sha)));
  return diffCache.get(key);
}

function unitSize(u) {
  const files = unitDiff(u);
  let lines = 0;
  for (const f of files.values()) lines += f.added.length + f.removed.length;
  return { lines, files: files.size };
}

const significant = (t) => {
  const s = t.trim();
  return s.length >= MIN_SIGNIFICANT_LINE && !IMPORT_LINE.test(s);
};

function toRanges(lines) {
  const sorted = [...new Set(lines)].sort((a, b) => a - b);
  const ranges = [];
  for (const n of sorted) {
    const last = ranges[ranges.length - 1];
    if (last && n - last[1] <= RANGE_GAP) last[1] = n;
    else ranges.push([n, n]);
  }
  return ranges;
}

// ── blame удалённых фиксом строк → коммит → единица ──
function blameLines(rev, file, lines) {
  const out = new Map(); // line → commit
  const ranges = toRanges(lines);
  for (const [a, b] of ranges) {
    let txt;
    try { txt = git('blame', '--porcelain', '-L', `${a},${b}`, rev, '--', file); } catch { continue; }
    for (const m of txt.matchAll(/^([0-9a-f]{40}) \d+ (\d+)(?: \d+)?$/gm)) out.set(Number(m[2]), m[1]);
  }
  return out;
}

// ── метка contract: фикс поменял вызов функции, объявленной в другом файле ──
const CALLEE_RE = /(?<![\w$.])([A-Za-z_$][\w$]{2,})\s*\(/g;
const KEYWORDS = new Set('if for while switch catch return typeof function await async new super import export require'.split(' '));
function contractCallees(fixUnit, file, rec) {
  const removedCalls = new Map();
  for (const r of rec.removed) for (const m of r.text.matchAll(CALLEE_RE)) if (!KEYWORDS.has(m[1])) removedCalls.set(m[1], r.text.trim());
  const out = [];
  for (const a of rec.added) {
    for (const m of a.text.matchAll(CALLEE_RE)) {
      const name = m[1];
      if (!removedCalls.has(name) || removedCalls.get(name) === a.text.trim()) continue;
      let declFiles = [];
      try {
        // POSIX ERE: `git grep -E` не знает `\s` и `\b` — с ними поиск молча ничего не находит
        const W = '([^[:alnum:]_$]|$)';
        declFiles = git('grep', '-lE',
          `((function|class|def)[[:space:]]+${name}${W}|(const|let|var)[[:space:]]+${name}[[:space:]]*=|^[[:space:]]*(export[[:space:]]+)?(async[[:space:]]+)?${name}[[:space:]]*\\([^)]*\\)[[:space:]]*(:[^{]*)?\\{)`,
          fixUnit.base, '--')
          .split('\n').filter(Boolean).map((l) => l.replace(/^[^:]+:/, ''));
      } catch { declFiles = []; }
      // Единственное объявление в другом файле И относительный импорт в файле фикса: иначе
      // ловятся общие имена (`path`, `type`, `handle`), объявленные десятком модулей.
      if (declFiles.length !== 1 || declFiles[0] === file) continue;
      if (importsRelatively(fixUnit.base, file, name)) out.push(name);
    }
  }
  return [...new Set(out)];
}

const fileTextCache = new Map();
/** Файл на ревизии импортирует `name` из относительного модуля (`./x`, `../y`) — JS/TS и Python. */
function importsRelatively(rev, file, name) {
  const key = `${rev}:${file}`;
  if (!fileTextCache.has(key)) {
    let text = '';
    try { text = git('show', key); } catch { text = ''; }
    fileTextCache.set(key, text);
  }
  const text = fileTextCache.get(key);
  const n = name.replace(/[$]/g, '\\$');
  // JS/TS: относительный путь или алиас корня проекта (`@/`, `~/`, `src/`)
  const js = new RegExp(`import[^;]*\\b${n}\\b[^;]*from\\s*['"](?:\\.{1,2}/|@/|~/|src/)`, 'm');
  if (js.test(text)) return true;
  // Python: относительный импорт или абсолютный из пакета верхнего уровня самого репозитория
  for (const m of text.matchAll(new RegExp(`^from\\s+(\\.+[\\w.]*|[\\w.]+)\\s+import\\s+[^\\n]*\\b${n}\\b`, 'gm'))) {
    if (m[1].startsWith('.') || topLevelPackages(rev).has(m[1].split('.')[0])) return true;
  }
  return false;
}

const topLevelCache = new Map();
/** Каталоги верхнего уровня репозитория — «свои» Python-пакеты для абсолютных импортов. */
function topLevelPackages(rev) {
  if (!topLevelCache.has(rev)) {
    let names = [];
    try { names = git('ls-tree', '-d', '--name-only', rev).split('\n').filter(Boolean); } catch { names = []; }
    topLevelCache.set(rev, new Set(names));
  }
  return topLevelCache.get(rev);
}

function slug(s, max = 40) {
  return String(s).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, max).replace(/-+$/, '');
}

// ── основной проход ──
const units = loadUnits();
const unitOfCommit = new Map();
for (let i = 0; i < units.length; i++) {
  const u = units[i];
  const commits = u.isMerge ? git('rev-list', `${u.base}..${u.sha}`).split('\n').filter(Boolean) : [u.sha];
  for (const c of commits) if (!unitOfCommit.has(c)) unitOfCommit.set(c, i);
}

const fixes = units.map((u, i) => ({ u, i })).filter(({ u }) => isFix(u));
const hitsByUnit = new Map(); // i → { locations: Map(file → Set(line)), fixes: [], contract: Set }
let fixSkipped = 0;
for (const { u: f, i: fi } of fixes) {
  const diff = unitDiff(f, 0);
  let removedCount = 0;
  for (const rec of diff.values()) removedCount += rec.removed.length;
  if (removedCount === 0 || removedCount > LIMITS.fixRemoved) { fixSkipped++; continue; }
  for (const [file, rec] of diff) {
    if (TEST_PATH.test(file) || !CODE_PATH.test(file)) continue;
    const sig = rec.removed.filter((r) => significant(r.text));
    if (!sig.length) continue;
    const blame = blameLines(f.base, file, sig.map((r) => r.line));
    const contract = contractCallees(f, file, rec);
    for (const r of sig) {
      const commit = blame.get(r.line);
      const ui = commit ? unitOfCommit.get(commit) : undefined;
      if (ui === undefined || ui === fi) continue;
      const culprit = units[ui];
      if ((f.date - culprit.date) / 86_400_000 > LIMITS.maxAgeDays) continue;
      // место в диффе виновника — по тексту
      const cdiff = unitDiff(culprit).get(file);
      const match = cdiff?.added.find((a) => a.text.trim() === r.text.trim());
      if (!match) continue;
      if (!hitsByUnit.has(ui)) hitsByUnit.set(ui, { locations: new Map(), fixes: new Map(), contract: new Set() });
      const h = hitsByUnit.get(ui);
      if (!h.locations.has(file)) h.locations.set(file, new Set());
      h.locations.get(file).add(match.line);
      h.fixes.set(f.sha, f);
      for (const c of contract) h.contract.add(c);
    }
  }
}

const tp = [];
for (const [ui, h] of hitsByUnit) {
  const u = units[ui];
  const size = unitSize(u);
  if (size.lines > LIMITS.buggyLines || size.files > LIMITS.buggyFiles) continue;
  const mustFindLocations = [...h.locations].map(([file, set]) => toRanges([...set]).map(([a, b]) => ({ file, lines: [a, b] }))).flat();
  tp.push({ u, size, mustFindLocations, fixes: [...h.fixes.values()], contract: [...h.contract] });
}
tp.sort((a, b) => (b.contract.length - a.contract.length) || (b.u.date - a.u.date));

const touched = new Set(hitsByUnit.keys());
const now = Date.now();
const cleanPool = units
  .map((u, i) => ({ u, i }))
  .filter(({ u, i }) => !touched.has(i) && !isFix(u) && (now - u.date) / 86_400_000 >= CLEAN_MIN_AGE_DAYS)
  .filter(({ u }) => !/^Revert\b/i.test(u.title))
  .map(({ u, i }) => ({ u, i, size: unitSize(u) }))
  .filter(({ u, size }) => size.lines >= CLEAN_MIN_LINES && size.lines <= LIMITS.buggyLines && size.files <= LIMITS.buggyFiles
    && [...unitDiff(u).keys()].some((p) => CODE_PATH.test(p) && !TEST_PATH.test(p)));
// равномерно по времени, детерминированно
const step = Math.max(1, Math.floor(cleanPool.length / Math.max(1, CLEAN_WANT)));
const clean = cleanPool.filter((_, k) => k % step === 0).slice(0, CLEAN_WANT);

console.log(`[harvest-local] ${NAME} ${BRANCH} since ${SINCE}: единиц ${units.length}, фиксов ${fixes.length} (пропущено по размеру ${fixSkipped})`);
console.log(`  tp-кандидатов ${tp.length} (из них contract ${tp.filter((t) => t.contract.length).length}), clean-пул ${cleanPool.length} → ${clean.length}`);
for (const t of tp.slice(0, TP_MAX)) {
  console.log(`  tp ${t.u.ref || t.u.sha.slice(0, 8)} ${t.u.date.toISOString().slice(0, 10)} ${t.size.lines}л/${t.size.files}ф loc=${t.mustFindLocations.length}${t.contract.length ? ` contract=${t.contract.join(',')}` : ''} · ${t.u.title.slice(0, 70)}`);
}
for (const c of clean) console.log(`  clean ${c.u.ref || c.u.sha.slice(0, 8)} ${c.u.date.toISOString().slice(0, 10)} ${c.size.lines}л · ${c.u.title.slice(0, 70)}`);

if (EMIT) {
  const outDir = resolve(EMIT);
  const write = async (id, meta) => {
    await mkdir(join(outDir, id), { recursive: true });
    await writeFile(join(outDir, id, 'meta.json'), `${JSON.stringify(meta, null, 2)}\n`, 'utf8');
  };
  const refSlug = (u) => (u.ref ? u.ref.replace(/[#!]/, '') : u.sha.slice(0, 8));
  for (const t of tp.slice(0, TP_MAX)) {
    const id = `${NAME}-${refSlug(t.u)}-${slug(t.u.title)}`;
    await write(id, {
      id,
      type: 'tp',
      tags: t.contract.length ? ['contract'] : [],
      description: `${t.u.ref || ''} «${t.u.title}» — дефект, который потом исправил ${t.fixes.map((f) => f.ref || f.sha.slice(0, 8)).join(', ')}`,
      source: {
        local: NAME,
        branch: BRANCH,
        sha: t.u.sha,
        base: t.u.base,
        ref: t.u.ref,
        fixes: t.fixes.map((f) => ({ sha: f.sha, ref: f.ref, title: f.title })),
        evidence: [
          `дифф виновника: ${t.size.lines} строк в ${t.size.files} файл(ах)`,
          'локации: строки, удалённые фиксом, найдены git blame на родителе фикса в этой единице и сопоставлены по тексту с добавленными ею',
          ...(t.contract.length ? [`фикс поменял вызов функций из другого файла: ${t.contract.join(', ')}`] : []),
        ],
      },
      expect: { mustFindLocations: t.mustFindLocations },
    });
  }
  for (const c of clean) {
    const id = `${NAME}-clean-${refSlug(c.u)}-${slug(c.u.title)}`;
    await write(id, {
      id,
      type: 'clean',
      description: `${c.u.ref || ''} «${c.u.title}» — ни одна строка не затронута фиксами ≥ ${CLEAN_MIN_AGE_DAYS} дней`,
      source: { local: NAME, branch: BRANCH, sha: c.u.sha, base: c.u.base, ref: c.u.ref },
      expect: { wouldBlock: false },
    });
  }
  console.log(`[harvest-local] записано: tp ${Math.min(tp.length, TP_MAX)}, clean ${clean.length} → ${outDir}`);
}
