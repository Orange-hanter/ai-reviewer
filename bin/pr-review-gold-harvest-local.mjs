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
 *
 * Режим `--blast` (RFC 21) вместо tp/clean ищет «PR сломал вызывающий код вне PR»: единица P
 * сменила сигнатуру S в файле A, а фикс ≤ 120 дней спустя поправил вызов S в файле B, которого
 * нет в P, причём строка вызова (git blame на родителе фикса) старше P. Вызов привязан к A по
 * импорту/получателю (`relatedCall`), а не по одному имени. Мету пишет только для пар, явно
 * отобранных глазами: `REVIEW_GOLD_BLAST_KEEP="<sha8 P>:<S>,…" … --blast --emit gold-private`.
 * `REVIEW_GOLD_BLAST_DEBUG=1` печатает отброшенные «не тот S» и «P велик».
 */
import { execFileSync } from 'node:child_process';
import { mkdir, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';

const FLAGS = new Set(['--repo', '--name', '--branch', '--since', '--emit', '--clean', '--tp-max']);
// Флаги без значения. `--blast` — отдельный режим (RFC 21): вместо tp/clean ищет PR, сменивший
// контракт функции, чей вызов ВНЕ PR потом исправил фикс.
const BOOL_FLAGS = new Set(['--blast']);
// Неизвестный флаг — ошибка: «--emit gold-private» одним аргументом иначе молча даёт прогон без записи.
for (let i = 2; i < process.argv.length; i += BOOL_FLAGS.has(process.argv[i]) ? 1 : 2) {
  if (!FLAGS.has(process.argv[i]) && !BOOL_FLAGS.has(process.argv[i])) {
    console.error(`неизвестный аргумент «${process.argv[i]}»; флаги: ${[...FLAGS, ...BOOL_FLAGS].join(' ')}`);
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
const BLAST = process.argv.includes('--blast');

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

// ── режим --blast (RFC 21): PR сменил контракт S в файле A, фикс потом поправил вызов S в B вне PR ──

// Объявление функции/метода с именем в группе 1 — по тексту строки, для JS/TS, Python, Rust, Go.
// Строка-вызов `foo(x);` под второй вариант не попадает: после закрывающей скобки там `;`/`,`/`)`.
const DECL_RES = [
  /\b(?:function\*?|def|fn)\s+([A-Za-z_$][\w$]*)\s*[(<]/,
  /\bfunc\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)\s*[([]/,
  /(?:\b(?:const|let|var)\s+|^\s*(?:(?:public|private|protected|readonly|static)\s+)+)([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:function\b|\([^)]*\)\s*(?::[^=]+)?=>|\($|[A-Za-z_$][\w$]*\s*=>)/,
  /^\s*(?:(?:export|default|public|private|protected|static|async|override|abstract|pub(?:\([\w:]+\))?)\s+)*(?!(?:if|for|while|switch|catch|return|function|new|await|super|this)\b)([A-Za-z_$][\w$]*)\s*(?:<[^>]*>)?\s*\((?:[^()]*\)\s*(?::\s*[^;,=]+)?\s*[{;]?\s*$|[^)]*$)/,
  // свойство-функция объекта/типа: `foo: async (a) => …`, `foo: (a: X) => Y;`
  /^\s*(?:readonly\s+)?([A-Za-z_$][\w$]*)\??\s*:\s*(?:async\s*)?\([^)]*(?:\)\s*(?::\s*[^=]+)?=>|$)/,
];
const BLAST_SKIP_NAMES = new Set('constructor render main init get set run handle call apply bind toString valueOf'.split(' '));

/** Объявления файла: имя → отсортированный список нормализованных сигнатур (с учётом многострочных параметров). */
function declsIn(text, file = '') {
  const lines = text.split('\n');
  const out = new Map();
  // форма «метод класса» (`foo(a) {`) — только у C-подобных; в Python/Go/Rust объявление всегда с ключевым словом
  const methodForm = !/\.(?:py|go|rs|rb|sh|sql|ya?ml)$/i.test(file);
  for (let i = 0; i < lines.length; i++) {
    let name = null; let isMethod = false;
    for (let k = 0; k < DECL_RES.length; k++) {
      if (k === 3 && !methodForm) continue;
      const m = DECL_RES[k].exec(lines[i]);
      if (m) { name = m[1]; isMethod = k === 3; break; }
    }
    if (!name || KEYWORDS.has(name) || BLAST_SKIP_NAMES.has(name)) continue;
    // сигнатура: от строки объявления до закрытия первой скобки параметров (не дальше 25 строк)
    let sig = ''; let depth = 0; let opened = false; let tail = ''; let endJ = i;
    for (let j = i; j < Math.min(lines.length, i + 25); j++) {
      const l = lines[j];
      sig += ` ${l.trim()}`;
      endJ = j;
      for (let c = 0; c < l.length; c++) {
        const ch = l[c];
        if (ch === '(') { depth++; opened = true; } else if (ch === ')') { depth--; if (opened && depth <= 0) { tail = l.slice(c + 1); break; } }
      }
      if (opened && depth <= 0) break;
    }
    // метод без ключевого слова отличаем от многострочного вызова по телу: после `)` — `{` (тип возврата допустим);
    // в .ts допустим и метод интерфейса/порта без тела: `)` + обязательный тип возврата + `;`
    const tsSig = /\.tsx?$/.test(file) && /^\s*:\s*[^;,=]+;\s*$/.test(tail);
    if (isMethod && !tsSig && !/^\s*(?::\s*[^;,=]+?)?\s*\{/.test(tail) && !(/^\s*(?::\s*[^;,=]+?)?\s*$/.test(tail) && /^\s*\{/.test(lines[endJ + 1] || ''))) continue;
    const norm = sig.replace(/\s+/g, ' ').replace(/[{:]\s*$/, '').replace(/,\s*\)/g, ')').trim();
    if (!out.has(name)) out.set(name, []);
    out.get(name).push(norm);
  }
  for (const v of out.values()) v.sort();
  return out;
}

const showCache = new Map();
/** Текст файла на ревизии; null — файла там нет. */
function showFile(rev, file) {
  const key = `${rev}:${file}`;
  if (!showCache.has(key)) {
    let text = null;
    // stderr глушим: «файла нет на ревизии» здесь — штатный ответ, а не ошибка
    try { text = execFileSync('git', ['-C', REPO, 'show', key], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, stdio: ['ignore', 'pipe', 'ignore'] }); } catch { text = null; }
    showCache.set(key, text);
  }
  return showCache.get(key);
}

const contractChangeCache = new Map();
/** Единица P: имя → [{file, before, after}] — функции, чья сигнатура в файле изменилась (есть и до, и после). */
function contractChanges(u) {
  if (contractChangeCache.has(u.sha)) return contractChangeCache.get(u.sha);
  const out = new Map();
  for (const [file, rec] of unitDiff(u, 0)) {
    if (TEST_PATH.test(file) || !CODE_PATH.test(file)) continue;
    const before = showFile(u.base, file);
    const after = showFile(u.sha, file);
    if (before == null || after == null) continue;
    const db = declsIn(before, file);
    const da = declsIn(after, file);
    for (const [name, sigsA] of da) {
      const sigsB = db.get(name);
      if (!sigsB || sigsB.join('\n') === sigsA.join('\n')) continue;
      if (!out.has(name)) out.set(name, []);
      out.get(name).push({ file, before: sigsB, after: sigsA });
    }
  }
  contractChangeCache.set(u.sha, out);
  return out;
}

const BLAST_CALL_RE = /(?<![\w$])([A-Za-z_$][\w$]{2,})\s*(?:<[^>()]*>)?\s*\(/g;

/**
 * Вызов `S` в B относится к объявлению в A, а не к одноимённой функции другого модуля:
 * `recv.S(` — получатель похож на имя файла/каталога A или на класс, объявленный в A
 * (`this.usersService` ↔ users.service.ts, `task_service` ↔ task_service.py);
 * голый `S(` — B импортирует S из модуля с именем файла A (Go — тот же пакет-каталог).
 */
function relatedCall(callText, s, fileA, fileB, revA, textB) {
  const norm = (x) => String(x).toLowerCase().replace(/[^a-z0-9]/g, '');
  const parts = fileA.split('/');
  const baseRaw = parts[parts.length - 1].replace(/\.[^.]+$/, '');
  const baseA = norm(baseRaw);
  const dirA = norm(parts[parts.length - 2] || '');
  const sx = s.replace(/[$]/g, '\\$');
  const recvM = new RegExp(`([A-Za-z_$][\\w$]*)\\s*(?:\\?\\.|\\.|::)\\s*${sx}\\s*(?:<[^>()]*>)?\\s*\\(`).exec(callText);
  if (recvM) {
    // получатель — псевдоним модуля A: `from pkg import task_service as tasks`, `import * as tg from './telegram.js'`
    const r0 = recvM[1];
    const aliasRes = [
      new RegExp(`^\\s*from\\s+[\\w.]+\\s+import\\s+(?:[^\\n]*,\\s*)?([\\w]+)\\s+as\\s+${r0}\\b`, 'm'),
      new RegExp(`^\\s*import\\s+([\\w.]+)\\s+as\\s+${r0}\\b`, 'm'),
      new RegExp(`import\\s+(?:\\*\\s+as\\s+)?${r0}\\s+from\\s*['"]([^'"]+)['"]`),
    ];
    for (const re of aliasRes) {
      const am = re.exec(textB);
      if (!am) continue;
      const seg = norm(am[1].replace(/\.(?:[cm]?[jt]sx?|vue|svelte)$/, '').split(/[/.]/).filter(Boolean).pop() || '');
      if (seg === baseA || (baseRaw === 'index' && seg === dirA)) return true;
    }
    const recv = norm(r0.replace(/^_+/, ''));
    if (recv.length < 3 || ['this', 'self', 'super'].includes(recv)) return false;
    if (recv === baseA || recv === dirA || (baseA.length >= 5 && (recv.includes(baseA) || baseA.includes(recv)))) return true;
    const textA = showFile(revA, fileA) || '';
    const classes = [...textA.matchAll(/\b(?:class|struct|interface)\s+([A-Za-z_]\w*)/g)].map((m) => norm(m[1]));
    if (classes.some((c) => c.length >= 5 && (c === recv || recv.endsWith(c) || c.endsWith(recv)))) return true;
    // DI через интерфейс: `private readonly devs: DevelopersUseCasePort` в B, `class X implements DevelopersUseCasePort` в A
    const typeM = new RegExp(`\\b${r0}\\s*[?!]?\\s*:\\s*([A-Za-z_]\\w*)`).exec(textB);
    if (typeM) {
      const t = norm(typeM[1]);
      const implemented = [...textA.matchAll(/\bimplements\s+([\w\s,<>]+?)\s*\{/g)]
        .flatMap((m) => m[1].split(',').map((x) => norm(x.replace(/<.*$/, ''))));
      if (classes.includes(t) || implemented.includes(t)) return true;
    }
    return false;
  }
  if (/\.go$/.test(fileA) && fileA.split('/').slice(0, -1).join('/') === fileB.split('/').slice(0, -1).join('/')) return true;
  const modOk = (mod) => {
    // `../shared/dishCost.js` → dishCost; `cod_doc.agent.retry` → retry
    const seg = norm(mod.replace(/\.(?:[cm]?[jt]sx?|vue|svelte)$/, '').split(/[/.]/).filter(Boolean).pop() || '');
    return seg === baseA || (baseRaw === 'index' && seg === dirA) || (/^(?:mod|__init__)$/.test(baseRaw) && seg === dirA);
  };
  for (const m of textB.matchAll(new RegExp(`import\\s*(?:type\\s*)?\\{[^}]*\\b${sx}\\b[^}]*\\}\\s*from\\s*['"]([^'"]+)['"]`, 'g'))) if (modOk(m[1])) return true;
  for (const m of textB.matchAll(new RegExp(`^\\s*from\\s+([\\w.]+)\\s+import\\s*(?:\\([^)]*?|[^\\n(]*?)\\b${sx}\\b`, 'gm'))) if (modOk(m[1])) return true;
  for (const m of textB.matchAll(new RegExp(`^\\s*use\\s+([\\w:]+)::\\{?[^;]*\\b${sx}\\b`, 'gm'))) if (modOk(m[1].replace(/::/g, '/'))) return true;
  return false;
}

/** Хунки фикса в нумерации СТАРОГО файла: файл → [[from, to]] (вставка без удалений — [a+1, a+1]). */
function fixHunksOld(u) {
  const out = new Map();
  let cur = null;
  for (const raw of git('diff', '-U0', '--no-color', '--no-renames', u.base, u.sha).split('\n')) {
    const fm = /^\+\+\+ b\/(.+)$/.exec(raw);
    if (fm) { cur = fm[1]; if (!out.has(cur)) out.set(cur, []); continue; }
    if (raw.startsWith('diff --git ')) { cur = null; continue; }
    const h = /^@@ -(\d+)(?:,(\d+))? \+/.exec(raw);
    if (h && cur) {
      const a = Number(h[1]); const n = h[2] === undefined ? 1 : Number(h[2]);
      out.get(cur).push(n === 0 ? [a + 1, a + 1] : [a, a + n - 1]);
    }
  }
  return out;
}

/**
 * Места вызова, которые фикс тронул, в нумерации файла на родителе фикса:
 *  - удалённая строка с вызовом `S(`, если `S(` есть и среди добавленных (вызов изменён, а не убран);
 *  - открытый многострочный вызов `S(` (до 15 строк выше хунка), внутри которого фикс поменял
 *    аргументы, — сама строка `S(` при этом может остаться нетронутой.
 */
function callSitesTouched(textBase, rec, hunks) {
  const lines = textBase.split('\n');
  const addedCalls = new Set();
  for (const a of rec.added) for (const m of a.text.matchAll(BLAST_CALL_RE)) addedCalls.add(m[1]);
  const okName = (n) => !KEYWORDS.has(n) && !BLAST_SKIP_NAMES.has(n);
  const sites = new Map(); // line → { line, text, names:Set }
  const add = (line, name) => {
    if (!sites.has(line)) sites.set(line, { line, text: lines[line - 1] ?? '', names: new Set() });
    sites.get(line).names.add(name);
  };
  for (const r of rec.removed) {
    if (DECL_RES.slice(0, 3).some((re) => re.test(r.text))) continue; // объявление, не вызов
    for (const m of r.text.matchAll(BLAST_CALL_RE)) if (okName(m[1]) && addedCalls.has(m[1])) add(r.line, m[1]);
  }
  for (const [from] of hunks) {
    // глубина скобок, открытых выше строки `from`: идём вверх и ищем вызов, чья скобка ещё не закрыта
    let depth = 0;
    for (let k = from - 1; k >= Math.max(1, from - 15); k--) {
      const l = lines[k - 1] ?? '';
      let found = null; let stop = false;
      for (let c = l.length - 1; c >= 0; c--) {
        if (l[c] === ')') depth--;
        else if (l[c] === '(') {
          depth++;
          if (depth > 0) {
            const m = /([A-Za-z_$][\w$]{2,})\s*(?:<[^>()]*>)?\s*$/.exec(l.slice(0, c));
            if (m && okName(m[1])) found = m[1];
            stop = true; // ближайшая незакрытая скобка: вызов или нет — дальше вверх не ищем
            break;
          }
        }
      }
      if (found && !DECL_RES.some((re) => re.test(l))) add(k, found);
      if (stop) break;
    }
  }
  return [...sites.values()].map((s) => ({ ...s, names: [...s.names] }));
}

async function harvestBlast() {
  const cands = new Map(); // pi → { locations: Map(file → Map(line → …)), fixes: Map, symbols: Map(S → {file, before, after}) }
  const funnel = { sites: 0, olderThanP: 0, outsideP: 0, sigChanged: 0, related: 0, onTreeP: 0 };
  let fixSkippedB = 0;
  for (const { u: f, i: fi } of fixes) {
    const diff = unitDiff(f, 0);
    let removedCount = 0; let addedCount = 0;
    for (const rec of diff.values()) { removedCount += rec.removed.length; addedCount += rec.added.length; }
    // фикс, только ДОБАВИВШИЙ аргумент в многострочный вызов, ничего не удаляет — здесь он тоже годится
    if (removedCount + addedCount === 0 || removedCount > LIMITS.fixRemoved) { fixSkippedB++; continue; }
    // единицы-кандидаты P: раньше фикса, не старше 120 дней
    const window = [];
    for (let pi = fi - 1; pi >= 0; pi--) {
      if ((f.date - units[pi].date) / 86_400_000 > LIMITS.maxAgeDays) break;
      window.push(pi);
    }
    const hunks = fixHunksOld(f);
    for (const [fileB, rec] of diff) {
      if (TEST_PATH.test(fileB) || !CODE_PATH.test(fileB)) continue;
      const textBase = showFile(f.base, fileB);
      if (textBase == null) continue;
      const sites = callSitesTouched(textBase, rec, hunks.get(fileB) || []);
      if (!sites.length) continue;
      const blame = blameLines(f.base, fileB, sites.map((c) => c.line));
      for (const { line: callLine, text: callText, names } of sites) {
        funnel.sites++;
        const commit = blame.get(callLine);
        const ci = commit ? unitOfCommit.get(commit) : -1; // вне окна SINCE — старше любой единицы
        if (ci === undefined) continue;
        const r = { line: callLine, text: callText };
        for (const pi of window) {
          if (ci >= pi) continue; // вызов появился в P или позже — не «P не обновил старый вызов»
          funnel.olderThanP++;
          const p = units[pi];
          const pdiff = unitDiff(p, 0);
          if (pdiff.has(fileB)) continue; // B в P — это не blast
          funnel.outsideP++;
          const changes = contractChanges(p);
          for (const s of names) {
            if (!changes.has(s)) continue;
            funnel.sigChanged++;
            const textAtP = showFile(p.sha, fileB);
            if (textAtP == null) continue;
            const ch = (changes.get(s) || []).filter((c) => c.file !== fileB && relatedCall(r.text, s, c.file, fileB, p.sha, textAtP));
            if (!ch.length) {
              if (process.env.REVIEW_GOLD_BLAST_DEBUG) console.log(`  [не связан] P=${p.sha.slice(0, 8)} F=${f.sha.slice(0, 8)} S=${s} A=${changes.get(s).map((c) => c.file).join(',')} B=${fileB}:${r.line} · ${r.text.trim().slice(0, 100)}`);
              continue;
            }
            funnel.related++;
            const want = r.text.trim();
            const hits = textAtP.split('\n').map((t, k) => ({ t: t.trim(), n: k + 1 })).filter((x) => x.t === want);
            if (!hits.length) continue;
            funnel.onTreeP++;
            const lineAtP = hits.sort((a, b) => Math.abs(a.n - r.line) - Math.abs(b.n - r.line))[0].n;
            if (!cands.has(pi)) cands.set(pi, { locations: new Map(), fixes: new Map(), symbols: new Map() });
            const c = cands.get(pi);
            if (!c.locations.has(fileB)) c.locations.set(fileB, new Map());
            c.locations.get(fileB).set(lineAtP, { s, fix: f.sha, callText: want });
            c.fixes.set(f.sha, f);
            c.symbols.set(s, ch[0]);
          }
        }
      }
    }
  }

  const out = [];
  for (const [pi, c] of cands) {
    const u = units[pi];
    const size = unitSize(u);
    if (size.lines > LIMITS.buggyLines || size.files > LIMITS.buggyFiles) {
      if (process.env.REVIEW_GOLD_BLAST_DEBUG) console.log(`  [велик] P=${u.sha.slice(0, 8)} ${size.lines}л/${size.files}ф S=${[...c.symbols.keys()].join(',')} · ${u.title.slice(0, 70)}`);
      continue;
    }
    out.push({ u, size, ...c });
  }
  out.sort((a, b) => b.u.date - a.u.date);
  console.log(`[harvest-local --blast] ${NAME} ${BRANCH} since ${SINCE}: единиц ${units.length}, фиксов ${fixes.length} (пропущено по размеру ${fixSkippedB}), blast-кандидатов ${out.length}`);
  console.log(`  воронка: вызовов в фиксах ${funnel.sites} → старше P ${funnel.olderThanP} → B вне P ${funnel.outsideP} → P сменил сигнатуру S ${funnel.sigChanged} → вызов к S из A ${funnel.related} → строка на дереве P ${funnel.onTreeP} → по размеру P ${out.length} единиц`);
  for (const t of out) {
    console.log(`  blast ${t.u.sha.slice(0, 8)} ${t.u.date.toISOString().slice(0, 10)} ${t.size.lines}л/${t.size.files}ф · ${t.u.title.slice(0, 70)}`);
    for (const [s, ch] of t.symbols) console.log(`    S=${s} A=${ch.file}\n      до:    ${ch.before.join(' | ').slice(0, 200)}\n      после: ${ch.after.join(' | ').slice(0, 200)}`);
    for (const [file, m] of t.locations) for (const [line, x] of m) console.log(`    B=${file}:${line} S=${x.s} F=${x.fix.slice(0, 8)} · ${x.callText.slice(0, 120)}`);
  }

  if (!EMIT) return;
  // Мету пишем только для явно перечисленных через REVIEW_GOLD_BLAST_KEEP="sha8:S,…" — отбор глазами
  // обязателен (RFC 21): автоматический кандидат без проверки в набор не идёт.
  const keep = new Set(String(process.env.REVIEW_GOLD_BLAST_KEEP || '').split(',').map((x) => x.trim()).filter(Boolean));
  const outDir = resolve(EMIT);
  let written = 0;
  for (const t of out) {
    for (const [s, ch] of t.symbols) {
      if (!keep.has(`${t.u.sha.slice(0, 8)}:${s}`)) continue;
      const locs = [];
      const fixShas = new Set();
      for (const [file, m] of t.locations) for (const [line, x] of m) if (x.s === s) { locs.push({ file, lines: [line, line] }); fixShas.add(x.fix); }
      const fixList = [...fixShas].map((sha) => t.fixes.get(sha));
      const id = `${NAME}-${t.u.sha.slice(0, 8)}-blast-${slug(s)}`;
      const meta = {
        id,
        type: 'tp',
        tags: ['blast'],
        description: `«${t.u.title}» сменил контракт ${s} в ${ch.file}, а вызов вне PR остался старым — его потом исправил ${fixList.map((f) => f.ref || f.sha.slice(0, 8)).join(', ')}`,
        source: {
          local: NAME,
          branch: BRANCH,
          sha: t.u.sha,
          base: t.u.base,
          ref: t.u.ref,
          fixes: fixList.map((f) => ({ sha: f.sha, ref: f.ref, title: f.title })),
          evidence: [
            `дифф виновника: ${t.size.lines} строк в ${t.size.files} файл(ах)`,
            `сигнатура ${s} в ${ch.file}: «${ch.before.join(' | ')}» → «${ch.after.join(' | ')}»`,
            'локации: строка вызова в файле вне PR на дереве PR; git blame на родителе фикса — вызов старше PR',
          ],
        },
        expect: {
          mustFindLocations: locs,
          note: `S=${s}, A=${ch.file}, фикс ${fixList.map((f) => f.sha.slice(0, 8)).join(', ')}`,
        },
      };
      await mkdir(join(outDir, id), { recursive: true });
      await writeFile(join(outDir, id, 'meta.json'), `${JSON.stringify(meta, null, 2)}\n`, 'utf8');
      written++;
    }
  }
  console.log(`[harvest-local --blast] записано: ${written} → ${outDir}`);
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
if (BLAST) {
  await harvestBlast();
  process.exit(0);
}
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
