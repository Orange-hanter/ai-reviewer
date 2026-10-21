/** R6+: ast-grep prescan — AST anti-patterns alongside eslint (fail-open). */

import { access } from 'node:fs/promises';
import { rmSync } from 'node:fs';
import { resolve, dirname, join, relative, posix } from 'node:path';
import { fileURLToPath } from 'node:url';
import { getProfile } from './profile.mjs';
import { getInputSource, materializeFromBase } from './trusted-input.mjs';
import { parseSgconfigDirs } from './router.mjs';
import {
  selectLintableEntries,
  buildPrescanPathMap,
  resolvePrescanDiffKey,
  buildPrescanPromptSection,
  toolPathToAppPath,
  runCommand,
  logToolFailure,
  normalizeAppPath,
} from './prescan-shared.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
export const DEFAULT_ASTGREP_CONFIG = resolve(__dir, '../ast-grep/sgconfig.yml');
export const DEFAULT_ASTGREP_BLOCK_RULES = ['raw-x-forwarded-for'];

const ASTGREP_SEV = { error: 'critical', warning: 'major', info: 'minor', hint: 'nit' };

/**
 * Scope и allowlist правил живут в профиле проекта: правило «client не импортирует server»
 * имеет смысл только там, где такие каталоги есть, а исключение вроде «сырой X-Forwarded-For
 * допустим в прокси-модуле» знает только сам проект. Пустой профиль → правило действует
 * ровно там, где его нашёл ast-grep.
 */
function ruleScope(ruleId) {
  return getProfile().astgrep.scopes.get(ruleId) || null;
}

function ruleAllows(ruleId, path) {
  return Boolean(getProfile().astgrep.allow.get(ruleId)?.has(path));
}

/** ast-grep ruleId → severity; blockRules → critical. */
export function mapAstgrepRuleSeverity(ruleId, astSeverity = 'warning', blockRules) {
  const rules = blockRules instanceof Set ? blockRules : new Set(blockRules || DEFAULT_ASTGREP_BLOCK_RULES);
  if (rules.has(ruleId)) return 'critical';
  return ASTGREP_SEV[astSeverity] || 'major';
}

/** Пост-фильтр: scope правил + allowlist из профиля (globs в sgconfig не знают слоёв проекта). */
export function filterAstgrepFindings(findings) {
  return findings.filter((f) => {
    const norm = normalizeAppPath(f.file);
    const scope = ruleScope(f.ruleId);
    if (scope && !scope.test(norm)) return false;
    return !ruleAllows(f.ruleId, norm);
  });
}

/** Парсит stdout ast-grep --json=stream → findings. */
export function astgrepStreamToFindings(stdout, { blockRules, pathMap, cwd } = {}) {
  const rules = blockRules instanceof Set ? blockRules : new Set(blockRules || DEFAULT_ASTGREP_BLOCK_RULES);
  const findings = [];
  for (const line of String(stdout || '').split('\n')) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let row;
    try {
      row = JSON.parse(trimmed);
    } catch {
      continue;
    }
    if (!row?.ruleId || !row?.file) continue;
    const appPath = toolPathToAppPath(row.file, cwd);
    const fileKey = resolvePrescanDiffKey(appPath, pathMap);
    const lineNo = Number(row.range?.start?.line ?? 0) + 1;
    const astSeverity = String(row.severity || 'warning').toLowerCase();
    const severity = mapAstgrepRuleSeverity(row.ruleId, astSeverity, rules);
    findings.push({
      file: fileKey,
      line: lineNo,
      severity,
      confidence: 1,
      title: `ast-grep: ${row.ruleId}`,
      body: String(row.message || '').trim(),
      model: 'astgrep-prescan',
      prescan: true,
      ruleId: row.ruleId,
      prescanBlock: rules.has(row.ruleId),
    });
  }
  return filterAstgrepFindings(findings);
}

/** @deprecated alias */
export const astgrepPathToAppPath = toolPathToAppPath;

/** Запуск ast-grep на changed lintable paths; fail-open при сбое. */
export async function runAstgrepPrescan(changedPaths, cfg, log = () => {}) {
  const { prescan } = cfg;
  const ast = prescan?.astgrep;
  if (!prescan?.enabled || !ast?.enabled) {
    return { findings: [], promptSection: '', scanned: 0 };
  }

  const appDir = resolve(process.cwd(), prescan.appDir || '.');
  let configPath = ast.configPath?.startsWith('/')
    ? ast.configPath
    : resolve(process.cwd(), ast.configPath || DEFAULT_ASTGREP_CONFIG);

  // Доверенный прогон: sgconfig и каталоги его правил — из base, а не из checkout'а PR
  // (иначе PR, удаливший правило, снимает свою же находку). ast-grep читает их с диска сам,
  // поэтому копия base кладётся во временный каталог с той же раскладкой.
  let base;
  try { base = materializeAstgrepConfig(configPath, log); } catch (e) {
    // Копию из PR вместо base не подставляем: prescan ast-grep fail-open, как при любом сбое.
    log(`prescan ast-grep: копия конфига из base не собрана (${e.message}) — skip`);
    return { findings: [], promptSection: '', scanned: 0, error: e.message };
  }
  if (base?.missing) return { findings: [], promptSection: '', scanned: 0, skipped: 'no-config' };
  if (base) configPath = base.configPath;
  try {
    return await scanWithConfig(changedPaths, cfg, configPath, appDir, log);
  } finally {
    if (base) rmSync(base.root, { recursive: true, force: true });
  }
}

