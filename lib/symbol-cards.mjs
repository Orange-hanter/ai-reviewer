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
import { bareLabel } from './graphify.mjs';

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
  for (const { spec, clause } of importStatements(lines)) {
    if (isPackageSpecifier(spec)) addLocalNames(names, clause);
  }
  return names;
}

/** Операторы `import … from 'x'` и `const … = require('x')`: спецификатор и часть с именами. */
function* importStatements(lines) {
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
    if (!spec) continue;
    const clause = isImport
      ? stmt.slice('import'.length, spec.index)
      : stmt.slice(stmt.search(/\s/) + 1, stmt.indexOf('='));
    yield { spec: spec[2], clause, isRequire };
  }
}

const IDENT_RE = /^[A-Za-z_$][\w$]*$/;

/**
 * Привязки из ОТНОСИТЕЛЬНЫХ импортов файла: локальное имя → { spec, imported }, где
 * imported — имя в модуле, `'*'` для `import * as ns` и `'default'` для импорта по умолчанию.
 * По ним символ ищется в файле модуля, а не по уникальности имени во всём графе (аудит, п. 1.12).
 */
export function relativeImportBindings(lines) {
  const out = new Map();
  for (const { spec, clause, isRequire } of importStatements(lines)) {
    if (isPackageSpecifier(spec)) continue;
    const open = clause.indexOf('{');
    const close = clause.indexOf('}', open + 1);
    const head = (open >= 0 ? clause.slice(0, open) : clause).replace(/^\s*type\s+/, '');
    for (const part of head.split(',')) {
      const t = part.trim();
      if (!t) continue;
      if (t.startsWith('*')) {
        const ns = t.split(/\s/).pop();
        if (IDENT_RE.test(ns)) out.set(ns, { spec, imported: '*' });
      } else if (IDENT_RE.test(t)) {
        out.set(t, { spec, imported: isRequire ? '*' : 'default' });
      }
    }
    if (open >= 0 && close > open) {
      for (const part of clause.slice(open + 1, close).split(',')) {
        const tokens = part.replace(/^\s*type\s+/, '').replace(':', ' as ').split(' as ').map((x) => x.trim());
        const imported = tokens[0];
        const local = tokens[tokens.length - 1];
        if (IDENT_RE.test(imported) && IDENT_RE.test(local)) out.set(local, { spec, imported });
      }
    }
  }
  return out;
}

const MODULE_EXTS = ['.ts', '.tsx', '.mts', '.cts', '.js', '.jsx', '.mjs', '.cjs'];

/**
 * Файл модуля по относительному спецификатору — среди файлов, которые знает граф: точный путь,
 * TS-соглашение `./x.js` → `x.ts`, перебор расширений, `index.*`.
 */
export function resolveImportPath(callerPath, spec, hasFile) {
  const parts = String(callerPath).split('/');
  parts.pop();
  for (const seg of String(spec).split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') { if (!parts.length) return null; parts.pop(); } else parts.push(seg);
  }
  const base = parts.join('/');
  const stem = base.replace(/\.(?:m|c)?jsx?$/, '');
  const candidates = [base];
  for (const ext of MODULE_EXTS) candidates.push(`${stem}${ext}`);
  for (const ext of MODULE_EXTS) candidates.push(`${base}/index${ext}`);
  return candidates.find((c) => hasFile(c)) ?? null;
}

/** Узлы графа по файлу (нормализованный путь) — строится один раз на индекс. */
const nodesByFileCache = new WeakMap();
function nodesByFile(index, root) {
  let map = nodesByFileCache.get(index);
  if (map) return map;
  map = new Map();
  for (const n of index.byId?.values?.() || []) {
    const f = normalizeNodePath(n.source_file, root);
    if (!f) continue;
    if (!map.has(f)) map.set(f, []);
    map.get(f).push(n);
  }
  nodesByFileCache.set(index, map);
  return map;
}

/** Узел с голым именем `name` в конкретном файле — единственный, иначе null. */
function nodeInFile(byFile, file, name) {
  const found = (byFile.get(file) || []).filter((n) => bareLabel(n.label) === name);
  return found.length === 1 ? found[0] : null;
}

