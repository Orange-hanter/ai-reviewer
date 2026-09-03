/**
 * Фаза 2, шаг 2: lib/structure-lcov.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/structure_lcov (порт парсера LCOV, наблюдений покрытия и
 * эвристики matchLcovFunction).
 *
 * Публичное API не изменилось (те же экспорты: parseLcov/lcovObservations/
 * matchLcovFunction синхронно). Синхронные функции вызывают бинарь через
 * execFileSync: JSON {"op", ...args} на stdin, ответ — JSON в формате JS-возврата.
 *
 * Резолвинг бинаря (по аналогии с lib/structure-scip.mjs):
 *   AI_REVIEWER_STRUCTURE_LCOV_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/structure_lcov;
 *   AI_REVIEWER_STRUCTURE_LCOV_FORCE_JS=1 — принудительно чистый JS (паритет/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm test/eval
 * идут раньше cargo build, см. .github/workflows/ci.yml), обёртка деградирует на чистый
 * JS fallback ниже — эталонную копию исходной реализации. Если бинарь ЕСТЬ, но упал
 * (non-zero exit) — ошибка пробрасывается, fallback не используется: молчаливая
 * подмена реализации при битом Rust-коде скрывала бы регрессии.
 *
 * Контрактные стыки:
 *   - byPath из parseLcov — JS Map и JSON'ом не ходит: бинарь возвращает
 *     {hash, files, warnings, totals}, обёртка пересобирает byPath из files;
 *   - entityByPath в lcovObservations — Map: обёртка разворачивает её в
 *     entityEntries ([[path, [entities]]]) до вызова.
 *
 * Нюансы паритета (проверены parity-прогоном rust/parity/structure-lcov.parity.mjs):
 *   - Number()/isInteger/isFinite для полей DA/FN/FNDA/BRDA (' 5', '1e2', '0x10'
 *     принимаются как в JS; 'abc' → NaN → запись отбрасывается);
 *   - одноимённые FN при совпадении числа FNDA привязываются по порядку записей
 *     (attribution 'order'), при разошедшемся числе — hits null + warning;
 *   - абсолютные SF требуют repoRoot (node path.resolve/relative портированы
 *     лексически, относительный repoRoot клеится к cwd вызывающего процесса);
 *   - сортировки localeCompare (ICU root) и побайтовый дефолтный Array.sort.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { isAbsolute, relative, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { normalizeRepoPath, sha256, stableId } from './structure-evidence.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_LCOV_BIN = resolve(__dir, '../rust/target/debug/structure_lcov');

function resolveLcovBin() {
  if (process.env.AI_REVIEWER_STRUCTURE_LCOV_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_STRUCTURE_LCOV_BIN || '').trim() || DEFAULT_LCOV_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). Ошибка бинаря пробрасывается. */
