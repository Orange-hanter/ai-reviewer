import { readFile } from 'node:fs/promises';
import { MARKERS } from './lib/constants.mjs';
import { loadReviewConfig, parseCli } from './lib/config.mjs';
import { createLogger } from './lib/util.mjs';
import { parseDiff, extractChangedSymbols, readChangedFiles, buildChunks } from './lib/diff.mjs';
import { createOllamaClient } from './lib/ollama.mjs';
import { createPromptBuilder, buildChangedFilesSection } from './lib/prompt.mjs';
import { createGraphifyClient } from './lib/graphify.mjs';
import { createGithubClient } from './lib/github.mjs';
import { createReviewEngine } from './lib/engine.mjs';
import { resolveBlockers, createFindingsFormatter, filterFalsePositiveFindings } from './lib/findings.mjs';
import { selectVerifierCandidates, applyVerifierVerdicts } from './lib/verifier.mjs';
import { applyRefutation, formatRefutationLog } from './lib/specialists.mjs';
import { createPrescanRunner, dedupeAgainstPrescan, formatPrescanLog } from './lib/prescan.mjs';
import { routeDiff, formatRouteLog } from './lib/router.mjs';
import { createObservability, slimFindings, countBySeverity, PR_TITLE_LOG_MAX } from './lib/observability.mjs';
import { buildSummary, buildRouterSummary } from './lib/report.mjs';
import { buildReviewJson, writeReviewExports } from './lib/export.mjs';
import { parseTrustedAuthors } from './lib/feedback.mjs';
import { buildTriageContext, runAutoTriage } from './lib/triage.mjs';

function logFindingsStage(obs, stage, findings) {
  if (!obs?.enabled) return;
  obs.logEvent('findings_stage', {
    stage,
    count: findings.length,
    bySeverity: countBySeverity(findings),
    sample: slimFindings(findings),
  });
}

async function persistExports(cfg, payload, log, obs) {
  if (!cfg.export?.enabled) return null;
  if (cfg.cli?.dryRun && !cfg.export.dryRun) return null;
  try {
    const out = await writeReviewExports(cfg, payload, log);
    obs?.logEvent('export', {
      ok: true,
      files: out?.written?.length ?? 0,
      sarifResults: out?.sarifResults ?? 0,
    });
    return out;
  } catch (e) {
    log('export R9 failed (fail-open):', e.message);
    obs?.logEvent('export', { ok: false, error: String(e.message || e).slice(0, 200) });
    return null;
  }
}

async function notifyTelegram(cfg, log, { pr, counts, total, htmlUrl }) {
  const { token, chat } = cfg.telegram;
  if (!token || !chat) return;
  const text = `🔍 Авто-ревью PR #${pr.number}: «${pr.title}»\n` +
    `${total} замечаний — 🔴${counts.critical} 🟠${counts.major} 🟡${counts.minor} 🔵${counts.nit}\n${htmlUrl}`;
  try {
    const res = await fetch(`https://api.telegram.org/bot${token}/sendMessage`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ chat_id: chat, text, disable_web_page_preview: true }),
    });
    if (!res.ok) log('telegram ping failed (не критично):', res.status, await res.text().catch(() => ''));
  } catch (e) { log('telegram ping failed (не критично):', e.message); }
}

