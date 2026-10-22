/**
 * Окна контекста: вокруг строк-кандидатов (verifier) и вокруг изменённых строк (advisors
 * и жюри, RFC 07). Оба читателя пользуются одной функцией окон, но центрируются на разном:
 * verifier знает строку находки, advisors — только то, какие строки трогает PR.
 *
 * Verifier Pass B получает содержимое файлов из той же карты, что и основное ревью
 * (`readChangedFiles` в lib/diff.mjs): каждая запись — `### path` + тело в тройных
 * кавычках, до `limits.maxFileLines` строк (1500). Для второго прохода это почти
 * всегда лишнее: verifier проверяет 1–2 конкретные строки на файл, а платит за весь
 * файл — и делает это моделью, отличной от advisor'ов, то есть без всякого шанса
 * на попадание в кэш префикса.
 *
 * Здесь — чистая функция окон: вокруг каждой строки-кандидата берётся ±margin строк,
 * пересекающиеся окна сливаются, пропуски заменяются маркером `… (lines N–M not shown)`
 * (английский — как и весь промпт с 0.6; текст маркера цитируют системные промпты).
 * Маркер обязателен и в голове, и в хвосте: без него реальные номера строк невосстановимы,
 * а verifier обязан отвечать номером строки из исходного файла, а не из вырезки.
 *
 * Записи-маркеры добавленных файлов (`addedFileMarker` в lib/diff.mjs — заголовок без
 * тела в кавычках) проходят насквозь: резать там нечего.
 */

