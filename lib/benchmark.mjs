/** Pure scoring для gold/benchmark отчётов (без LLM). Entelligence-style метрики. */

function normTitle(s) {
  return String(s || '').toLowerCase().trim();
}

function titleMatches(findings, needle) {
  const n = normTitle(needle);
  if (!n) return false;
  return (findings || []).some((f) => normTitle(f.title).includes(n));
}

/** Допуск по строке — тот же ±3, по которому carry-over считает находку «той же». */
export const DEFAULT_LOCATION_TOLERANCE = 3;

function normPath(s) {
  return String(s || '').trim().replace(/^\.\//, '');
}

/**
 * Попадание в размеченную локацию дефекта: та же строка того же файла в пределах допуска.
 * Язык-независимая замена подстрочной сверке заголовков — нужна там, где разметка приходит
 * из чужого репозитория (её формулировки не совпадают с формулировками движка).
 * @param {object[]} findings — находки прогона ({ file, line, … })
 * @param {{ file: string, lines?: [number, number], line?: number }} target
 * @param {number} tolerance — допуск в строках в обе стороны
 */
export function locationMatches(findings, target, tolerance = DEFAULT_LOCATION_TOLERANCE) {
  const file = normPath(target?.file);
  if (!file) return false;
  const range = Array.isArray(target?.lines) ? target.lines : null;
  const from = Number(range ? range[0] : target?.line);
  const to = Number(range ? (range[1] ?? range[0]) : target?.line);
  const bounded = Number.isFinite(from) && Number.isFinite(to);
  const tol = Number.isFinite(tolerance) ? tolerance : DEFAULT_LOCATION_TOLERANCE;
  return (findings || []).some((f) => {
    if (normPath(f?.file) !== file) return false;
    // Локация без строк — «где-то в этом файле»: файла достаточно.
    if (!bounded) return true;
    const line = Number(f?.line);
    if (!Number.isFinite(line)) return false;
    return line >= Math.min(from, to) - tol && line <= Math.max(from, to) + tol;
  });
}

function formatLocation(target) {
  const range = Array.isArray(target?.lines) ? target.lines : null;
  if (range) return `${normPath(target.file)}:${range[0]}${range[1] != null && range[1] !== range[0] ? `-${range[1]}` : ''}`;
  if (target?.line != null) return `${normPath(target.file)}:${target.line}`;
  return normPath(target?.file);
}

/**
 * Оценка одного gold-кейса по meta.expect и фактическому результату прогона.
 * @param {object} meta — meta.json кейса (expect.wouldBlock, mustFindTitles, mustNotFindTitles)
 * @param {{ wouldBlock: boolean, findings: object[] }} run
 */
export function scoreGoldCase(meta, { wouldBlock, findings }) {
  const expect = meta?.expect || {};
  const precisionHints = [];
  const recallHits = [];
  const mustFindHits = [];
  const mustNotFindViolations = [];

  let wouldBlockMatch = expect.wouldBlock === undefined || wouldBlock === expect.wouldBlock;
  if (expect.wouldBlock !== undefined && !wouldBlockMatch) {
    precisionHints.push(`wouldBlock: ожидали ${expect.wouldBlock}, получили ${wouldBlock}`);
  }

  for (const t of expect.mustFindTitles || []) {
    if (titleMatches(findings, t)) {
      recallHits.push(t);
      mustFindHits.push(t);
    } else {
      precisionHints.push(`recall: нет находки с «${t}» в title`);
    }
  }

  for (const t of expect.mustNotFindTitles || []) {
    if (titleMatches(findings, t)) {
      mustNotFindViolations.push(t);
      precisionHints.push(`precision: неожиданная находка с «${t}» в title`);
    }
  }

  // Локации считаются отдельным счётчиком: «нашёл нужное слово» и «попал в нужную строку» —
  // разные величины, и сводить их в одну цифру recall значит потерять обе.
  const tolerance = expect.locationTolerance ?? DEFAULT_LOCATION_TOLERANCE;
  const locations = expect.mustFindLocations || [];
  const locationHits = [];
  const locationMisses = [];
  for (const loc of locations) {
    if (locationMatches(findings, loc, tolerance)) locationHits.push(loc);
    else {
      locationMisses.push(loc);
      precisionHints.push(`recall: нет находки в ${formatLocation(loc)} (±${tolerance})`);
    }
  }

  const ok = wouldBlockMatch && mustNotFindViolations.length === 0
    && (expect.mustFindTitles || []).every((t) => mustFindHits.includes(t))
    && locationMisses.length === 0;

  return {
    ok,
    precisionHints,
    recallHits,
    wouldBlockMatch,
    mustFindHits,
    mustNotFindViolations,
    locationHits,
    locationMisses,
  };
}

/**
 * Агрегация серии scoreGoldCase → precision/recall style summary.
 * @param {Array<{ id, type, score: ReturnType<scoreGoldCase>, wouldBlock, findingsCount? }>} results
 */
export function aggregateBenchmark(results) {
  const total = results?.length || 0;
  let passed = 0;
  let wouldBlockExpected = 0;
  let wouldBlockMatched = 0;
  let recallTargets = 0;
  let recallHits = 0;
  let locationTargets = 0;
  let locationHits = 0;
  // Кейсы, где найдена ХОТЯ БЫ ОДНА размеченная строка. Заголовочная величина: счёт по
  // локациям наказывает дважды за один дефект, размазанный по нескольким строкам (один
  // guard-блок, продублированный в двух методах, даёт две локации, но дефект один).
  let locationCases = 0;
  let locationCaseHits = 0;
  let fpViolations = 0;
  const byType = {};

  for (const r of results || []) {
    if (r.score?.ok) passed++;
    const type = r.type || 'unknown';
    if (!byType[type]) byType[type] = { total: 0, passed: 0 };
    byType[type].total++;
    if (r.score?.ok) byType[type].passed++;

    if (r.expect?.wouldBlock !== undefined) {
      wouldBlockExpected++;
      if (r.score?.wouldBlockMatch) wouldBlockMatched++;
    }
    const mustFind = r.expect?.mustFindTitles || [];
    recallTargets += mustFind.length;
    recallHits += r.score?.mustFindHits?.length || 0;
    const caseLocations = (r.expect?.mustFindLocations || []).length;
    locationTargets += caseLocations;
    locationHits += r.score?.locationHits?.length || 0;
    if (caseLocations) {
      locationCases++;
      if ((r.score?.locationHits?.length || 0) > 0) locationCaseHits++;
    }
    fpViolations += r.score?.mustNotFindViolations?.length || 0;
  }

  return {
    total,
    passed,
    passRate: total ? passed / total : null,
    wouldBlock: {
      expected: wouldBlockExpected,
      matched: wouldBlockMatched,
      rate: wouldBlockExpected ? wouldBlockMatched / wouldBlockExpected : null,
    },
    recall: {
      targets: recallTargets,
      hits: recallHits,
      rate: recallTargets ? recallHits / recallTargets : null,
    },
    // Заголовочная величина замера — по кейсам; счёт по локациям остаётся рядом как
    // более строгая, но менее осмысленная (см. RFC 03, «Скоринг»).
    locationCaseRecall: {
      cases: locationCases,
      hits: locationCaseHits,
      rate: locationCases ? locationCaseHits / locationCases : null,
    },
    locationRecall: {
      targets: locationTargets,
      hits: locationHits,
      rate: locationTargets ? locationHits / locationTargets : null,
    },
    precision: {
      fpViolations,
      rate: recallTargets + fpViolations > 0
        ? 1 - fpViolations / (recallTargets + fpViolations)
        : null,
    },
    byType,
  };
}

/** Оценка из JSONL/JSON записи прогона (findings + wouldBlock + meta). */
export function scoreFromRecord(record) {
  // Кейс, упавший в прогоне, приходит без меты: пустой `expect` нечему нарушить, и без
  // этой ветки падение считалось бы успехом.
  if (record?.error) {
    return {
      id: record.id || '?',
      type: record.type,
      expect: undefined,
      wouldBlock: null,
      findingsCount: 0,
      score: { ok: false, precisionHints: [`прогон упал: ${record.error}`], recallHits: [], wouldBlockMatch: false, mustFindHits: [], mustNotFindViolations: [], locationHits: [], locationMisses: [] },
    };
  }
  const meta = record?.meta || { expect: record?.expect || {} };
  const findings = record?.findings || [];
  const wouldBlock = record?.wouldBlock === true || record?.code === 1;
  const score = scoreGoldCase(meta, { wouldBlock, findings });
  return {
    id: record?.id || meta?.id || '?',
    type: meta?.type || record?.type,
    expect: meta?.expect,
    wouldBlock,
    findingsCount: findings.length,
    score,
  };
}
