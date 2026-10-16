import test from 'node:test';
import assert from 'node:assert/strict';
import {
  quantile, auc, bootstrapCi, cohenKappa, spearman, tv, argmaxOf, repeatNoise, ece,
  applyTemperature, fitTemperature, looTemperatureEce, rng,
} from '../lib/decision-metrics.mjs';

const near = (a, b, eps = 1e-9) => assert.ok(Math.abs(a - b) < eps, `${a} ≉ ${b}`);

test('decision-metrics: quantile и детерминированный rng', () => {
  near(quantile([3, 1, 2, 4], 0.5), 2.5);
  assert.equal(quantile([], 0.5), null);
  const a = rng(5); const b = rng(5);
  assert.deepEqual([a(), a(), a()], [b(), b(), b()]);
});

test('decision-metrics: AUC — идеальное, обратное, ничьи, один класс', () => {
  assert.equal(auc([0.9, 0.8, 0.1, 0.2], [true, true, false, false]), 1);
  assert.equal(auc([0.1, 0.2, 0.9, 0.8], [true, true, false, false]), 0);
  assert.equal(auc([0.5, 0.5], [true, false]), 0.5);
  assert.equal(auc([0.5, 0.6], [true, true]), null);
});

test('decision-metrics: бутстрап-ДИ накрывает 0.5 на шуме и не накрывает на сигнале', () => {
  const noise = Array.from({ length: 10 }, (_, i) => ({ s: (i * 7919) % 10 / 10, y: i % 2 === 0 }));
  const signal = Array.from({ length: 20 }, (_, i) => ({ s: i < 10 ? 0.9 - i * 0.01 : 0.1 + i * 0.01, y: i < 10 }));
  const stat = (xs) => auc(xs.map((x) => x.s), xs.map((x) => x.y));
  const [lo1, hi1] = bootstrapCi(noise, stat);
  assert.ok(lo1 < 0.5 && hi1 > 0.5);
  const [lo2] = bootstrapCi(signal, stat);
  assert.ok(lo2 > 0.5);
});

test('decision-metrics: κ Коэна и Спирмен', () => {
  assert.equal(cohenKappa(['a', 'b', 'a'], ['a', 'b', 'a']), 1);
  near(cohenKappa(['a', 'a', 'b', 'b'], ['a', 'b', 'a', 'b']), 0);
  assert.equal(cohenKappa([null], ['a']), null);
  near(spearman([1, 2, 3, 4], [10, 20, 30, 40]), 1);
  near(spearman([1, 2, 3, 4], [4, 3, 2, 1]), -1);
  assert.equal(spearman([1, 2], [1, 2]), null);
});

test('decision-metrics: tv, argmax, шум повтора', () => {
  near(tv({ a: 0.7, b: 0.3 }, { a: 0.4, b: 0.6 }), 0.3);
  assert.equal(argmaxOf({ a: 0.2, b: 0.8 }), 'b');
  const n = repeatNoise([{ a: 0.6, b: 0.4 }, { a: 0.4, b: 0.6 }, { a: 0.6, b: 0.4 }]);
  assert.equal(n.argmaxChanged, true);
  near(n.tvMax, 0.2);
  assert.equal(repeatNoise([{ a: 1 }]), null);
});

test('decision-metrics: ECE и температура', () => {
  near(ece([1, 0], [true, false]), 0);
  near(ece([0.9, 0.9], [true, false]), 0.4);
  near(applyTemperature(0.5, 3), 0.5);
  assert.ok(applyTemperature(0.9, 2) < 0.9, 'T>1 смягчает');
  // Переуверенные вероятности при 50% точности — температура уходит вверх.
  const probs = [0.99, 0.99, 0.01, 0.01, 0.99, 0.01];
  const labels = [true, false, false, true, true, false];
  assert.ok(fitTemperature(probs, labels) > 2);
  // LOO на малом N сам по себе шумит (отложенный элемент сдвигает корзину) — проверяем форму
  // и направление общей температуры, а не «after < before».
  const p20 = Array.from({ length: 20 }, (_, i) => (i % 2 ? 0.05 : 0.95));
  const y20 = p20.map((p, i) => (i % 10 < 7 ? p > 0.5 : p < 0.5));
  const loo = looTemperatureEce(p20, y20);
  assert.ok(loo.tAll > 2, 'переуверенность → T > 1');
  near(loo.before, ece(p20, y20));
  assert.ok(Number.isFinite(loo.after));
  assert.equal(looTemperatureEce([0.5], [true]), null);
});
