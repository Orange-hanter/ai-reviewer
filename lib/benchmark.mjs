/** Pure scoring для gold/benchmark отчётов (без LLM). Entelligence-style метрики. */

function normTitle(s) {
  return String(s || '').toLowerCase().trim();
}

function titleMatches(findings, needle) {
  const n = normTitle(needle);
  if (!n) return false;
  return (findings || []).some((f) => normTitle(f.title).includes(n));
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

  const ok = wouldBlockMatch && mustNotFindViolations.length === 0
    && (expect.mustFindTitles || []).every((t) => mustFindHits.includes(t));

  return {
    ok,
    precisionHints,
    recallHits,
    wouldBlockMatch,
    mustFindHits,
    mustNotFindViolations,
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
