/** Pass B: verifier для critical/major кандидатов (R3). Чистые функции + LLM-runner. */

const SEV_ORDER = { critical: 0, major: 1, minor: 2, nit: 3 };

/** Кандидаты на второй проход — severity из gate.blockOn, critical первыми. */
export function selectVerifierCandidates(findings, gate, max = 12, advisory = {}) {
  const tiers = new Set(gate.blockOn || []);
  const blockers = findings
    .filter((f) => tiers.has(f.severity) && !f.prescan)
    .sort((a, b) => (SEV_ORDER[a.severity] ?? 9) - (SEV_ORDER[b.severity] ?? 9))
    .slice(0, max);
  // Advisory-кандидаты: minor/nit, набравшие доказуемость по actionability. Verifier для них —
  // не про блокировку merge (её severity не даёт), а про право занять строку в PR: в инлайн
  // должно уходить проверенное, а не просто уверенное. Кандидатов выбирает вызывающий,
  // здесь только отсечка по лимиту и защита от повторов.
  const { candidates = [], max: advisoryMax = 0 } = advisory;
  if (!advisoryMax || !candidates.length) return blockers;
  const seen = new Set(blockers);
  const extra = candidates.filter((f) => !seen.has(f) && !f.prescan).slice(0, advisoryMax);
  return [...blockers, ...extra];
}

/**
 * Применяет verdicts verifier к findings.
 * rejected → убираем из списка; downgrade → понижаем severity; verified → оставляем.
 * Нет verdict для кандидата → fail-open (оставляем как есть), а при strictUnverified —
 * fail-closed для блоков: понижаем до minor (блок требует подтверждения verifier).
 */
export function applyVerifierVerdicts(allFindings, candidates, verdicts, log = () => {}, { strictUnverified = false } = {}) {
  const verdictMap = new Map();
  for (const v of verdicts || []) {
    if (v && Number.isInteger(v.candidateIndex)) verdictMap.set(v.candidateIndex, v);
  }
  const toRemove = new Set();
  const stats = { verified: 0, rejected: 0, downgraded: 0, missing: 0, unverified: 0 };

  for (let i = 0; i < candidates.length; i++) {
    const f = candidates[i];
    const v = verdictMap.get(i);
    const decision = v?.decision;
    if (!decision) {
      stats.missing++;
      if (strictUnverified) {
        f.severity = 'minor';
        f.verifierStatus = 'unverified';
        f.verifierReason = 'verifier не подтвердил — понижено до minor (REVIEW_VERIFIER_STRICT)';
        stats.unverified++;
        log(`verifier unverified → minor: [${f.file}:${f.line}] «${f.title}»`);
      }
      continue;
    }
    if (decision === 'rejected') {
      toRemove.add(f);
      f.verifierStatus = 'rejected';
      f.verifierReason = v.reason || '';
      stats.rejected++;
      log(`verifier rejected: [${f.severity}] ${f.file}:${f.line} «${f.title}» — ${v.reason || 'без причины'}`);
    } else if (decision === 'downgrade') {
      f.severity = ['minor', 'nit'].includes(v.severity) ? v.severity : 'minor';
      f.verifierStatus = 'downgraded';
      f.verifierReason = v.reason || '';
      stats.downgraded++;
      log(`verifier downgraded: «${f.title}» → ${f.severity}`);
    } else {
      f.verifierStatus = 'verified';
      f.verifierReason = v.reason || '';
      stats.verified++;
    }
  }

  return {
    findings: allFindings.filter((f) => !toRemove.has(f)),
    stats,
  };
}

/** LLM Pass B — один batch-вызов на список кандидатов. */
export function createVerifierRunner(cfg, { ollama, prompt, log, observability }) {
  async function verifyFindings(candidates, ctx) {
    if (!candidates.length) return [];
    const messages = prompt.buildVerifierMessages({
      candidates,
      annotated: ctx.annotated,
      fileContents: ctx.fileContents,
      blastBlocks: ctx.blastBlocks || [],
      pr: ctx.pr,
      changedFilesSection: ctx.changedFilesSection || '',
    });
    const t0 = Date.now();
    try {
      const result = await ollama.chatJson(messages, cfg.verifier.model, {
        temperature: cfg.verifier.temperature,
      });
      const verdicts = Array.isArray(result.verdicts) ? result.verdicts : [];
      observability?.logEvent('llm_call', {
        callType: 'verifier',
        model: cfg.verifier.model,
        candidates: candidates.length,
        duration_ms: Date.now() - t0,
        ok: true,
        verdicts: verdicts.length,
      });
      return verdicts;
    } catch (e) {
      observability?.logEvent('llm_call', {
        callType: 'verifier',
        model: cfg.verifier.model,
        candidates: candidates.length,
        duration_ms: Date.now() - t0,
        ok: false,
        error: e.message?.slice(0, 200),
      });
      throw e;
    }
  }

  return { verifyFindings };
}
