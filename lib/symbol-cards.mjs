/**
 * Карточки символов (RFC 14, шаг 2): строка объявления того, что вызывают изменённые строки.
 *
 * Модель видит дифф, окна ±margin вокруг изменений и blast-radius экспортов PR. Объявления
 * функции, которую зовёт добавленная строка, она не видит, если оно лежит вне окна или в
 * файле вне PR, — и тогда либо пишет «`X` не определена», либо домысливает контракт
 * («второй аргумент не поддерживает `strict`»). Карточка — одна строка объявления с местом в
 * коде: доказывает, что символ существует и какова его сигнатура, и ничего не говорит о теле.
 *
 * Источник места — граф Graphify (`resolveUniqueNode`), источник текста — сам файл в рабочем
 * дереве (в CI это версия PR). Граф не хранит сигнатур, а свежесть графа здесь не нужна:
 * если строка `source_location` не содержит имени, граф устарел и карточки нет.
 *
 * Конец объявления ищется лексером пары скобок без зависимостей (в графе есть только начало,
 * `endLine` SCIP — конец токена имени). Ошибка лексера стоит только диапазона, не карточки.
 *
 * Только JS, в Rust не портируется: это оркестрация входа модели, как `lib/chunk-routing.mjs`.
 * В Rust меняется лишь рендер готовых строк в промпте (`rust/src/prompt.rs`).
 */

import { readFileSync, realpathSync, statSync } from 'node:fs';
import { isAbsolute, relative, resolve, sep } from 'node:path';
import { resolveUniqueNode } from './graphify.mjs';

/** v1 — семейство JS/TS: для него верны правила «где кончается объявление» (`{`, `=>`, `;`). */
export const SYMBOL_CARD_EXTS = Object.freeze(['ts', 'tsx', 'js', 'jsx', 'mjs', 'cjs']);

const KEYWORDS = new Set(('if for while switch catch return typeof function await async new super this import export '
  + 'require class throw delete void in of yield do else try case default with let const var instanceof').split(' '));

/** Глобалы языка и тестовых раннеров: у них нет объявления в репозитории. */
const GLOBALS = new Set(('console Promise JSON Object Array String Number Boolean Math Date Error TypeError RangeError '
  + 'RegExp Map Set WeakMap WeakSet Symbol BigInt Intl Reflect Proxy parseInt parseFloat isNaN isFinite setTimeout '
  + 'clearTimeout setInterval clearInterval setImmediate queueMicrotask structuredClone fetch URL URLSearchParams '
  + 'TextEncoder TextDecoder AbortController Buffer process globalThis encodeURIComponent decodeURIComponent '
  + 'describe it test expect assert beforeEach afterEach before after vi jest').split(' '));

/**
 * `new Name(` и `name(` — имя вызываемого. Вызов через точку (`obj.name(`) не берётся: по
 * одному имени метод не разрешить — `assert.ok(` сопоставился бы с первым попавшимся `ok()`
 * в графе, и модель получила бы чужую сигнатуру как факт.
 */