/**
 * Копия sgconfig и его ruleDirs/utilDirs из REVIEW_BASE_REF во временный каталог.
 * null — доверия нет или конфиг не из репозитория (пакет движка); {missing} — нет в base.
 */
export function materializeAstgrepConfig(configPath, log = () => {}, source = getInputSource()) {
  if (!source.trusted || !source.isRepoPath(configPath)) return null;
  const rel = relative(source.cwd, resolve(source.cwd, configPath)).split('\\').join('/');
  let text;
  try { text = source.readText(configPath); } catch {
    log(`prescan: ast-grep config ${rel} нет в ${source.baseRef} — skip`);
    return { missing: true };
  }
  const confDir = posix.dirname(rel);
  const dirs = parseSgconfigDirs(text);
  // Не разобрали — берём весь каталог конфига: правила вне него ast-grep тогда не найдёт
  // (fail-open prescan), но и копию из PR не прочитает.
  const trees = dirs ? [rel, ...dirs.map((d) => posix.normalize(posix.join(confDir, d)))] : [confDir === '.' ? rel : confDir];
  const { root, files } = materializeFromBase(source, trees);
  log(`prescan: ast-grep config из ${source.baseRef} (${files} файл(ов))${dirs ? '' : ' — sgconfig не разобран, взят каталог конфига'}`);
  return { root, configPath: join(root, rel) };
}

async function scanWithConfig(changedPaths, cfg, configPath, appDir, log) {
  const { prescan } = cfg;
  const ast = prescan.astgrep;
  try {
    await access(configPath);
  } catch {
    log('prescan: ast-grep config не найден — skip');
    return { findings: [], promptSection: '', scanned: 0, skipped: 'no-config' };
  }

  const localBin = resolve(appDir, 'node_modules/.bin/ast-grep');
  let bin;
  try {
    await access(localBin);
    bin = localBin;
  } catch {
    const configured = ast.bin || 'ast-grep';
    if (configured.includes('/') || configured.startsWith('.')) {
      const absBin = resolve(process.cwd(), configured);
      try {
        await access(absBin);
        bin = absBin;
      } catch {
        log(`prescan: ast-grep bin not found (${configured}) — skip`);
        return { findings: [], promptSection: '', scanned: 0, skipped: 'no-ast-grep' };
      }
    } else {
      bin = configured;
    }
  }

  const { entries, total, truncated } = selectLintableEntries(changedPaths, prescan.maxFiles);
  if (truncated) {
    log(`prescan ast-grep: lintable файлов ${total} > лимита ${prescan.maxFiles}`);
  }
  if (!entries.length) return { findings: [], promptSection: '', scanned: 0 };

  const pathMap = buildPrescanPathMap(entries);
  const relPaths = entries.map((e) => (prescan.appDir && e.appPath.startsWith(`${prescan.appDir}/`)
    ? e.appPath.slice(prescan.appDir.length + 1)
    : e.appPath));
  const args = ['scan', '-c', configPath, '--json=stream', ...relPaths];

  try {
    const { code, stdout, stderr } = await runCommand(bin, args, { cwd: appDir });
    if (!stdout.trim()) {
      if (code !== 0) logToolFailure(log, 'ast-grep', { code, stdout, stderr });
      return {
        findings: [], promptSection: '', scanned: entries.length, astgrepExit: code,
        lintableTotal: total, truncated,
        error: code !== 0 ? (stderr || stdout || 'empty stdout').trim().slice(0, 200) : undefined,
      };
    }
    if (code !== 0) logToolFailure(log, 'ast-grep', { code, stdout, stderr });
    const blockRuleSet = new Set(
      Array.isArray(ast.blockRules) ? ast.blockRules.filter(Boolean) : DEFAULT_ASTGREP_BLOCK_RULES,
    );
    const findings = astgrepStreamToFindings(stdout, {
      blockRules: blockRuleSet, pathMap, cwd: appDir,
    });
    return {
      findings,
      promptSection: buildPrescanPromptSection(findings),
      scanned: entries.length,
      lintableTotal: total,
      truncated,
      astgrepExit: code,
    };
  } catch (e) {
    log(`prescan ast-grep failed (fail-open): ${e.message}`);
    return { findings: [], promptSection: '', scanned: 0, error: e.message };
  }
}