/** Заголовок + тело записи contentMap; null — запись не в формате «### path ```body```». */
function splitEntry(entry) {
  const m = /^(### [^\n]*)\n```\n([\s\S]*)\n```$/.exec(String(entry ?? ''));
  return m ? { head: m[1], body: m[2] } : null;
}

/** Слить пересекающиеся и соприкасающиеся интервалы (вход не обязан быть отсортирован). */
function mergeRanges(ranges) {
  const sorted = [...ranges].sort((a, b) => a[0] - b[0]);
  const out = [];
  for (const [s, e] of sorted) {
    const last = out[out.length - 1];
    // `s <= last[1] + 1` — соприкасающиеся окна тоже сливаем: маркер «пропущено 0 строк»
    // между ними был бы враньём.
    if (last && s <= last[1] + 1) last[1] = Math.max(last[1], e);
    else out.push([s, e]);
  }
  return out;
}

const skipMarker = (from, to) => `… (lines ${from}–${to} not shown)`;

/**
 * Вырезать из записи contentMap окна ±margin вокруг строк `lines`.
 * Возвращает запись в том же формате (заголовок + тройные кавычки).
 *
 * Запись возвращается БЕЗ изменений, когда: формат не распознан (маркер добавленного
 * файла), список строк пуст (нечего центрировать — отдаём файл целиком) или окна
 * покрывают файл целиком (резать нечего).
 */
export function windowFileEntry(entry, lines, { margin = 80 } = {}) {
  const parts = splitEntry(entry);
  if (!parts) return entry;
  const centers = (lines || []).map(Number).filter((n) => Number.isFinite(n) && n > 0);
  if (!centers.length) return entry;

  const bodyLines = parts.body.split('\n');
  const total = bodyLines.length;
  const windows = mergeRanges(centers.map((n) => [
    Math.max(1, n - margin),
    Math.min(total, n + margin),
  ]).filter(([s, e]) => s <= e && s <= total));
  if (!windows.length) return entry;
  if (windows.length === 1 && windows[0][0] === 1 && windows[0][1] === total) return entry;

  const out = [];
  let prev = 0;
  for (const [s, e] of windows) {
    if (s > prev + 1) out.push(skipMarker(prev + 1, s - 1));
    out.push(...bodyLines.slice(s - 1, e));
    prev = e;
  }
  if (prev < total) out.push(skipMarker(prev + 1, total));
  return `${parts.head}\n\`\`\`\n${out.join('\n')}\n\`\`\``;
}

/**
 * Контекст изменённых файлов для advisors и жюри (RFC 07): в каждом файле — окна ±margin
 * вокруг строк, которые PR трогает, а не файл целиком.
 *
 * Центр окна здесь другой, чем у verifier'а, и другим быть не может: advisors бегут ДО
 * первой находки, кандидатов ещё нет. Единственный детерминированный центр на этой стадии —
 * `validLines` из parseDiff: номера RIGHT-стороны для `+`-строк и контекстных строк хунка.
 *
 * Файл короче `minChars` отдаётся целиком: там, где файл помещается в пару окон, усечение
 * не экономит ни байта, а риск утверждения об отсутствии по вырезке берёт целиком.
 *
 * `margin = 0` — это ВЫКЛЮЧЕНО (окна не считаются вовсе, карта возвращается как есть), а не
 * «окно нулевой ширины» (оставить только сами изменённые строки) — ровно наоборот по смыслу.
 * Это и есть откат решения одной переменной.
 *
 * @param {Map<string, string>} contentMap записи из readChangedFiles
 * @param {Map<string, {validLines: Set<number>}>} files карта parseDiff — центры окон
 * @param {{margin?: number, minChars?: number}} opts
 * @returns {{ map: Map<string, string>, windowed: Set<string>, before: number, after: number }}
 */
export function windowChunkFileContents(contentMap, files, { margin = 40, minChars = 8000 } = {}) {
  const source = contentMap || new Map();
  let before = 0;
  for (const [, entry] of source) before += String(entry ?? '').length;
  // Возвращаем ИСХОДНУЮ карту, а не её копию: при выключенных окнах поведение обязано быть
  // побайтово прежним, и лишняя пересборка Map только приглашает расхождение.
  if (!(margin > 0)) return { map: source, windowed: new Set(), before, after: before };

  const map = new Map();
  const windowed = new Set();
  let after = 0;
  for (const [path, entry] of source) {
    const text = String(entry ?? '');
    // Порог считаем по ЗАПИСИ целиком (заголовок + кавычки), как и before/after: иначе
    // «сколько стоит файл» в логе и «попал ли он под порог» — разные величины.
    const lines = text.length > minChars ? [...(files?.get(path)?.validLines || [])] : [];
    const out = lines.length ? windowFileEntry(text, lines, { margin }) : text;
    if (out !== text) windowed.add(path);
    map.set(path, out);
    after += out.length;
  }
  return { map, windowed, before, after };
}

/**
 * Строки записи файла (формат readChangedFiles), где используется `symbol`: вызов `symbol(` /
 * `obj.symbol(` как отдельное слово или — для имени с заглавной (React-компонент) — JSX-тег
 * `<Symbol`. Для blast-находки (RFC 21): строка из графа — место ребра, а не использования
 * (бывает строкой импорта), и окно verifier'а должно стоять там, где проявляется контракт.
 * @returns {number[]} номера строк (с 1) в порядке файла
 */
export function findCallLines(entry, symbol) {
  const parts = splitEntry(entry);
  const name = String(symbol || '').trim();
  if (!parts || !/^[\w$]+$/.test(name)) return [];
  const esc = name.replace(/\$/g, '\\$');
  const call = `(^|[^\\w$])${esc}\\s*\\(`;
  // BLV-010: компонент проявляет контракт в JSX, а не в вызове.
  const jsx = /^[A-Z]/.test(name) ? `|<${esc}(?=[\\s/>]|$)` : '';
  const re = new RegExp(`${call}${jsx}`);
  const out = [];
  parts.body.split('\n').forEach((line, i) => { if (re.test(line)) out.push(i + 1); });
  return out;
}

/**
 * Контекст файлов для verifier: только файлы кандидатов, и в них — только окна
 * вокруг строк кандидатов.
 *
 * Файл, у которого ХОТЬ ОДИН кандидат без строки, отдаётся целиком: центрировать окно
 * не на чем, а выкинуть область, к которой относится находка без строки, — значит
 * лишить verifier'а возможности её подтвердить.
 *
 * Файлы вне PR (`outside`, RFC 21) — вызывающий код blast-кандидатов: только окно
 * ±outsideMargin вокруг строки вызова, целиком не отдаются никогда.
 *
 * @param {Map<string, string>} contentMap записи из readChangedFiles
 * @param {Array<{file?: string, line?: number}>} candidates кандидаты verifier'а
 * @param {{ margin?: number, outside?: Map<string, string>, outsideMargin?: number }} [opts]
 * @returns {{ text: string, files: number, before: number, after: number }}
 */
export function buildVerifierFileContents(contentMap, candidates, { margin = 80, outside = null, outsideMargin = 20 } = {}) {
  const byFile = new Map();
  for (const c of candidates || []) {
    const path = c?.file;
    if (!path) continue;
    if (!byFile.has(path)) byFile.set(path, { lines: [], full: false });
    const rec = byFile.get(path);
    const line = Number(c.line);
    if (Number.isFinite(line) && line > 0) rec.lines.push(line);
    else rec.full = true;
  }

  const original = [];
  const kept = [];
  for (const [path, entry] of contentMap || []) {
    const rec = byFile.get(path);
    if (!rec) continue;
    original.push(entry);
    kept.push(rec.full ? entry : windowFileEntry(entry, rec.lines, { margin }));
  }
  for (const [path, entry] of outside || []) {
    const rec = byFile.get(path);
    if (!rec?.lines.length || contentMap?.has(path)) continue;
    original.push(entry);
    kept.push(windowFileEntry(entry, rec.lines, { margin: outsideMargin }));
  }
  const text = kept.join('\n');
  // `before` считается ровно так же, как собирался старый контекст (join('\n') по тем же
  // записям) — иначе замер в логе сравнивал бы разные величины.
  return { text, files: kept.length, before: original.join('\n').length, after: text.length };
}
