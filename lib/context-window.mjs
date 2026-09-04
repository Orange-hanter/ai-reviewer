/**
 * Окна контекста вокруг строк-кандидатов.
 *
 * Verifier Pass B получает содержимое файлов из той же карты, что и основное ревью
 * (`readChangedFiles` в lib/diff.mjs): каждая запись — `### path` + тело в тройных
 * кавычках, до `limits.maxFileLines` строк (1500). Для второго прохода это почти
 * всегда лишнее: verifier проверяет 1–2 конкретные строки на файл, а платит за весь
 * файл — и делает это моделью, отличной от advisor'ов, то есть без всякого шанса
 * на попадание в кэш префикса.
 *
 * Здесь — чистая функция окон: вокруг каждой строки-кандидата берётся ±margin строк,
 * пересекающиеся окна сливаются, пропуски заменяются маркером `… (строки N–M пропущены)`.
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

const skipMarker = (from, to) => `… (строки ${from}–${to} пропущены)`;

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
 * Контекст файлов для verifier: только файлы кандидатов, и в них — только окна
 * вокруг строк кандидатов.
 *
 * Файл, у которого ХОТЬ ОДИН кандидат без строки, отдаётся целиком: центрировать окно
 * не на чем, а выкинуть область, к которой относится находка без строки, — значит
 * лишить verifier'а возможности её подтвердить.
 *
 * @param {Map<string, string>} contentMap записи из readChangedFiles
 * @param {Array<{file?: string, line?: number}>} candidates кандидаты verifier'а
 * @returns {{ text: string, files: number, before: number, after: number }}
 */
export function buildVerifierFileContents(contentMap, candidates, { margin = 80 } = {}) {
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
  const text = kept.join('\n');
  // `before` считается ровно так же, как собирался старый контекст (join('\n') по тем же
  // записям) — иначе замер в логе сравнивал бы разные величины.
  return { text, files: kept.length, before: original.join('\n').length, after: text.length };
}
