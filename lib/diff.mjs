/**
 * Фаза 1, шаг 1: lib/diff.mjs — тонкая JS-обёртка над Rust-бинарём rust/target/debug/diff.
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Все потребители
 * (pipeline.mjs, lib/router.mjs, lib/eval.mjs, тесты) вызывают функции diff СИНХРОННО,
 * поэтому вместо async execFile используется execFileSync: бинарь вызывается на каждую
 * операцию с JSON {"op", ...args} на stdin, ответ — JSON с результатом в формате JS-возврата.
 *
 * Резолвинг бинаря (по аналогии с astgrep/graphify):
 *   AI_REVIEWER_DIFF_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/diff;
 *   AI_REVIEWER_DIFF_FORCE_JS=1 — принудительно чистый JS (паритетные прогоны/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm test/eval
 * идут раньше cargo build, см. .github/workflows/ci.yml), обёртка деградирует на чистый
 * JS fallback ниже — эталонную копию исходной реализации. Если бинарь ЕСТЬ, но упал
 * (non-zero exit) — ошибка пробрасывается, fallback не используется: молчаливая
 * подмена реализации при битом Rust-коде скрывала бы регрессии.
 *
 * readChangedFiles не портирован в Rust намеренно: это fs-I/O с log-колбэком и
 * process.cwd(), а не чистая функция; поведение сохранено без изменений.
 */

import { existsSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { resolveContainedPath } from './util.mjs';
import { fileURLToPath } from 'node:url';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_DIFF_BIN = resolve(__dir, '../rust/target/debug/diff');

function resolveDiffBin() {
  if (process.env.AI_REVIEWER_DIFF_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_DIFF_BIN || '').trim() || DEFAULT_DIFF_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveDiffBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

function contentMapToJson(contentMap) {
  return Object.fromEntries([...(contentMap || [])].map(([k, v]) => [k, String(v ?? '')]));
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

/** Unescape git C-quoting внутри quoted-пути: \\ \" \n \t и октальные \ooo (байты UTF-8). */
export function unescapeGitPath(s) {
  const r = runRust('unescapeGitPath', { s });
  if (r !== null) return r;
  return unescapeGitPathJs(s);
}

/** Путь из строки `+++ b/path` (quoted-форма `+++ "b/path with spaces"` — с unescape). */
export const parseDiffNewPath = (line) => {
  const r = runRust('parseDiffNewPath', { line });
  if (r !== null) return r; // null (не распознано) приходит как JSON null
  return parseDiffNewPathJs(line);
};

/** Путь (b-side) из строки `diff --git a/x b/y` (quoted-форма — с unescape). */
export const parseDiffGitPath = (line) => {
  const r = runRust('parseDiffGitPath', { line });
  if (r !== null) return r;
  return parseDiffGitPathJs(line);
};

/**
 * @typedef {Object} DiffFileRecord — запись файла из parseDiff (`files.get(path)`).
 * @property {Set<number>} validLines номера строк RIGHT-стороны, на которые можно вешать inline
 * @property {string[]} changed тексты изменённых строк (`+`/`-`) для blast-radius и prescan
 * @property {Map<number,string>} text номер строки RIGHT-стороны → её текст; читают
 *   `findingFingerprint` и `filterStalePriorFindings` (lib/findings.mjs) — по нему решается,
 *   жива ли находка прошлого прогона. Формат менять только вместе с ними.
 */

/** @returns {{files: Map<string, DiffFileRecord>, annotated: string, changedText: string}} */
export function parseDiff(diff) {
  const r = runRust('parseDiff', { diff });
  if (r !== null) {
    const files = new Map();
    for (const [path, rec] of Object.entries(r.files || {})) {
      files.set(path, {
        validLines: new Set(rec.validLines),
        changed: rec.changed,
        text: new Map(Object.entries(rec.text || {}).map(([k, v]) => [Number(k), v])),
      });
    }
    return { files, annotated: r.annotated, changedText: r.changedText };
  }
  return parseDiffJs(diff);
}

/** Имена экспортов из текста (дифф или целый файл). */
export function extractChangedSymbols(changedText, maxSymbols = 25) {
  const r = runRust('extractChangedSymbols', { text: changedText, maxSymbols });
  if (r !== null) return r;
  return extractChangedSymbolsJs(changedText, maxSymbols);
}

/**
 * Символы для Graphify blast-radius: сначала из изменённых строк, затем добор
 * экспортами из полных текстов затронутых `.ts`/`.js`/`.py` файлов (правка тела функции
 * без строки `export`/`def` иначе давала 0 symbols → пустой blast).
 */
export function collectBlastSymbols(changedText, contentMap, maxSymbols = 25) {
  const r = runRust('collectBlastSymbols', {
    changedText, contentMap: contentMapToJson(contentMap), maxSymbols,
  });
  if (r !== null) return r;
  return collectBlastSymbolsJs(changedText, contentMap, maxSymbols);
}

/**
 * Пути, добавленные этим PR (секция `diff --git` с `new file mode`).
 * Чистый JS: I/O нет, но и Rust-порта пока нет — при портировании обязана попасть в
 * rust/src/diff.rs вместе с parity-кейсом, иначе правка уедет только в fallback.
 */
/**
 * Строки диффа — только по `\n`, как у git и Rust-порта (`rust/src/diff.rs`). Флаг `/m` и `.`
 * в JS считают границей строки ещё `\r`, U+2028 и U+2029, а их автор PR кладёт внутрь своей
 * строки кода: `+x\u2028deleted file mode 100644` подделывал бы заголовок секции.
 */
const diffLines = (text) => String(text || '').split('\n');
const firstDiffLine = (seg, prefix) => diffLines(seg).find((l) => l.startsWith(prefix)) || '';
/** Секции по строкам `diff --git ` (преамбула до первой — отдельным куском, как раньше). */
const splitDiffSections = (diff) => String(diff || '').split(/(?<=\n)(?=diff --git )/);

export function collectAddedPaths(diff) {
  const out = new Set();
  for (const seg of splitDiffSections(diff)) {
    if (!diffLines(seg).some((l) => l.startsWith('new file mode '))) continue;
    const path = parseDiffNewPath(firstDiffLine(seg, '+++ '))
      || parseDiffGitPath(firstDiffLine(seg, 'diff --git '));
    if (path) out.add(path);
  }
  return out;
}

/**
 * Пути, удалённые этим PR. Ключ — b-путь из `diff --git`, тот же, под которым parseDiff
 * кладёт файл в `files`. Читатель — prescan: линтеру нечего проверять в файле, которого в
 * рабочем дереве уже нет. Чистый JS, как collectAddedPaths.
 *
 * Признак — только строка `deleted file mode` в ЗАГОЛОВКЕ секции (до первого `@@`). Искать
 * `+++ /dev/null` по всей секции нельзя: добавленная строка с текстом `++ /dev/null` в диффе
 * выглядит ровно так, и живой файл выпадал из prescan — обход blockRules (RFC 17).
 */
export function collectDeletedPaths(diff) {
  const out = new Set();
  for (const seg of splitDiffSections(diff)) {
    if (!isDeletedSectionHeader(seg)) continue;
    const path = parseDiffGitPath(firstDiffLine(seg, 'diff --git '));
    if (path) out.add(path);
  }
  return out;
}

/** Заголовок секции (до первого хунка) объявляет удаление файла. */
function isDeletedSectionHeader(seg) {
  for (const line of diffLines(seg)) {
    if (line.startsWith('@@ ')) return false;
    if (line.startsWith('deleted file mode ')) return true;
  }
  return false;
}

/**
 * Полный текст добавленного файла — это его собственный дифф: каждая строка пришла как `+`.
 * Прикладывать его вторым экземпляром значит платить за один и тот же код дважды в каждом
 * вызове каждой роли. Вместо контента кладём одну строку-указатель: модель должна понимать,
 * что контекст не потерян, а совпадает с диффом ниже (без неё «файла нет в контексте»
 * превращается в находку про невидимый код).
 */
const addedFileMarker = (path) => `### ${path}\n(новый файл — его полное содержимое целиком приведено в диффе ниже)`;

/**
 * Читает затронутые файлы для контекста модели. Не портирован в Rust: fs-I/O + log-колбэк.
 * `addedPaths` — пути, добавленные этим PR (см. addedFileMarker).
 */
export async function readChangedFiles(files, limits, log = () => {}, { addedPaths = null } = {}) {
  const map = new Map();
  const root = resolve(process.cwd());
  for (const path of files.keys()) {
    if (/(^|\/)package-lock\.json$|\.(lock|png|jpe?g|gif|svg|ico|pdf|min\.js|map|heic|heif|webp|avif|xlsx?|docx?|pptx?|zip|gz|tgz|bz2|7z|rar|mp[34]|mov|avi|woff2?|ttf|otf|eot|wasm|jar|pyc|so|dylib|dll|exe)$/i.test(path)) continue;
    if (addedPaths?.has(path)) { map.set(path, addedFileMarker(path)); continue; }
    // Containment после realpath: symlink, уже лежащий в репозитории (PR меняет его цель или
    // соседний код), не должен отдавать модели файл снаружи. `resolve` + startsWith этого не
    // видел. Нет файла (удалён в PR) — тихий пропуск, как раньше.
    let abs;
    try {
      abs = await resolveContainedPath(path, { rootDir: root, mustExist: true });
    } catch (e) {
      if (e?.code !== 'ENOENT') log(`пропуск файла вне репо: ${path}`);
      continue;
    }
    let buf;
    try { buf = await readFile(abs); } catch { continue; }
    // Страховка поверх списка расширений: бинарь определяем по NUL. Окно — весь файл,
    // но не более 64КБ (детерминированная защита без чтения гигабайтов в память).
    if (buf.subarray(0, Math.min(buf.length, 65_536)).includes(0)) { log(`пропуск бинарного файла (NUL byte): ${path}`); continue; }
    const content = buf.toString('utf8');
    const lines = content.split('\n');
    let body = lines.slice(0, limits.maxFileLines).join('\n');
    if (lines.length > limits.maxFileLines) {
      body += `\n… (показаны первые ${limits.maxFileLines} из ${lines.length} строк)`;
    }
    map.set(path, `### ${path}\n\`\`\`\n${body}\n\`\`\``);
  }
  return map;
}

export function splitSegmentByHunks(seg, maxBytes) {
  const r = runRust('splitSegmentByHunks', { seg, maxBytes });
  if (r !== null) return r;
  return splitSegmentByHunksJs(seg, maxBytes);
}

/**
 * Собирает чанки для LLM. Возвращает массив {annotated, fileContents, paths, partialPaths,
 * unknownPaths?}: unknownPaths присутствует только когда true (как spread в JS-оригинале).
 */
export function buildChunks(annotated, contentMap, limits) {
  const r = runRust('buildChunks', {
    annotated, contentMap: contentMapToJson(contentMap), limits,
  });
  if (r !== null) return r;
  return buildChunksJs(annotated, contentMap, limits);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/diff.mjs до Фазы 1).
// Используется только когда Rust-бинарь отсутствует; при битом бинаре НЕ подменяет его.
// ---------------------------------------------------------------------------

function unescapeGitPathJs(s) {
  if (!s.includes('\\')) return s;
  const bytes = [];
  const push = (ch) => bytes.push(...Buffer.from(ch, 'utf8'));
  for (let i = 0; i < s.length; i++) {
    if (s[i] !== '\\') { push(s[i]); continue; }
    const n = s[i + 1];
    if (n === '\\' || n === '"') { push(n); i++; continue; }
    if (n === 'n') { bytes.push(10); i++; continue; }
    if (n === 't') { bytes.push(9); i++; continue; }
    const oct = s.slice(i + 1, i + 4);
    if (/^[0-7]{1,3}$/.test(oct)) { bytes.push(parseInt(oct, 8)); i += oct.length; continue; }
    push(s[i]); // неизвестный escape — оставляем как есть
  }
  return Buffer.from(bytes).toString('utf8');
}

const stripCr = (line) => (line.endsWith('\r') ? line.slice(0, -1) : line);

const parseDiffNewPathJs = (line) => {
  const l = stripCr(line);
  const q = l.match(/^\+\+\+ "b\/((?:[^"\\]|\\.)*)"$/);
  if (q) return unescapeGitPathJs(q[1]);
  return l.match(/^\+\+\+ b\/(.+)$/)?.[1] ?? null;
};

const parseDiffGitPathJs = (line) => {
  const l = stripCr(line);
  const q = l.match(/ "b\/((?:[^"\\]|\\.)*)"$/);
  if (q) return unescapeGitPathJs(q[1]);
  return l.match(/ b\/(.+)$/)?.[1] ?? null;
};

