/** Учёт токенов и оценка стоимости LLM-вызовов за прогон. */

function parsePriceMap(raw) {
  if (!raw || typeof raw !== 'string' || !raw.trim()) return null;
  try {
    const parsed = JSON.parse(raw);
    return parsed && typeof parsed === 'object' ? parsed : null;
  } catch {
    return null;
  }
}

/** JSON map {"model": {"in": dollars/1M prompt, "out": dollars/1M completion}} из REVIEW_PRICE_JSON. */
export function loadPriceMap(env = process.env) {
  return parsePriceMap(env.REVIEW_PRICE_JSON);
}

/** Оценка $ по usage и таблице цен; null если цены неизвестны. */
export function estimateCost(usage, model, priceMap) {
  if (!usage || !priceMap || !model) return null;
  const rates = priceMap[model] || priceMap['*'] || priceMap.default;
  if (!rates || typeof rates !== 'object') return null;
  const inRate = Number(rates.in);
  const outRate = Number(rates.out);
  if (!Number.isFinite(inRate) && !Number.isFinite(outRate)) return null;
  const prompt = Number(usage.prompt_tokens) || 0;
  const completion = Number(usage.completion_tokens) || 0;
  const dollars = (Number.isFinite(inRate) ? (prompt / 1_000_000) * inRate : 0)
    + (Number.isFinite(outRate) ? (completion / 1_000_000) * outRate : 0);
  return Number.isFinite(dollars) ? dollars : null;
}

/** Аккумулятор usage за прогон (engine → pipeline → buildSummary). */
export function createCostTracker(priceMap = null) {
  const totals = {
    prompt_tokens: 0,
    completion_tokens: 0,
    total_tokens: 0,
    calls: 0,
    cost_usd: 0,
    cost_known: false,
  };
  const byModel = new Map();

  return {
    record(usage, model, callType) {
      if (!usage) return;
      totals.prompt_tokens += usage.prompt_tokens || 0;
      totals.completion_tokens += usage.completion_tokens || 0;
      totals.total_tokens += usage.total_tokens || 0;
      totals.calls++;
      const cost = estimateCost(usage, model, priceMap);
      if (cost != null) {
        totals.cost_usd += cost;
        totals.cost_known = true;
      }
      const key = `${callType || '?'}/${model || '?'}`;
      const m = byModel.get(key) || { calls: 0, prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 };
      m.calls++;
      m.prompt_tokens += usage.prompt_tokens || 0;
      m.completion_tokens += usage.completion_tokens || 0;
      m.total_tokens += usage.total_tokens || 0;
      byModel.set(key, m);
    },
    snapshot() {
      return {
        ...totals,
        byModel: Object.fromEntries(byModel),
      };
    },
  };
}

/** Короткая строка для sticky: `💰 ~$0.12 · 45k tokens`. */
export function formatCostStickyLine(snapshot) {
  if (!snapshot || !snapshot.total_tokens) return '';
  const tokens = snapshot.total_tokens;
  const tokStr = tokens >= 1_000_000
    ? `${(tokens / 1_000_000).toFixed(1)}M`
    : tokens >= 1000
      ? `${Math.round(tokens / 1000)}k`
      : String(tokens);
  if (snapshot.cost_known && snapshot.cost_usd > 0) {
    const usd = snapshot.cost_usd < 0.01
      ? snapshot.cost_usd.toFixed(3)
      : snapshot.cost_usd < 1
        ? snapshot.cost_usd.toFixed(2)
        : snapshot.cost_usd.toFixed(2);
    return `💰 ~$${usd} · ${tokStr} tokens`;
  }
  if (tokens > 0) return `💰 ${tokStr} tokens`;
  return '';
}
