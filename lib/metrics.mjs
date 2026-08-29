/** R7+: агрегация метрик из observability JSONL (pr-review-runs.jsonl). Чистые функции. */

/** JSONL → records; битые строки пропускаем. */
export function parseJsonl(text) {
  return String(text || '')
    .split('\n')
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => { try { return JSON.parse(l); } catch { return null; } })
    .filter(Boolean);
}

// Сколько последних значений стабильности держим для строки тренда: больше — и строка отчёта
// перестаёт читаться, меньше — по ней не видно направления.
const STABILITY_RECENT_MAX = 5;

/** Сводка по событиям прогонов: runs, router, Graphify, llm_call, fp_filter, verifier, stability, verdict, time_budget. */
export function aggregateMetrics(records) {
  // Последний вердикт по каждому sha — чтобы считать СМЕНЫ решения между соседними прогонами
  // одного коммита. Наружу не отдаём: отчёту нужны числа, а не история по каждому sha.
  const verdictBySha = new Map();
  const agg = {
    runs: { total: 0, blocked: 0, llmFailed: 0, crashed: 0 },
    router: {},
    blastRadius: {
      plans: 0,
      symbolsTotal: 0,
      symbolsResolved: 0,
      affectedNonempty: 0,
      graphFresh: { fresh: 0, stale: 0, unknown: 0 },
      skipReasons: {},
    },
    llm: { total: 0, ok: 0, prompt_tokens: 0, completion_tokens: 0, total_tokens: 0, byModel: new Map() },
    fpFilterRemoved: 0,
    verifier: { verified: 0, rejected: 0, downgraded: 0, unverified: 0, skipped: 0, errors: 0 },
    stability: { events: 0, runs: 0, sum: 0, kept: 0, added: 0, gone: 0, recent: [] },
    verdict: { events: 0, blocked: 0, flips: 0, repeatedShas: 0 },
    timeBudget: { hits: 0, skippedChunks: 0 },
  };
  for (const r of records || []) {
    switch (r?.type) {
      case 'run_end':
        agg.runs.total++;
        if (r.wouldBlock) agg.runs.blocked++;
        if (r.llmFailed) agg.runs.llmFailed++;
        if (r.crashed) agg.runs.crashed++;
        break;
      case 'router':
        if (r.route) agg.router[r.route] = (agg.router[r.route] || 0) + 1;
        break;
      case 'plan': {
        const blast = r.blast_radius;
        if (!blast || typeof blast !== 'object') break;
        agg.blastRadius.plans++;
        agg.blastRadius.symbolsTotal += Number(blast.symbols_total) || 0;
        agg.blastRadius.symbolsResolved += Number(blast.symbols_resolved) || 0;
        agg.blastRadius.affectedNonempty += Number(blast.affected_nonempty) || 0;
        const freshness = blast.graph_fresh === true ? 'fresh' : blast.graph_fresh === false ? 'stale' : 'unknown';
        agg.blastRadius.graphFresh[freshness]++;
        for (const [reason, count] of Object.entries(blast.skip_reasons || {})) {
          agg.blastRadius.skipReasons[reason] = (agg.blastRadius.skipReasons[reason] || 0) + (Number(count) || 0);
        }
        break;
      }
      case 'llm_call': {
        agg.llm.total++;
        if (r.ok) agg.llm.ok++;
        agg.llm.prompt_tokens = (agg.llm.prompt_tokens || 0) + (Number(r.prompt_tokens) || 0);
        agg.llm.completion_tokens = (agg.llm.completion_tokens || 0) + (Number(r.completion_tokens) || 0);
        agg.llm.total_tokens = (agg.llm.total_tokens || 0) + (Number(r.total_tokens) || 0);
        const key = `${r.callType || '?'}/${r.model || '?'}`;
        const m = agg.llm.byModel.get(key) || { calls: 0, ok: 0, ms: 0 };
        m.calls++;
        if (r.ok) m.ok++;
        const ms = Number(r.duration_ms);
        if (Number.isFinite(ms) && ms >= 0) m.ms += ms;
        agg.llm.byModel.set(key, m);
        break;
      }
      case 'fp_filter':
        agg.fpFilterRemoved += Number(r.removed) || 0;
        break;
      case 'verifier':
        agg.verifier.verified += Number(r.verified) || 0;
        agg.verifier.rejected += Number(r.rejected) || 0;
        agg.verifier.downgraded += Number(r.downgraded) || 0;
        agg.verifier.unverified += Number(r.unverified) || 0;
        break;
      case 'verifier_skip':
        agg.verifier.skipped++;
        break;
      case 'verifier_error':
        agg.verifier.errors++;
        break;
      case 'stability': {
        agg.stability.events++;
        agg.stability.kept += Number(r.kept) || 0;
        agg.stability.added += Number(r.added) || 0;
        agg.stability.gone += Number(r.gone) || 0;
        // Прогоны без базы сравнения (первый на PR) и с пустым набором пишут stability: null —
        // в среднее они не входят, иначе «сравнивать было не с чем» осело бы в тренде как 0%.
        const value = typeof r.stability === 'number' && Number.isFinite(r.stability) ? r.stability : null;
        if (value == null) break;
        agg.stability.runs++;
        agg.stability.sum += value;
        agg.stability.recent.push(value);
        if (agg.stability.recent.length > STABILITY_RECENT_MAX) agg.stability.recent.shift();
        break;
      }
      case 'verdict': {
        agg.verdict.events++;
        const blocked = r.blocked === true;
        if (blocked) agg.verdict.blocked++;
        const sha = String(r.sha || '');
        // Без sha смену вердикта приписать некуда: в общую долю прогон войдёт, в детерминизм нет.
        if (!sha) break;
        const seen = verdictBySha.get(sha);
        if (!seen) { verdictBySha.set(sha, { last: blocked, runs: 1 }); break; }
        seen.runs++;
        // Коммит попадает в знаменатель детерминизма ровно один раз — на втором прогоне;
        // без этого «0 смен» на 20 разных sha читалось бы как измеренная воспроизводимость.
        if (seen.runs === 2) agg.verdict.repeatedShas++;
        if (seen.last !== blocked) agg.verdict.flips++;
        seen.last = blocked;
        break;
      }
      case 'time_budget':
        agg.timeBudget.hits++;
        agg.timeBudget.skippedChunks += Number(r.skippedChunks) || 0;
        break;
      default:
        break;
    }
  }
  return agg;
}