function parseDiffJs(diff) {
  const files = new Map();
  const annotatedParts = [];
  const changedLines = [];
  let curPath = null;
  let gitPath = null;
  let deletedHeader = false;
  let inHunk = false;
  let newLine = 0;

  for (const raw of diff.split('\n')) {
    if (raw.startsWith('diff --git')) {
      curPath = null; gitPath = parseDiffGitPathJs(raw); deletedHeader = false; inHunk = false;
      annotatedParts.push(raw); continue;
    }
    if (!inHunk && raw.startsWith('deleted file mode ')) deletedHeader = true;
    // Заголовки `---`/`+++` бывают только до первого хунка секции. Внутри хунка это строки
    // содержимого `-- …`/`++ …`: прими их за заголовок — curPath сбрасывался, и всё, что ниже,
    // выпадало из changed/changedText/validLines (подделка строкой в комментарии, RFC 17).
    if (!inHunk && raw.startsWith('+++ ')) {
      // Удалённый файл (`+++ /dev/null`) — путь из `diff --git`: без него удаление
      // `auth/guard.ts` не попадало ни в changedPaths, ни в роутер, ни в changedText
      // для blast-radius — PR «удалил проверку + поправил README» уходил в light (RFC 17).
      // Только в заголовке секции с `deleted file mode`: внутри хунка `+++ /dev/null` —
      // это добавленная строка `++ /dev/null`, а не заголовок.
      const deleted = deletedHeader && !inHunk && /^\+\+\+ \/dev\/null$/.test(stripCr(raw));
      curPath = parseDiffNewPathJs(raw) ?? (deleted ? gitPath : null);
      // text: номер строки RIGHT-стороны → её содержимое. Потребитель — findingFingerprint
      // и filterStalePriorFindings (lib/findings.mjs): «жива» ли находка прошлого прогона,
      // решается по коду строки, а не по её номеру. Поле обязано пережить рефакторинг парсера.
      if (curPath && !files.has(curPath)) files.set(curPath, { validLines: new Set(), changed: [], text: new Map() });
      annotatedParts.push(raw);
      continue;
    }
    if (!inHunk && raw.startsWith('--- ')) { annotatedParts.push(raw); continue; }
    const hunk = raw.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (hunk) { newLine = Number(hunk[1]); inHunk = true; annotatedParts.push(raw); continue; }
    if (raw.startsWith('\\')) { annotatedParts.push(raw); continue; }
    if (!curPath) { annotatedParts.push(raw); continue; }
    const rec = files.get(curPath);
    if (raw.startsWith('+') && (inHunk || !raw.startsWith('+++'))) {
      rec.validLines.add(newLine);
      rec.changed.push(raw.slice(1));
      rec.text.set(newLine, raw.slice(1));
      changedLines.push(raw.slice(1));
      annotatedParts.push(`${String(newLine).padStart(5)}: ${raw}`);
      newLine++;
    } else if (raw.startsWith('-') && (inHunk || !raw.startsWith('---'))) {
      rec.changed.push(raw.slice(1));
      changedLines.push(raw.slice(1));
      annotatedParts.push(`     : ${raw}`);
    } else {
      rec.validLines.add(newLine);
      rec.text.set(newLine, raw.startsWith(' ') ? raw.slice(1) : raw);
      annotatedParts.push(`${String(newLine).padStart(5)}: ${raw}`);
      newLine++;
    }
  }
  return { files, annotated: annotatedParts.join('\n'), changedText: changedLines.join('\n') };
}

