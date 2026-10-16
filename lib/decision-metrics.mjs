/**
 * Метрики офлайн-замера decision-моделей (RFC 21, этап 0) — чистые функции без I/O.
 *
 * Что меряем и почему так:
 *   - согласие с метками — AUC по p(real) (ранговая, не требует порога и калибровки) с
 *     бутстрап-ДИ: на десятках меток точечная оценка без интервала ничего не значит;
 *   - шум повтора и смещение порядка — РАЗДЕЛЬНО: у Decision-1 TV повтора ≈ 0.04 сравним с
 *     эффектом порядка ≈ 0.10 (проба 2026-10-16), без разделения шум сойдёт за смещение;
 *   - κ Коэна между моделями — независим ли второй сигнал (критерий K4);
 *   - калибровка (ECE, температура) — только разведочно: на таком N порог не подбирается.
 */

/** Детерминированный ГПСЧ (mulberry32) — бутстрап воспроизводим между прогонами отчёта. */
export function rng(seed = 1) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6D2B79F5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Квантиль (линейная интерполяция), пустой массив → null. */
export function quantile(values, q) {
  const xs = values.filter(Number.isFinite).sort((a, b) => a - b);
  if (!xs.length) return null;
  const pos = (xs.length - 1) * q;
  const lo = Math.floor(pos);
  const hi = Math.ceil(pos);
  return xs[lo] + (xs[hi] - xs[lo]) * (pos - lo);
}

export function mean(values) {
  const xs = values.filter(Number.isFinite);
  return xs.length ? xs.reduce((s, x) => s + x, 0) / xs.length : null;
}

/**
 * AUC (Манн–Уитни): вероятность, что случайный положительный получит балл выше случайного
 * отрицательного; ничьи — по 0.5. Нет одного из классов → null.
 */
export function auc(scores, labels) {
  const pos = [];
  const neg = [];
  scores.forEach((s, i) => {
    if (!Number.isFinite(s)) return;
    (labels[i] ? pos : neg).push(s);
  });
  if (!pos.length || !neg.length) return null;
  let sum = 0;
  for (const p of pos) {
    for (const n of neg) sum += p > n ? 1 : p === n ? 0.5 : 0;
  }
  return sum / (pos.length * neg.length);
}

/**
 * Перцентильный бутстрап-ДИ статистики `stat(sample)` по элементам `items`.
 * Выборки, где статистика не определена (null), пропускаются.
 */
export function bootstrapCi(items, stat, { b = 2000, alpha = 0.05, seed = 7 } = {}) {
  if (!items.length) return null;
  const r = rng(seed);
  const vals = [];
  for (let k = 0; k < b; k++) {
    const sample = Array.from({ length: items.length }, () => items[Math.floor(r() * items.length)]);
    const v = stat(sample);
    if (Number.isFinite(v)) vals.push(v);
  }
  if (vals.length < b / 2) return null;
  return [quantile(vals, alpha / 2), quantile(vals, 1 - alpha / 2)];
}

/** κ Коэна двух разметок одних элементов (категории — строки). Пары с null пропускаются. */
export function cohenKappa(a, b) {
  const pairs = a.map((x, i) => [x, b[i]]).filter(([x, y]) => x != null && y != null);
  if (!pairs.length) return null;
  const n = pairs.length;
  const cats = [...new Set(pairs.flat())];
  const po = pairs.filter(([x, y]) => x === y).length / n;
  let pe = 0;
  for (const c of cats) {
    pe += (pairs.filter(([x]) => x === c).length / n) * (pairs.filter(([, y]) => y === c).length / n);
  }
  return pe === 1 ? (po === 1 ? 1 : 0) : (po - pe) / (1 - pe);
}

function ranks(xs) {
  const idx = xs.map((x, i) => [x, i]).sort((p, q) => p[0] - q[0]);
  const r = new Array(xs.length);
  for (let i = 0; i < idx.length;) {
    let j = i;
    while (j + 1 < idx.length && idx[j + 1][0] === idx[i][0]) j++;
    const avg = (i + j) / 2 + 1;
    for (let k = i; k <= j; k++) r[idx[k][1]] = avg;
    i = j + 1;
  }
  return r;
}

