import { readFile } from 'node:fs/promises';
import { loadReviewConfig, parseCli } from './lib/config.mjs';
import { createLogger, publicErrorLabel } from './lib/util.mjs';
import { parseDiff, collectBlastSymbols, readChangedFiles, buildChunks } from './lib/diff.mjs';
import { createLlmClient } from './lib/llm.mjs';
import { createCostTracker } from './lib/cost.mjs';
import { emitBlastRadiusFindings } from './lib/blast-findings.mjs';
import { createPromptBuilder, buildChangedFilesSection } from './lib/prompt.mjs';
import { createGraphifyClient } from './lib/graphify.mjs';
import { createGithubClient } from './lib/github.mjs';
import { createReviewEngine } from './lib/engine.mjs';
import { resolveBlockers, createFindingsFormatter, filterFalsePositiveFindings, filterStalePriorFindings } from './lib/findings.mjs';
import { selectVerifierCandidates, applyVerifierVerdicts } from './lib/verifier.mjs';
import { routeFindings, findingLocationKey } from './lib/actionability.mjs';
import { applyRefutation, formatRefutationLog } from './lib/specialists.mjs';
import { createPrescanRunner, dedupeAgainstPrescan, formatPrescanLog } from './lib/prescan.mjs';
import { routeDiff, formatRouteLog, extractFileDiffSections, classifyFilePath } from './lib/router.mjs';
import { createObservability, slimFindings, countBySeverity, PR_TITLE_LOG_MAX } from './lib/observability.mjs';
import { buildSummary, buildRouterSummary, buildLlmUnavailableSummary } from './lib/report.mjs';
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