const EXPORT_SYMBOL_PATTERNS = [
  /\bexport\s+(?:default\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)/g,
  /\bexport\s+(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)/g,
  /\bexport\s+(?:interface|type|enum)\s+([A-Za-z_$][\w$]*)/g,
  /\bexport\s+const\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:<[^>]*>\s*)?\(/g,
  /\bexport\s+const\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?function/g,
  // `export { foo, bar as baz }` — частый случай shared API без `export function` в hunk.
  /\bexport\s*\{([^}]+)\}/g,
  // Python: аналог экспорта — публичное (без `_`) определение верхнего уровня, в колонке 0.
  // Методы (с отступом) не берём: имена вроде `get`/`update` в графе неоднозначны и
  // съели бы лимит символов. Якорь `(?:^|\n)` вместо флага `m`: у JS-флага `m` границы
  // строк шире (`\r`, U+2028), чем у Rust `(?m)`, — паритет держится на одинаковом тексте.
  /(?:^|\n)(?:async[ \t]+)?def[ \t]+([A-Za-z][A-Za-z0-9_]*)[ \t]*\(/g,
  /(?:^|\n)class[ \t]+([A-Za-z][A-Za-z0-9_]*)[ \t]*[(:]/g,
];

function extractChangedSymbolsJs(changedText, maxSymbols = 25) {
  const out = new Set();
  // trim обязателен: у одиночного `export { foo }` группа захватывает пробелы вокруг имени,
  // и в blast-radius уходило « foo » — такого узла в графе нет, символ молча терялся.
  const add = (n) => { const name = n?.trim(); if (name && name.length >= 3) out.add(name); };
  for (const re of EXPORT_SYMBOL_PATTERNS) {
    const pattern = new RegExp(re.source, re.flags);
    let m;
    while ((m = pattern.exec(changedText))) {
      if (m[1].includes(',') || m[1].includes(' as ')) {
        for (const part of m[1].split(',')) {
          const token = part.trim();
          if (!token || token === 'type' || token === 'typeof') continue;
          // `bar as baz` → для blast-radius нужен исходный binding `bar`.
          const name = token.replace(/^type\s+/, '').split(/\s+as\s+/i)[0]?.trim();
          add(name);
        }
      } else {
        add(m[1]);
      }
    }
  }
  return [...out].slice(0, maxSymbols);
}

function collectBlastSymbolsJs(changedText, contentMap, maxSymbols = 25) {
  const out = [];
  const seen = new Set();
  const push = (name) => {
    if (!name || name.length < 3 || seen.has(name) || out.length >= maxSymbols) return;
    seen.add(name);
    out.push(name);
  };
  for (const name of extractChangedSymbolsJs(changedText, maxSymbols)) push(name);
  if (!contentMap || out.length >= maxSymbols) return out;
  for (const [path, text] of contentMap) {
    if (!/\.(tsx?|mts|cts|mjs|cjs|jsx?|pyi?)$/i.test(path)) continue;
    for (const name of extractChangedSymbolsJs(String(text || ''), maxSymbols)) {
      push(name);
      if (out.length >= maxSymbols) return out;
    }
  }
  return out;
}

function splitSegmentByHunksJs(seg, maxBytes) {
  if (seg.length <= maxBytes) return [seg];
  const lines = seg.split('\n');
  const firstHunk = lines.findIndex((l) => l.startsWith('@@ '));
  if (firstHunk < 0) {
    const out = [];
    for (let off = 0; off < seg.length; off += maxBytes) out.push(seg.slice(off, off + maxBytes));
    return out;
  }
  const header = lines.slice(0, firstHunk).join('\n');
  const hunks = [];
  let cur = [];
  for (const l of lines.slice(firstHunk)) {
    if (l.startsWith('@@ ') && cur.length) { hunks.push(cur.join('\n')); cur = []; }
    cur.push(l);
  }
  if (cur.length) hunks.push(cur.join('\n'));
  const pieces = [];
  let buf = [], size = header.length;
  const flush = () => { if (buf.length) { pieces.push(`${header}\n${buf.join('\n')}`); buf = []; size = header.length; } };
  for (const h of hunks) {
    if (buf.length && size + h.length + 1 > maxBytes) flush();
    buf.push(h); size += h.length + 1;
  }
  flush();
  return pieces.length ? pieces : [seg];
}

function buildChunksJs(annotated, contentMap, limits) {
  // `+++` есть не у всех секций (rename-only, mode-only): для них путь берём из заголовка
  // `diff --git`. Иначе файл не попадёт в состав чанка, и находки по нему будут ошибочно
  // понижены как «про невидимый файл».
  const pathOf = (seg) => parseDiffNewPathJs(firstDiffLine(seg, '+++ '))
    || parseDiffGitPathJs(firstDiffLine(seg, 'diff --git '))
    || '';
  const fileSegs = splitDiffSections(annotated).filter((s) => s.trim());
  let total = 0;
  for (const seg of fileSegs) {
    total += Math.min(seg.length, limits.maxDiffBytes)
      + Math.min((contentMap.get(pathOf(seg)) || '').length, limits.maxContextBytes);
  }
  const effChunk = Math.max(limits.chunkBytes, Math.ceil(total / limits.maxChunks));
  const items = [];
  // Файл, обрезанный лимитом, виден не целиком ровно как разрезанный по хункам — и должен
  // получить ту же пометку, иначе карта снова обещает больше, чем показывает.
  const truncated = new Set();
  for (const seg of fileSegs) {
    const path = pathOf(seg);
    const diff = seg.length > limits.maxDiffBytes
      ? seg.slice(0, limits.maxDiffBytes) + '\n… (файл обрезан в диффе)' : seg;
    if (diff !== seg && path) truncated.add(path);
    let content = contentMap.get(path) || '';
    if (content.length > limits.maxContextBytes) {
      content = content.slice(0, limits.maxContextBytes) + '\n… (файл обрезан)';
      if (path) truncated.add(path);
    }
    const pieces = splitSegmentByHunksJs(diff, effChunk);
    pieces.forEach((s, pieceIdx) => {
      // Полный контент файла — только в ПЕРВОМ куске файла: не дублируем в каждый чанк
      // (экономия токенов; у следующих кусков остаётся аннотированный дифф с номерами строк).
      items.push({
        s,
        path,
        // «Частично виден» считается ПО ЧАНКУ (сколько кусков файла сюда попало против
        // общего числа), а не по файлу: иначе чанк, которому достались все куски, зря
        // объявляется неполным и модель перестраховывается на ровном месте.
        content: pieceIdx === 0 ? content : '',
        size: s.length + (pieceIdx === 0 ? content.length : 0),
      });
    });
  }
  const chunks = [];
  let curSegs = [], curContent = [], bytes = 0;
  let curPieces = new Map(); // путь → сколько его кусков попало в текущий чанк
  let curUnknown = false; // в чанке есть секция, у которой не удалось определить путь
  const totalPieces = new Map(); // путь → сколько кусков всего у файла
  // Считаем куски, а не берём it.pieces: один путь может прийти несколькими секциями диффа
  // (например, rename + модификация), и тогда «всего кусков» — это их сумма.
  for (const it of items) if (it.path) totalPieces.set(it.path, (totalPieces.get(it.path) || 0) + 1);
  const flush = () => {
    if (!curSegs.length) return;
    // paths — файлы PR, реально видимые в этом чанке. Из них строится карта PR в промпте:
    // без неё чанк без файла X заявляет «изменений в X нет» (absence-claim), а жюри
    // подтверждает это, сидя в том же слепом контексте.
    const paths = [...curPieces.keys()];
    chunks.push({
      annotated: curSegs.join('\n'),
      fileContents: curContent.join('\n\n'),
      // Секция без обоих заголовков (`+++` и `diff --git`) не даёт пути: состав чанка неполный,
      // и решать по нему «этот файл не виден» нельзя — потребители обязаны деградировать.
      ...(curUnknown ? { unknownPaths: true } : {}),
      paths,
      partialPaths: paths.filter((p) => truncated.has(p) || curPieces.get(p) < (totalPieces.get(p) || 1)),
    });
    curSegs = []; curContent = []; curPieces = new Map(); curUnknown = false; bytes = 0;
  };
  for (const it of items) {
    if (curSegs.length && bytes + it.size > effChunk) flush();
    curSegs.push(it.s); if (it.content) curContent.push(it.content);
    if (it.path) curPieces.set(it.path, (curPieces.get(it.path) || 0) + 1);
    else curUnknown = true;
    bytes += it.size;
  }
  flush();
  // paths: [] — состав чанка неизвестен (дифф без заголовков `diff --git`); потребители
  // трактуют это как «весь PR здесь» и карту не рисуют.
  return chunks.length ? chunks : [{ annotated, fileContents: '', paths: [], partialPaths: [] }];
}
