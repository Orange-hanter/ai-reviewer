import { readFile, readdir } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseDiff } from './diff.mjs';
import { filterFalsePositiveFindings, clusterFindings, resolveBlockers } from './findings.mjs';
import { buildRepoPathIndex } from './absent-path.mjs';
import { selectVerifierCandidates, applyVerifierVerdicts } from './verifier.mjs';
import { applyRefutation } from './specialists.mjs';
import { loadReviewConfig } from './config.mjs';
import { existsSync } from 'node:fs';
import { defaultRustBin } from './rust-bin.mjs';

/** Кейс помечен requiresRust: его поведение rust-owned (RFC 20), без бинарей проверять нечего. */
const RUST_GATE_BINS = ['specialists', 'verifier', 'findings'];
const rustGateAvailable = () => RUST_GATE_BINS.every((m) => existsSync(defaultRustBin(m)));

const __dir = dirname(fileURLToPath(import.meta.url));
export const DEFAULT_FIXTURES_DIR = join(__dir, '../fixtures');

/** Загружает все подкаталоги fixtures с meta.json + diff.patch + findings.json. */
export async function loadFixtures(fixturesDir = DEFAULT_FIXTURES_DIR) {
  let entries;
  try {
    entries = await readdir(fixturesDir, { withFileTypes: true });
  } catch {
    return [];
  }
  const out = [];
  for (const ent of entries) {
    if (!ent.isDirectory()) continue;
    const dir = join(fixturesDir, ent.name);
    try {
      const meta = JSON.parse(await readFile(join(dir, 'meta.json'), 'utf8'));
      const diff = await readFile(join(dir, 'diff.patch'), 'utf8');
      const findingsPayload = JSON.parse(await readFile(join(dir, 'findings.json'), 'utf8'));
      const expected = meta.expected || {};
      let verifierVerdicts = null;
      try {
        verifierVerdicts = JSON.parse(await readFile(join(dir, 'verifier.json'), 'utf8')).verdicts;
      } catch { /* optional */ }
      // Дерево репозитория потребителя (по пути на строку) — для FP-семейства 7.
      let repoFiles = null;
      try {
        repoFiles = (await readFile(join(dir, 'repo-files.txt'), 'utf8')).split('\n').map((s) => s.trim()).filter(Boolean);
      } catch { /* optional */ }
      out.push({
        id: meta.id || ent.name,
        dir,
        description: meta.description || '',
        diff,
        findings: findingsPayload.findings || [],
        expected,
        gateOverride: meta.gate || {},
        verifierVerdicts,
        repoFiles,
        requiresRust: Boolean(meta.requiresRust),
      });
    } catch { /* skip incomplete fixture dirs */ }
  }
  return out.sort((a, b) => a.id.localeCompare(b.id));
}

/** Прогон post-processing (filter → verifier? → cluster → gate) на fixture без LLM.
 *
 * `expected` (meta.json):
 * - `shouldBlock` / `blockerCount` / `findingCount` — итог гейта и размер списка после filter
 * - `titles` — titles, которые **обязаны быть среди blockers** (блокер-контракт)
 * - `noTitles` — titles, которых **не должно остаться ни в одном finding**
 *   (не только среди blockers): иначе FP, понижающий severity вместо drop, остаётся green
 */
export function runFixtureCase(fixture, baseGate, prescanCfg) {
  const gate = { ...baseGate, ...fixture.gateOverride };
  if (fixture.requiresRust && !rustGateAvailable()) {
    return {
      id: fixture.id, description: fixture.description, pass: true, skipped: true,
      errors: [], note: 'пропущен: нет Rust-бинарей specialists/verifier/findings (rust-owned, RFC 20)',
      wouldBlock: false, blockerCount: 0, findingCount: 0, clusterCount: 0,
      verifierStats: null, refutationStats: null, blockers: [],
    };
  }
  const { files } = parseDiff(fixture.diff);
  const changedPaths = [...files.keys()];
  const repoIndex = buildRepoPathIndex(fixture.repoFiles, changedPaths);
  let working = filterFalsePositiveFindings(fixture.findings, changedPaths, () => {}, { repoIndex });
  let refutationStats = null;
  let verifierStats = null;
  if (working.some((f) => f.advisorClaims?.length)) {
    const applied = applyRefutation(working, () => {}, {
      strict: true, blockOn: gate.blockOn, skipBlast: (gate.blastVerify ?? '0') !== '0',
    });
    working = applied.findings;
    refutationStats = applied.stats;
  }
  if (fixture.verifierVerdicts?.length) {
    const candidates = selectVerifierCandidates(working, gate, 12);
    const applied = applyVerifierVerdicts(working, candidates, fixture.verifierVerdicts);
    working = applied.findings;
    verifierStats = applied.stats;
  }
  const { findings, clusters, blockers, wouldBlock } = resolveBlockers(
    working, changedPaths, gate, () => {}, { skipFpFilter: true, prescan: prescanCfg },
  );

  const exp = fixture.expected;
  const errors = [];

  if (exp.shouldBlock !== undefined && wouldBlock !== exp.shouldBlock) {
    errors.push(`shouldBlock: ожидали ${exp.shouldBlock}, получили ${wouldBlock}`);
  }
  if (exp.findingCount !== undefined && findings.length !== exp.findingCount) {
    errors.push(`findingCount: ожидали ${exp.findingCount}, получили ${findings.length}`);
  }
  if (exp.blockerCount !== undefined && blockers.length !== exp.blockerCount) {
    errors.push(`blockerCount: ожидали ${exp.blockerCount}, получили ${blockers.length}`);
  }
  if (exp.titles?.length) {
    const got = new Set(blockers.map((b) => b.title));
    for (const t of exp.titles) {
      if (!got.has(t)) errors.push(`нет блокера с title «${t}»`);
    }
  }
  if (exp.noTitles?.length) {
    // Смотрим все оставшиеся findings, не только blockers: иначе FP-фильтр, который
    // понижает severity вместо удаления, оставляет кейс зелёным.
    const got = new Set(findings.map((f) => f.title));
    for (const t of exp.noTitles) {
      if (got.has(t)) errors.push(`неожиданная находка «${t}»`);
    }
  }

  return {
    id: fixture.id,
    description: fixture.description,
    pass: errors.length === 0,
    errors,
    wouldBlock,
    blockerCount: blockers.length,
    findingCount: findings.length,
    clusterCount: clusters.length,
    verifierStats,
    refutationStats,
    blockers: blockers.map((b) => ({
      file: b.file, line: b.line, severity: b.severity, title: b.title,
      confidence: b.confidence, consensusBlock: b.consensusBlock,
    })),
  };
}