/** Совместимость с тестовыми/custom Graphify-клиентами старого API, возвращавшими массив. */
function normalizeBlastRadius(result, symbols) {
  if (Array.isArray(result)) {
    return {
      blocks: result,
      metrics: {
        symbols_total: symbols.length,
        symbols_resolved: null,
        affected_nonempty: result.length,
        graph_fresh: null,
        graph_status: 'unknown',
        skip_reason: 'client_did_not_report',
        skip_reasons: { client_did_not_report: 1 },
      },
    };
  }
  return {
    blocks: Array.isArray(result?.blocks) ? result.blocks : [],
    metrics: result?.metrics ?? {
      symbols_total: symbols.length,
      symbols_resolved: null,
      affected_nonempty: 0,
      graph_fresh: null,
      graph_status: 'unknown',
      skip_reason: 'client_did_not_report',
      skip_reasons: { client_did_not_report: 1 },
    },
  };
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

/** Стадия verifier Pass B: кандидаты critical/major → второй проход. Fail-open при сбое LLM. */
async function runVerifierStage(allFindings, {
  engine, gate, verifier, deadline, contentMap, annotated, blastBlocks, pr, changedFilesSection,
  actionability, actionabilityCtx, files,
}, log, obs) {
  const empty = { findings: allFindings, stats: null };
  if (!verifier.enabled) return empty;
  if (deadline && deadline - Date.now() < 120_000) {
    log('verifier pass B пропущен: тайм-бюджет на исходе (<2 мин в запасе)');
    obs.logEvent('verifier_skip', { reason: 'time_budget' });
    return empty;
  }
  // Advisory-кандидаты на подтверждение: доказуемые minor/nit, которые претендуют на строку
  // в PR. Verifier здесь работает не на гейт (severity их не блокирует), а на право занять
  // внимание человека — в инлайн должно уходить проверенное.
  const advisoryPool = actionability?.verifyAdvisory
    ? routeFindings(allFindings, files, {
      inlineOn: gate.blockOn,
      threshold: actionability.threshold,
      maxAdvisoryInline: actionability.verifyAdvisory,
      strict: gate.strict,
      ctx: actionabilityCtx || {},
    }).filter((s) => s.promoted).map((s) => s.finding)
    : [];
  const candidates = selectVerifierCandidates(allFindings, gate, verifier.maxCandidates, {
    candidates: advisoryPool,
    max: actionability?.verifyAdvisory || 0,
  });
  if (advisoryPool.length) log(`verifier: +${advisoryPool.length} advisory-кандидат(ов) по доказуемости`);
  if (!candidates.length) {
    obs.logEvent('verifier_skip', { reason: 'no_candidates' });
    return empty;
  }
  log(`verifier pass B: ${candidates.length} кандидат(ов) ${gate.blockOn.join('/')} · модель ${verifier.model}`);
  try {
    // Verifier получает только файлы/секции диффа с кандидатами, а не весь PR —
    // иначе на большом PR это самый жирный промпт пайплайна.
    const candFiles = new Set(candidates.map((c) => c.file).filter(Boolean));
    const fileContents = [...contentMap.entries()]
      .filter(([p]) => candFiles.has(p))
      .map(([, v]) => v)
      .join('\n');
    const sections = [...extractFileDiffSections(annotated).entries()]
      .filter(([p]) => candFiles.has(p))
      .map(([, s]) => s);
    const annotatedScoped = sections.length ? sections.join('\n') : annotated;
    log(`verifier ctx: ${candFiles.size} файл(ов) с кандидатами из ${contentMap.size}, дифф ${annotatedScoped.length}/${annotated.length} симв.`);
    const verdicts = await engine.verifyFindings(candidates, {
      annotated: annotatedScoped, fileContents, blastBlocks, pr, changedFilesSection,
    });
    const applied = applyVerifierVerdicts(allFindings, candidates, verdicts, log, {
      strictUnverified: verifier.strictBlocks,
    });
    // Strict + overflow капа REVIEW_VERIFIER_MAX: eligible-находки сверх капа не верифицируются —
    // блокировать без верификации они не должны (бэклог #7) → minor. Только при успешном verifier.
    let overflowN = 0;
    if (verifier.strictBlocks) {
      const candSet = new Set(candidates);
      for (const f of applied.findings) {
        if (gate.blockOn.includes(f.severity) && !f.prescan && !candSet.has(f)) {
          f.severity = 'minor';
          f.verifierStatus = 'overflow';
          f.verifierReason = `сверх капа verifier (${verifier.maxCandidates}) — понижено до minor`;
          overflowN++;
        }
      }
      if (overflowN) {
        applied.stats.overflow = overflowN;
        log(`verifier strict: ${overflowN} находок сверх капа ${verifier.maxCandidates} → minor (без верификации не блокируют)`);
      }
    }
    log(`verifier итог: verified=${applied.stats.verified} rejected=${applied.stats.rejected} downgraded=${applied.stats.downgraded} missing=${applied.stats.missing}`);
    obs.logEvent('verifier', applied.stats);
    logFindingsStage(obs, 'after_verifier', applied.findings);
    return { findings: applied.findings, stats: applied.stats };
  } catch (e) {
    log('verifier pass failed (fail-open, findings сохранены):', e.message);
    obs.logEvent('verifier_error', { error: e.message?.slice(0, 200) });
    return empty;
  }
}

/** Публикует inline и чистит устаревшие только после успешного post. */
async function publishInline(github, { inline, headSha, maxInline, cleanupWithoutPost = false }, log) {
  const inlineToPost = inline.slice(0, maxInline);
  if (inline.length > inlineToPost.length) {
    log(`inline-комментов ${inline.length} > лимита ${maxInline} — постим первые ${maxInline} (полные счётчики в sticky)`);
  }
  if (!inlineToPost.length && cleanupWithoutPost) {
    const cleared = await github.deleteStaleInline();
    return { posted: 0, total: inline.length, cleared };
  }
  // post-then-delete: сначала новый review, потом чистка старых инлайнов (кроме только что
  // запощенного) — при сбое постинга старые комментарии не теряются.
  const reviewId = await github.postReview({ inline: inlineToPost, headSha });
  // reviewId=null (GitHub не вернул id) → чистку пропускаем: иначе сотрём только что запощенное.
  const cleared = reviewId
    ? await github.deleteStaleInline({ exceptReviewId: reviewId })
    : (log('review id не получен — пропускаем чистку старых инлайнов'), 0);
  return { posted: inlineToPost.length, total: inline.length, cleared };
}

/** Стадия публикации: post-then-delete inline → upsert sticky. */
async function publishReview(github, { inline, md, headSha, maxInline }, log) {
  const published = await publishInline(github, { inline, headSha, maxInline }, log);
  const op = await github.upsertSticky(md);
  return { ...published, op };
}

async function notifyTelegram(cfg, log, { pr, counts, total, htmlUrl, reason }) {
  const { token, chat } = cfg.telegram;
  if (!token || !chat) return;
  const text = `🔍 Авто-ревью PR #${pr.number}: «${pr.title}»\n` +
    `${total} замечаний — 🔴${counts.critical} 🟠${counts.major} 🟡${counts.minor} 🔵${counts.nit}` +
    `${reason ? `\n⚠️ ${reason}` : ''}\n${htmlUrl}`;
  try {
    const res = await fetch(`https://api.telegram.org/bot${token}/sendMessage`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ chat_id: chat, text, disable_web_page_preview: true }),
    });
    if (!res.ok) log('telegram ping failed (не критично):', res.status, await res.text().catch(() => ''));
  } catch (e) { log('telegram ping failed (не критично):', e.message); }
}

