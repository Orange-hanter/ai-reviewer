/**
 * Walkthrough отчёта — описание изменений от жюри, а не вердикт (0.6.3). Раньше брали summary
 * одного чанка: оно говорило «блокирующих дефектов нет» о своей части, а блок приходил из
 * другой — sticky противоречил сам себе.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { pickAggregateSummary } from '../lib/engine.mjs';

const part = (summary, { hasCode = true, model = 'jury', error } = {}) => ({ model, hasCode, res: error ? { error } : { summary, findings: [] } });

test('walkthrough: summary нескольких кодовых чанков по порядку, без повторов, не больше трёх', () => {
  const done = [
    part('Добавлена страница Swagger UI.'),
    part('Документация API (markdown).', { hasCode: false }),
    part('Генератор пишет served-спеку.'),
    part('Добавлена страница Swagger UI.'),
    part('Pre-commit хук добавляет спеку в индекс.'),
    part('Четвёртый кодовый чанк.'),
    part(''),
    part('чужая модель', { model: 'other' }),
    part(null, { error: 'timeout' }),
  ];
  assert.equal(
    pickAggregateSummary('jury', done),
    'Добавлена страница Swagger UI. Генератор пишет served-спеку. Pre-commit хук добавляет спеку в индекс.',
  );
});

test('walkthrough: без кодовых чанков — summary остальных, без summary — пусто', () => {
  assert.equal(pickAggregateSummary('jury', [part('Только доки.', { hasCode: false })]), 'Только доки.');
  assert.equal(pickAggregateSummary('jury', [part('')]), '');
  assert.equal(pickAggregateSummary('jury', []), '');
});
