import { SEV, MARKERS } from './constants.mjs';
import { DEFAULT_FP_PATTERNS_PATH, getFpPatterns, matchFpPatterns } from './fp-patterns.mjs';
import { normalizeAdvisorClaims } from './specialists.mjs';

export function normalizeConfidence(raw) {
  if (raw == null || raw === '') return 1;
  const n = Number(raw);
  if (!Number.isFinite(n)) return 1;
  return Math.min(1, Math.max(0, n));
}

/** Нормализация одной находки из JSON модели (включая confidence 0–1). */
export function normalizeFinding(f, model) {
  if (!f || typeof f !== 'object') return null;
  return {
    file: typeof f.file === 'string' ? f.file : '',
    line: Number(f.line) || 0,
    severity: ['critical', 'major', 'minor', 'nit'].includes(f.severity) ? f.severity : 'minor',
    title: typeof f.title === 'string' ? f.title : 'замечание',
    body: typeof f.body === 'string' ? f.body : '',
    suggestion: typeof f.suggestion === 'string' ? f.suggestion : undefined,
    confidence: normalizeConfidence(f.confidence),
    advisorClaims: normalizeAdvisorClaims(f.advisorClaims),
    model,
  };
}

export function matchesKnownFalsePositive(text, changedPaths, opts = {}) {
  const patternsFile = opts.patternsFile || DEFAULT_FP_PATTERNS_PATH;
  const patterns = opts.patterns || getFpPatterns(patternsFile);
  return Boolean(matchFpPatterns(text, changedPaths, patterns));
}

export function filterFalsePositiveFindings(findings, changedPaths, log = () => {}, fpOpts = {}) {
  return findings.filter((f) => {
    const text = `${f.title} ${f.body}`;
    if (matchesKnownFalsePositive(text, changedPaths, fpOpts)) {
      log(`filtered false finding: ${f.title}`);
      return false;
    }
    return true;
  });
}

/** Группирует находки по file|line-bucket; мутирует элементы (consensus, consensusBlock). */
export function clusterFindings(allFindings, gate) {
  const { blockOn, blockSingle, consensusMin, confidenceMin = 0.8 } = gate;
  const keyOf = (f) => `${f.file}|${f.line > 0 ? Math.floor((f.line - 1) / 3) : 'out'}`;
  const clusters = new Map();
  for (const f of allFindings) {
    const k = keyOf(f);
    if (!clusters.has(k)) clusters.set(k, []);
    clusters.get(k).push(f);
  }
  for (const group of clusters.values()) {
    const models = new Set(group.map((f) => f.model));
    const singleModels = new Set(group.filter((f) => blockSingle.includes(f.severity)).map((f) => f.model));
    const consensusModels = new Set(
      group.filter((f) => blockOn.includes(f.severity) && !blockSingle.includes(f.severity)).map((f) => f.model),
    );
    const tierBlock = singleModels.size >= 1 || consensusModels.size >= consensusMin;
    const blockCandidates = group.filter((f) => blockOn.includes(f.severity));
    const maxConf = blockCandidates.length
      ? Math.max(...blockCandidates.map((f) => normalizeConfidence(f.confidence)))
      : 1;
    const confidenceBlock = !blockCandidates.length || maxConf >= confidenceMin;
    const consensusBlock = tierBlock && confidenceBlock;
    for (const f of group) {
      f.confidence = normalizeConfidence(f.confidence);
      f.consensus = models.size >= 2;
      f.clusterModels = [...models];
      f.confidenceBlock = confidenceBlock;
      f.consensusBlock = consensusBlock;
    }
  }
  return [...clusters.values()];
}

/** Prescan: блокируют только rule из blockRules; REVIEW_PRESCAN_BLOCK=0 → advisory. */
export function applyPrescanBlockPolicy(clusters, prescanCfg) {
  if (!prescanCfg || prescanCfg.enabled === false) return;
  for (const group of clusters) {
    for (const f of group) {
      if (!f.prescan) continue;
      if (prescanCfg.blockEnabled === false || !f.prescanBlock) {
        f.consensusBlock = false;
      }
    }
  }
}

/** Filter → cluster → блокеры (pipeline + eval harness). */
export function resolveBlockers(allFindings, changedPaths, gate, log = () => {}, opts = {}) {
  const { skipFpFilter = false, prescan: prescanCfg, patternsFile } = opts;
  const fpOpts = patternsFile ? { patternsFile } : {};
  const findings = skipFpFilter
    ? allFindings
    : filterFalsePositiveFindings(allFindings, changedPaths, log, fpOpts);
  const clusters = clusterFindings(findings, gate);
  applyPrescanBlockPolicy(clusters, prescanCfg);
  const blockers = clusters
    .filter((g) => g.some((f) => f.consensusBlock))
    .map((g) => g.find((f) => f.consensusBlock) || g[0]);
  return { findings, clusters, blockers, wouldBlock: blockers.length > 0 };
}

export function createFindingsFormatter(cfg) {
  const defaultModel = cfg.ollama.model;
  const inlineOn = new Set(cfg.inlineOn?.length ? cfg.inlineOn : ['critical', 'major']);

  function findingBlock(f) {
    const head = `${SEV[f.severity] || '⚪'} **${f.severity} (${f.model || defaultModel}): ${f.title || 'замечание'}**`;
    const sug = f.suggestion ? `\n\n\`\`\`suggestion\n${f.suggestion}\n\`\`\`` : '';
    return `${head}\n\n${f.body || ''}${sug}`;
  }

  function fmtInlineGroup(group) {
    const models = [...new Set(group.map((f) => f.model || defaultModel))];
    const consensus = models.length >= 2 ? ' · ⚠ консенсус (обе модели)' : '';
    const blocks = group.map(findingBlock).join('\n\n---\n\n');
    return `${MARKERS.inline}\n${blocks}\n\n<sub>🤖 авто-ревью (${models.join(' + ')})${consensus}</sub>`;
  }

  function classify(findings, files) {
    const groups = new Map();
    const outside = [];
    for (const f of findings) {
      const rec = files.get(f.file);
      const line = Number(f.line) || 0;
      const inlineEligible = inlineOn.has(f.severity)
        && rec && line > 0 && rec.validLines.has(line);
      if (inlineEligible) {
        const k = `${f.file}:${line}`;
        if (!groups.has(k)) groups.set(k, []);
        groups.get(k).push(f);
      } else {
        outside.push(f);
      }
    }
    const inline = [...groups.values()].map((group) => ({
      path: group[0].file, line: group[0].line, side: 'RIGHT', body: fmtInlineGroup(group),
    }));
    return { inline, outside };
  }

  return { classify, findingBlock };
}
