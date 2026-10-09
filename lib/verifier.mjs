/**
 * Фаза 3, шаг 3: lib/verifier.mjs — JS-обёртка над Rust-бинарём
 * rust/target/debug/verifier (порт чистых частей Pass B: selectVerifierCandidates,
 * applyVerifierVerdicts).
 *
 * LLM-runner (createVerifierRunner) намеренно остаётся здесь, в JS: он ходит
 * через инжектимый ollama (llm-бинарь Фазы 3, шаг 1) и инжектимый prompt
 * (DI-моки тестов/пайплайна подменяют оба — они должны остаться внутри
 * процесса Node). Диспетчеризация чистых функций — env на каждый вызов:
 *   AI_REVIEWER_VERIFIER_BIN — override; по умолчанию <repo>/rust/target/debug/verifier;
 *   AI_REVIEWER_VERIFIER_FORCE_JS=1 — принудительно чистый JS (паритет/отладка).
 * Бинаря нет (CI до cargo build) — эталонный JS fallback ниже; бинарь битый —
 * ошибка пробрасывается (молчаливая подмена скрывала бы регрессии).
 *
 * Контракт с бинарём — индексы вместо ссылок: findings не круглятся через JSON
 * без потери идентичности объектов, поэтому обёртка сопоставляет кандидатов с
 * позициями в массиве (indexOf по ссылке, как Set-семантика в JS). Если advisory-
 * кандидат или кандидат apply НЕ из этого массива — обёртка деградирует на
 * чистый JS (такой вход для бинаря невыразим, а не «битый»). Мутации
 * (severity/verifierStatus/verifierReason) бинарь возвращает по индексу,
 * обёртка накладывает на исходные объекты — семантика разделяемых ссылок
 * пайплайна сохранена. Паритет — rust/parity/verifier.parity.mjs.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_VERIFIER_BIN = resolve(__dir, '../rust/target/debug/verifier');

function resolveVerifierBin() {
  if (process.env.AI_REVIEWER_VERIFIER_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_VERIFIER_BIN || '').trim() || DEFAULT_VERIFIER_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveVerifierBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

/** Все объекты candidates — элементы массива findings (по ссылке)? */
function allFrom(findings, candidates) {
  return Array.isArray(candidates) && candidates.every((c) => findings.includes(c));
}

const SEV_ORDER = { critical: 0, major: 1, minor: 2, nit: 3 };

/** Эталонная копия selectVerifierCandidates (fallback при отсутствии бинаря). */
function selectVerifierCandidatesJs(findings, gate, max = 12, advisory = {}) {
  const tiers = new Set(gate.blockOn || []);
  const blockers = findings
    .filter((f) => tiers.has(f.severity) && !f.prescan)
    .sort((a, b) => (SEV_ORDER[a.severity] ?? 9) - (SEV_ORDER[b.severity] ?? 9))
    .slice(0, max);
  // Advisory-кандидаты: minor/nit, набравшие доказуемость по actionability. Verifier для них —
  // не про блокировку merge (её severity не даёт), а про право занять строку в PR: в инлайн
  // должно уходить проверенное, а не просто уверенное. Кандидатов выбирает вызывающий,
  // здесь только отсечка по лимиту и защита от повторов.
  const { candidates = [], max: advisoryMax = 0 } = advisory;
  if (!advisoryMax || !candidates.length) return blockers;
  const seen = new Set(blockers);
  const extra = candidates.filter((f) => !seen.has(f) && !f.prescan).slice(0, advisoryMax);
  return [...blockers, ...extra];
}

/** Кандидаты на второй проход — severity из gate.blockOn, critical первыми. */
export function selectVerifierCandidates(findings, gate, max = 12, advisory = {}) {
  const { candidates = [], max: advisoryMax = 0 } = advisory || {};
  // Индексы advisory в findings (дедуп в JS — по идентичности объекта; объект
  // вне массива для бинаря невыразим → деградация на чистый JS).
  if (!allFrom(findings, candidates)) return selectVerifierCandidatesJs(findings, gate, max, advisory);
  const r = runRust('select', {
    findings,
    blockOn: gate?.blockOn || [],
    max,
    advisory: { candidateIndices: candidates.map((c) => findings.indexOf(c)), max: advisoryMax },
  });
  if (r === null) return selectVerifierCandidatesJs(findings, gate, max, advisory);
  return r.indices.map((i) => findings[i]);
}

