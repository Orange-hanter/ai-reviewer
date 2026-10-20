import { createHash } from 'node:crypto';
import { realpath } from 'node:fs/promises';
import { dirname, posix, resolve, sep } from 'node:path';

/** Логин, от которого движок пишет sticky и inline, если проект не задал иной. */
export const DEFAULT_TRUSTED_AUTHORS = 'github-actions[bot]';

/** `'a, b'` → `['a','b']`; пустая строка → `[]` (= доверять всем, см. isTrustedReviewAuthor). */
export function parseTrustedAuthors(raw = DEFAULT_TRUSTED_AUTHORS) {
  return String(raw).split(',').map((s) => s.trim()).filter(Boolean);
}

/**
 * Доверенный автор комментария бота — exact allowlist логина (без учёта регистра) и, если
 * GitHub прислал `user.type`, обязательно `Bot`. Пустой allowlist — доверять всем
 * (явный откат через `REVIEW_TRUSTED_AUTHORS=`).
 */
export function isTrustedReviewAuthor(login, trustedAuthors, user) {
  if (!trustedAuthors?.length) return true;
  if (!login) return false;
  const l = login.toLowerCase();
  if (!trustedAuthors.some((a) => l === String(a).toLowerCase())) return false;
  if (user?.type && user.type !== 'Bot') return false;
  return true;
}

export const numEnv = (name, def, env = process.env) => {
  const n = Number(env[name]);
  return Number.isFinite(n) && n > 0 ? n : def;
};

/**
 * Env-число ≥ 0, где НОЛЬ — значащее значение («выключено»), а не повод взять дефолт.
 * Отличие от numEnv принципиально: `REVIEW_CONTEXT_MARGIN=0` — это откат окон контекста
 * (RFC 07), и через numEnv он молча превратился бы в дефолтные 40.
 *
 * Пустая строка и отсутствие переменной — по-прежнему «не задано»: `VAR=` в workflow
 * означает «не передали», а не «ноль», и трактовать её как ноль значило бы выключать
 * фичу тем, кто просто объявил переменную пустой.
 */
export const nonNegEnv = (name, def, env = process.env) => {
  const raw = env[name];
  if (raw == null || (typeof raw === 'string' && raw.trim() === '')) return def;
  const n = Number(raw);
  return Number.isFinite(n) && n >= 0 ? n : def;
};

/** Env-число в диапазоне [0, 1] (доли/рейты); вне диапазона → дефолт. */
export const ratioEnv = (name, def, env = process.env) => {
  const n = Number(env[name]);
  return Number.isFinite(n) && n >= 0 && n <= 1 ? n : def;
};

/** Env-флаг как REVIEW_LLM_REQUIRED: 0/false/no/off → false; 1/true/yes/on → true. */
export function envFlag(name, defaultOn = true, env = process.env) {
  const raw = env[name];
  if (raw == null || String(raw).trim() === '') return defaultOn;
  const v = String(raw).trim().toLowerCase();
  if (['0', 'false', 'no', 'off'].includes(v)) return false;
  if (['1', 'true', 'yes', 'on'].includes(v)) return true;
  return defaultOn;
}

/**
 * Трёхзначный режим включения `0` | `shadow` | `1` (путь `0` → `shadow` → дефолт, RFC 13/21).
 * `shadow` — буквально; остальное — по правилам envFlag; нераспознанное → `def`.
 */
export function envMode(name, def = '0', env = process.env) {
  const raw = env[name];
  if (raw == null || String(raw).trim() === '') return def;
  const v = String(raw).trim().toLowerCase();
  if (v === 'shadow') return 'shadow';
  if (['0', 'false', 'no', 'off'].includes(v)) return '0';
  if (['1', 'true', 'yes', 'on'].includes(v)) return '1';
  return def;
}

export const parseModelList = (raw, { exclude = [] } = {}) => [...new Set(
  String(raw || '').split(',').map((s) => s.trim()).filter(Boolean),
)].filter((m) => !exclude.includes(m));

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export const isTransient = (status) => status === 429 || (status >= 500 && status <= 599);

export const backoffMs = (attempt, retryAfter) => {
  const ra = Number(retryAfter);
  if (Number.isFinite(ra) && ra > 0) return Math.min(ra * 1000, 30_000);
  return Math.min(1000 * 2 ** attempt, 15_000);
};

export async function mapLimit(items, limit, fn) {
  const out = new Array(items.length);
  let i = 0;
  const workers = Array.from({ length: Math.max(1, Math.min(limit, items.length)) }, async () => {
    while (i < items.length) { const idx = i++; out[idx] = await fn(items[idx]); }
  });
  await Promise.all(workers);
  return out;
}