/** Спирмен (Пирсон над рангами со средними рангами ничьих). */
export function spearman(x, y) {
  const pairs = x.map((v, i) => [v, y[i]]).filter(([p, q]) => Number.isFinite(p) && Number.isFinite(q));
  if (pairs.length < 3) return null;
  const rx = ranks(pairs.map((p) => p[0]));
  const ry = ranks(pairs.map((p) => p[1]));
  const mx = mean(rx);
  const my = mean(ry);
  let num = 0, dx = 0, dy = 0;
  for (let i = 0; i < rx.length; i++) {
    num += (rx[i] - mx) * (ry[i] - my);
    dx += (rx[i] - mx) ** 2;
    dy += (ry[i] - my) ** 2;
  }
  return dx && dy ? num / Math.sqrt(dx * dy) : null;
}

/** TV двух распределений-объектов {value: p}. */
export function tv(a, b) {
  const keys = new Set([...Object.keys(a || {}), ...Object.keys(b || {})]);
  let s = 0;
  for (const k of keys) s += Math.abs((a?.[k] || 0) - (b?.[k] || 0));
  return s / 2;
}

export function argmaxOf(p) {
  let best = null;
  for (const [k, v] of Object.entries(p || {})) if (best === null || v > p[best]) best = k;
  return best;
}

/**
 * Шум повтора по элементу: распределения R вызовов на одном порядке вариантов →
 * сменился ли argmax и наибольшая попарная TV.
 */
export function repeatNoise(dists) {
  const ok = dists.filter(Boolean);
  if (ok.length < 2) return null;
  const args = ok.map(argmaxOf);
  let tvMax = 0;
  for (let i = 0; i < ok.length; i++) for (let j = i + 1; j < ok.length; j++) tvMax = Math.max(tvMax, tv(ok[i], ok[j]));
  return { argmaxChanged: new Set(args).size > 1, tvMax };
}

/**
 * ECE бинарных вероятностей: |средняя уверенность − точность| по корзинам, взвешенно.
 * `probs` — p(положительного), `labels` — boolean.
 */
export function ece(probs, labels, bins = 10) {
  const items = probs.map((p, i) => [p, labels[i]]).filter(([p]) => Number.isFinite(p));
  if (!items.length) return null;
  let total = 0;
  for (let b = 0; b < bins; b++) {
    const lo = b / bins;
    const hi = (b + 1) / bins;
    const inBin = items.filter(([p]) => (p > lo || (b === 0 && p >= lo)) && p <= hi);
    if (!inBin.length) continue;
    const conf = mean(inBin.map(([p]) => p));
    const acc = inBin.filter(([, y]) => y).length / inBin.length;
    total += (inBin.length / items.length) * Math.abs(conf - acc);
  }
  return total;
}

const EPS = 1e-6;
const logit = (p) => Math.log(Math.min(1 - EPS, Math.max(EPS, p)) / (1 - Math.min(1 - EPS, Math.max(EPS, p))));
const sigmoid = (z) => 1 / (1 + Math.exp(-z));

/** p' = σ(logit(p)/T) — температурное масштабирование бинарной вероятности. */
export function applyTemperature(p, t) {
  return sigmoid(logit(p) / t);
}

/** Температура по сетке, минимизирующая NLL на (probs, labels). */
export function fitTemperature(probs, labels) {
  let best = { t: 1, nll: Infinity };
  for (let t = 0.25; t <= 8.0001; t *= 1.1) {
    let nll = 0;
    probs.forEach((p, i) => {
      const q = Math.min(1 - EPS, Math.max(EPS, applyTemperature(p, t)));
      nll -= labels[i] ? Math.log(q) : Math.log(1 - q);
    });
    if (nll < best.nll) best = { t, nll };
  }
  return best.t;
}

/**
 * ECE после температуры, подобранной leave-one-out: каждый элемент калибруется температурой,
 * подобранной на остальных. Разведка, не порог.
 */
export function looTemperatureEce(probs, labels) {
  if (probs.length < 4) return null;
  const calibrated = probs.map((p, i) => {
    const t = fitTemperature(probs.filter((_, j) => j !== i), labels.filter((_, j) => j !== i));
    return applyTemperature(p, t);
  });
  return { before: ece(probs, labels), after: ece(calibrated, labels), tAll: fitTemperature(probs, labels) };
}