const CALL_RE = /\bnew\s+([A-Za-z_$][\w$]*)|(?<![\w$])([A-Za-z_$][\w$]*)\s*\(/g;

/** Длиннее — минифицированный или сгенерированный код: вызовы из него не собираем. */
const MAX_CALL_LINE_CHARS = 2000;

/**
 * Стоит ли перед позицией `idx` точка (через пробелы): `obj . name(` — метод. Просмотр назад
 * ограничен: lookbehind `(?<!\.\s*)` в регулярке пересматривал пробелы на каждой позиции и
 * давал квадратичный разбор строки из пробелов.
 */
function precededByDot(line, idx) {
  for (let k = idx - 1, steps = 0; k >= 0 && steps < 64; k--, steps++) {
    const c = line[k];
    if (c === '.') return true;
    if (c !== ' ' && c !== '\t') return false;
  }
  return false;
}

/** Спецификатор пакета, а не файла репозитория: `node:fs`, `react`, `@scope/pkg` (не `@/` и не `~/`). */
function isPackageSpecifier(spec) {
  return !/^(?:\.|\/|@\/|~\/|#)/.test(spec);
}

/** Оператор импорта длиннее — не разбираем: защита от разбора гигантской строки автора PR. */
const MAX_IMPORT_CHARS = 2000;
/** Сколько строк собирать в многострочный `import {\n a,\n b\n} from 'x'`. */
const MAX_IMPORT_LINES = 40;

/** `from 'x'` / `require('x')` в конце оператора. Якорь `$` и без соседних `\s*` — линейно. */
const FROM_SPEC_RE = /\bfrom\s*(['"])([^'"\n]{1,300})\1/;
const REQUIRE_SPEC_RE = /\brequire\(\s*(['"])([^'"\n]{1,300})\1\s*\)/;

function addLocalNames(names, clause) {
  // clause: `d`, `d, { a, b as c }`, `* as ns`, `{ a, b: c }` — разбор строковыми операциями
  const open = clause.indexOf('{');
  const close = clause.indexOf('}', open + 1);
  const add = (n) => { const id = String(n || '').trim(); if (/^[A-Za-z_$][\w$]*$/.test(id)) names.add(id); };
  const head = (open >= 0 ? clause.slice(0, open) : clause).replace(/^\s*type\s+/, '');
  for (const part of head.split(',')) {
    const t = part.trim();
    if (t.startsWith('*')) add(t.split(/\s/).pop());
    else add(t);
  }
  if (open >= 0 && close > open) {
    for (const part of clause.slice(open + 1, close).split(',')) {
      const tokens = part.replace(':', ' as ').split(' as ');
      add(tokens[tokens.length - 1]);
    }
  }
}

/**
 * Имена, импортированные из пакетов: их объявления в репозитории нет, а одноимённый узел
 * графа — чужой (`ok` из `node:assert` ≠ `ok()` из какого-то файла).
 *
 * Без одной большой регулярки: в ней соседние `\s*` делили одни и те же пробелы, и строка
 * `import a` + тысячи пробелов разбиралась кубически (543 мс на 1000) — текст пишет автор PR.
 */
export function packageImportedNames(lines) {
  const names = new Set();
  const src = lines || [];
  for (let i = 0; i < src.length; i++) {
    const first = String(src[i]);
    const trimmed = first.trimStart();
    const isImport = trimmed.startsWith('import ') || trimmed.startsWith('import{');
    const isRequire = !isImport && /^(?:const|let|var)\s/.test(trimmed) && first.includes('require(');
    if (!isImport && !isRequire) continue;
    let stmt = trimmed.slice(0, MAX_IMPORT_CHARS);
    // многострочный import: дособрать до `from '…'`
    for (let j = i + 1; isImport && !FROM_SPEC_RE.test(stmt) && j < Math.min(src.length, i + MAX_IMPORT_LINES); j++) {
      stmt = `${stmt} ${String(src[j]).trim()}`.slice(0, MAX_IMPORT_CHARS);
    }
    const spec = (isImport ? FROM_SPEC_RE : REQUIRE_SPEC_RE).exec(stmt);
    if (!spec || !isPackageSpecifier(spec[2])) continue;
    const clause = isImport
      ? stmt.slice('import'.length, spec.index)
      : stmt.slice(stmt.search(/\s/) + 1, stmt.indexOf('='));
    addLocalNames(names, clause);
  }
  return names;
}

/** Строка аннотированного диффа с добавленной строкой: `   12: +  code`. */
const ADDED_LINE_RE = /^\s*\d+: \+(.*)$/;

const MAX_DECL_CHARS = 160;
const MAX_HEADER_LINES = 3;
const MAX_SCAN_LINES = 2000;
const MAX_FILE_BYTES = 2 * 1024 * 1024;

export function isSymbolCardPath(path) {
  const m = /\.([A-Za-z0-9]+)$/.exec(String(path || ''));
  return Boolean(m && SYMBOL_CARD_EXTS.includes(m[1].toLowerCase()));
}

/**
 * Убрать строковые литералы и комментарий строки: внутри них `name(` — не вызов.
 * Альтернативы в литерале не пересекаются (`\\.` или любой НЕ-`\\`): с `(?!\1).` обратный
 * слэш разбирался двумя путями, и незакрытая строка из `\\\\…` давала экспоненциальный
 * перебор — текст строки пишет автор PR.
 */
function stripLiteralsAndComment(line) {
  return String(line)
    .replace(/(['"`])(?:\\.|(?!\1)[^\\])*\1/g, '""')
    .replace(/\/\/.*$/, '')
    .replace(/\/\*.*?\*\//g, '');
}

/**
 * Имена, которые вызывают добавленные строки, со счётом строк-вызывателей.
 * @param {string[]} addedLines — текст добавленных строк (без `+`)
 * @returns {Map<string, number>} имя → число добавленных строк, где оно вызвано
 */
export function extractUsedIdentifiers(addedLines) {
  const counts = new Map();
  for (const raw of addedLines || []) {
    const trimmed = String(raw).trim();
    if (!trimmed || trimmed.length > MAX_CALL_LINE_CHARS || /^(?:\/\/|\*|\/\*)/.test(trimmed)) continue;
    const line = stripLiteralsAndComment(raw);
    const seen = new Set();
    for (const m of line.matchAll(CALL_RE)) {
      const name = m[1] || m[2];
      if (!name || name.length < 2 || KEYWORDS.has(name) || GLOBALS.has(name)) continue;
      if (m[2] && precededByDot(line, m.index)) continue;
      if (seen.has(name)) continue;
      seen.add(name);
      counts.set(name, (counts.get(name) || 0) + 1);
    }
  }
  return counts;
}

/** Объявление имени в тексте: функция, класс, const/let/var, метод, свойство-функция. */
export function declarationRegex(name) {
  const n = name.replace(/[$]/g, '\\$');
  return new RegExp(
    `(?:function\\*?\\s+${n}\\b|class\\s+${n}\\b|(?:const|let|var)\\s+${n}\\s*=|`
    // `[ \t]`, не `\s`: с флагом m `^\s*` захватывал переводы строк, и файл из пустых строк
    // разбирался квадратично — окна контекста пишет автор PR.
    + `^[ \\t]*(?:export[ \\t]+)?(?:(?:async|static|get|set|public|private|protected)[ \\t]+)*${n}[ \\t]*\\([^)\\n]*\\)[ \\t]*(?::[^{\\n]*)?\\{|`
    + `\\b${n}\\s*:\\s*(?:async\\s*)?(?:function\\b|\\())`,
    'm',
  );
}

/** Добавленные строки из аннотированной секции диффа файла. */
export function addedLinesOfSection(section) {
  const out = [];
  for (const line of String(section || '').split('\n')) {
    const m = ADDED_LINE_RE.exec(line);
    if (m) out.push(m[1]);
  }
  return out;
}

/** После этих слов `/` начинает литерал регулярки: `return /[}]/.test(s)`. */
const REGEX_AFTER_KEYWORD = /(?:^|[^\w$])(?:return|typeof|case|do|else|in|of|void|yield|throw|delete|instanceof|await)\s*$/;

/** Может ли `/` после этого символа начинать литерал регулярки (а не деление). */
function regexCanStart(prev) {
  return !prev || /[(,=:[!&|?{};+\-*%<>~^]/.test(prev);
}

/**
 * Объявление, начинающееся на строке `startIdx` (0-based): текст заголовка (до тела) и
 * строка конца (0-based) — пара к открывающей `{` тела, `;` на нулевой глубине или конец
 * выражения стрелочной функции. Неоднозначность (незакрытый литерал, > maxLines) →
 * `endIdx: null`: диапазон теряется, заголовок остаётся.
 *
 * Чистая функция над массивом строк — её же возьмёт окно verifier по границам функции.
 */
export function scanDeclaration(lines, startIdx, { maxLines = MAX_SCAN_LINES } = {}) {
  const src = Array.isArray(lines) ? lines : [];
  if (startIdx < 0 || startIdx >= src.length) return { header: '', endIdx: null };
  let paren = 0; let bracket = 0; let brace = 0;
  let bodyOpen = false; let arrowExpr = false; let headerDone = false;
  let header = '';
  /** стек контекстов: 'code' | 'tpl'; у 'code' внутри шаблона — своя глубина фигурных */
  const stack = [{ kind: 'code', depth: 0 }];
  let state = 'code'; // code | sq | dq | line | block | regex | regexClass
  let prevSig = '';
  const last = Math.min(src.length, startIdx + maxLines);

  for (let li = startIdx; li < last; li++) {
    const line = src[li];
    if (state === 'line') state = 'code';
    for (let i = 0; i < line.length; i++) {
      const ch = line[i];
      const nx = line[i + 1];
      const top = stack[stack.length - 1];
      if (state === 'block') { if (ch === '*' && nx === '/') { state = 'code'; i++; } continue; }
      const commentStart = state === 'code' && top.kind === 'code' && ch === '/' && (nx === '/' || nx === '*');
      if (!headerDone && !commentStart) header += ch;
      if (state === 'sq' || state === 'dq') {
        if (ch === '\\') { i++; continue; }
        if ((state === 'sq' && ch === "'") || (state === 'dq' && ch === '"')) state = 'code';
        continue;
      }
      if (state === 'regex' || state === 'regexClass') {
        if (ch === '\\') { i++; continue; }
        if (state === 'regex' && ch === '[') state = 'regexClass';
        else if (state === 'regexClass' && ch === ']') state = 'regex';
        else if (state === 'regex' && ch === '/') { state = 'code'; prevSig = '/'; }
        continue;
      }
      if (top.kind === 'tpl') {
        if (ch === '\\') { i++; continue; }
        if (ch === '`') { stack.pop(); prevSig = '`'; continue; }
        if (ch === '$' && nx === '{') { stack.push({ kind: 'code', depth: 0 }); i++; }
        continue;
      }
      // code
      if (ch === '/' && nx === '/') { state = 'line'; break; }
      if (ch === '/' && nx === '*') { state = 'block'; i++; continue; }
      if (ch === "'") { state = 'sq'; continue; }
      if (ch === '"') { state = 'dq'; continue; }
      if (ch === '`') { stack.push({ kind: 'tpl' }); continue; }
      if (ch === '/' && (regexCanStart(prevSig) || REGEX_AFTER_KEYWORD.test(line.slice(0, i)))) { state = 'regex'; continue; }
      if (stack.length > 1) {
        // внутри `${…}` шаблона: своя глубина, закрывающая `}` возвращает в шаблон
        if (ch === '{') top.depth++;
        else if (ch === '}') { if (top.depth === 0) { stack.pop(); continue; } top.depth--; }
        if (!/\s/.test(ch)) prevSig = ch;
        continue;
      }
      if (ch === '(') paren++;
      else if (ch === ')') paren--;
      else if (ch === '[') bracket++;
      else if (ch === ']') bracket--;
      else if (ch === '{') {
        if (!bodyOpen && !arrowExpr && paren === 0 && bracket === 0 && brace === 0) {
          bodyOpen = true;
          if (!headerDone) { header = header.slice(0, -1); headerDone = true; }
        }
        brace++;
      } else if (ch === '}') {
        brace--;
        if (bodyOpen && brace === 0) return { header: tidyHeader(header), endIdx: li };
        if (brace < 0) return { header: tidyHeader(header), endIdx: null };
      } else if (ch === ';' && paren === 0 && bracket === 0 && brace === 0 && !bodyOpen) {
        if (!headerDone) header = header.slice(0, -1);
        return { header: tidyHeader(header), endIdx: li };
      } else if (ch === '=' && nx === '>' && paren === 0 && bracket === 0 && brace === 0 && !bodyOpen) {
        if (!headerDone) header += '>';
        i++;
        // `=> {` — тело в скобках, его закроет ветка `{`; иначе — выражение до `;` или конца строки
        const rest = line.slice(i + 1).trimStart();
        if (rest && rest[0] !== '{') { arrowExpr = true; headerDone = true; }
        else if (!rest) {
          const nextLine = (src[li + 1] || '').trimStart();
          if (nextLine[0] !== '{') { arrowExpr = true; headerDone = true; }
        }
        prevSig = '>';
        continue;
      }
      if (!/\s/.test(ch)) prevSig = ch;
    }
    if (state === 'sq' || state === 'dq' || state === 'regex' || state === 'regexClass') {
      return { header: tidyHeader(header), endIdx: null }; // незакрытый литерал на строке
    }
    if (!headerDone) {
      header += '\n';
      if (li - startIdx + 1 >= MAX_HEADER_LINES) headerDone = true;
    }
    if (arrowExpr && paren === 0 && bracket === 0 && brace === 0 && stack.length === 1 && state !== 'block'
      && !/[,([{=+\-*/&|?:.]\s*$/.test(line.trimEnd())) {
      // выражение продолжается цепочкой или оператором на следующей строке: `.reduce(…)`, `? a : b`
      const next = src.slice(li + 1, last).find((l) => l.trim());
      if (!next || !/^\s*(?:\?\.|\.|\?|:|&&|\|\||\?\?|[+\-*/%]|=>)/.test(next)) {
        return { header: tidyHeader(header), endIdx: li };
      }
    }
  }
  return { header: tidyHeader(header), endIdx: null };
}

function tidyHeader(header) {
  const s = String(header).replace(/\s+/g, ' ').trim().replace(/[=:]\s*$/, (m) => m.trim());
  return s.length > MAX_DECL_CHARS ? `${s.slice(0, MAX_DECL_CHARS - 1)}…` : s;
}

/** Конец объявления (0-based) или null. Обёртка над scanDeclaration для внешних читателей. */
export function findDeclarationEnd(lines, startIdx, opts) {
  return scanDeclaration(lines, startIdx, opts).endIdx;
}

function normalizeNodePath(p, root) {
  let s = String(p || '').replace(/\\/g, '/');
  if (!s) return '';
  if (isAbsolute(s)) {
    const rel = relative(root, s);
    if (rel.startsWith('..') || isAbsolute(rel)) return '';
    s = rel.split(sep).join('/');
  }
  return s.replace(/^\.\//, '');
}

function nodeLine(node) {
  const m = /^L?(\d+)/.exec(String(node?.source_location ?? ''));
  return m ? Number(m[1]) : 0;
}

/**
 * Чтение файла рабочего дерева построчно, с кэшем и защитой от выхода за корень.
 *
 * Путь приходит из графа, а граф и дерево в CI — версия PR: автор PR может положить
 * symlink `lib/x.mjs → ~/.ssh/…`, и строка чужого файла уехала бы в промпт внешней модели.
 * Поэтому сверяется ПОСЛЕ разыменования ссылок (realpath), а не только по `resolve`.
 */
export function createLineReader(root = process.cwd()) {
  let base;
  try { base = realpathSync(resolve(root)); } catch { base = resolve(root); }
  const cache = new Map();
  return (path) => {
    if (cache.has(path)) return cache.get(path);
    let lines = null;
    try {
      const real = realpathSync(resolve(base, path));
      if (real !== base && real.startsWith(base + sep) && statSync(real).size <= MAX_FILE_BYTES) {
        lines = readFileSync(real, 'utf8').split('\n');
      }
    } catch { lines = null; }
    cache.set(path, lines);
    return lines;
  };
}

function bump(reasons, key, n = 1) { reasons[key] = (reasons[key] || 0) + n; }

/**
 * Карточки по файлам PR.
 *
 * @param {string[]} paths — изменённые пути
 * @param {object} opts
 * @param {object|null} opts.graphIndex — `buildGraphIndex` (null → карточек нет)
 * @param {Map<string,string>} opts.sections — аннотированная секция диффа по пути
 * @param {Map<string,string>} opts.windows — текст контекста, который модель видит по пути
 * @param {(path: string) => string[]|null} opts.readLines — строки файла рабочего дерева
 * @param {{ bodyMaxLines?: number }} [opts.limits]
 * @param {string} [opts.root]
 * @returns {{ byPath: Map<string, object[]>, metrics: object }}
 */
export function buildSymbolCards(paths, { graphIndex, sections, windows, readLines, limits = {}, root = process.cwd() } = {}) {
  const byPath = new Map();
  const skip = {};
  let candidates = 0; let resolved = 0; let cards = 0;
  if (!graphIndex) {
    return { byPath, metrics: { candidates, resolved, cards, skip_reasons: { graph_unavailable: 1 } } };
  }
  const bodyMaxLines = Math.max(0, Number(limits.bodyMaxLines) || 0);
  for (const path of paths || []) {
    if (!isSymbolCardPath(path)) continue;
    const used = extractUsedIdentifiers(addedLinesOfSection(sections?.get?.(path)));
    if (!used.size) continue;
    const visible = `${sections?.get?.(path) ?? ''}\n${windows?.get?.(path) ?? ''}`;
    // Импорты читаются из полного файла: шапка с импортами обычно вне окна вокруг правок.
    const external = packageImportedNames([...(readLines(path) || []), ...addedLinesOfSection(sections?.get?.(path))]);
    const list = [];
    for (const [name, count] of used) {
      candidates++;
      if (external.has(name)) { bump(skip, 'package_import'); continue; }
      if (declarationRegex(name).test(visible)) { bump(skip, 'visible'); continue; }
      const node = resolveUniqueNode(graphIndex, name);
      if (!node) { bump(skip, 'unresolved_or_ambiguous'); continue; }
      const file = normalizeNodePath(node.source_file, root);
      const line = nodeLine(node);
      if (!file || !line) { bump(skip, 'no_location'); continue; }
      if (!isSymbolCardPath(file)) { bump(skip, 'non_js_target'); continue; }
      const lines = readLines(file);
      if (!lines || line > lines.length) { bump(skip, 'unreadable'); continue; }
      if (!lines[line - 1].includes(name)) { bump(skip, 'stale_graph'); continue; }
      resolved++;
      const { header, endIdx } = scanDeclaration(lines, line - 1);
      if (!header) { bump(skip, 'empty_header'); continue; }
      const endLine = endIdx === null ? null : endIdx + 1;
      const card = { name, path: file, line, endLine, declaration: header, count };
      if (bodyMaxLines && endLine && endLine - line + 1 <= bodyMaxLines) {
        card.body = lines.slice(line - 1, endLine).join('\n');
      }
      list.push(card);
      cards++;
    }
    if (list.length) {
      list.sort((a, b) => b.count - a.count || a.name.localeCompare(b.name));
      byPath.set(path, list);
    }
  }
  return { byPath, metrics: { candidates, resolved, cards, skip_reasons: skip } };
}

/** Строка карточки для промпта: место в коде, затем объявление или короткое тело целиком. */
export function renderSymbolCard(card) {
  const range = card.endLine && card.endLine !== card.line ? `L${card.line}–L${card.endLine}` : `L${card.line}`;
  return card.body
    ? `// ${card.path}:${range} (full body)\n${card.body}`
    : `// ${card.path}:${range}\n${card.declaration}`;
}

/**
 * Вход хэша RFC 05 по файлу: путь и текст объявления БЕЗ номеров строк. Номера сдвигаются
 * от любой правки выше объявления или внутри тела — с ними каждая такая правка заставляла бы
 * заново ревьюить все файлы-вызыватели. Находка вызывающего файла не зависит от того, на
 * какой строке лежит неизменённое объявление. Короткое тело входит целиком: там изменение
 * текста — изменение входа по существу.
 */
export function symbolCardsHashInput(cards) {
  return (cards || [])
    .map((c) => `${c.path}\n${c.body ?? c.declaration}`)
    .sort()
    .join('\n\n');
}

/**
 * Карточки чанка: объединение карточек его файлов без повторов, приоритет — число
 * изменённых строк, вызывающих символ; лимит по числу и по символам.
 * @returns {{ cards: string[], dropped: number, chars: number }}
 */
export function selectChunkCards(byPath, chunkPaths, { max = 20, maxChars = 3000 } = {}) {
  const merged = new Map();
  for (const p of chunkPaths || []) {
    for (const c of byPath?.get?.(p) || []) {
      const key = `${c.path}:${c.line}`;
      const prev = merged.get(key);
      if (prev) prev.count += c.count;
      else merged.set(key, { ...c });
    }
  }
  const ordered = [...merged.values()].sort((a, b) => b.count - a.count || a.path.localeCompare(b.path) || a.line - b.line);
  const out = [];
  let chars = 0;
  let dropped = 0;
  for (const c of ordered) {
    const text = renderSymbolCard(c);
    if (out.length >= max || chars + text.length > maxChars) { dropped++; continue; }
    out.push(text);
    chars += text.length;
  }
  return { cards: out, dropped, chars };
}
