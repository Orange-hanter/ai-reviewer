import { stableId } from './structure-evidence.mjs';

function normalized(value) {
  return String(value || '').replace(/\(\)+$/, '').replace(/^\./, '').toLowerCase();
}

function contractLabel(contract, entityById) {
  return normalized(entityById.get(contract.entityRef)?.label);
}

export function suggestObligationLinks(obligations, facts, { limit = 5 } = {}) {
  const entityById = new Map((facts?.entities || []).map((entity) => [entity.observedId, entity]));
  return (obligations || [])
    .filter((obligation) => !(obligation.contractRefs || []).length)
    .map((obligation) => {
      const statement = normalized(obligation.statement);
      const docPath = normalized(obligation.docRef);
      const candidates = (facts?.contracts || []).map((contract) => {
        const entity = entityById.get(contract.entityRef);
        const label = contractLabel(contract, entityById);
        let score = 0;
        const reasons = [];
        if (label && new RegExp(`\\b${label.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\b`, 'i').test(statement)) {
          score += 5;
          reasons.push('symbol-mentioned');
        }
        if (entity?.path && docPath && docPath.split('/').some((part) => part.length > 2 && entity.path.toLowerCase().includes(part))) {
          score += 2;
          reasons.push('document-path-overlap');
        }
        if (obligation.moduleRef && entity?.boundaryId?.includes(String(obligation.moduleRef))) {
          score += 3;
          reasons.push('module-boundary');
        }
        if (contract.kind === 'signature') {
          score += 1;
          reasons.push('semantic-signature');
        }
        return {
          contractRef: contract.id,
          entityRef: contract.entityRef,
          score,
          confidence: score >= 6 ? 'extracted' : score >= 3 ? 'inferred' : 'heuristic',
          reasons,
        };
      }).filter((candidate) => candidate.score > 0)
        .sort((a, b) => b.score - a.score || a.contractRef.localeCompare(b.contractRef))
        .slice(0, limit);
      return {
        obligationRef: obligation.id,
        candidates,
        resolution: candidates.length === 1 && candidates[0].score >= 6 ? 'unique' : candidates.length ? 'ambiguous' : 'unresolved',
      };
    })
    .sort((a, b) => a.obligationRef.localeCompare(b.obligationRef));
}

function finding(ruleId, subjectRefs, {
  priority = 'medium',
  remediationTarget,
  summary,
  evidence = [],
  missingEvidence = [],
  snapshotFingerprint,
}) {
  const fingerprint = stableId('structure-finding', ruleId, ...subjectRefs);
  return {
    id: fingerprint,
    fingerprint,
    ruleId,
    priority,
    status: 'open',
    subjectRefs: [...new Set(subjectRefs)].sort(),
    remediationTarget,
    summary,
    evidence: [...new Set(evidence)].sort(),
    missingEvidence: [...new Set(missingEvidence)].sort(),
    firstSeenSnapshot: snapshotFingerprint,
    lastSeenSnapshot: snapshotFingerprint,
    resolvedBySnapshot: null,
  };
}

