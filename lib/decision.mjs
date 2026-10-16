/**
 * Decision-модели в shadow — RFC 21.
 *
 * Decision-модель получает вход и вопрос с закрытым набором вариантов и возвращает
 * вероятность каждого варианта, не генерируя текста. Здесь два вопроса:
 *   Q1 `fp_family`   — к какому семейству FP (docs/false-positives.md) относится находка или
 *                      она настоящая; сопоставляется с регекс-фильтром fp-patterns;
 *   Q2 `chunk_route` — нужно ли чанку жюри специалистов; сопоставляется с chunkNeedsSpecialists.
 *
 * Граница, которую нельзя размывать: вердикт НЕ пишется ни в одно поле находки и ни во что,
 * что читают гейт, actionability, sticky, inline, export или футер. Он живёт только в событиях
 * observability `decision_*`. Модуль работает на копиях полей и ничего не мутирует; любое
 * исключение превращается в `decision_error`. Structure-модули не импортируются.
 *
 * HTTP-вызов — в Rust-бинаре `llm` (режим `decide`, rust/src/llm/decide.rs; RFC 20: новое
 * поведение модуля llm — только в Rust). JS-копии вызова нет: без бинаря shadow молчит.
 * Здесь — то, что живёт между вызовами: бюджет вызовов на прогон, параллелизм, выключение
 * после исчерпанной квоты, свой учёт цены (общий costTracker печатается в sticky).
 */

import { existsSync } from 'node:fs';
import { defaultRustBin } from './rust-bin.mjs';
import { runRustChat } from './llm.mjs';
import { redactSecretsForLog } from './util.mjs';
import { matchFpPatterns, getFpPatterns } from './fp-patterns.mjs';
import { chunkNeedsSpecialists } from './chunk-routing.mjs';

/** Вход моделей ~32K токенов; символов с запасом под инструкции и варианты. */
export const DECISION_MAX_STATE_CHARS = 48_000;
/** ±строк диффа вокруг строки находки во входе Q1. */
export const FP_WINDOW_MARGIN = 15;
/** Повтор после transient/сети внутри одной перестановки. */
const DECISION_HTTP_RETRIES = 1;

/**
 * Q1. Порядок вариантов — порядок семейств в docs/false-positives.md; `real` последним.
 * Описания на английском — как системные промпты Rust (RFC 20).
 */
export const FP_QUESTION = Object.freeze({
  name: 'fp_family',
  instructions: 'A code-review bot produced the finding below for the shown diff fragment. '
    + 'Decide whether it is a real defect or a false positive, and if false, of which kind.',
  choices: Object.freeze([
    { value: 'f1_in_diff', description: 'Claims a file or test is missing from the PR, but it is present in the diff.' },
    { value: 'f2_convergence', description: 'Re-asserts an earlier finding that the current code already addressed.' },
    { value: 'f3_neighbor_commit', description: 'Ignores a fix made in a neighbouring commit of the same PR.' },
    { value: 'f4_hypothetical', description: 'Describes code, behaviour or an attack path that does not exist in the shown code.' },
    { value: 'f5_outdated', description: 'Relies on outdated knowledge of a language, library or tool instead of the code.' },
    { value: 'f6_unseen', description: 'Asserts a problem about code the reviewer could not see (outside its part of the PR).' },
    { value: 'f7_absent_path', description: 'Claims a file or module does not exist although it exists in the repository.' },
    { value: 'f8_retry_leak', description: 'Claims a resource or timer leaks inside a retry loop although it is released each attempt.' },
    { value: 'other_fp', description: 'A false positive of another kind, e.g. an intentional, documented design decision.' },
    { value: 'real', description: 'A real defect that a careful reviewer would want fixed.' },
  ].map(Object.freeze)),
});

/** Q2. `jury` первым — исходный порядок совпадает с безопасным дефолтом. */
export const CHUNK_QUESTION = Object.freeze({
  name: 'chunk_route',
  instructions: 'A part of a pull request is shown below (file paths and the diff). Decide whether '
    + 'reviewing it needs a panel of specialist reviewers or a single general reviewer is enough.',
  choices: Object.freeze([
    { value: 'jury', description: 'Executable logic, configuration that runs, or security-relevant code with non-trivial behaviour.' },
    { value: 'single', description: 'Documentation, data, fixtures, snapshots or a trivial change with no executable behaviour.' },
  ].map(Object.freeze)),
});

const FP_VALUES = new Set(FP_QUESTION.choices.map((c) => c.value));