/** Собирает пайплайн из конфига; зависимости инжектируются для тестов. */
export function createReviewPipeline(cfg, deps = {}) {
  const log = deps.log ?? createLogger();
  const die = deps.die ?? ((msg) => { log('SOFT-FAIL:', msg); process.exit(0); });

  const ollama = deps.ollama ?? createOllamaClient(cfg, log);
  const prompt = deps.prompt ?? createPromptBuilder();
  const graphify = deps.graphify ?? createGraphifyClient(cfg);
  const github = deps.github ?? createGithubClient(cfg, log);
  const findingsFmt = deps.findingsFmt ?? createFindingsFormatter(cfg);
  const prescan = deps.prescan ?? createPrescanRunner(cfg, { log });

  async function finishObs(obs, data) {
    if (!obs?.enabled) return;
    await obs.flush(data);
  }

  /** @returns {Promise<number>} exit code */
  async function run() {
    const { cli, reviewMode, ollama: oCfg, moa, models, gate, limits, verifier, specialists } = cfg;
    const { dryRun, diffFile } = cli;
    const postCreds = cfg.github.repo && cfg.github.prNumber && cfg.github.token;

    if (!oCfg.key) die('нет OLLAMA_API_KEY');
    if (reviewMode === 'moa' && !moa.references.length) die('MOA_REFERENCE_MODELS пуст — задай хотя бы один advisor');
    if (!diffFile && !postCreds) die('нет REPO/PR_NUMBER/GITHUB_TOKEN для загрузки диффа');
    if (!dryRun && !postCreds) die('постинг требует REPO/PR_NUMBER/GITHUB_TOKEN — для локального прогона добавь --dry-run');

    const obs = createObservability(cfg, { log });
    let finalObs = { exitCode: 0 };

    try {
      const engine = deps.engine ?? createReviewEngine(cfg, { ollama, prompt, log, observability: obs });

      let pr = { number: cfg.github.prNumber, title: '(local diff)', html_url: '' };
      let headSha = '';
      let diff;

      if (diffFile) {
        diff = await readFile(diffFile, 'utf8');
        log(`дифф из файла ${diffFile} (${diff.length} байт)`);
      } else {
        const loaded = await github.loadDiff();
        if (!loaded) die('не удалось загрузить дифф PR (нет REPO/PR/token или сбой GitHub API)');
        if (loaded.draft) die('PR — черновик, пропускаем');
        pr = loaded.pr;
        headSha = loaded.headSha;
        diff = loaded.diff;
        log(`PR #${pr.number} «${pr.title}» · дифф ${diff.length} байт · head ${headSha.slice(0, 7)}`);
      }

      if (!diff.trim()) die('пустой дифф');

      const { files, annotated, changedText } = parseDiff(diff);
      const changedPaths = [...files.keys()];
      const route = routeDiff(changedPaths, diff, cfg.router);
      log(`router R4: ${formatRouteLog(route)}`);
      obs.logEvent('run_start', {
        pr: pr.number,
        prTitle: String(pr.title || '').slice(0, PR_TITLE_LOG_MAX),
        headSha: headSha?.slice(0, 7),
        reviewMode,
        dryRun,
        files: changedPaths.length,
        diffBytes: diff.length,
      });
      obs.logEvent('router', { route: route.route, reason: route.reason, files: route.files?.map((f) => f.class) });

      if (route.route === 'skip' || route.route === 'light') {
        const md = buildRouterSummary(cfg, { route, pr, headSha, changedPaths });
        finalObs = { exitCode: 0, route: route.route, wouldBlock: false };
        await persistExports(cfg, buildReviewJson({
          pr, headSha, reviewMode, route, wouldBlock: false,
          counts: { critical: 0, major: 0, minor: 0, nit: 0 },
          total: 0, findings: [], blockers: [], reviews: [],
        }), log, obs);
        if (dryRun) {
          log(`=== DRY-RUN: router ${route.route} — LLM пропущен ===`);
          console.log('\n--- SUMMARY MARKDOWN ---\n' + md);
          return 0;
        }
        if (postCreds) {
          await github.deleteStaleInline().catch(() => {});
          await github.upsertSticky(md);
          log(`router ${route.route}: sticky обновлён, LLM не вызывался`);
        }
        return 0;
      }

      const symbols = extractChangedSymbols(changedText, limits.maxSymbols);
      log(`файлов: ${files.size} · символов под blast-radius: ${symbols.length}`, symbols);

      const prepT0 = Date.now();
      const [blastBlocks, contentMap, priorFindings, prescanResult] = await Promise.all([
        symbols.length ? graphify.buildBlastRadius(symbols) : Promise.resolve([]),
        readChangedFiles(files, limits, log),
        github.readPriorFindings().catch(() => []),
        prescan.run(changedPaths),
      ]);
      log(`prep parallel: ${Date.now() - prepT0}мс (graphify+files+prior+prescan)`);

      const changedFilesSection = buildChangedFilesSection(changedPaths);
      const chunks = buildChunks(annotated, contentMap, limits);
      const note = chunks.length > 1
        ? `PR большой — разбит на ${chunks.length} частей для полного охвата (каждая ревьюится отдельно).`
        : '';
      log(`blast-radius блоков: ${blastBlocks.length} · файлов с контекстом: ${contentMap.size} · чанков: ${chunks.length}`);
      obs.logEvent('plan', {
        chunks: chunks.length,
        symbols: symbols.length,
        blastBlocks: blastBlocks.length,
        models: models.list,
        llmConcurrency: limits.concurrency,
        chunkBytes: limits.chunkBytes,
      });

      if (priorFindings.length) log(`прошлых находок подгружено: ${priorFindings.length} (сходимость)`);
      log(`prescan R6: ${formatPrescanLog(prescanResult)}`);
      obs.logEvent('prescan', {
        findings: prescanResult.findings.length,
        scanned: prescanResult.scanned,
        skipped: prescanResult.skipped,
        truncated: prescanResult.truncated || false,
        lintableTotal: prescanResult.lintableTotal,
        byTool: prescanResult.byTool,
        error: prescanResult.error,
        eslintError: prescanResult.eslint?.error,
        astgrepError: prescanResult.astgrep?.error,
        sample: slimFindings(prescanResult.findings),
      });

      log(`режим: ${reviewMode}${reviewMode === 'moa'
        ? ` · ${specialists.enabled ? `specialists: ${specialists.assignments.map((a) => `${a.role}=${a.model}`).join(', ')}` : `advisors: ${moa.references.join(', ')}`} · aggregator: ${moa.aggregator}` : ''} · модели: ${models.list.join(', ')} · порог: ${gate.consensusMin}`);

      const reviews = await engine.reviewAllModels(chunks, {
        blastBlocks, pr, priorFindings, changedFilesSection, prescanSection: prescanResult.promptSection,
      });
      const ok = reviews.filter((r) => !r.error);
      const llmFindings = ok.flatMap((r) => r.findings);
      let allFindings = [
        ...prescanResult.findings,
        ...dedupeAgainstPrescan(llmFindings, prescanResult.findings),
      ];
      logFindingsStage(obs, 'after_llm_merge', allFindings);

      if (!ok.length && !allFindings.length) {
        const why = reviews.map((r) => `${r.model}: ${r.error}`).join('; ');
        log('все модели недоступны:', why);
        await persistExports(cfg, buildReviewJson({
          pr, headSha, reviewMode, route, wouldBlock: false,
          counts: { critical: 0, major: 0, minor: 0, nit: 0 },
          total: 0, findings: [], blockers: [], reviews: [],
          llmFailed: true,
          llmErrors: reviews.map((r) => ({ model: r.model, error: String(r.error || '').slice(0, 200) })),
        }), log, obs);
        if (dryRun) {
          finalObs = { exitCode: 1, llmFailed: true, dryRun: true, wouldBlock: false };
          return 1;
        }
        const body = `${MARKERS.review}\n## 🤖 Авто код-ревью\n\n⚠️ ИИ-ревью временно недоступно (${why.slice(0, 200)}).\n\n<sub>Модели: ${models.list.join(', ')}. Это не блокирует merge.</sub>`;
        try { await github.upsertSticky(body); } catch (e2) { log('и заметку не запостили:', e2.message); }
        finalObs = { exitCode: 0, llmFailed: true, wouldBlock: false };
        return 0;
      }
      if (!ok.length && allFindings.length) {
        log('LLM недоступен — продолжаем с prescan-only findings (R6)');
        obs.logEvent('llm_failed', { prescanOnly: true });
      }

      let refutationStats = null;
      if (specialists.enabled) {
        const applied = applyRefutation(allFindings, log, { strict: true, blockOn: gate.blockOn });
        allFindings = applied.findings;
        refutationStats = applied.stats;
        log(`refutation R5: ${formatRefutationLog(refutationStats)}`);
        obs.logEvent('refutation', refutationStats);
        logFindingsStage(obs, 'after_refutation', allFindings);
      }

      const beforeFilter = allFindings.length;
      allFindings = filterFalsePositiveFindings(allFindings, changedPaths, log, {
        patternsFile: cfg.feedback?.patternsFile,
      });
      if (allFindings.length < beforeFilter) {
        log(`ложных находок отфильтровано: ${beforeFilter - allFindings.length}`);
      }
      obs.logEvent('fp_filter', { before: beforeFilter, after: allFindings.length, removed: beforeFilter - allFindings.length });
      logFindingsStage(obs, 'after_fp_filter', allFindings);

      let verifierStats = null;
      if (verifier.enabled) {
        const candidates = selectVerifierCandidates(allFindings, gate, verifier.maxCandidates);
        if (candidates.length) {
          log(`verifier pass B: ${candidates.length} кандидат(ов) ${gate.blockOn.join('/')} · модель ${verifier.model}`);
          try {
            const fileContents = [...contentMap.values()].join('\n');
            const verdicts = await engine.verifyFindings(candidates, {
              annotated, fileContents, blastBlocks, pr, changedFilesSection,
            });
            const applied = applyVerifierVerdicts(allFindings, candidates, verdicts, log);
            allFindings = applied.findings;
            verifierStats = applied.stats;
            log(`verifier итог: verified=${verifierStats.verified} rejected=${verifierStats.rejected} downgraded=${verifierStats.downgraded} missing=${verifierStats.missing}`);
            obs.logEvent('verifier', verifierStats);
            logFindingsStage(obs, 'after_verifier', allFindings);
          } catch (e) {
            log('verifier pass failed (fail-open, findings сохранены):', e.message);
            obs.logEvent('verifier_error', { error: e.message?.slice(0, 200) });
          }
        } else {
          obs.logEvent('verifier_skip', { reason: 'no_candidates' });
        }
      }

      const resolved = resolveBlockers(allFindings, changedPaths, gate, log, {
        skipFpFilter: true, prescan: cfg.prescan,
      });
      allFindings = resolved.findings;
      const blockerClusters = resolved.clusters.filter((g) => g[0]?.consensusBlock);
      const blockers = resolved.blockers;
      obs.logEvent('gate', {
        wouldBlock: blockers.length > 0,
        blockers: blockers.length,
        sample: slimFindings(blockers),
      });
      logFindingsStage(obs, 'after_gate', allFindings);

      const { inline, outside } = findingsFmt.classify(allFindings, files);
      const { md, counts, total } = buildSummary(cfg, {
        reviews, ok, allFindings, outside, blastBlocks, files, pr, headSha, note, blockerClusters,
        verifierStats, refutationStats, prescanResult,
      });

      await persistExports(cfg, buildReviewJson({
        pr, headSha, reviewMode, route, wouldBlock: blockers.length > 0,
        counts, total, findings: allFindings, blockers, reviews,
        verifierStats, refutationStats, prescanResult,
      }), log, obs);

      if (dryRun) {
        log('=== DRY-RUN: вывод не постится ===');
        log(`модели-ок: ${ok.map((r) => r.model).join(', ')} · findings=${total} · inline=${inline.length} · outside=${outside.length}`);
        log(blockers.length
          ? `⛔ WOULD BLOCK merge: ${blockers.length} кластер(ов) × ${gate.blockOn.join('/')}`
          : `✅ не блокирует (${reviewMode === 'moa' ? 'MoA-aggregator' : `консенсус ≥${gate.consensusMin}`})`);
        console.log('\n--- INLINE COMMENTS ---');
        for (const c of inline) console.log(`${c.path}:${c.line}\n  ${c.body.split('\n').slice(0, 2).join(' / ')}`);
        console.log('\n--- SUMMARY MARKDOWN ---\n' + md);
        const exitCode = blockers.length ? 1 : 0;
        finalObs = { exitCode, dryRun: true, wouldBlock: blockers.length > 0, blockers: blockers.length, total, counts };
        return exitCode;
      }

      const cleared = await github.deleteStaleInline();
      const inlineToPost = inline.slice(0, limits.maxInline);
      if (inline.length > inlineToPost.length) {
        log(`inline-комментов ${inline.length} > лимита ${limits.maxInline} — постим первые ${limits.maxInline} (полные счётчики в sticky)`);
      }
      await github.postReview({ inline: inlineToPost, headSha });
      const op = await github.upsertSticky(md);
      log(`review запощен (${inlineToPost.length}/${inline.length} inline, очищено старых ${cleared}) · summary ${op}`);

      if (cfg.triage?.enabled) {
        try {
          const triageCtx = buildTriageContext({
            allFindings,
            blockers,
            changedPaths,
            headSha,
            blockOn: gate.blockOn,
            patternsFile: cfg.feedback?.patternsFile,
            trustedAuthors: parseTrustedAuthors(cfg.feedback?.trustedAuthors),
          });
          const triageStats = await runAutoTriage(github, triageCtx, log);
          obs?.logEvent('auto_triage', triageStats);
          if (triageStats.resolved) log(`auto-triage: resolved ${triageStats.resolved} thread(s)`);
        } catch (e) {
          log('auto-triage failed (fail-open):', e.message);
          obs?.logEvent('auto_triage_error', { error: String(e.message || e).slice(0, 200) });
        }
      }

      await notifyTelegram(cfg, log, { pr, counts, total, htmlUrl: pr.html_url });

      if (blockers.length) {
        log(`⛔ merge ЗАБЛОКИРОВАН: ${blockers.length} кластер(ов) уровня ${gate.blockOn.join('/')}`);
        finalObs = { exitCode: 1, wouldBlock: true, blockers: blockers.length, total, counts };
        return 1;
      }
      log(`готово — блокеров нет (${reviewMode}).`);
      finalObs = { exitCode: 0, wouldBlock: false, blockers: 0, total, counts };
      return 0;
    } catch (e) {
      finalObs = {
        exitCode: 0,
        crashed: true,
        error: String(e?.message || e).slice(0, 200),
      };
      throw e;
    } finally {
      await finishObs(obs, finalObs);
    }
  }

  return { run };
}

/** CLI: загрузка конфига и запуск. */
export async function runReview(env = process.env, argv = process.argv.slice(2)) {
  const cfg = loadReviewConfig(env, parseCli(argv));
  const pipeline = createReviewPipeline(cfg);
  try {
    process.exit(await pipeline.run());
  } catch (e) {
    console.error('[pr-review]', e.stack || e.message);
    process.exit(0);
  }
}
