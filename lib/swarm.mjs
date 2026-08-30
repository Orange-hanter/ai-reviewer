/**
 * E22/E23: контракт внешнего вердикта для PR-swarm оркестратора (ZAIrgRush и аналоги).
 *
 * Принцип «двух независимых судей»: swarm не должен сам себе выставлять merge-гейт.
 * ai-reviewer отдаёт strong-вердикт только когда блокеры подтверждены независимо
 * (prescan или verifierStatus === 'verified') — параллель strong=verdict в ZAIrgRush.
 *
 * E23: blast_radius.affected_files → suggestedScopeFiles для расширения task scope
 * за пределы диффа PR (callers вне PR, которые затронуты изменениями).
 */

import { BLAST_FINDING_SOURCE } from './constants.mjs';

export const SWARM_CONTRACT_VERSION = 1;

/** Блокер достаточно «сильный», чтобы стать внешним verdict для swarm. */
export function isStrongBlocker(f) {
  if (!f) return false;
  if (f.prescan) return true;
  if (f.verifierStatus === 'verified') return true;
  return false;
}

/**
 * Пути из blast-radius, которых нет в изменённых файлах PR — кандидаты в scope задач swarm.
 * @param {string[]} affectedFiles
 * @param {Iterable<string>} changedPaths
 */
export function suggestedScopeFiles(affectedFiles, changedPaths) {
  const changed = new Set(changedPaths || []);
  return [...new Set(affectedFiles || [])]
    .filter((p) => p && !changed.has(p))
    .sort();
}

/**
 * Стабильный machine-readable вердикт для оркестратора.
 *
 * @param {object} input
 * @param {boolean} input.wouldBlock
 * @param {object[]} [input.blockers]
 * @param {object} [input.blastMetrics] — metrics из Graphify buildBlastRadius
 * @param {Iterable<string>} [input.changedPaths]
 * @param {string} [input.headSha]
 * @param {object} [input.pr]
 * @param {object} [input.verdict] — sticky/artifact verdict + reproducibility
 * @returns {object}
 */
export function buildSwarmVerdict(input = {}) {
  const blockers = Array.isArray(input.blockers) ? input.blockers : [];
  const wouldBlock = Boolean(input.wouldBlock);
  const strongBlockers = blockers.filter(isStrongBlocker);
  // Strong: гейт блокирует И каждый блокер — независимо подтверждён (prescan/verifier).
  // Soft blast majors без verifier → wouldBlock=false (cluster soft-gate), сюда не попадают.
  const strong = wouldBlock && blockers.length > 0 && strongBlockers.length === blockers.length;

  const blast = input.blastMetrics && typeof input.blastMetrics === 'object'
    ? input.blastMetrics
    : {};
  const affected = Array.isArray(blast.affected_files) ? blast.affected_files : [];
  const scope = suggestedScopeFiles(affected, input.changedPaths);

  return {
    version: SWARM_CONTRACT_VERSION,
    role: 'external-verdict',
    generatedAt: new Date().toISOString(),
    headSha: String(input.headSha || '').slice(0, 40),
    pr: {
      number: input.pr?.number ?? 0,
      title: String(input.pr?.title || '').slice(0, 200),
      url: input.pr?.html_url || input.pr?.url || '',
    },
    wouldBlock,
    /** true → оркестратор может трактовать как second-judge block (как strong=verdict). */
    strong,
    blockers: blockers.map((f) => ({
      file: f.file || '',
      line: Number(f.line) || 0,
      severity: f.severity || '',
      title: String(f.title || '').slice(0, 200),
      source: f.source || undefined,
      prescan: f.prescan || undefined,
      verifierStatus: f.verifierStatus || undefined,
      blastCaller: f.blastCaller || undefined,
      confidence: f.confidence,
    })),
    strongBlockers: strongBlockers.length,
    blastScope: {
      affected_files: affected,
      symbols_total: blast.symbols_total ?? 0,
      symbols_resolved: blast.symbols_resolved ?? 0,
      affected_nonempty: blast.affected_nonempty ?? 0,
      graph_status: blast.graph_status || 'unknown',
      cross_community: blast.cross_community ?? 0,
      skip_reason: blast.skip_reason || undefined,
    },
    /** E23: файлы вне диффа PR, которые swarm должен рассмотреть в task scope. */
    suggestedScopeFiles: scope,
    verdict: input.verdict || undefined,
    notes: [
      strong
        ? 'strong: all blockers independently confirmed (prescan or verifier)'
        : wouldBlock
          ? 'wouldBlock without full strong confirmation — treat as advisory for swarm'
          : 'no merge block',
      scope.length
        ? `blast scope: ${scope.length} file(s) outside PR diff`
        : 'blast scope empty',
      input.sourceHint === BLAST_FINDING_SOURCE
        ? 'blast-radius findings alone never set strong without verifier'
        : undefined,
    ].filter(Boolean),
  };
}