/**
 * Семейство каждого паттерна пакетного feedback/fp-patterns.jsonl. Формат jsonl не меняется:
 * у потребителя свои паттерны, их id сюда не попадут и получат `null`. Большая часть пакетных
 * паттернов — «намеренный дизайн» этого репозитория: в семейства 1–8 не ложатся → `other_fp`.
 * Тест держит покрытие: новый id в jsonl без записи здесь — красный.
 */
export const FP_FAMILY_BY_PATTERN_ID = Object.freeze({
  'e2e-in-diff': 'f1_in_diff',
  'file-absent-in-diff': 'f1_in_diff',
  'gha-version-hallucination': 'f5_outdated',
  'metrics-absent-observability': 'f6_unseen',
  'softblast-typedef-cosmetic': 'other_fp',
  'graphify-path-norm-speculation': 'f4_hypothetical',
  'graphify-depth-shell-injection': 'f4_hypothetical',
  'blast-source-literal-softblast': 'other_fp',
  'parse-blast-dependents-export-exists': 'f6_unseen',
  'costtracker-engine-deps-intentional': 'other_fp',
  'graphify-symbol-dash-flag-fp': 'f4_hypothetical',
  'softblast-module-boundary-intentional': 'other_fp',
  'redact-vs-obs-duplicate-ok': 'other_fp',
  'fp-findings-cluster-intentional': 'other_fp',
  'crosschunk-filterFindingsFixedInDiff': 'f6_unseen',
  'crosschunk-emitBlast-dropAmbiguous': 'f6_unseen',
  'test-whitebox-graphify-import': 'other_fp',
  'test-fp-import-channels-ok': 'other_fp',
  'test-fixture-secrets-ok': 'other_fp',
  'redact-query-ampersand-edge': 'other_fp',
  'escape-md-ticks-intentional': 'other_fp',
  'metrics-bymodel-object-contract': 'other_fp',
  'plan-todo-completeness': 'other_fp',
  'mcp-oversized-eof-tail': 'f4_hypothetical',
  'structure-cli-base-source-required': 'other_fp',
  'trust-tier-default-documented': 'other_fp',
  'index-reexport-primitives-ok': 'other_fp',
  'trust-tier-validated-in-receipt': 'f6_unseen',
  'structure-namespace-parallel-ok': 'other_fp',
  'hotspot-confidence-conservative': 'other_fp',
  'property-evidence-vs-bound': 'other_fp',
  'context-truncated-conservative': 'other_fp',
  'drift-unique-threshold-strict-by-design': 'other_fp',
  'drift-path-overlap-heuristic': 'other_fp',
  'export-env-read-intentional': 'other_fp',
  'crossboundary-deepest-prefix-ok': 'other_fp',
  'lcov-match-edge-case-heuristic': 'other_fp',
  'lcov-kind-list-coupling-ok': 'other_fp',
  'mcp-pin-args-both-allowed': 'other_fp',
  'scip-truncation-drops-edges-by-design': 'other_fp',
  'scip-definition-choice-deterministic': 'other_fp',
  'test-body-boundary-heuristic': 'other_fp',
  'short-contract-labels-heuristic': 'other_fp',
  'structure-primitives-single-source': 'other_fp',
  'structure-symlink-guard-landed': 'f3_neighbor_commit',
  'graph-sha-prefix-compare-ok': 'other_fp',
  'schema-path-pattern-duplication-ok': 'other_fp',
  'schema-no-cross-field-check-ok': 'other_fp',
  'clamp-test-asserts-behaviour': 'other_fp',
  'mcp-test-watchdog-ok': 'other_fp',
  'mutation-line-zero-unreachable': 'f4_hypothetical',
});

/** Семейство по id паттерна; неизвестный id или null → null. */
export function fpFamilyOfPattern(id) {
  const family = id ? FP_FAMILY_BY_PATTERN_ID[id] : null;
  return family && FP_VALUES.has(family) ? family : null;
}

/**
 * Окно аннотированной секции диффа (формат parseDiff: `    12: +код`, `     : -старое`)
 * вокруг строки `line`: строки с номером в [line−margin, line+margin] и удалённые строки
 * между ними. Заголовки секции (`diff --git`, `---`, `+++`) сохраняются. Нет строки — вся
 * секция (вход всё равно урежет бинарь).
 */