export function computeStructureDrift({
  facts,
  assessment,
  obligationsExport,
  confirmedClaims = [],
  linkSuggestions = null,
}) {
  const findings = [];
  const contractIds = new Set((facts.facts?.contracts || []).map((contract) => contract.id));
  const entityIds = new Set((facts.facts?.entities || []).map((entity) => entity.observedId));
  const suggestions = linkSuggestions || suggestObligationLinks(obligationsExport?.obligations, facts.facts);
  const suggestionByObligation = new Map(suggestions.map((item) => [item.obligationRef, item]));
  if (facts.provenance.graphStatus !== 'fresh') {
    findings.push(finding('structure.graph_stale', ['structure:graph'], {
      priority: 'info',
      remediationTarget: 'code',
      summary: 'Rebuild the structure graph at the pinned commit.',
      evidence: [`graph-status:${facts.provenance.graphStatus}`],
      missingEvidence: ['fresh_graph'],
      snapshotFingerprint: assessment.fingerprint,
    }));
  }
  for (const obligation of obligationsExport?.obligations || []) {
    if (obligation.status !== 'confirmed') continue;
    const linked = (obligation.contractRefs || []).filter((ref) => contractIds.has(ref));
    if (!obligation.contractRefs?.length) {
      const suggestion = suggestionByObligation.get(obligation.id);
      findings.push(finding('docs.obligation_unlinked', [obligation.id], {
        remediationTarget: 'claim',
        summary: `Confirm a code contract link for ${obligation.id}.`,
        evidence: suggestion?.candidates?.map((candidate) => `candidate:${candidate.contractRef}:${candidate.score}`) || [],
        missingEvidence: ['confirmed_contract_link'],
        snapshotFingerprint: assessment.fingerprint,
      }));
    } else if (!linked.length) {
      findings.push(finding('docs.contract_link_broken', [obligation.id, ...obligation.contractRefs], {
        priority: 'high',
        remediationTarget: 'docs',
        summary: `Documented contract link for ${obligation.id} does not exist in the pinned snapshot.`,
        evidence: [`obligation:${obligation.id}:${obligation.contentHash}`],
        missingEvidence: ['existing_contract'],
        snapshotFingerprint: assessment.fingerprint,
      }));
    }
  }
  for (const claim of confirmedClaims || []) {
    if (claim.status !== 'confirmed') continue;
    const exists = claim.kind === 'entity_exists' ? entityIds.has(claim.subjectRef) : contractIds.has(claim.subjectRef);
    if (!exists) {
      findings.push(finding('contract.confirmed_claim_drift', [claim.id, claim.subjectRef], {
        priority: 'high',
        remediationTarget: 'docs',
        summary: `Confirmed ${claim.kind} claim no longer resolves to observed code.`,
        evidence: [`claim:${claim.id}:${claim.contentHash || ''}`],
        missingEvidence: ['observed_subject'],
        snapshotFingerprint: assessment.fingerprint,
      }));
    }
  }
  for (const scenario of assessment.assessments?.contractScenarios || []) {
    if (!['missing', 'partial'].includes(scenario.status)) continue;
    findings.push(finding('scenario.must_obligation_gap', [scenario.obligationRef, ...scenario.contractRefs], {
      priority: scenario.status === 'missing' ? 'high' : 'medium',
      remediationTarget: 'test',
      summary: `Scenario ${scenario.kind} is ${scenario.status}: ${scenario.statusReason}`,
      evidence: scenario.evidence,
      missingEvidence: scenario.missingEvidence,
      snapshotFingerprint: assessment.fingerprint,
    }));
  }
  return {
    version: 1,
    kind: 'structure_drift',
    factsFingerprint: facts.fingerprint,
    assessmentFingerprint: assessment.fingerprint,
    obligationsRevision: obligationsExport?.revision || 'none',
    temporalAlignment: assessment.temporalAlignment,
    findings: findings.sort((a, b) => a.fingerprint.localeCompare(b.fingerprint)),
    linkSuggestions: suggestions,
  };
}

export function reconcileStructureFindings(previous, current, snapshotFingerprint) {
  const previousByFingerprint = new Map((previous || []).map((item) => [item.fingerprint, item]));
  const currentByFingerprint = new Map((current || []).map((item) => [item.fingerprint, item]));
  const out = [];
  for (const item of current || []) {
    const prior = previousByFingerprint.get(item.fingerprint);
    out.push(prior ? {
      ...prior,
      ...item,
      status: item.status === 'resolved' ? 'resolved' : 'open',
      firstSeenSnapshot: prior.firstSeenSnapshot || item.firstSeenSnapshot,
      lastSeenSnapshot: snapshotFingerprint,
    } : item);
  }
  for (const prior of previous || []) {
    if (currentByFingerprint.has(prior.fingerprint)) continue;
    out.push({
      ...prior,
      status: prior.ruleId === 'docs.obligation_unlinked' ? 'superseded' : 'pending_verify',
      lastSeenSnapshot: snapshotFingerprint,
      resolvedBySnapshot: null,
    });
  }
  return out.sort((a, b) => a.fingerprint.localeCompare(b.fingerprint));
}

export function applyStructureWaivers(findings, waivers, now = new Date()) {
  const active = new Map((waivers || [])
    .filter((waiver) => waiver.owner && waiver.reason && waiver.expiresAt
      && Date.parse(waiver.expiresAt) > now.getTime())
    .map((waiver) => [waiver.findingFingerprint, waiver]));
  return (findings || []).map((item) => {
    const waiver = active.get(item.fingerprint);
    return waiver ? { ...item, suppressed: true, waiver } : { ...item, suppressed: false };
  });
}

export function triageStructureFindings(findings, { limit = 20 } = {}) {
  const rank = { high: 3, medium: 2, info: 1 };
  return (findings || [])
    .filter((item) => !item.suppressed && !['resolved', 'superseded'].includes(item.status))
    .sort((a, b) => (rank[b.priority] || 0) - (rank[a.priority] || 0)
      || a.ruleId.localeCompare(b.ruleId)
      || a.fingerprint.localeCompare(b.fingerprint))
    .slice(0, limit)
    .map((item) => ({
      ...item,
      explain: [
        item.summary,
        item.missingEvidence.length ? `Missing: ${item.missingEvidence.join(', ')}` : 'Evidence threshold satisfied.',
      ],
      nextAction: item.remediationTarget,
    }));
}
