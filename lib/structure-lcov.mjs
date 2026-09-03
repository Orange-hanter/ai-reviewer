import { isAbsolute, relative, resolve } from 'node:path';
import { normalizeRepoPath, sha256, stableId } from './structure-evidence.mjs';

function normalizeLcovPath(value, repoRoot = '') {
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

function ratio(covered, total) {
  return total > 0 ? covered / total : null;
}

function finishRecord(record) {
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
  return {
    path: record.path,
    ambiguousFunctionNames,
    lines: {
      found: lineEntries.length,
      hit: lineHit,
      ratio: ratio(lineHit, lineEntries.length),
      entries: lineEntries.map(([line, hits]) => ({ line, hits })),
    },
    functions: {
      found: functions.length,
      // неизвестное исполнение не считается ни покрытым, ни непокрытым: в знаменателе
      // остаются только функции, про которые есть однозначный ответ
      hit: functions.filter((fn) => fn.hits > 0).length,
      ratio: ratio(
        functions.filter((fn) => fn.hits > 0).length,
        functions.filter((fn) => fn.hits != null).length,
      ),
      entries: functions,
    },
    branches: {
      found: branchEntries.length,
      hit: branchHit,
      ratio: ratio(branchHit, branchEntries.length),
      entries: branchEntries,
    },
  };
}

export function parseLcov(text, { repoRoot = '' } = {}) {
  const records = [];
  const warnings = [];
  let current = null;

  for (const rawLine of String(text || '').split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    if (line.startsWith('SF:')) {
      if (current) records.push(finishRecord(current));
      try {
        current = {
          path: normalizeLcovPath(line.slice(3), repoRoot),
          lines: new Map(),
          functions: [],
          functionHits: [],
          branches: [],
        };
      } catch (error) {
        warnings.push(error.message);
        current = null;
      }
      continue;
    }
    if (line === 'end_of_record') {
      if (current) records.push(finishRecord(current));
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
    } else if (line.startsWith('BRDA:')) {
      const [lineNo, block, branch, takenRaw] = line.slice(5).split(',').map((part) => part ?? '');
      if (lineNo === '' || !Number.isInteger(Number(lineNo))) continue;
      const taken = takenRaw === '-' ? 0 : Number(takenRaw);
      if (takenRaw !== '' && Number.isFinite(taken)) {
        current.branches.push({ line: Number(lineNo), block, branch, taken });
      }
    }
  }
  if (current) records.push(finishRecord(current));

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

export function lcovObservations(lcov, entityByPath = new Map()) {
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
      const matched = matchLcovFunction(fn, entities);
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

function normalizedName(value) {
  return String(value || '')
    .replace(/^.*[.#/]/, '')
    .replace(/\(\)$/, '')
    .replace(/^["']|["']$/g, '')
    .toLowerCase();
}

export function matchLcovFunction(fn, entities) {
  // 'constructor' приходит только из SCIP (KIND_NAMES), graphify.inferKind его не выдаёт
  const callable = (entities || []).filter((entity) => (
    ['function', 'method', 'constructor'].includes(entity.kind)
  ));
  const exactLine = callable.filter((entity) => entity.line === fn.line);
  const name = normalizedName(fn.name);
  const exact = exactLine.filter((entity) => normalizedName(entity.label) === name);
  if (exact.length === 1) return { entity: exact[0], confidence: 'exact' };
  if (exactLine.length === 1) return { entity: exactLine[0], confidence: 'extracted' };
  const near = callable.filter((entity) => (
    Math.abs((entity.line || 0) - fn.line) <= 3 && normalizedName(entity.label) === name
  ));
  if (near.length === 1) return { entity: near[0], confidence: 'inferred' };
  return { entity: null, confidence: exact.length > 1 || near.length > 1 ? 'ambiguous' : 'unresolved' };
}
