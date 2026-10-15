/**
 * Исчерпанная квота провайдера LLM (402 / квотный 429): отдельная метка в sticky и совет,
 * который не обещает, что перезапуск поможет. Маркер в тексте ошибки ставит Rust chat()
 * (rust/src/llm.rs, is_quota_exhausted); здесь — его чтение отчётом.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { publicErrorLabel } from '../lib/util.mjs';
import { buildLlmUnavailableSummary } from '../lib/report.mjs';
import { defaultRustBin } from '../lib/rust-bin.mjs';

const QUOTA_ERR = 'LLM 429 (квота провайдера исчерпана): {"error":{"message":"weekly usage limit"}}';

test('publicErrorLabel: квота раньше кода HTTP, rate limit остаётся rate limit', () => {
  assert.equal(publicErrorLabel(QUOTA_ERR), 'квота провайдера исчерпана');
  assert.equal(publicErrorLabel('LLM 402 (квота провайдера исчерпана): Insufficient credits'), 'квота провайдера исчерпана');
  assert.equal(publicErrorLabel('LLM 429: rate limited'), 'HTTP 429 (rate limit)');
});

test('sticky при исчерпанной квоте не зовёт перезапустить', {
  skip: existsSync(defaultRustBin('report')) && process.env.AI_REVIEWER_REPORT_FORCE_JS !== '1'
    ? false : 'совет различает только Rust-отчёт (RFC 20); deprecated JS-копия его не знает',
}, () => {
  const md = buildLlmUnavailableSummary({
    llmRequired: true,
    whyPublic: `m1: ${publicErrorLabel(QUOTA_ERR)}`,
    models: ['m1'],
  });
  assert.match(md, /Квота провайдера LLM исчерпана — перезапуск не поможет/);
  assert.doesNotMatch(md, /Перезапустите job позже/);
  const rate = buildLlmUnavailableSummary({ llmRequired: true, whyPublic: 'm1: HTTP 429 (rate limit)', models: ['m1'] });
  assert.match(rate, /Перезапустите job позже/);
});