export function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.keys(value).sort().map((key) => (
      `${JSON.stringify(key)}:${canonicalJson(value[key])}`
    )).join(',')}}`;
  }
  return JSON.stringify(value);
}

export function sha256(value) {
  const data = typeof value === 'string' || Buffer.isBuffer(value)
    ? value
    : canonicalJson(value);
  return createHash('sha256').update(data).digest('hex');
}

export function stableId(prefix, ...parts) {
  return `${prefix}:${sha256(parts.map((part) => String(part ?? '')).join('\0')).slice(0, 24)}`;
}

/**
 * Валидатор repo-relative путей: отклоняет абсолютные пути, `..`-выходы и NUL.
 * Не путать с normalizeAppPath из lib/prescan-shared.mjs — тот только добавляет
 * appDir-префикс layout'а и ничего не валидирует; здесь префиксы не добавляются.
 */
export function normalizeRepoPath(value) {
  const raw = String(value || '').replaceAll('\\', '/').trim();
  if (!raw || raw.includes('\0') || raw.startsWith('/') || /^[A-Za-z]:\//.test(raw)) {
    throw new Error(`unsafe repository path: ${JSON.stringify(raw)}`);
  }
  const normalized = posix.normalize(raw).replace(/^\.\//, '');
  if (!normalized || normalized === '..' || normalized.startsWith('../')) {
    throw new Error(`unsafe repository path: ${JSON.stringify(raw)}`);
  }
  return normalized;
}

async function realpathOrAncestor(path) {
  let current = path;
  while (true) {
    try { return await realpath(current); } catch { /* не существует — поднимаемся к предку */ }
    const parent = dirname(current);
    if (parent === current) throw new Error(`path escapes allowed root: ${path}`);
    current = parent;
  }
}

/**
 * Containment пути внутри root: realpath root'а и цели (или ближайшего существующего
 * предка — для записи в ещё не созданные каталоги), поэтому symlink внутри root,
 * указывающий наружу, не обходит проверку. mustExist — для чтения: цель обязана
 * существовать, realpath раскрывает symlink самой цели.
 */
export async function resolveContainedPath(targetPath, { rootDir, mustExist = false } = {}) {
  const root = await realpath(resolve(rootDir || process.cwd()));
  const candidate = resolve(root, String(targetPath));
  let real;
  if (mustExist) {
    try {
      real = await realpath(candidate);
    } catch (error) {
      // цели нет: containment проверяем по ближайшему существующему предку,
      // а если он внутри root — пробрасываем исходную ошибку (ENOENT)
      const ancestor = await realpathOrAncestor(candidate);
      if (ancestor !== root && !ancestor.startsWith(`${root}${sep}`)) {
        throw new Error(`path escapes allowed root: ${targetPath}`);
      }
      throw error;
    }
  } else {
    real = await realpathOrAncestor(candidate);
  }
  if (real !== root && !real.startsWith(`${root}${sep}`)) {
    throw new Error(`path escapes allowed root: ${targetPath}`);
  }
  return candidate;
}

/**
 * Адаптивный семафор параллелизма (провайдер LLM): жёсткий cap + AIMD —
 * при 429 мультипликативное снижение (×0.7, floor 2), за серию успехов +1 до max.
 * concurrency=10 chunk-pipelines × 3 specialists без него давал burst → 429 (PR #348).
 */
export function createAdaptiveSemaphore(max) {
  const hardMax = Math.max(1, max);
  let limit = hardMax;
  let running = 0;
  let sinceIncrease = 0;
  const queue = [];
  const pump = () => {
    while (running < limit && queue.length) {
      running += 1;
      queue.shift()();
    }
  };
  return {
    async acquire() {
      if (running < limit) { running += 1; return; }
      await new Promise((resolve) => queue.push(resolve));
    },
    release() {
      running = Math.max(0, running - 1);
      sinceIncrease += 1;
      if (sinceIncrease >= 25 && limit < hardMax) { limit += 1; sinceIncrease = 0; }
      pump();
    },
    throttle() {
      limit = Math.max(2, Math.floor(limit * 0.7));
      sinceIncrease = 0;
    },
    stats() { return { limit, running, queued: queue.length }; },
  };
}

export function createLogger(prefix = '[pr-review]') {
  return (...a) => console.log(prefix, ...a);
}

/**
 * Безопасная короткая метка ошибки для ПУБЛИЧНОГО sticky-комментария.
 * Сырой текст ошибки (фрагменты ответа API, URL, возможные секреты) остаётся
 * в CI-логе и observability JSONL — в PR уходит только категория.
 */
export function publicErrorLabel(err) {
  const m = String(err?.message || err || '');
  if (/aborted|timed?\s*out|ETIMEDOUT/i.test(m)) return 'таймаут';
  if (/тайм-бюджет|time[_ ]?budget/i.test(m)) return 'тайм-бюджет';
  if (/fetch failed|network|socket|ECONN|EAI_AGAIN|terminated/i.test(m)) return 'сеть';
  // Маркер ставит Rust chat() на 402 и квотном 429 (rust/src/llm.rs, is_quota_exhausted).
  if (m.includes('квота провайдера исчерпана')) return 'квота провайдера исчерпана';
  const http = /\b([45]\d{2})\b/.exec(m)?.[1];
  if (http === '429') return 'HTTP 429 (rate limit)';
  if (http) return `HTTP ${http}`;
  if (/json|parse|unexpected token/i.test(m)) return 'невалидный ответ';
  return 'ошибка вызова';
}

/** Маскирует типичные секреты в тексте для логов / observability (не для sticky). */
export function redactSecretsForLog(text, maxLen = 200) {
  let s = String(text ?? '');
  // Quoted values first (могут содержать пробелы), затем unquoted до ,; или EOL.
  s = s
    .replace(/(authorization:\s*(?:bearer|basic|digest)\s+)\S+/gi, '$1***')
    .replace(
      /\b(api[_-]?key|access[_-]?token|client[_-]?secret|secret|password)\s*[:=]\s*(['"])(?:\\.|(?!\2).)*\2/gi,
      '$1=***',
    )
    .replace(
      /\b(api[_-]?key|access[_-]?token|client[_-]?secret|secret|password)\s*[:=]\s*(?!\*\*\*)[^\n,;&]+/gi,
      '$1=***',
    )
    .replace(/([?&](?:api_key|access_token|token|secret|key)=)[^&\s]+/gi, '$1***')
    .replace(
      /\b(sk-[A-Za-z0-9_-]{8,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}|AIza[0-9A-Za-z_-]{20,}|AKIA[0-9A-Z]{16})\b/g,
      '***',
    );
  if (s.length > maxLen) s = `${s.slice(0, Math.max(0, maxLen - 1))}…`;
  return s;
}
