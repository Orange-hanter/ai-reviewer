/** Pass B: verifier для critical/major кандидатов (R3). Чистые функции + LLM-runner. */

const SEV_ORDER = { critical: 0, major: 1, minor: 2, nit: 3 };

/** Кандидаты на второй проход — severity из gate.blockOn, critical первыми. */
export function selectVerifierCandidates(findings, gate, max = 12) {
  const tiers = new Set(gate.blockOn || []);
  return findings
    .filter((f) => tiers.has(f.severity) && !f.prescan)
    .sort((a, b) => (SEV_ORDER[a.severity] ?? 9) - (SEV_ORDER[b.severity] ?? 9))
    .slice(0, max);
}

/**
 * Применяет verdicts verifier к findings.
 * rejected → убираем из списка; downgrade → понижаем severity; verified → оставляем.
 * Нет verdict для кандидата → fail-open (оставляем как есть).
 */
export function applyVerifierVerdicts(allFindings, candidates, verdicts, log = () => {}) {
  const verdictMap = new Map();
  for (const v of verdicts || []) {
    if (v && Number.isInteger(v.candidateIndex)) verdictMap.set(v.candidateIndex, v);
  }
  const toRemove = new Set();
  const stats = { verified: 0, rejected: 0, downgraded: 0, missing: 0 };

  for (let i = 0; i < candidates.length; i++) {
    const f = candidates[i];
    const v = verdictMap.get(i);
    const decision = v?.decision;
    if (!decision) {
      stats.missing++;
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