/** Агрегированные метрики по прогону fixtures (для CI-порога). */
export function computeEvalMetrics(results) {
  const total = results.length;
  const passed = results.filter((r) => r.pass).length;
  const failed = total - passed;
  const blockCases = results.filter((r) => r.expectedShouldBlock !== undefined);
  const blockPass = blockCases.filter((r) => r.pass).length;
  const fp = results.filter((r) => r.pass === false && r.wouldBlock && r.expectedShouldBlock === false).length;
  return {
    total,
    passed,
    failed,
    passRate: total ? passed / total : 1,
    precisionNote: blockCases.length ? `block-cases pass: ${blockPass}/${blockCases.length}` : 'n/a',
  };
}

/**
 * Форматирует итог eval для stdout / CI log.
 * Комментарии к каждому кейсу — «что проверили» и «почему pass/fail».
 */
export function formatEvalReport(results, metrics) {
  const lines = [
    '=== pr-review eval harness ===',
    `Итого: ${metrics.passed}/${metrics.total} passed (${(metrics.passRate * 100).toFixed(0)}%)`,
    metrics.precisionNote,
    '',
  ];
  for (const r of results) {
    const mark = r.pass ? '✅' : '❌';
    lines.push(`${mark} ${r.id}${r.description ? ` — ${r.description}` : ''}`);
    if (r.pass) {
      lines.push(`   → wouldBlock=${r.wouldBlock}, blockers=${r.blockerCount}, findings=${r.findingCount}`);
      if (r.blockers.length) {
        for (const b of r.blockers) {
          lines.push(`   · [${b.severity}] ${b.file}:${b.line} conf=${b.confidence ?? '—'} «${b.title}»`);
        }
      } else {
        lines.push('   · блокеров нет (ожидаемо для non-block кейсов)');
      }
      if (r.verifierStats?.rejected) {
        lines.push(`   · verifier отклонил ${r.verifierStats.rejected} FP`);
      }
      if (r.refutationStats?.dropped) {
        lines.push(`   · refutation dropped ${r.refutationStats.dropped} FP`);
      }
    } else {
      for (const e of r.errors) lines.push(`   ✗ ${e}`);
      lines.push(`   факт: wouldBlock=${r.wouldBlock}, blockers=${r.blockerCount}`);
    }
    lines.push('');
  }
  if (metrics.failed) {
    lines.push(`⛔ ${metrics.failed} fixture(s) FAILED — не меняйте модель/промпт без зелёного eval`);
  } else {
    lines.push('✅ все fixtures passed — гейт post-processing стабилен');
  }
  return lines.join('\n');
}

/** Прогон всех fixtures; возвращает { results, metrics, ok }. */
export async function runEvalHarness({ fixturesDir = DEFAULT_FIXTURES_DIR, env = process.env } = {}) {
  const cfg = loadReviewConfig({ ...env, REVIEW_MODE: 'consensus', OLLAMA_MODELS: 'm1,m2' });
  const fixtures = await loadFixtures(fixturesDir);
  const results = fixtures.map((f) => {
    const r = runFixtureCase(f, cfg.gate, cfg.prescan);
    r.expectedShouldBlock = f.expected.shouldBlock;
    return r;
  });
  const metrics = computeEvalMetrics(results);
  return { results, metrics, ok: metrics.failed === 0, report: formatEvalReport(results, metrics) };
}
