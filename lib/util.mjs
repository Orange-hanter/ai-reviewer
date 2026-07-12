export const numEnv = (name, def, env = process.env) => {
  const n = Number(env[name]);
  return Number.isFinite(n) && n > 0 ? n : def;
};

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

export function createLogger(prefix = '[pr-review]') {
  return (...a) => console.log(prefix, ...a);
}
