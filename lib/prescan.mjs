/** R6: eslint pre-scan на changed files (детерминизм + dedup LLM). */

import { access } from 'node:fs/promises';
import { resolve } from 'node:path';
import {
  normalizeAppPath,
  isLintablePath,
  selectLintableEntries,
  buildPrescanPathMap,
  resolvePrescanDiffKey,
  buildPrescanPromptSection,
  toolPathToAppPath,
  runCommand,
  logToolFailure,
} from './prescan-shared.mjs';

export {
  normalizeAppPath,
  isLintablePath,
  selectLintableEntries,
  buildPrescanPathMap,
  resolvePrescanDiffKey,
  buildPrescanPromptSection,
  toolPathToAppPath,
} from './prescan-shared.mjs';

export {
  runCombinedPrescan,
  createPrescanRunner,
  formatPrescanLog,
} from './prescan-runner.mjs';

const DEFAULT_BLOCK_RULES = ['no-restricted-imports', 'no-restricted-globals'];
const NIT_RULES = new Set(['max-len']);

/** @deprecated Используй diffKey из selectLintableEntries; оставлен для совместимости. */
export function diffRelativePath(path) {
  return path.startsWith('app/') ? path.slice(4) : path;
}

export function selectLintablePaths(changedPaths, max = 30) {
  return selectLintableEntries(changedPaths, max).entries.map((e) => e.appPath);
}

/** eslint abs path → app/-relative path. */
export function eslintAbsToAppPath(abs, cwd = process.cwd()) {
  return toolPathToAppPath(abs, cwd);
}

/** eslint ruleId → severity для AI gate; blockRules → critical. */
export function mapEslintRuleSeverity(ruleId, eslintSeverity = 2, blockRules) {
  const rules = blockRules instanceof Set ? blockRules : new Set(blockRules || DEFAULT_BLOCK_RULES);
  if (rules.has(ruleId)) return 'critical';
  if (NIT_RULES.has(ruleId)) return 'nit';
  return eslintSeverity === 2 ? 'major' : 'minor';
}

/** Парсит stdout eslint --format json → findings. */
export function eslintJsonToFindings(results, { blockRules, pathMap } = {}) {
  if (!Array.isArray(results)) return [];
  const rules = blockRules instanceof Set ? blockRules : new Set(blockRules || DEFAULT_BLOCK_RULES);
  const findings = [];
  for (const file of results) {
    const appPath = eslintAbsToAppPath(file.filePath || '');
    const fileKey = resolvePrescanDiffKey(appPath, pathMap);
    for (const msg of file.messages || []) {
      if (!msg || msg.severity === 0) continue;
      const ruleId = msg.ruleId || 'eslint';
      const severity = mapEslintRuleSeverity(ruleId, msg.severity, rules);
      findings.push({
        file: fileKey,
        line: Number(msg.line) || 0,
        severity,
        confidence: 1,
        title: `eslint: ${ruleId}`,
        body: String(msg.message || '').trim(),
        model: 'eslint-prescan',
        prescan: true,
        ruleId,
        prescanBlock: rules.has(ruleId),
      });
    }
  }
  return findings;
}

export function findingLocationKey(f) {
  return `${f.file}|${f.line > 0 ? Math.floor((f.line - 1) / 3) : 'out'}`;
}

function locationKeyAliases(f) {
  const p = f.file || '';
  const keys = new Set([findingLocationKey(f)]);
  if (p.startsWith('app/')) keys.add(findingLocationKey({ ...f, file: p.slice(4) }));
  else if (p) keys.add(findingLocationKey({ ...f, file: `app/${p}` }));
  return keys;
}

/** Убирает LLM-findings на тех же file|line-bucket, что уже покрыты prescan. */
export function dedupeAgainstPrescan(llmFindings, prescanFindings) {
  if (!prescanFindings.length) return llmFindings;
  const covered = new Set();
  for (const f of prescanFindings) {
    for (const k of locationKeyAliases(f)) covered.add(k);
  }
  return llmFindings.filter((f) => {
    for (const k of locationKeyAliases(f)) {
      if (covered.has(k)) return false;
    }
    return true;
  });
}

/** Запуск eslint на списке app/-paths; fail-open при сбое. */
export async function runEslintPrescan(changedPaths, cfg, log = () => {}) {
  const { prescan } = cfg;
  if (!prescan?.enabled) return { findings: [], promptSection: '', scanned: 0 };

  const appDir = resolve(process.cwd(), prescan.appDir);
  try {
    await access(resolve(appDir, 'node_modules/eslint/package.json'));
  } catch {
    log('prescan: eslint недоступен (нет app/node_modules) — skip');
    return { findings: [], promptSection: '', scanned: 0, skipped: 'no-eslint' };
  }

  const { entries, total, truncated } = selectLintableEntries(changedPaths, prescan.maxFiles);
  if (truncated) {
    log(`prescan: lintable файлов ${total} > лимита ${prescan.maxFiles} — сканируем первые ${prescan.maxFiles}`);
  }
  if (!entries.length) return { findings: [], promptSection: '', scanned: 0 };

  const pathMap = buildPrescanPathMap(entries);
  const relPaths = entries.map((e) => e.appPath.replace(/^app\//, ''));
  const localBin = resolve(appDir, 'node_modules/.bin/eslint');
  let bin;
  let eslintArgs;
  try {
    await access(localBin);
    bin = localBin;
    eslintArgs = ['--format', 'json', '--no-error-on-unmatched-pattern', ...relPaths];
  } catch {
    bin = prescan.eslintBin || 'npx';
    eslintArgs = ['eslint', '--format', 'json', '--no-error-on-unmatched-pattern', ...relPaths];
  }

  try {
    const { code, stdout, stderr } = await runCommand(bin, eslintArgs, { cwd: appDir });
    if (!stdout.trim()) {
      if (code !== 0) logToolFailure(log, 'eslint', { code, stdout, stderr });
      return {
        findings: [], promptSection: '', scanned: entries.length, eslintExit: code,
        lintableTotal: total, truncated,
        error: code !== 0 ? (stderr || stdout || 'empty stdout').trim().slice(0, 200) : undefined,
      };
    }
    let parsed;
    try {
      parsed = JSON.parse(stdout);
    } catch {
      log('prescan eslint: не JSON stdout');
      return { findings: [], promptSection: '', scanned: entries.length, lintableTotal: total, truncated, error: 'bad json' };
    }
    const blockList = prescan.blockRules?.filter(Boolean);
    const blockRuleSet = new Set(blockList?.length ? blockList : DEFAULT_BLOCK_RULES);
    const findings = eslintJsonToFindings(parsed, { blockRules: blockRuleSet, pathMap });
    return {
      findings,
      promptSection: buildPrescanPromptSection(findings),
      scanned: entries.length,
      lintableTotal: total,
      truncated,
      eslintExit: code,
    };
  } catch (e) {
    log(`prescan eslint failed (fail-open): ${e.message}`);
    return {
      findings: [], promptSection: '', scanned: 0, lintableTotal: total, truncated,
      error: e.message,
    };
  }
}
