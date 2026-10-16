/**
 * Decision-модели в shadow — RFC 21. JS-часть — только оркестрация, которую зовёт pipeline.mjs.
 *
 * Логика в Rust, JS-копий нет:
 *   - вопросы Q1/Q2, таблица семейств FP, построители входа — бинарь `decision`
 *     (rust/src/decision.rs);
 *   - HTTP-вызов модели — бинарь `llm`, режим `decide` (rust/src/llm/decide.rs).
 * Без бинарей shadow пишет одно `decision_error {reason: 'no-binary'}` и молчит.
 *
 * Здесь — то, что живёт между вызовами внутри прогона: бюджет вызовов, параллелизм,
 * выключение после исчерпанной квоты, свой учёт цены (общий costTracker печатается в
 * sticky), и fire-and-forget shadow. В JS остаётся, потому что это состояние процесса
 * pipeline.mjs, а матчинг FP-регексов (lookahead) не портирован.
 *
 * Граница, которую нельзя размывать: вердикт НЕ пишется ни в одно поле находки и ни во что,
 * что читают гейт, actionability, sticky, inline, export или футер. Он живёт только в событиях
 * observability `decision_*`. Модуль работает на копиях полей и ничего не мутирует; любое
 * исключение превращается в `decision_error`. Structure-модули не импортируются.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { defaultRustBin } from './rust-bin.mjs';
import { runRustChat } from './llm.mjs';
import { redactSecretsForLog } from './util.mjs';
import { matchFpPatterns, getFpPatterns } from './fp-patterns.mjs';
import { chunkNeedsSpecialists } from './chunk-routing.mjs';

/** Вход моделей ~32K токенов; символов с запасом (дублирует MAX_STATE_CHARS в decision.rs). */
const DECISION_MAX_STATE_CHARS = 48_000;
/** Повтор после transient/сети внутри одной перестановки. */
const DECISION_HTTP_RETRIES = 1;

/** Бинарь `llm` с режимом decide; FORCE_JS у llm = «нет бинаря» (JS-копии decide нет). */
export function resolveDecisionBin(env = process.env) {
  if (env.AI_REVIEWER_LLM_FORCE_JS === '1') return null;
  const bin = (env.AI_REVIEWER_LLM_BIN || '').trim() || defaultRustBin('llm');
  return existsSync(bin) ? bin : null;
}

/** Бинарь `decision` (вопросы, построители, отчёт замера). */
export function resolveDecisionOpsBin(env = process.env) {
  const bin = (env.AI_REVIEWER_DECISION_BIN || '').trim() || defaultRustBin('decision');
  return existsSync(bin) ? bin : null;
}