const pct = (ok, total) => (total ? `${Math.round((ok / total) * 100)}%` : '—');

/** Текстовый отчёт по сводке (для stdout / CI log). */
export function formatMetricsReport(agg, source = '') {
  const { runs, router, blastRadius, llm, fpFilterRemoved, verifier, stability, verdict, timeBudget } = agg;
  const lines = [
    `=== pr-review metrics${source ? ` (${source})` : ''} ===`,
    `Прогоны: ${runs.total} · блоков ${runs.blocked} · llm-fail ${runs.llmFailed} · crash ${runs.crashed}`,
  ];
  const routerParts = Object.entries(router).map(([k, v]) => `${k} ${v}`).join(', ');
  if (routerParts) lines.push(`Router: ${routerParts}`);
  if (blastRadius.plans) {
    lines.push(
      `Graphify blast-radius: plans ${blastRadius.plans} · symbols resolved ${blastRadius.symbolsResolved}/${blastRadius.symbolsTotal} · ` +
      `non-empty ${blastRadius.affectedNonempty} · graph fresh ${blastRadius.graphFresh.fresh}/${blastRadius.plans}`,
    );
    const freshness = blastRadius.graphFresh;
    if (freshness.stale || freshness.unknown) {
      lines.push(`  Graph state: stale ${freshness.stale}, unknown ${freshness.unknown}`);
    }
    const reasons = Object.entries(blastRadius.skipReasons).sort((a, b) => b[1] - a[1]);
    if (reasons.length) lines.push(`  Empty/skip reasons: ${reasons.map(([reason, count]) => `${reason} ${count}`).join(', ')}`);
  }
  lines.push(`LLM вызовы: ${llm.total}, ok ${pct(llm.ok, llm.total)}`);
  if (llm.total_tokens) {
    lines.push(`  Tokens: prompt ${llm.prompt_tokens}, completion ${llm.completion_tokens}, total ${llm.total_tokens}`);
  }
  const models = [...llm.byModel.entries()].sort((a, b) => b[1].calls - a[1].calls);
  for (const [key, m] of models) {
    const avg = m.calls ? (m.ms / m.calls / 1000).toFixed(1) : '—';
    lines.push(`  ${key}: ${m.calls} вызов(ов), ok ${pct(m.ok, m.calls)}, avg ${avg}с`);
  }
  lines.push(`FP-filter: снято ${fpFilterRemoved} находок`);
  lines.push(
    `Verifier: verified ${verifier.verified}, rejected ${verifier.rejected}, ` +
    `downgraded ${verifier.downgraded}, unverified ${verifier.unverified}, ` +
    `skipped ${verifier.skipped}, errors ${verifier.errors}`,
  );
  if (stability.events) {
    // Среднее считаем по прогонам с ОПРЕДЕЛЁННОЙ метрикой, но печатаем и общее число событий:
    // иначе «среднее 80%» по одному прогону из двадцати выглядело бы как устойчивый результат.
    const avg = stability.runs ? `${Math.round((stability.sum / stability.runs) * 100)}%` : '—';
    const trend = stability.recent.length
      ? ` · тренд ${stability.recent.map((v) => `${Math.round(v * 100)}%`).join(' → ')}`
      : '';
    // kept/added/gone — слагаемые самой метрики, то есть ТОЧКИ КОДА, а не находки (одна строка
    // с тремя замечаниями — одна точка). Помечаем прямо в строке: иначе читатель отчёта делит
    // эти числа как находки и получает не то среднее, что напечатано слева.
    lines.push(
      `Стабильность находок: среднее ${avg} по ${stability.runs}/${stability.events} прогон(ам) · ` +
      `kept ${stability.kept} / added ${stability.added} / gone ${stability.gone} (точки кода)${trend}`,
    );
  }
  if (verdict.events) {
    // Доля блокирующих прогонов и число смен вердикта — разные вопросы, поэтому в одной строке,
    // но раздельно: «блокировали 4 из 6» без смен на одном sha — это шесть разных диффов
    // (норма), а те же 4 из 6 при трёх сменах на одном коммите — недетерминированный гейт.
    const flipNote = verdict.repeatedShas
      ? `смен вердикта на одном sha: ${verdict.flips} (коммитов с ≥2 прогонами: ${verdict.repeatedShas})`
      : 'повторных прогонов на одном sha не было — детерминизм не измерен';
    lines.push(
      `Вердикт гейта: блокировали ${verdict.blocked}/${verdict.events} прогон(ов) ` +
      `(${pct(verdict.blocked, verdict.events)}) · ${flipNote}`,
    );
  }
  if (timeBudget.hits) {
    lines.push(`⏱ Тайм-бюджет: ${timeBudget.hits} прогон(ов) с пропусками, чанков пропущено ${timeBudget.skippedChunks}`);
  }
  return lines.join('\n');
}