/** Эталонная копия applyVerifierVerdicts (fallback при отсутствии бинаря). */
function applyVerifierVerdictsJs(allFindings, candidates, verdicts, log, { strictUnverified = false } = {}) {
  const verdictMap = new Map();
  for (const v of verdicts || []) {
    if (v && Number.isInteger(v.candidateIndex)) verdictMap.set(v.candidateIndex, v);
  }
  const toRemove = new Set();
  const stats = { verified: 0, rejected: 0, downgraded: 0, missing: 0, unverified: 0 };

  for (let i = 0; i < candidates.length; i++) {
    const f = candidates[i];
    const v = verdictMap.get(i);
    const decision = v?.decision;
    if (!decision) {
      stats.missing++;
      if (strictUnverified) {
        f.severity = 'minor';
        f.verifierStatus = 'unverified';
        f.verifierReason = 'verifier не подтвердил — понижено до minor (REVIEW_VERIFIER_STRICT)';
        stats.unverified++;
        log(`verifier unverified → minor: [${f.file}:${f.line}] «${f.title}»`);
      }
      continue;
    }
    if (decision === 'rejected') {
      toRemove.add(f);
      f.verifierStatus = 'rejected';
      f.verifierReason = v.reason || '';
      stats.rejected++;
      log(`verifier rejected: [${f.severity}] ${f.file}:${f.line} «${f.title}» — ${v.reason || 'без причины'}`);
    } else if (decision === 'downgrade') {
      f.severity = ['minor', 'nit'].includes(v.severity) ? v.severity : 'minor';
      f.verifierStatus = 'downgraded';
      f.verifierReason = v.reason || '';
      stats.downgraded++;
      log(`verifier downgraded: «${f.title}» → ${f.severity}`);
    } else {
      f.verifierStatus = 'verified';
      f.verifierReason = v.reason || '';
      stats.verified++;
    }
  }

  return {
    findings: allFindings.filter((f) => !toRemove.has(f)),
    stats,
  };
}

/**
 * Применяет verdicts verifier к findings.
 * rejected → убираем из списка; downgrade → понижаем severity; verified → оставляем.
 * Нет verdict для кандидата → fail-open (оставляем как есть), а при strictUnverified —
 * fail-closed для блоков: понижаем до minor (блок требует подтверждения verifier).
 */
export function applyVerifierVerdicts(allFindings, candidates, verdicts, log = () => {}, { strictUnverified = false } = {}) {
  if (!allFrom(allFindings, candidates)) {
    return applyVerifierVerdictsJs(allFindings, candidates, verdicts, log, { strictUnverified });
  }
  const r = runRust('apply', {
    allFindings,
    candidateIndices: candidates.map((c) => allFindings.indexOf(c)),
    verdicts: verdicts || [],
    strictUnverified,
  });
  if (r === null) return applyVerifierVerdictsJs(allFindings, candidates, verdicts, log, { strictUnverified });
  // Мутации по индексу — на исходные объекты (семантика разделяемых ссылок).
  for (const m of r.mutations) Object.assign(allFindings[m.index], m.set);
  for (const line of r.logs) log(line);
  return { findings: r.kept.map((i) => allFindings[i]), stats: r.stats };
}

/** LLM Pass B — один batch-вызов на список кандидатов. */
export function createVerifierRunner(cfg, { ollama, prompt, log, observability }) {
  async function verifyFindings(candidates, ctx) {
    if (!candidates.length) return [];
    const messages = prompt.buildVerifierMessages({
      candidates,
      annotated: ctx.annotated,
      fileContents: ctx.fileContents,
      blastBlocks: ctx.blastBlocks || [],
      pr: ctx.pr,
      changedFilesSection: ctx.changedFilesSection || '',
      ...(ctx.symbolCards?.length ? { symbolCards: ctx.symbolCards } : {}),
    });
    // Размер входа verifier'а — единственного вызова прогона, который engine не оборачивает
    // (он ходит в ollama напрямую). Без этого поля gold-замер не может показать, что дали
    // окна контекста вместо файлов: у всех остальных ролей prompt_chars пишет engine.
    const promptChars = messages.reduce((n, m) => n + String(m?.content ?? '').length, 0);
    const t0 = Date.now();
    try {
      const result = await ollama.chatJson(messages, cfg.verifier.model, {
        temperature: cfg.verifier.temperature,
      });
      const verdicts = Array.isArray(result.verdicts) ? result.verdicts : [];
      const usage = result?.__usage || null;
      observability?.logEvent('llm_call', {
        callType: 'verifier',
        model: cfg.verifier.model,
        candidates: candidates.length,
        duration_ms: Date.now() - t0,
        ok: true,
        verdicts: verdicts.length,
        prompt_chars: promptChars,
        ...(usage ? {
          prompt_tokens: usage.prompt_tokens,
          completion_tokens: usage.completion_tokens,
          total_tokens: usage.total_tokens,
        } : {}),
      });
      return verdicts;
    } catch (e) {
      observability?.logEvent('llm_call', {
        callType: 'verifier',
        model: cfg.verifier.model,
        candidates: candidates.length,
        duration_ms: Date.now() - t0,
        ok: false,
        prompt_chars: promptChars,
        error: e.message?.slice(0, 200),
      });
      throw e;
    }
  }

  return { verifyFindings };
}