/** Синхронный вызов операции бинаря `decision`; нет бинаря — исключение `no-binary`. */
export function runDecisionOp(op, args = {}, bin = resolveDecisionOpsBin()) {
  if (!bin) throw Object.assign(new Error('no-binary'), { code: 'no-binary' });
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

function createLimiter(max) {
  let active = 0;
  const queue = [];
  const next = () => {
    if (active >= max || !queue.length) return;
    active++;
    const { fn, resolve, reject } = queue.shift();
    Promise.resolve().then(fn).then(resolve, reject).finally(() => {
      active--;
      next();
    });
  };
  return (fn) => new Promise((resolve, reject) => {
    queue.push({ fn, resolve, reject });
    next();
  });
}

/**
 * Клиент decision-модели. `decide(state, question)` никогда не бросает: возвращает либо
 * ответ бинаря `{answer, usage, latencyMs, calls, servedModel, truncated}`, либо
 * `{error, status?, quota?}`. Бюджет — `maxCalls` HTTP-вызовов на прогон, перестановки
 * считаются; после квоты клиент отказывает сразу (circuit breaker).
 */
export function createDecisionClient(cfg, { costTracker = null, runBin = runRustChat, bin = resolveDecisionBin() } = {}) {
  const d = cfg.decision;
  const permutations = Math.max(1, Math.min(d.permutations, 10));
  const limit = createLimiter(Math.max(1, d.concurrency));
  const stats = { calls: 0, budgetHit: false, quotaHit: false, errors: 0, costUsd: 0, tokens: 0 };

  async function decide(state, question) {
    if (!bin) return { error: 'no-binary' };
    if (!d.key) return { error: 'no-key' };
    if (stats.quotaHit) return { error: 'quota-off' };
    if (stats.calls + permutations > d.maxCalls) {
      stats.budgetHit = true;
      return { error: 'budget' };
    }
    stats.calls += permutations;
    return limit(async () => {
      if (stats.quotaHit) return { error: 'quota-off' };
      const input = JSON.stringify({
        decide: true, url: `${d.base}/decisions`, key: d.key, model: d.model, state, question,
        permutations, httpRetries: DECISION_HTTP_RETRIES, timeoutMs: d.timeoutMs,
        maxStateChars: DECISION_MAX_STATE_CHARS,
      });
      const execTimeoutMs = d.timeoutMs * permutations * (DECISION_HTTP_RETRIES + 1) + 5_000;
      let res;
      try {
        res = await runBin(bin, input, { execTimeoutMs });
      } catch (e) {
        stats.errors++;
        return { error: redactSecretsForLog(String(e?.message || e), 300) };
      }
      const usage = res.__error ? res.__error.usage : res.usage;
      record(usage);
      if (res.__error) {
        stats.errors++;
        if (res.__error.quota) stats.quotaHit = true;
        return {
          error: redactSecretsForLog(String(res.__error.error || 'decision error'), 300),
          status: res.__error.status ?? null, quota: Boolean(res.__error.quota),
        };
      }
      return res;
    });
  }

  function record(usage) {
    if (!usage) return;
    stats.tokens += usage.total_tokens || 0;
    if (Number.isFinite(usage.cost)) stats.costUsd += usage.cost;
    costTracker?.record(usage, d.model, 'decision');
  }

  return { decide, stats: () => ({ ...stats }), model: d.model, permutations };
}

/** Сводка ответа для события: top-3 вместо всех вероятностей. */
function slimAnswer(res) {
  const a = res.answer || {};
  const top3 = Object.entries(a.probabilities || {})
    .sort((x, y) => y[1] - x[1]).slice(0, 3)
    .map(([value, p]) => [value, Math.round(p * 1e4) / 1e4]);
  return {
    argmax: a.argmax ?? null, top3, flip: Boolean(a.flip),
    tvMax: Number.isFinite(a.tvMax) ? Math.round(a.tvMax * 1e4) / 1e4 : null,
    confidence: Number.isFinite(a.confidence) ? Math.round(a.confidence * 1e4) / 1e4 : null,
    refusal: a.refusal ?? null, truncated: Boolean(res.truncated),
  };
}

/**
 * Shadow-слой над клиентом: `onFindings` (Q1 до FP-фильтра) и `onChunks` (Q2) запускают
 * вызовы без ожидания, `drain(ms)` ждёт хвост не дольше `ms` и пишет `decision_summary`.
 * Ничего не возвращает в пайплайн и не бросает наружу. `ops` — вызов бинаря `decision`
 * (подменяется в тестах).
 */
export function createDecisionShadow({ cfg, decider, obs, log = () => {}, ops = runDecisionOp }) {
  const pending = new Set();
  const questions = new Set(cfg.decision.questions);
  const t0 = Date.now();
  let errors = 0;
  let asked = 0;
  let qs = null;

  const logError = (question, err) => {
    errors++;
    try {
      obs.logEvent('decision_error', {
        question, reason: err?.error || String(err?.message || err), status: err?.status ?? null,
        quota: Boolean(err?.quota),
      });
    } catch { /* obs fail-open */ }
  };

  const track = (question, fn) => {
    asked++;
    const p = Promise.resolve().then(fn).catch((e) => logError(question, e));
    pending.add(p);
    p.finally(() => pending.delete(p));
  };

  const getQuestions = () => (qs ??= ops('questions'));

  function onFindings(findings, { changedPaths = [], patternsFile, diffSections = null } = {}) {
    if (!questions.has('fp')) return;
    try {
      const patterns = getFpPatterns(patternsFile || undefined);
      // Prescan — детерминированная улика, а не вывод модели: классифицировать нечего.
      const copies = (findings || []).filter((f) => f && !f.prescan).map((f) => ({
        file: f.file, line: f.line, severity: f.severity, model: f.model,
        title: f.title, body: f.body, source: f.source ?? null,
      }));
      if (!copies.length) return;
      const regexIds = copies.map((c) => matchFpPatterns(`${c.title} ${c.body}`, changedPaths, patterns));
      const built = ops('fpStates', {
        items: copies.map((c, i) => ({ finding: c, section: diffSections?.get?.(c.file) || '', regexPatternId: regexIds[i] })),
      });
      const question = getQuestions().fp;
      copies.forEach((copy, i) => track('fp', async () => {
        const res = await decider.decide(built[i].state, question);
        if (res.error) return logError('fp', res);
        obs.logEvent('decision_fp', {
          file: copy.file, line: copy.line, severity: copy.severity, model: copy.model,
          source: copy.source, title: String(copy.title || '').slice(0, 120),
          regexPatternId: regexIds[i], regexFamily: built[i].regexFamily ?? null,
          decider: { model: res.servedModel || decider.model, ...slimAnswer(res) },
          latencyMs: res.latencyMs ?? null,
        });
      }));
    } catch (e) {
      logError('fp', e);
    }
  }

  function onChunks(chunks, routingCtx = null) {
    if (!questions.has('chunk')) return;
    try {
      const list = chunks || [];
      if (!list.length) return;
      const classOf = typeof routingCtx?.fileClass === 'function' ? routingCtx.fileClass : () => 'other';
      const classes = {};
      for (const c of list) for (const p of c?.paths || []) classes[p] ??= classOf(p);
      const states = ops('chunkStates', {
        chunks: list.map((c) => ({ paths: c?.paths || [], annotated: c?.annotated || '' })), classes,
      });
      const question = getQuestions().chunk;
      list.forEach((chunk, chunkIndex) => {
        // Маршрут выключен (REVIEW_CHUNK_ROUTING=0) — детерминированно все чанки с жюри.
        const deterministic = routingCtx ? (chunkNeedsSpecialists(chunk, routingCtx) ? 'jury' : 'single') : 'jury';
        const nPaths = Array.isArray(chunk?.paths) ? chunk.paths.length : 0;
        track('chunk', async () => {
          const res = await decider.decide(states[chunkIndex], question);
          if (res.error) return logError('chunk', res);
          const answer = slimAnswer(res);
          obs.logEvent('decision_chunk', {
            chunkIndex, nPaths, deterministic, decision: answer.argmax,
            agree: answer.argmax === deterministic,
            decider: { model: res.servedModel || decider.model, ...answer },
            latencyMs: res.latencyMs ?? null,
          });
        });
      });
    } catch (e) {
      logError('chunk', e);
    }
  }

  async function drain(timeoutMs) {
    try {
      if (pending.size) {
        let timer;
        const timedOut = await Promise.race([
          Promise.allSettled([...pending]).then(() => false),
          new Promise((r) => { timer = setTimeout(() => r(true), Math.max(0, timeoutMs)); }),
        ]);
        clearTimeout(timer);
        if (timedOut) logError('drain', { error: 'drain-timeout' });
      }
      const s = decider.stats();
      obs.logEvent('decision_summary', {
        mode: cfg.decision.mode, model: decider.model, permutations: decider.permutations,
        asked, calls: s.calls, budgetHit: s.budgetHit, quotaHit: s.quotaHit, errors,
        tokens: s.tokens, costUsd: Math.round(s.costUsd * 1e6) / 1e6, durationMs: Date.now() - t0,
      });
    } catch (e) {
      log(`decision shadow: сводка не записана — ${e?.message || e}`);
    }
  }

  return { onFindings, onChunks, drain };
}