/**
 * Вызовы через пространство имён `ns.fn(` — только для `ns` из `import * as ns from './x'`:
 * там модуль известен, и метод по одному имени разрешать не нужно.
 * @returns {Map<string, { ns: string, fn: string, count: number }>} ключ — `ns.fn`
 */
export function extractNamespaceCalls(addedLines, namespaces) {
  const out = new Map();
  if (!namespaces?.size) return out;
  for (const raw of addedLines || []) {
    const trimmed = String(raw).trim();
    if (!trimmed || trimmed.length > MAX_CALL_LINE_CHARS || /^(?:\/\/|\*|\/\*)/.test(trimmed)) continue;
    const line = stripLiteralsAndComment(trimmed);
    const seen = new Set();
    for (const m of line.matchAll(/(?<![\w$.])([A-Za-z_$][\w$]*)\.([A-Za-z_$][\w$]*)[ \t]*\(/g)) {
      if (!namespaces.has(m[1])) continue;
      const key = `${m[1]}.${m[2]}`;
      if (seen.has(key)) continue;
      seen.add(key);
      const prev = out.get(key);
      if (prev) prev.count++;
      else out.set(key, { ns: m[1], fn: m[2], count: 1 });
    }
  }
  return out;
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

/** Начало объявления метода: модификаторы, имя, дженерики, `(`. Без хвоста — он разбирается кодом. */
const METHOD_HEAD_RE = /^(?:(?:public|private|protected|static|async|abstract|readonly|override|get|set|export|declare)[ \t]+)*([A-Za-z_$][\w$]*)[ \t]*(?:<[^<>()\n]{0,200}>)?[ \t]*\(/;

/**
 * Имя метода, который ОБЪЯВЛЯЕТ эта строка, — не вызов (аудит RFC 14, п. 1.1):
 * `listTypes(filter): Promise<T>;`, `async run(a) {`, `private x<T>(a: T): void {`.
 * Отличие от вызова-оператора `run(a);` — аннотация типа или `{` сразу после `)`.
 * Хвост разбирается циклом, не регуляркой: пересекающиеся классы пробелов в хвосте давали
 * квадратичный перебор на каждой добавленной строке.
 */
function methodDeclaredName(line) {
  const m = METHOD_HEAD_RE.exec(line);
  if (!m) return null;
  let depth = 0;
  let close = -1;
  for (let i = m[0].length - 1; i < line.length; i++) {
    if (line[i] === '(') depth++;
    else if (line[i] === ')' && --depth === 0) { close = i; break; }
  }
  if (close < 0) return null;
  const rest = line.slice(close + 1).trim();
  if (rest === '{') return m[1];
  if (rest.startsWith(':') && !rest.includes('=') && !rest.includes('(')) return m[1];
  return null;
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
    // Разбирается та же строка, что прошла предел длины: с `raw` пробелы по краям обходили его.
    const line = stripLiteralsAndComment(trimmed);
    const declared = methodDeclaredName(line);
    const seen = new Set();
    for (const m of line.matchAll(CALL_RE)) {
      const name = m[1] || m[2];
      if (!name || name.length < 2 || KEYWORDS.has(name) || GLOBALS.has(name)) continue;
      if (m[2] && precededByDot(line, m.index)) continue;
      if (m[2] && name === declared && m.index === line.indexOf(name)) continue;
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
    `(?:function\\*?\\s+${n}\\b|(?:class|interface|enum|namespace|type)\\s+${n}\\b|(?:const|let|var)\\s+${n}\\s*=|`
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
  // TS до тела (аудит, п. 1.6): `<…>` дженериков и `{…}` литералов типа не открывают тело,
  // `=>` в аннотации (`): (x: T) => R {`) — не стрелочная функция.
  let angle = 0; let typeBrace = 0; let seenAssign = false;
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
      const top0 = paren === 0 && bracket === 0 && brace === 0 && !bodyOpen && !arrowExpr;
      if (top0 && ch === '<' && /[\w$<]/.test(prevSig)) { angle++; prevSig = ch; continue; }
      if (top0 && ch === '>' && angle > 0) { angle--; prevSig = ch; continue; }
      if (top0 && ch === '{' && (angle > 0 || typeBrace > 0 || /[:|&]/.test(prevSig))) { typeBrace++; prevSig = ch; continue; }
      if (top0 && ch === '}' && typeBrace > 0) { typeBrace--; prevSig = ch; continue; }
      if (top0 && ch === '=' && nx !== '>' && nx !== '=' && !/[=!<>]/.test(line[i - 1] || '') && angle === 0 && typeBrace === 0) seenAssign = true;
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
      } else if (ch === ';' && paren === 0 && bracket === 0 && brace === 0 && angle === 0 && typeBrace === 0 && !bodyOpen) {
        if (!headerDone) header = header.slice(0, -1);
        return { header: tidyHeader(header), endIdx: li };
      } else if (ch === '=' && nx === '>' && paren === 0 && bracket === 0 && brace === 0 && !bodyOpen) {
        if (!headerDone) header += '>';
        i++;
        // `=>` — стрелочная функция объявления только после присваивания (`const f = (a) =>`,
        // поле класса `h = () =>`) или в свойстве объекта (`name: (a) =>`); в остальных
        // случаях это стрелка внутри аннотации типа, и заголовок продолжается.
        if (!seenAssign && !/^\s*(?:[\w$]+\s*:\s*)?(?:async\s*)?\(/.test(header) && !/^\s*(?:async\s+)?[\w$]+\s*=>/.test(header)) {
          prevSig = '>';
          continue;
        }
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

const MAX_OVERLOADS = 8;

/**
 * Объявление для карточки (аудит, п. 1.6): заголовок, плюс то, без чего сигнатура врёт.
 * - Перегрузки TS (`function f(a: string): void;` × N, затем реализация) — все подряд,
 *   иначе карточка сужает сигнатуру до первой.
 * - Класс — с сигнатурой `constructor(…)`: для `new X(a, b)` важна именно она, а не
 *   `class X extends Y`.
 * @returns {{ header: string, endIdx: number|null }}
 */
export function cardDeclaration(lines, startIdx, name) {
  const first = scanDeclaration(lines, startIdx);
  let { header, endIdx } = first;
  if (!header) return first;
  const n = String(name || '').replace(/[$]/g, '\\$');
  const startLine = String(lines[startIdx] ?? '').trim();
  if (n && endIdx === startIdx && startLine.endsWith(';')) {
    const sigRe = new RegExp(`^(?:export\\s+)?(?:declare\\s+)?(?:async\\s+)?function\\s+${n}\\b|^(?:(?:public|private|protected|static|async|abstract|override)\\s+)*${n}\\s*[<(]`);
    const sigs = [header];
    let j = startIdx + 1;
    for (let k = 0; k < MAX_OVERLOADS && j < lines.length; k++) {
      const t = String(lines[j]).trim();
      if (!t || /^(?:\/\/|\*|\/\*)/.test(t)) { j++; continue; }
      if (!sigRe.test(t)) break;
      const r = scanDeclaration(lines, j);
      if (!r.header) break;
      sigs.push(r.header);
      endIdx = r.endIdx;
      // перегрузка — одна строка с `;`; реализация с телом закрывает серию
      if (r.endIdx === null || r.endIdx !== j || !t.endsWith(';')) break;
      j = r.endIdx + 1;
    }
    header = [...new Set(sigs)].join('\n');
  }
  if (/(?:^|\s)class\s/.test(header) && endIdx !== null) {
    for (let k = startIdx + 1; k <= endIdx && k < lines.length; k++) {
      if (/^\s*(?:(?:public|private|protected)\s+)?constructor\s*\(/.test(String(lines[k]))) {
        const ctor = scanDeclaration(lines, k).header;
        if (ctor) header = `${header} { ${ctor} }`;
        break;
      }
    }
  }
  return { header, endIdx };
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

/** Тесты, моки и фикстуры: их `jest.fn()` — не объявление символа приложения (аудит, п. 1.1). */
const TEST_SOURCE_RE = /(?:^|\/)(?:__tests__|__mocks__|__fixtures__|tests?|spec|fixtures?)\/|\.(?:spec|test|stories)\.[cm]?[jt]sx?$/i;

/**
 * Узел графа для имени — только если он ЕДИНСТВЕННЫЙ среди всех способов назвать символ:
 * `name`, `name()` и «голая» метка без `()`, с учётом регистра (`Parser` ≠ `parser`).
 * `resolveUniqueNode` (blast-radius) возвращает точное совпадение `name`, даже если `name()` есть в
 * другом файле — для карточки это чужая сигнатура как факт (аудит, п. 1.3). Метки методов
 * (`.method()`) по голому имени не совпадают и сюда не попадают.
 */
export function resolveCardNode(index, name) {
  if (!index || !name) return { node: null, reason: 'unresolved' };
  const lc = name.toLowerCase();
  const seen = new Map();
  for (const list of [index.byExactLabel?.get(lc), index.byExactLabel?.get(`${lc}()`), index.byBareLabel?.get(lc)]) {
    for (const n of list || []) {
      if (n?.id != null && bareLabel(n.label) === name) seen.set(String(n.id), n);
    }
  }
  if (seen.size === 1) return { node: [...seen.values()][0], reason: null };
  return { node: null, reason: seen.size ? 'ambiguous' : 'unresolved' };
}

const LOCAL_SCAN_MAX_LINE = 2000;

/**
 * Связано ли имя в самом файле: объявление, параметр функции, деструктуризация, catch.
 * Тогда вызов идёт к локальному символу, и одноимённый узел из другого файла — чужой
 * (аудит, п. 1.2: `function f(log)`, `const { log } = deps`, `(resolve, reject) =>`).
 * Построчно и с пределом длины строки — минифицированный файл не разбирается.
 */
export function hasLocalBinding(lines, name) {
  const word = new RegExp(`(?:^|[^\\w$])${name.replace(/[$]/g, '\\$')}(?![\\w$])`);
  const decl = declarationRegex(name);
  for (const raw of lines || []) {
    const line = String(raw);
    if (line.length > LOCAL_SCAN_MAX_LINE || !line.includes(name)) continue;
    if (decl.test(line)) return true;
    // деструктуризация `const { a, b: name } = …` / `const [name] = …`
    const destr = /\b(?:const|let|var)\s*([{[][^=]*[}\]])\s*=/.exec(line);
    if (destr && word.test(destr[1])) return true;
    // параметры: `function f(…name…)`, `(…name…) =>`, `name =>`, `catch (name)`, метод `m(…name…) {`
    for (const m of line.matchAll(/\(([^()]*)\)\s*(?::[^={]*)?(?:=>|\{)/g)) {
      if (word.test(m[1])) return true;
    }
    if (new RegExp(`(?:^|[^\\w$.])${name.replace(/[$]/g, '\\$')}\\s*=>`).test(line)) return true;
  }
  return false;
}

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
 * @param {string} [opts.appDir] — `profile.layout.appDir`: префикс для путей графа, построенного от него
 * @returns {{ byPath: Map<string, object[]>, metrics: object }}
 */
export function buildSymbolCards(paths, { graphIndex, sections, windows, readLines, limits = {}, root = process.cwd(), appDir = '' } = {}) {
  const byPath = new Map();
  const skip = {};
  let candidates = 0; let resolved = 0; let cards = 0; let importResolved = 0;
  if (!graphIndex) {
    return { byPath, metrics: { candidates, resolved, cards, skip_reasons: { graph_unavailable: 1 } } };
  }
  const bodyMaxLines = Math.max(0, Number(limits.bodyMaxLines) || 0);
  appDir = String(appDir || '').replace(/^\.\/+|\/+$/g, '');
  const byFile = nodesByFile(graphIndex, root);
  const hasFile = (f) => byFile.has(f) || Boolean(appDir && f.startsWith(`${appDir}/`) && byFile.has(f.slice(appDir.length + 1)));
  const graphPath = (f) => (byFile.has(f) ? f : f.slice(appDir.length + 1));
  for (const path of paths || []) {
    if (!isSymbolCardPath(path)) continue;
    const added = addedLinesOfSection(sections?.get?.(path));
    const callerLines = readLines(path);
    // Импорты читаются из полного файла: шапка с импортами обычно вне окна вокруг правок.
    const headerLines = [...(callerLines || []), ...added];
    const bindings = relativeImportBindings(headerLines);
    const namespaces = new Set([...bindings].filter(([, b]) => b.imported === '*').map(([local]) => local));
    const used = extractUsedIdentifiers(added);
    const nsCalls = extractNamespaceCalls(added, namespaces);
    if (!used.size && !nsCalls.size) continue;
    const visible = `${sections?.get?.(path) ?? ''}\n${windows?.get?.(path) ?? ''}`;
    const external = packageImportedNames(headerLines);
    const callerIsTest = TEST_SOURCE_RE.test(path);
    // Модуль относительного импорта → файл графа; null — не нашли (реэкспорт, алиас).
    const moduleFile = (spec) => {
      const f = resolveImportPath(path, spec, hasFile);
      return f ? graphPath(f) : null;
    };
    const queue = [];
    for (const [name, count] of used) {
      const b = bindings.get(name);
      queue.push({ name, count, binding: b && b.imported !== '*' ? b : null });
    }
    for (const { ns, fn, count } of nsCalls.values()) queue.push({ name: fn, count, binding: { spec: bindings.get(ns).spec, imported: fn }, viaNamespace: true });

    const list = [];
    for (const { name, count, binding, viaNamespace } of queue) {
      candidates++;
      if (!viaNamespace && external.has(name)) { bump(skip, 'package_import'); continue; }
      if (declarationRegex(name).test(visible)) { bump(skip, 'visible'); continue; }
      let node = null;
      if (binding) {
        // Импорт называет модуль: ищем символ в НЁМ, а не уникальное имя по всему графу.
        const file = moduleFile(binding.spec);
        if (file && binding.imported !== 'default') node = nodeInFile(byFile, file, binding.imported);
        else if (viaNamespace) { bump(skip, file ? 'namespace_member_unresolved' : 'import_unresolved'); continue; }
      }
      if (!node) {
        const r = resolveCardNode(graphIndex, name);
        if (!r.node) { bump(skip, r.reason); continue; }
        node = r.node;
      } else {
        importResolved++;
      }
      let file = normalizeNodePath(node.source_file, root);
      const line = nodeLine(node);
      if (!file || !line) { bump(skip, 'no_location'); continue; }
      if (!isSymbolCardPath(file)) { bump(skip, 'non_js_target'); continue; }
      if (!callerIsTest && TEST_SOURCE_RE.test(file)) { bump(skip, 'test_source'); continue; }
      let lines = readLines(file);
      // граф построен от layout.appDir (`graphify update app`): пути узлов без префикса
      if (!lines && appDir && !file.startsWith(`${appDir}/`)) {
        const prefixed = `${appDir}/${file}`;
        const alt = readLines(prefixed);
        if (alt) { lines = alt; file = prefixed; }
      }
      if (!lines || line > lines.length) { bump(skip, 'unreadable'); continue; }
      if (!binding && file !== path && hasLocalBinding(callerLines, name)) { bump(skip, 'local_shadow'); continue; }
      // Строка графа должна ОБЪЯВЛЯТЬ имя, а не просто содержать его (аудит, п. 0.2):
      // узел на строке вызова `const r = run(a)` дал бы «объявление» — сам вызов.
      const symbol = binding?.imported && binding.imported !== 'default' ? binding.imported : name;
      if (!declarationRegex(symbol).test(lines[line - 1])) { bump(skip, 'stale_graph'); continue; }
      const { header, endIdx } = cardDeclaration(lines, line - 1, symbol);
      if (!header || !header.includes(symbol)) { bump(skip, 'not_a_declaration'); continue; }
      resolved++;
      const endLine = endIdx === null ? null : endIdx + 1;
      const card = { name: symbol, path: file, line, endLine, declaration: header, count };
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
  return { byPath, metrics: { candidates, resolved, cards, via_import: importResolved, skip_reasons: skip } };
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
 * изменённых строк, вызывающих символ; лимит по числу и по символам. Лимит — на чанк, а не
 * на файл, поэтому состав карточек файла зависит от соседей по чанку; в хэш RFC 05 он не
 * входит (аудит, п. 0.4) — перенос находок это не ломает, но инкрементальный прогон может
 * показать файлу другой набор, чем полный.
 * @returns {{ cards: string[], names: string[], dropped: number, chars: number, visible: number }}
 */
export function selectChunkCards(byPath, chunkPaths, { max = 20, maxChars = 3000, visibleText = '' } = {}) {
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
  const names = [];
  let chars = 0;
  let dropped = 0;
  let visible = 0;
  for (const c of ordered) {
    // объявление уже видно в окне другого файла этого чанка — дубль (аудит RFC 14, п. 1.8)
    if (visibleText && declarationRegex(c.name).test(visibleText)) { visible++; continue; }
    const text = renderSymbolCard(c);
    if (out.length >= max || chars + text.length > maxChars) { dropped++; continue; }
    out.push(text);
    names.push(c.name);
    chars += text.length;
  }
  return { cards: out, names, dropped, chars, visible };
}

/**
 * Утверждение об отсутствии вплотную к идентификатору в обратных кавычках — датчик класса,
 * ради которого был задуман шаг 1 RFC 14. Замер шага 0 видел только заголовки; здесь — и тела.
 * Квантификаторы ограничены: текст находки пишет модель по коду автора PR.
 */
const ABSENCE_WORD = '(?:не\\s+определ[а-яё]*|не\\s+существу[а-яё]*|не\\s+найден[а-яё]*|не\\s+экспортир[а-яё]*|не\\s+импортир[а-яё]*|is\\s+not\\s+defined|does\\s+not\\s+exist|is\\s+not\\s+exported|is\\s+undefined)';
const ABSENCE_CLAIM_RE = new RegExp(
  `\`[A-Za-z_$][\\w$.]{0,80}\`[^.;\`\\n]{0,40}${ABSENCE_WORD}|${ABSENCE_WORD}[^.;\`\\n]{0,40}\`[A-Za-z_$][\\w$.]{0,80}\``,
  'i',
);

export function isAbsenceClaim(finding) {
  return ABSENCE_CLAIM_RE.test(`${finding?.title ?? ''}\n${finding?.body ?? ''}`);
}

function mentionsAny(finding, names) {
  if (!names?.size) return false;
  const text = `${finding?.title ?? ''}\n${finding?.body ?? ''}\n${finding?.quote ?? ''}`;
  for (const n of names) {
    if (text.includes(n) && new RegExp(`(?:^|[^\\w$])${n.replace(/[$]/g, '\\$')}(?![\\w$])`).test(text)) return true;
  }
  return false;
}

/**
 * Связь находок с карточками (аудит RFC 14, п. 1.5): без неё пользу от шума не отличить.
 * Находка «упоминает карточку», если её текст называет символ, чья карточка была в чанке
 * её файла. Считается на выходе LLM (до фильтров) и в финале — с вердиктом verifier и
 * маршрутом: дошла ли до строки.
 *
 * @param {{ chunks: object[], llmFindings: object[], finalFindings: object[], inline: object[] }} args
 */
export function symbolCardsOutcome({ chunks = [], llmFindings = [], finalFindings = [], inline = [] } = {}) {
  const namesByFile = new Map();
  const allNames = new Set();
  for (const c of chunks) {
    for (const n of c.symbolCardNames || []) allNames.add(n);
    for (const p of c.paths || []) {
      if (!c.symbolCardNames?.length) continue;
      const set = namesByFile.get(p) || new Set();
      for (const n of c.symbolCardNames) set.add(n);
      namesByFile.set(p, set);
    }
  }
  const mentions = (f) => mentionsAny(f, namesByFile.get(f?.file));
  const llmMentioning = llmFindings.filter(mentions);
  const finalMentioning = finalFindings.filter(mentions);
  const inlineSet = new Set(inline);
  const byVerifier = {};
  const bySeverity = {};
  for (const f of finalMentioning) {
    const v = f.verifierStatus || 'none';
    byVerifier[v] = (byVerifier[v] || 0) + 1;
    bySeverity[f.severity || '?'] = (bySeverity[f.severity || '?'] || 0) + 1;
  }
  return {
    names: [...allNames].sort().slice(0, 50),
    llm: { findings: llmFindings.length, mentioning: llmMentioning.length, absenceClaims: llmFindings.filter(isAbsenceClaim).length },
    final: {
      findings: finalFindings.length,
      mentioning: finalMentioning.length,
      inline: finalMentioning.filter((f) => inlineSet.has(f)).length,
      bySeverity,
      byVerifier,
    },
  };
}