/**
 * FP-фильтр → beforeResolve → resolveBlockers: единый путь для fail-closed и основного pipeline.
 * Возвращает findings, clusters, blockers, список снятых FP-паттернами находок и результат beforeResolve.
 */
export async function runGateResolution({ findings: inputFindings, changedPaths, gate, prescanCfg, patternsFile, log, beforeResolve }) {
  const beforeFp = inputFindings;
  const beforeFpCount = beforeFp.length;
  const afterFp = filterFalsePositiveFindings(beforeFp, changedPaths, log, { patternsFile });
  const afterFpCount = afterFp.length;
  const afterFpSet = new Set(afterFp);
  const fpRemoved = beforeFp.filter((f) => !afterFpSet.has(f));
  const intermediate = beforeResolve ? await beforeResolve(afterFp) : { findings: afterFp };
  const resolved = resolveBlockers(intermediate.findings, changedPaths, gate, log, {
    skipFpFilter: true, prescan: prescanCfg,
  });
  return { ...intermediate, ...resolved, fpRemoved, beforeFpCount, afterFpCount };
}

/** Собирает пайплайн из конфига; зависимости инжектируются для тестов. */
export function createReviewPipeline(cfg, deps = {}) {
  const log = deps.log ?? createLogger();
  const die = deps.die ?? ((msg) => { log('SOFT-FAIL:', msg); process.exit(0); });

  const costTracker = deps.costTracker ?? createCostTracker(cfg.cost?.priceMap);
  const ollama = deps.ollama ?? createLlmClient(cfg, log, { costTracker });
  const prompt = deps.prompt ?? createPromptBuilder();
  const graphify = deps.graphify ?? createGraphifyClient(cfg);
  const github = deps.github ?? createGithubClient(cfg, log);
  const findingsFmt = deps.findingsFmt ?? createFindingsFormatter(cfg);
  const prescan = deps.prescan ?? createPrescanRunner(cfg, { log });
  const telegramNotifier = deps.notifyTelegram ?? notifyTelegram;

  async function finishObs(obs, data) {
    if (!obs?.enabled) return;
    await obs.flush(data);
  }

  /** @returns {Promise<number>} exit code */
  async function run() {
    const { cli, reviewMode, ollama: oCfg, moa, models, gate, limits, verifier, specialists } = cfg;
    const { dryRun, diffFile } = cli;
    const postCreds = cfg.github.repo && cfg.github.prNumber && cfg.github.token;

    if (!oCfg.key) die('нет LLM API key (задайте LLM_API_KEY или OLLAMA_API_KEY)');
    if (reviewMode === 'moa' && !moa.references.length) die('MOA_REFERENCE_MODELS пуст — задай хотя бы один advisor');
    if (!diffFile && !postCreds) die('нет REPO/PR_NUMBER/GITHUB_TOKEN для загрузки диффа');
    if (!dryRun && !postCreds) die('постинг требует REPO/PR_NUMBER/GITHUB_TOKEN — для локального прогона добавь --dry-run');

    const obs = createObservability(cfg, { log });
    let finalObs = { exitCode: 0 };

    try {
      const engine = deps.engine ?? createReviewEngine(cfg, { ollama, prompt, log, observability: obs, costTracker });

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

      const prepT0 = Date.now();
      // Сначала читаем файлы: collectBlastSymbols добирает exports из полного текста,
      // а не только из hunk-строк (иначе правка тела функции → 0 symbols → пустой blast).
      const [contentMap, rawPriorFindings, prescanResult] = await Promise.all([
        readChangedFiles(files, limits, log),
        (github.readPriorStickyData
          ? github.readPriorStickyData()
          : github.readPriorFindings().then((findings) => ({ findings, runs: [], sha: '' }))
        ).catch((e) => {
          // Молчаливый провал стоил бы сходимости: без prior findings прогон начинает с нуля.
          log(`prior sticky недоступен (${e.message}) — прогон без carry-over`);
          return { findings: [], runs: [], sha: '' };
        }),
        prescan.run(changedPaths),
      ]);
      const symbols = collectBlastSymbols(changedText, contentMap, limits.maxSymbols);
      log(`файлов: ${files.size} · символов под blast-radius: ${symbols.length}`, symbols);
      // Freshness измеряем даже если diff не содержит экспортов: иначе `graph_fresh`
      // был бы неизвестен именно в тех review-plans, где blast-radius пустой.
      const blastResult = await graphify.buildBlastRadius(symbols);
      const { blocks: blastBlocks, metrics: blastMetrics } = normalizeBlastRadius(blastResult, symbols);
      log(`prep parallel: ${Date.now() - prepT0}мс (graphify+files+prior+prescan)`);
      // Fingerprint-инвалидация carry-over ДО LLM: устаревшие prior findings не подмешиваем.
      // Нормализуем форму: клиент github инжектируется (DI-тесты, кастомные провайдеры) и
      // может отдать массив по старому контракту или неполный объект. Ни то, ни другое не
      // должно ронять прогон на первом же обращении к .findings.
      const priorSticky = {
        findings: Array.isArray(rawPriorFindings?.findings) ? rawPriorFindings.findings : [],
        runs: Array.isArray(rawPriorFindings?.runs) ? rawPriorFindings.runs : [],
        sha: typeof rawPriorFindings?.sha === 'string' ? rawPriorFindings.sha : '',
      };
      const priorFindings = filterStalePriorFindings(priorSticky.findings, files, log);
      if (priorFindings.length !== priorSticky.findings.length) {
        obs.logEvent('prior_findings', { loaded: priorSticky.findings.length, kept: priorFindings.length });
      }

      // Контекст доказуемости для маршрутизации advisory: что уже было в прошлом прогоне,
      // что подтвердил prescan, какого класса файл. Строится ОДИН раз — его читают и
      // verifier-стадия (кого дополнительно проверить), и classify (кого поднять в строку).
      const actionabilityCtx = {
        priorKeys: new Set(priorFindings.map(findingLocationKey)),
        prescanKeys: new Set(prescanResult.findings.map(findingLocationKey)),
        fileClass: classifyFilePath,
      };
      const changedFilesSection = buildChangedFilesSection(changedPaths);
      const chunks = buildChunks(annotated, contentMap, limits);
      const note0 = chunks.length > 1
        ? `PR большой — разбит на ${chunks.length} частей для полного охвата (каждая ревьюится отдельно).`
        : '';
      // Дедлайн LLM-фазы: после него новые чанки пропускаем и постим частичный отчёт —
      // процесс завершается штатно (sticky/export/observability) до убийства job раннером.
      const deadline = limits.timeBudgetMs > 0 ? Date.now() + limits.timeBudgetMs : 0;
      log(`blast-radius блоков: ${blastBlocks.length} · symbols ${blastMetrics.symbols_resolved ?? '—'}/${blastMetrics.symbols_total} · affected ${blastMetrics.affected_nonempty} · graph ${blastMetrics.graph_status} · файлов с контекстом: ${contentMap.size} · чанков: ${chunks.length}`);
      const blastFindings = emitBlastRadiusFindings(blastBlocks, changedPaths, { symbols, model: 'blast-radius' });
      if (blastFindings.length) log(`blast-radius: +${blastFindings.length} детерминированных cross-layer caller(s)`);
      obs.logEvent('plan', {
        chunks: chunks.length,
        symbols: symbols.length,
        blastBlocks: blastBlocks.length,
        models: models.list,
        llmConcurrency: limits.concurrency,
        chunkBytes: limits.chunkBytes,
        blast_radius: blastMetrics,
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
        // changedPaths, а не готовая секция: карту PR («что в этом чанке, что ревьюится
        // отдельно») движок рисует под каждый чанк сам. changedFilesSection остаётся для
        // verifier-стадии — у неё контекст всего PR.
        blastBlocks, pr, priorFindings, changedPaths, prescanSection: prescanResult.promptSection,
        deadline,
      });
      // max, а не сумма: в consensus-режиме один пропущенный чанк считается у каждой модели.
      const skippedChunks = reviews.reduce((n, r) => Math.max(n, r.skippedChunks || 0), 0);
      if (skippedChunks) {
        log(`⏱ тайм-бюджет: ${skippedChunks} из ${chunks.length} чанков пропущены — отчёт частичный`);
        obs.logEvent('time_budget', { skippedChunks, totalChunks: chunks.length, budgetMs: limits.timeBudgetMs });
      }
      const note = [note0, skippedChunks
        ? `⏱ Тайм-бюджет исчерпан: ${skippedChunks} из ${chunks.length} частей не ревьюились — охват частичный, перезапустите ревью.`
        : ''].filter(Boolean).join(' ');
      const ok = reviews.filter((r) => !r.error);
      const llmFindings = ok.flatMap((r) => r.findings);
      let allFindings = [
        ...prescanResult.findings,
        ...blastFindings,
        ...dedupeAgainstPrescan(llmFindings, prescanResult.findings),
      ];
      logFindingsStage(obs, 'after_llm_merge', allFindings);

      if (!ok.length && (gate.llmRequired || !allFindings.length)) {
        const why = reviews.map((r) => `${r.model}: ${r.error}`).join('; ');
        const whyPublic = reviews.map((r) => `${r.model}: ${publicErrorLabel(r.error)}`).join('; ');
        log('все модели недоступны:', why, gate.llmRequired ? '— fail-closed (REVIEW_LLM_REQUIRED)' : '— fail-open');
        // Prescan-находки (eslint/ast-grep) не зависят от LLM — не теряем их. Прогоняем
        // LLM-независимые стадии (FP-фильтр, resolveBlockers с prescan.blockEnabled/blockRules);
        // refutation/verifier — LLM-стадии, при полном аутедже недоступны по определению.
        const gateResolution = await runGateResolution({
          findings: allFindings, changedPaths, gate, prescanCfg: cfg.prescan,
          patternsFile: cfg.feedback?.patternsFile, log,
        });
        allFindings = gateResolution.findings;
        const prescanBlockers = gateResolution.blockers;
        const fpRemoved = gateResolution.fpRemoved;
        if (fpRemoved.length) {
          log(`ложных находок отфильтровано: ${fpRemoved.length}`);
        }
        obs.logEvent('fp_filter', {
          before: gateResolution.beforeFpCount,
          after: gateResolution.afterFpCount,
          removed: fpRemoved.length,
        });
        logFindingsStage(obs, 'after_fp_filter', allFindings);
        const counts = countBySeverity(allFindings);
        const gateBlocked = gate.llmRequired || prescanBlockers.length > 0;
        obs.logEvent('llm_failed', { prescanOnly: true, required: gate.llmRequired });
        obs.logEvent('gate', {
          wouldBlock: gateBlocked,
          blockers: prescanBlockers.length,
          sample: slimFindings(prescanBlockers),
        });
        logFindingsStage(obs, 'after_gate', allFindings);
        await persistExports(cfg, buildReviewJson({
          pr, headSha, reviewMode, route,
          wouldBlock: gateBlocked,
          counts, total: allFindings.length, findings: allFindings,
          blockers: prescanBlockers, reviews: [],
          llmFailed: true,
          llmErrors: reviews.map((r) => ({ model: r.model, error: String(r.error || '').slice(0, 200) })),
        }), log, obs);
        if (dryRun) {
          finalObs = {
            exitCode: gateBlocked ? 1 : 0,
            llmFailed: true,
            dryRun: true,
            wouldBlock: gateBlocked,
            blockers: prescanBlockers.length,
            total: allFindings.length,
            counts,
          };
          return finalObs.exitCode;
        }
        const prescanBlockerSet = new Set(prescanBlockers);
        const prescanAdvisoryFindings = allFindings.filter((f) => f.prescan && !prescanBlockerSet.has(f));
        const body = buildLlmUnavailableSummary({
          llmRequired: gate.llmRequired,
          whyPublic,
          models: models.list,
          prescanBlockers,
          prescanAdvisoryFindings,
        });
        const { inline } = findingsFmt.classify(prescanBlockers, files);
        try {
          const pub = await publishInline(github, {
            inline, headSha, maxInline: limits.maxInline, cleanupWithoutPost: true,
          }, log);
          log(`fail-closed: запощено ${pub.posted}/${pub.total} prescan inline, очищено старых ${pub.cleared}`);
        } catch (e) {
          log('fail-closed: постинг/чистка inline упали (не критично):', String(e?.message || e).slice(0, 200));
        }
        try { await github.upsertSticky(body); } catch (e2) {
          log('и заметку не запостили:', String(e2?.message || e2).slice(0, 200));
        }
        // При неблокирующем fail-open не пингуем дежурного: sticky сохраняет диагноз без спама.
        if (gateBlocked) {
          await telegramNotifier(cfg, log, {
            pr, counts, total: allFindings.length, htmlUrl: pr.html_url,
            reason: 'fail-closed: LLM outage',
          });
        }
        finalObs = {
          exitCode: gateBlocked ? 1 : 0,
          llmFailed: true,
          wouldBlock: gateBlocked,
          blockers: prescanBlockers.length,
          total: allFindings.length,
          counts,
        };
        return finalObs.exitCode;
      }
      if (!ok.length && allFindings.length) {
        log('LLM недоступен — продолжаем с prescan-only findings (R6), REVIEW_LLM_REQUIRED=0');
        obs.logEvent('llm_failed', { prescanOnly: true, required: gate.llmRequired });
      }

      // FP-фильтр идёт ДО LLM-стадий (refutation/verifier): иначе verifier тратит токены
      // на известные ложные находки, а refutation может понизить severity уже отфильтрованного.
      const gateResolution = await runGateResolution({
        findings: allFindings, changedPaths, gate, prescanCfg: cfg.prescan,
        patternsFile: cfg.feedback?.patternsFile, log,
        beforeResolve: async (fpFilteredFindings) => {
          let currentFindings = fpFilteredFindings;

          // Refutation/verifier — LLM-стадии; при полном LLM-аутедже (ok.length === 0) пропускаем,
          // чтобы fail-closed и fall-through пути вели себя симметрично.
          let refutationStats = null;
          if (specialists.enabled && ok.length) {
            const applied = applyRefutation(currentFindings, log, { strict: true, blockOn: gate.blockOn });
            currentFindings = applied.findings;
            refutationStats = applied.stats;
            log(`refutation R5: ${formatRefutationLog(refutationStats)}`);
            obs.logEvent('refutation', refutationStats);
            logFindingsStage(obs, 'after_refutation', currentFindings);
          }

          const verifierStage = ok.length
            ? await runVerifierStage(currentFindings, {
                engine, gate, verifier, deadline, contentMap, annotated, blastBlocks, pr, changedFilesSection,
                actionability: cfg.actionability,
                actionabilityCtx,
                files,
              }, log, obs)
            : { findings: currentFindings, stats: null };
          currentFindings = verifierStage.findings;
          const verifierStats = verifierStage.stats;

          return { findings: currentFindings, refutationStats, verifierStats };
        },
      });
      allFindings = gateResolution.findings;
      const blockerClusters = gateResolution.clusters.filter((g) => g[0]?.consensusBlock);
      const blockers = gateResolution.blockers;
      const fpRemoved = gateResolution.fpRemoved;
      const refutationStats = gateResolution.refutationStats;
      const verifierStats = gateResolution.verifierStats;
      if (fpRemoved.length) {
        log(`ложных находок отфильтровано: ${fpRemoved.length}`);
      }
      obs.logEvent('fp_filter', { before: gateResolution.beforeFpCount, after: gateResolution.afterFpCount, removed: fpRemoved.length });
      logFindingsStage(obs, 'after_fp_filter', allFindings);
      obs.logEvent('gate', {
        wouldBlock: blockers.length > 0,
        blockers: blockers.length,
        sample: slimFindings(blockers),
      });
      logFindingsStage(obs, 'after_gate', allFindings);

      const { inline, outside } = findingsFmt.classify(allFindings, files, actionabilityCtx);
      const { md, counts, total, delta, verdict } = buildSummary(cfg, {
        reviews, ok, allFindings, outside, blastBlocks, blastMetrics, files, pr, headSha, note, blockerClusters,
        verifierStats, refutationStats, prescanResult, fpRemoved,
        // Δ считаем против ПОЛНОГО набора прошлого прогона, а не против отфильтрованного
        // carry-over: «закрыто» — это то, что ушло из отчёта, включая снятое инвалидацией.
        priorFindings: priorSticky.findings, priorRuns: priorSticky.runs, priorSha: priorSticky.sha,
        costSnapshot: costTracker.snapshot(),
      });
      // Дисперсия набора находок — измеримая величина, а не впечатление от чтения sticky:
      // в JSONL уходит Jaccard по точкам кода и его слагаемые, по ним недельный отчёт строит
      // тренд (бэклог P1 #8). Размеры списков Δ-блока — в `delta.findings`, они считаются по
      // другому ключу (находка = точка + формулировка) и метрику не задают.
      obs.logEvent('stability', delta);
      // Вердикт со sha — отдельным событием: `gate` не знает коммита, а без него нельзя отличить
      // «на разных диффах решения разные» (норма) от «на одном диффе решения разные» (гейт
      // недетерминирован, замер #533: блокер в 4 прогонах из 6 при неизменном диффе).
      obs.logEvent('verdict', verdict);

      await persistExports(cfg, buildReviewJson({
        pr, headSha, reviewMode, route, wouldBlock: blockers.length > 0, verdict,
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

      const pub = await publishReview(github, { inline, md, headSha, maxInline: limits.maxInline }, log);
      log(`review запощен (${pub.posted}/${pub.total} inline, очищено старых ${pub.cleared}) · summary ${pub.op}`);

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

      await telegramNotifier(cfg, log, { pr, counts, total, htmlUrl: pr.html_url });

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
