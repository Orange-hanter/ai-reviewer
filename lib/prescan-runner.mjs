/** Orchestration: eslint + ast-grep prescan в параллель. */

import { runEslintPrescan } from './prescan.mjs';
import { runAstgrepPrescan } from './astgrep.mjs';
import { mergePrescanSkipped, mergeLintableTotal } from './prescan-shared.mjs';

/** eslint + ast-grep параллельно; объединяет findings и prompt sections. */
export async function runCombinedPrescan(changedPaths, cfg, log = () => {}) {
  const [eslintResult, astgrepResult] = await Promise.all([
    runEslintPrescan(changedPaths, cfg, log),
    runAstgrepPrescan(changedPaths, cfg, log),
  ]);
  const findings = [...eslintResult.findings, ...astgrepResult.findings];
  const promptSection = [eslintResult.promptSection, astgrepResult.promptSection]
    .filter(Boolean)
    .join('\n\n');
  const errors = [eslintResult.error, astgrepResult.error].filter(Boolean);
  return {
    findings,
    promptSection,
    scanned: eslintResult.scanned || astgrepResult.scanned || 0,
    lintableTotal: mergeLintableTotal(eslintResult.lintableTotal, astgrepResult.lintableTotal),
    truncated: eslintResult.truncated || astgrepResult.truncated || false,
    skipped: mergePrescanSkipped(eslintResult.skipped, astgrepResult.skipped),
    eslint: eslintResult,
    astgrep: astgrepResult,
    byTool: {
      eslint: eslintResult.findings.length,
      astgrep: astgrepResult.findings.length,
    },
    error: errors.length ? errors.join('; ') : undefined,
  };
}

export function createPrescanRunner(cfg, { log = () => {}, exec = runCombinedPrescan } = {}) {
  return {
    run: (changedPaths) => exec(changedPaths, cfg, log),
  };
}

export function formatPrescanLog(result) {
  const trunc = result.truncated ? ` (truncated from ${result.lintableTotal})` : '';
  const byTool = result.byTool
    ? ` [eslint:${result.byTool.eslint}, ast-grep:${result.byTool.astgrep}]`
    : '';
  const err = result.error ? ` (warn: ${result.error.slice(0, 80)})` : '';
  return `${result.findings.length} issue(s) on ${result.scanned} file(s)${trunc}${byTool}${err}`;
}
