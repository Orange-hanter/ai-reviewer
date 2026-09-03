import { getProfile } from './profile.mjs';

/** Generic MoA advisors (REVIEW_SPECIALISTS=0) — три независимых prose-ревьюера. */
export const DEFAULT_MOA_REFERENCES = 'kimi-k2.7-code,glm-5.3,glm-5.3-flash';

/**
 * Specialist advisors (REVIEW_SPECIALISTS=1, default) — семантическая привязка ролей.
 * SWE-Bench / task-fit: kimi→correctness, glm-5.3→architecture, glm-5.3-flash→security patterns.
 */
export const DEFAULT_MOA_SPECIALIST_MODELS =
  'security:glm-5.3-flash,correctness:kimi-k2.7-code,architecture:glm-5.3';

/** Jury aggregator (JSON + advisorClaims) — вне пула advisors (MiniMax: batch code review). */
export const DEFAULT_MOA_AGGREGATOR = 'minimax-m3';

/** Pass B verifier — глубокий structured judge, вне advisors (DeepSeek Pro). */
export const DEFAULT_REVIEW_VERIFIER_MODEL = 'deepseek-v4-pro';

export const DEFAULT_OLLAMA_MODELS_FALLBACK = 'deepseek-v4-flash';

/**
 * Расширения путей в Graphify blast-блоках (единый источник для парсера и formatter).
 * Алфавитные токены без regex-метасимволов; при добавлении экранируйте в месте сборки RegExp.
 */
export const BLAST_PATH_EXTS = Object.freeze([
  'ts', 'tsx', 'js', 'jsx', 'mjs', 'cjs', 'vue', 'svelte', 'go', 'py', 'rs',
]);

/** Confidence рёбер Graphify → score для emitBlastRadiusFindings / top-N. */
export const BLAST_CONF_EXTRACTED = 0.9;
export const BLAST_CONF_INFERRED = 0.75;
/** Unknown ≤ INFERRED (не «обгоняет» явную пониженную метку). */
export const BLAST_CONF_UNKNOWN = 0.7;
export const BLAST_CONF_AMBIGUOUS = 0.55;
export const BLAST_FINDINGS_MAX = 8;

/** Маркер детерминированных blast-radius находок (`Finding.source` / model label). */
export const BLAST_FINDING_SOURCE = 'blast-radius';

/**
 * Правила репозитория для промпта: слои, канонические примитивы, чем проверяется поведение.
 * Приходят из профиля проекта (`repoRules` / `repoRulesFile`); без профиля блок пустой —
 * движок ревьюит «просто код», не выдумывая архитектуру проекта.
 */
export function repoRules() {
  return getProfile().repoRules || '';
}

export const SEV = { critical: '🔴', major: '🟠', minor: '🟡', nit: '🔵' };
export const VERDICT_RANK = { changes_requested: 2, comment: 1, approve: 0 };

/**
 * Маркеры комментариев активного профиля. Живой вид, а не снимок: профиль загружается
 * при старте прогона, а модули импортируются раньше — константа зафиксировала бы дефолт.
 */
export const MARKERS = Object.freeze({
  get ns() { return getProfile().markers.ns; },
  get review() { return getProfile().markers.review; },
  get inline() { return getProfile().markers.inline; },
  get dataKey() { return getProfile().markers.dataKey; },
  get dataRe() { return getProfile().markers.dataRe; },
});

/**
 * Тело машинного футера sticky (сырой JSON-текст) или null.
 *
 * Берём ПОСЛЕДНЕЕ вхождение маркера, а не первое: находка может процитировать пример
 * маркера в своём тексте — так и случилось на PR #531, где ревью привело в теле замечания
 * `<!-- ai-review-data:{"findings":[]} -->`. Разбор «до первого» хватал этот пример,
 * успешно парсил пустой объект, и отчёт выглядел чистым при шести живых находках.
 *
 * Закрытие ищем в порядке убывания надёжности: `-->` в самом конце тела (наш футер пишется
 * последним блоком) → последний `-->`, после которого хвост парсится как JSON → просто
 * последний. Последний вариант оставляет разбор ремонту JSON у вызывающего.
 */
export function extractStickyFooterRaw(body, markers = MARKERS) {
  const text = typeof body === 'string' ? body : '';
  const open = new RegExp(`<!--\\s*${markers.dataKey}`, 'g');
  let start = -1;
  for (let m = open.exec(text); m; m = open.exec(text)) start = m.index + m[0].length;
  if (start < 0) return null;
  const rest = text.slice(start);
  const anchored = /-->\s*$/.exec(rest);
  if (anchored) return rest.slice(0, anchored.index);
  // После футера что-то дописали (ручная правка комментария): перебираем закрытия с конца и
  // берём первое, дающее валидный JSON, — иначе обрезали бы наш блок на чужой стрелке.
  const positions = [];
  for (let i = rest.indexOf('-->'); i >= 0; i = rest.indexOf('-->', i + 1)) positions.push(i);
  for (const i of positions.reverse()) {
    const candidate = rest.slice(0, i);
    try {
      JSON.parse(candidate.trim());
      return candidate;
    } catch { /* пробуем более раннее закрытие */ }
  }
  return positions.length ? rest.slice(0, Math.max(...positions)) : rest;
}

/**
 * Наш комментарий — маркер в НАЧАЛЕ body (все наши посты начинаются с маркера).
 * includes() нельзя: процитированный маркер в чужом комментарии сделал бы его «нашим»
 * (перезапись sticky / удаление inline).
 */
export const hasMarker = (body, marker) => typeof body === 'string' && body.startsWith(marker);
