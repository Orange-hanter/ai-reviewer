/** R5: specialist advisors (security / correctness / architecture) + refutation. */

import { DEFAULT_MOA_SPECIALIST_MODELS } from './constants.mjs';

export const SPECIALIST_ROLES = ['security', 'correctness', 'architecture'];

/** Семантические дефолты (не зависят от порядка MOA_REFERENCE_MODELS). */
export const ROLE_DEFAULT_MODEL = {
  security: 'glm-5.3-flash',
  correctness: 'kimi-k2.7-code',
  architecture: 'glm-5.3',
};

function parseSpecialistBulk(bulk) {
  const fromBulk = {};
  for (const part of bulk.split(',').map((s) => s.trim()).filter(Boolean)) {
    const i = part.indexOf(':');
    if (i <= 0) continue;
    const role = part.slice(0, i).trim();
    const model = part.slice(i + 1).trim();
    if (SPECIALIST_ROLES.includes(role) && model) fromBulk[role] = model;
  }
  return fromBulk;
}

/**
 * Парсит MOA_SPECIALIST_MODELS / MOA_SPECIALIST_<ROLE>_MODEL / ROLE_DEFAULT_MODEL.
 * moaReferences — legacy fallback только если роль не задана явно (deprecated).
 */
export function parseSpecialistAssignments(env, moaReferences = []) {
  const fromEnv = parseSpecialistBulk(env.MOA_SPECIALIST_MODELS || DEFAULT_MOA_SPECIALIST_MODELS);
  for (const role of SPECIALIST_ROLES) {
    const key = `MOA_SPECIALIST_${role.toUpperCase()}_MODEL`;
    if (env[key]?.trim()) fromEnv[role] = env[key].trim();
  }
  return SPECIALIST_ROLES.map((role, i) => ({
    role,
    model: fromEnv[role] || ROLE_DEFAULT_MODEL[role] || moaReferences[i],
  }));
}

export function normalizeAdvisorClaims(raw) {
  if (!Array.isArray(raw)) return undefined;
  const claims = raw
    .filter((c) => c && SPECIALIST_ROLES.includes(c.role) &&
      ['verified', 'rejected', 'downgraded'].includes(c.status))
    .map((c) => {
      const claim = {
        role: c.role,
        status: c.status,
        reason: typeof c.reason === 'string' ? c.reason : '',
      };
      if (c.status === 'downgraded' && ['minor', 'nit'].includes(c.severity)) {
        claim.severity = c.severity;
      }
      return claim;
    });
  return claims.length ? claims : undefined;
}

/**
 * Refutation по advisorClaims на finding (aggregator jury).
 * - все rejected → drop
 * - verified → keep
 * - downgraded (no verified) → severity из claims (nit если любой nit, иначе minor)
 */
export function decideRefutation(claims) {
  if (!claims?.length) return { action: 'keep' };
  const statuses = claims.map((c) => c.status);
  if (statuses.every((s) => s === 'rejected')) return { action: 'drop' };
  if (statuses.includes('verified')) return { action: 'keep' };
  if (statuses.includes('downgraded')) {
    const downs = claims.filter((c) => c.status === 'downgraded');
    const severity = downs.some((c) => c.severity === 'nit') ? 'nit' : 'minor';
    return { action: 'downgrade', severity };
  }
  return { action: 'keep' };
}

/**
 * Применяет refutation ко всем findings.
 * strict: critical/major без advisorClaims → downgrade minor (не блок без jury).
 */
export function applyRefutation(allFindings, log = () => {}, { strict = false, blockOn = ['critical', 'major'] } = {}) {
  const tiers = new Set(blockOn || []);
  const stats = { kept: 0, dropped: 0, downgraded: 0, noClaims: 0, downgradedNoClaims: 0 };
  const out = [];

  for (const f of allFindings) {
    if (f.prescan) {
      out.push(f);
      continue;
    }
    const claims = f.advisorClaims;
    if (!claims?.length) {
      if (strict && tiers.has(f.severity)) {
        f.severity = 'minor';
        f.refutationStatus = 'downgraded_no_claims';
        stats.downgradedNoClaims++;
        stats.downgraded++;
        log(`refutation strict: [block-tier без advisorClaims] ${f.file}:${f.line} «${f.title}» → minor`);
        out.push(f);
        continue;
      }
      stats.noClaims++;
      out.push(f);
      continue;
    }
    const decision = decideRefutation(claims);
    if (decision.action === 'drop') {
      stats.dropped++;
      f.refutationStatus = 'dropped';
      log(`refutation dropped: [${f.severity}] ${f.file}:${f.line} «${f.title}» — все advisors rejected`);
      continue;
    }
    if (decision.action === 'downgrade') {
      f.severity = decision.severity || 'minor';
      f.refutationStatus = 'downgraded';
      stats.downgraded++;
      log(`refutation downgraded: «${f.title}» → ${f.severity}`);
    } else {
      f.refutationStatus = claims.some((c) => c.status === 'verified') ? 'verified' : 'kept';
      stats.kept++;
    }
    out.push(f);
  }

  return { findings: out, stats };
}

/** Сливает specialist references со всех чанков (для sticky report). */
export function mergeSpecialistReferences(chunkResults) {
  const map = new Map();
  for (const res of chunkResults || []) {
    for (const r of res?.references || []) {
      const key = `${r.role || 'advisor'}::${r.model}`;
      const prev = map.get(key);
      if (!prev) {
        map.set(key, { ...r, chunkCount: 1 });
        continue;
      }
      prev.chunkCount++;
      if (r.text && !r.error) {
        const tag = prev.chunkCount > 1 ? `\n\n[chunk ${prev.chunkCount}]\n` : '\n\n';
        prev.text = prev.text ? `${prev.text}${tag}${r.text}` : r.text;
      }
      if (r.error && !prev.text) prev.error = r.error;
    }
  }
  return [...map.values()];
}

export function formatRefutationLog(stats) {
  const extra = stats.downgradedNoClaims
    ? ` downgradedNoClaims=${stats.downgradedNoClaims}` : '';
  return `kept=${stats.kept} dropped=${stats.dropped} downgraded=${stats.downgraded} noClaims=${stats.noClaims}${extra}`;
}