export function diffWindowFor(section, line, margin = FP_WINDOW_MARGIN) {
  const text = String(section || '');
  const ln = Number(line);
  if (!text || !Number.isFinite(ln) || ln <= 0) return text;
  const out = [];
  let lastNo = 0;
  for (const raw of text.split('\n')) {
    if (/^(diff --git|--- |\+\+\+ )/.test(raw)) {
      out.push(raw);
      continue;
    }
    const m = /^\s*(\d*):/.exec(raw);
    if (!m) continue;
    const no = m[1] ? Number(m[1]) : lastNo;
    if (m[1]) lastNo = no;
    if (no >= ln - margin && no <= ln + margin) out.push(raw);
  }
  return out.join('\n');
}

/** Вход Q1: находка и окно диффа вокруг её строки. */
export function buildFpState(finding, diffWindow) {
  return [
    `Finding: [${finding.severity || '?'}] ${finding.file || '?'}:${finding.line ?? '?'}`,
    `Title: ${finding.title || ''}`,
    `Body: ${finding.body || ''}`,
    '',
    'Diff fragment (annotated with new line numbers):',
    diffWindow || '(no diff fragment for this file)',
  ].join('\n');
}

/** Вход Q2: пути чанка с классами и его дифф. */
export function buildChunkState(chunk, { fileClass } = {}) {
  const classOf = typeof fileClass === 'function' ? fileClass : () => 'other';
  const paths = Array.isArray(chunk?.paths) ? chunk.paths : [];
  return [
    'Files:',
    ...(paths.length ? paths.map((p) => `- ${p} (${classOf(p)})`) : ['- (unknown)']),
    '',
    'Diff:',
    String(chunk?.annotated || ''),
  ].join('\n');
}

/** Бинарь `llm` с режимом decide; JS-копии нет, поэтому FORCE_JS = «нет бинаря». */
export function resolveDecisionBin(env = process.env) {
  if (env.AI_REVIEWER_LLM_FORCE_JS === '1') return null;
  const bin = (env.AI_REVIEWER_LLM_BIN || '').trim() || defaultRustBin('llm');
  return existsSync(bin) ? bin : null;
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

/** Сводка ответа для события: top-3 вместо всех десяти вероятностей. */
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
 * Ничего не возвращает в пайплайн и не бросает наружу.
 */
export function createDecisionShadow({ cfg, decider, obs, log = () => {} }) {
  const pending = new Set();
  const questions = new Set(cfg.decision.questions);
  const t0 = Date.now();
  let errors = 0;
  let asked = 0;

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

  function onFindings(findings, { changedPaths = [], patternsFile, diffSections = null } = {}) {
    if (!questions.has('fp')) return;
    try {
      const patterns = getFpPatterns(patternsFile || undefined);
      for (const f of findings || []) {
        // Prescan — детерминированная улика, а не вывод модели: классифицировать нечего.
        if (!f || f.prescan) continue;
        const copy = {
          file: f.file, line: f.line, severity: f.severity, model: f.model,
          title: f.title, body: f.body, source: f.source ?? null,
        };
        const regexPatternId = matchFpPatterns(`${copy.title} ${copy.body}`, changedPaths, patterns);
        const window = diffWindowFor(diffSections?.get?.(copy.file) || '', copy.line);
        track('fp', async () => {
          const res = await decider.decide(buildFpState(copy, window), FP_QUESTION);
          if (res.error) return logError('fp', res);
          obs.logEvent('decision_fp', {
            file: copy.file, line: copy.line, severity: copy.severity, model: copy.model,
            source: copy.source, title: String(copy.title || '').slice(0, 120),
            regexPatternId, regexFamily: fpFamilyOfPattern(regexPatternId),
            decider: { model: res.servedModel || decider.model, ...slimAnswer(res) },
            latencyMs: res.latencyMs ?? null,
          });
        });
      }
    } catch (e) {
      logError('fp', e);
    }
  }

  function onChunks(chunks, routingCtx = null) {
    if (!questions.has('chunk')) return;
    try {
      (chunks || []).forEach((chunk, chunkIndex) => {
        // Маршрут выключен (REVIEW_CHUNK_ROUTING=0) — детерминированно все чанки с жюри.
        const deterministic = routingCtx ? (chunkNeedsSpecialists(chunk, routingCtx) ? 'jury' : 'single') : 'jury';
        const state = buildChunkState(chunk, routingCtx || {});
        const nPaths = Array.isArray(chunk?.paths) ? chunk.paths.length : 0;
        track('chunk', async () => {
          const res = await decider.decide(state, CHUNK_QUESTION);
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
        if (timedOut) logError('drain', { error: 'drain-timeout', pending: pending.size });
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