function runRust(op, args) {
  const bin = resolveLcovBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

/** Парсит LCOV-текст в {hash, files, byPath, warnings, totals}. */
export function parseLcov(text, { repoRoot = '' } = {}) {
  const r = runRust('parseLcov', { text: String(text || ''), repoRoot });
  if (r !== null) {
    const byPath = new Map();
    for (const file of r.files || []) byPath.set(file.path, file);
    return { ...r, byPath };
  }
  return parseLcovJs(text, { repoRoot });
}

/** По распарсенному LCOV строит наблюдения покрытия (файлы и функции). */
export function lcovObservations(lcov, entityByPath = new Map()) {
  const entries = entityByPath instanceof Map
    ? [...entityByPath.entries()]
    : Object.entries(entityByPath || {});
  const r = runRust('lcovObservations', { lcov, entityEntries: entries });
  if (r !== null) return r;
  return lcovObservationsJs(lcov, entityByPath instanceof Map ? entityByPath : new Map(entries));
}

/** Эвристика сопоставления LCOV-функции сущностям (exact/extracted/inferred/...). */
export function matchLcovFunction(fn, entities) {
  const r = runRust('matchLcovFunction', { fn, entities: entities || [] });
  if (r !== null) return r;
  return matchLcovFunctionJs(fn, entities);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/structure-lcov.mjs
// до Фазы 2). Используется только когда Rust-бинарь отсутствует; при битом бинаре
// НЕ подменяет его.
// ---------------------------------------------------------------------------

function normalizeLcovPathJs(value, repoRoot = '') {
  const raw = String(value || '').trim();
  if (!raw) throw new Error('LCOV SF path is empty');
  if (isAbsolute(raw)) {
    // без явного repoRoot абсолютный SF недетерминирован — отклоняем, а не молча берём cwd
    if (!repoRoot) throw new Error('absolute LCOV SF path requires explicit repoRoot');
    const rel = relative(resolve(repoRoot), resolve(raw)).replaceAll('\\', '/');
    return normalizeRepoPath(rel);
  }
  return normalizeRepoPath(raw);
}

function ratioJs(covered, total) {
  return total > 0 ? covered / total : null;
}

/**
 * Итоговые счётчики, которые писатель объявляет сам (RFC 02, шаг 1).
 *
 * Парсер считает `found`/`hit` по записям и до сих пор эти строки игнорировал. Но writer
 * кладёт в тот же файл собственный ответ, и расхождение — единственный дешёвый сигнал, что
 * наша модель формата разошлась с тем, кто его writes: обычно это слитые отчёты, где `FNDA`
 * схлопнуты по имени. Молча доверять своему счёту в таком файле нельзя.
 */
const DECLARED_KEYS = new Map([
  ['LF:', ['lines', 'found']],
  ['LH:', ['lines', 'hit']],
  ['FNF:', ['functions', 'found']],
  ['FNH:', ['functions', 'hit']],
  ['BRF:', ['branches', 'found']],
  ['BRH:', ['branches', 'hit']],
]);

/** Сверка «посчитали сами» против «объявил писатель». Пустой массив — расхождений нет. */
function declaredMismatches(record, computed) {
  const out = [];
  for (const [section, values] of Object.entries(record.declared || {})) {
    for (const [kind, declared] of Object.entries(values)) {
      const actual = computed[section]?.[kind];
      if (!Number.isFinite(declared) || !Number.isFinite(actual)) continue;
      // Функции с неразрешённой привязкой не попадают в наш `hit` по построению
      // (`hits: null` — не покрыта и не непокрыта), поэтому по FNH такой файл сверять нечем.
      if (section === 'functions' && kind === 'hit' && computed.ambiguousFunctionNames?.length) continue;
      if (declared !== actual) out.push({ section, kind, declared, actual });
    }
  }
  return out;
}

function finishRecordJs(record) {
  if (!record) return null;
  // FNDA адресует функцию ТОЛЬКО по имени, а FN в одном файле может объявить это имя
  // дважды (вложенные функции в разных областях — обычный JS). Раньше обе записи получали
  // один и тот же счётчик: непокрытая функция объявлялась covered с чужими хитами, причём
  // вопреки собственному DA той же строки, и файл рапортовал 2 из 2.
  //
  // Порядок спасает там, где имя не спасает: istanbul пишет FN обходом
  // `Object.values(fnMap)`, а FNDA — обходом `Object.entries(fnMap)`, то есть один и тот
  // же map в одном порядке (istanbul-reports/lib/lcovonly). Значит при совпадении числа
  // записей N-я FNDA относится к N-й одноимённой FN. Это порядок, а не ключ, поэтому такая
  // привязка помечается `attribution: 'order'` и не претендует на точную уверенность.
  // Если числа не совпали (типично после слияния отчётов, где FNDA схлопнуты по имени),
  // привязывать нечего — исполнение остаётся неизвестным.
  const hitsByName = new Map();
  for (const [name, hits] of record.functionHits) {
    if (hitsByName.has(name)) hitsByName.get(name).push(hits);
    else hitsByName.set(name, [hits]);
  }
  const declaredByName = new Map();
  for (const fn of record.functions) {
    declaredByName.set(fn.name, (declaredByName.get(fn.name) || 0) + 1);
  }
  const ambiguousFunctionNames = [...declaredByName.entries()]
    .filter(([name, count]) => count > 1 && (hitsByName.get(name)?.length || 0) !== count)
    .map(([name]) => name)
    .sort();
  const unresolvable = new Set(ambiguousFunctionNames);
  const seenByName = new Map();
  const functions = record.functions.map((fn) => {
    const ordinal = seenByName.get(fn.name) || 0;
    seenByName.set(fn.name, ordinal + 1);
    const duplicated = declaredByName.get(fn.name) > 1;
    if (unresolvable.has(fn.name)) {
      return {
        id: stableId('lcov-fn', record.path, fn.line, fn.name),
        name: fn.name,
        line: fn.line,
        hits: null,
        attribution: 'unresolved',
      };
    }
    const hits = duplicated
      ? Number(hitsByName.get(fn.name)[ordinal])
      : Number(hitsByName.get(fn.name)?.[0] || 0);
    return {
      id: stableId('lcov-fn', record.path, fn.line, fn.name),
      name: fn.name,
      line: fn.line,
      hits,
      attribution: duplicated ? 'order' : 'name',
    };
  }).sort((a, b) => a.line - b.line || a.name.localeCompare(b.name));
  const lineEntries = [...record.lines.entries()].sort(([a], [b]) => a - b);
  const branchEntries = record.branches.sort((a, b) => (
    a.line - b.line || a.block.localeCompare(b.block) || a.branch.localeCompare(b.branch)
  ));
  const lineHit = lineEntries.filter(([, hits]) => hits > 0).length;
  const branchHit = branchEntries.filter((branch) => branch.taken > 0).length;
  const computed = {
    path: record.path,
    ambiguousFunctionNames,
    lines: {
      found: lineEntries.length,
      hit: lineHit,
      ratio: ratioJs(lineHit, lineEntries.length),
      entries: lineEntries.map(([line, hits]) => ({ line, hits })),
    },
    functions: {
      found: functions.length,
      // неизвестное исполнение не считается ни покрытым, ни непокрытым: в знаменателе
      // остаются только функции, про которые есть однозначный ответ
      hit: functions.filter((fn) => fn.hits > 0).length,
      ratio: ratioJs(
        functions.filter((fn) => fn.hits > 0).length,
        functions.filter((fn) => fn.hits != null).length,
      ),
      entries: functions,
    },
    branches: {
      found: branchEntries.length,
      hit: branchHit,
      ratio: ratioJs(branchHit, branchEntries.length),
      entries: branchEntries,
    },
  };
  computed.declared = record.declared || {};
  computed.declaredMismatches = declaredMismatches(record, computed);
  return computed;
}

function parseLcovJs(text, { repoRoot = '' } = {}) {
  const records = [];
  const warnings = [];
  let current = null;

  for (const rawLine of String(text || '').split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    if (line.startsWith('SF:')) {
      if (current) records.push(finishRecordJs(current));
      try {
        current = {
          path: normalizeLcovPathJs(line.slice(3), repoRoot),
          lines: new Map(),
          functions: [],
          functionHits: [],
          branches: [],
          declared: {},
        };
      } catch (error) {
        warnings.push(error.message);
        current = null;
      }
      continue;
    }
    if (line === 'end_of_record') {
      if (current) records.push(finishRecordJs(current));
      current = null;
      continue;
    }
    if (!current) continue;
    if (line.startsWith('DA:')) {
      const [lineNo, hits] = line.slice(3).split(',');
      // пустые поля (`DA:,0`, `DA:5,`) — мусор, а не строка 0 с 0 хитов
      if (lineNo !== '' && hits !== undefined && hits !== ''
        && Number.isInteger(Number(lineNo)) && Number.isFinite(Number(hits))) {
        current.lines.set(Number(lineNo), Number(hits));
      }
    } else if (line.startsWith('FN:')) {
      const comma = line.indexOf(',');
      const lineNo = Number(line.slice(3, comma));
      const name = line.slice(comma + 1).trim();
      if (comma > 3 && Number.isInteger(lineNo) && name) current.functions.push({ line: lineNo, name });
    } else if (line.startsWith('FNDA:')) {
      const comma = line.indexOf(',');
      const hits = Number(line.slice(5, comma));
      const name = line.slice(comma + 1).trim();
      if (comma > 5 && Number.isFinite(hits) && name) current.functionHits.push([name, hits]);
    } else if (DECLARED_KEYS.has(`${line.split(':')[0]}:`)) {
      const prefix = `${line.split(':')[0]}:`;
      const [section, kind] = DECLARED_KEYS.get(prefix);
      const value = Number(line.slice(prefix.length).trim());
      if (Number.isInteger(value) && value >= 0) {
        if (!current.declared[section]) current.declared[section] = {};
        current.declared[section][kind] = value;
      }
    } else if (line.startsWith('BRDA:')) {
      const [lineNo, block, branch, takenRaw] = line.slice(5).split(',').map((part) => part ?? '');
      if (lineNo === '' || !Number.isInteger(Number(lineNo))) continue;
      const taken = takenRaw === '-' ? 0 : Number(takenRaw);
      if (takenRaw !== '' && Number.isFinite(taken)) {
        current.branches.push({ line: Number(lineNo), block, branch, taken });
      }
    }
  }
  if (current) records.push(finishRecordJs(current));

  const files = records.filter(Boolean).sort((a, b) => a.path.localeCompare(b.path));
  const duplicatePaths = [];
  const byPath = new Map();
  for (const file of files) {
    if (byPath.has(file.path)) duplicatePaths.push(file.path);
    byPath.set(file.path, file);
  }
  if (duplicatePaths.length) warnings.push(`duplicate LCOV paths: ${[...new Set(duplicatePaths)].join(', ')}`);
  for (const file of files) {
    if (file.ambiguousFunctionNames?.length) {
      warnings.push(`unresolvable LCOV function names in ${file.path}: ${file.ambiguousFunctionNames.join(', ')}`);
    }
    // Расхождение с объявленными счётчиками: наш счёт и ответ писателя разошлись, значит
    // модель формата неверна для этого файла — молчать об этом нельзя.
    for (const m of file.declaredMismatches || []) {
      warnings.push(
        `LCOV declared/computed mismatch in ${file.path}: ${m.section}.${m.kind} declared=${m.declared} computed=${m.actual}`,
      );
    }
  }

  return {
    hash: sha256(text),
    files,
    byPath,
    warnings,
    totals: {
      files: files.length,
      lines: {
        found: files.reduce((sum, file) => sum + file.lines.found, 0),
        hit: files.reduce((sum, file) => sum + file.lines.hit, 0),
      },
      functions: {
        found: files.reduce((sum, file) => sum + file.functions.found, 0),
        hit: files.reduce((sum, file) => sum + file.functions.hit, 0),
      },
      branches: {
        found: files.reduce((sum, file) => sum + file.branches.found, 0),
        hit: files.reduce((sum, file) => sum + file.branches.hit, 0),
      },
    },
  };
}

function lcovObservationsJs(lcov, entityByPath = new Map()) {
  const observations = [];
  for (const file of lcov?.files || []) {
    observations.push({
      id: stableId('coverage', 'file', file.path),
      level: 'code_coverage',
      subjectRef: `file:${file.path}`,
      status: file.lines.hit > 0 ? (file.lines.hit === file.lines.found ? 'covered' : 'partial') : 'uncovered',
      hitCount: null,
      covered: file.lines.hit,
      total: file.lines.found,
      confidence: 'exact',
      evidence: [`lcov:${file.path}:DA`],
    });
    const entities = entityByPath.get(file.path) || [];
    for (const fn of file.functions.entries) {
      const matched = matchLcovFunctionJs(fn, entities);
      // hits === null — имя в файле повторяется и числа FN/FNDA разошлись: исполнение по
      // LCOV не восстановимо. attribution === 'order' — привязка по порядку записей, она
      // опирается на поведение писателя, а не на ключ формата, поэтому «точной» не бывает.
      const unknownHits = fn.hits == null;
      const confidence = unknownHits
        ? 'ambiguous'
        : (fn.attribution === 'order' && matched.confidence === 'exact' ? 'extracted' : matched.confidence);
      observations.push({
        id: stableId('coverage', 'function', file.path, fn.line, fn.name),
        level: 'function_execution',
        subjectRef: matched.entity?.observedId || `lcov-fn:${file.path}:${fn.line}:${fn.name}`,
        status: unknownHits ? 'unresolved' : (fn.hits > 0 ? 'covered' : 'uncovered'),
        hitCount: unknownHits ? null : fn.hits,
        covered: null,
        total: null,
        confidence,
        evidence: [`lcov:${file.path}:FN:${fn.line}:${fn.name}`],
      });
    }
  }
  return observations.sort((a, b) => a.id.localeCompare(b.id));
}

function normalizedNameJs(value) {
  return String(value || '')
    .replace(/^.*[.#/]/, '')
    .replace(/\(\)$/, '')
    .replace(/^["']|["']$/g, '')
    .toLowerCase();
}

function matchLcovFunctionJs(fn, entities) {
  // 'constructor' приходит только из SCIP (KIND_NAMES), graphify.inferKind его не выдаёт
  const callable = (entities || []).filter((entity) => (
    ['function', 'method', 'constructor'].includes(entity.kind)
  ));
  const exactLine = callable.filter((entity) => entity.line === fn.line);
  const name = normalizedNameJs(fn.name);
  const exact = exactLine.filter((entity) => normalizedNameJs(entity.label) === name);
  if (exact.length === 1) return { entity: exact[0], confidence: 'exact' };
  if (exactLine.length === 1) return { entity: exactLine[0], confidence: 'extracted' };
  const near = callable.filter((entity) => (
    Math.abs((entity.line || 0) - fn.line) <= 3 && normalizedNameJs(entity.label) === name
  ));
  if (near.length === 1) return { entity: near[0], confidence: 'inferred' };
  return { entity: null, confidence: exact.length > 1 || near.length > 1 ? 'ambiguous' : 'unresolved' };
}
