import { readFile } from 'node:fs/promises';
import { resolve, sep } from 'node:path';

/** Unescape git C-quoting внутри quoted-пути: \\ \" \n \t и октальные \ooo (байты UTF-8). */
export function unescapeGitPath(s) {
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

/** Путь из строки `+++ b/path` (quoted-форма `+++ "b/path with spaces"` — с unescape). */
export const parseDiffNewPath = (line) => {
  const l = stripCr(line);
  const q = l.match(/^\+\+\+ "b\/((?:[^"\\]|\\.)*)"$/);
  if (q) return unescapeGitPath(q[1]);
  return l.match(/^\+\+\+ b\/(.+)$/)?.[1] ?? null;
};

/** Путь (b-side) из строки `diff --git a/x b/y` (quoted-форма — с unescape). */
export const parseDiffGitPath = (line) => {
  const l = stripCr(line);
  const q = l.match(/ "b\/((?:[^"\\]|\\.)*)"$/);
  if (q) return unescapeGitPath(q[1]);
  return l.match(/ b\/(.+)$/)?.[1] ?? null;
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
  const files = new Map();
  const annotatedParts = [];
  const changedLines = [];
  let curPath = null;
  let newLine = 0;

  for (const raw of diff.split('\n')) {
    if (raw.startsWith('diff --git')) { curPath = null; annotatedParts.push(raw); continue; }
    if (raw.startsWith('+++ ')) {
      curPath = parseDiffNewPath(raw);
      // text: номер строки RIGHT-стороны → её содержимое. Потребитель — findingFingerprint
      // и filterStalePriorFindings (lib/findings.mjs): «жива» ли находка прошлого прогона,
      // решается по коду строки, а не по её номеру. Поле обязано пережить рефакторинг парсера.
      if (curPath && !files.has(curPath)) files.set(curPath, { validLines: new Set(), changed: [], text: new Map() });
      annotatedParts.push(raw);
      continue;
    }
    if (raw.startsWith('--- ')) { annotatedParts.push(raw); continue; }
    const hunk = raw.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (hunk) { newLine = Number(hunk[1]); annotatedParts.push(raw); continue; }
    if (raw.startsWith('\\')) { annotatedParts.push(raw); continue; }
    if (!curPath) { annotatedParts.push(raw); continue; }
    const rec = files.get(curPath);
    if (raw.startsWith('+') && !raw.startsWith('+++')) {
      rec.validLines.add(newLine);
      rec.changed.push(raw.slice(1));
      rec.text.set(newLine, raw.slice(1));
      changedLines.push(raw.slice(1));
      annotatedParts.push(`${String(newLine).padStart(5)}: ${raw}`);
      newLine++;
    } else if (raw.startsWith('-') && !raw.startsWith('---')) {
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

export function extractChangedSymbols(changedText, maxSymbols = 25) {
  const out = new Set();
  const add = (n) => { if (n && n.length >= 3) out.add(n); };
  const patterns = [
    /\bexport\s+(?:default\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)/g,
    /\bexport\s+(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)/g,
    /\bexport\s+(?:interface|type|enum)\s+([A-Za-z_$][\w$]*)/g,
    /\bexport\s+const\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:<[^>]*>\s*)?\(/g,
    /\bexport\s+const\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?function/g,
  ];
  for (const re of patterns) { let m; while ((m = re.exec(changedText))) add(m[1]); }
  return [...out].slice(0, maxSymbols);
}

export async function readChangedFiles(files, limits, log = () => {}) {
  const map = new Map();
  const root = resolve(process.cwd());
  for (const path of files.keys()) {
    if (/(^|\/)package-lock\.json$|\.(lock|png|jpe?g|gif|svg|ico|pdf|min\.js|map|heic|heif|webp|avif|xlsx?|docx?|pptx?|zip|gz|tgz|bz2|7z|rar|mp[34]|mov|avi|woff2?|ttf|otf|eot|wasm|jar|pyc|so|dylib|dll|exe)$/i.test(path)) continue;
    const abs = resolve(root, path);
    if (abs !== root && !abs.startsWith(root + sep)) { log(`пропуск файла вне репо: ${path}`); continue; }
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

export function buildChunks(annotated, contentMap, limits) {
  // `+++` есть не у всех секций (rename-only, mode-only): для них путь берём из заголовка
  // `diff --git`. Иначе файл не попадёт в состав чанка, и находки по нему будут ошибочно
  // понижены как «про невидимый файл».
  const pathOf = (seg) => parseDiffNewPath(seg.match(/^\+\+\+ .*$/m)?.[0] || '')
    || parseDiffGitPath(seg.match(/^diff --git .*$/m)?.[0] || '')
    || '';
  const fileSegs = annotated.split(/(?=^diff --git )/m).filter((s) => s.trim());
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
    const pieces = splitSegmentByHunks(diff, effChunk);
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
