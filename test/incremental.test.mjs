/**
 * RFC 05 и сопутствующие зазоры инкрементального прогона.
 * Запуск: node --test test/incremental.test.mjs
 */
import './helpers/reference-profile.mjs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { writeFile, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { fileInputHashes, configFingerprint, readIncrementalState, INCREMENTAL_STATE_VERSION } from '../lib/incremental.mjs';
import { loadReviewConfig } from '../lib/config.mjs';
import { mergedTextToReferences } from '../lib/specialist-merge.mjs';
import { createReviewEngine } from '../lib/engine.mjs';
import { applyRefutation } from '../lib/specialists.mjs';
import { resolveBlockers } from '../lib/findings.mjs';

const INCREMENTAL_SRC = fileURLToPath(new URL('../lib/incremental.mjs', import.meta.url));

function fingerprint(extraEnv = {}) {
  const cfg = loadReviewConfig({
    OLLAMA_API_KEY: 'k',
    GITHUB_TOKEN: 't',
    REVIEW_OBSERVABILITY: '0',
    REVIEW_EXPORT: '0',
    REVIEW_AUTO_TRIAGE: '0',
    ...extraEnv,
  }, { dryRun: true, repo: 'o/r', prNumber: 1 });
  return configFingerprint({ ...cfg, version: cfg.profile?.version });
}

test('incremental.mjs без NUL — git/GitHub показывают дифф, а не бинарный ADDED', () => {
  const buf = readFileSync(INCREMENTAL_SRC);
  assert.equal(buf.indexOf(0), -1, 'NUL в исходнике прячет файл в GitHub-диффе');
});

test('readIncrementalState: состояние v1 отбрасывается — полный прогон после смены формулы хэша', async () => {
  assert.equal(INCREMENTAL_STATE_VERSION, 2);
  const dir = await mkdtemp(join(tmpdir(), 'incr-v1-'));
  const file = join(dir, 'state.json');
  try {
    await writeFile(file, JSON.stringify({ version: 1, hashes: { 'a.ts': 'x' }, findings: [{ file: 'a.ts' }] }));
    assert.equal(await readIncrementalState(file), null);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('fileInputHashes: дифф и контекст — разные слагаемые, не конкатенация', () => {
  const path = 'a.ts';
  const a = fileInputHashes([path], new Map([[path, 'DIFF']]), new Map([[path, 'CTX']]));
  const glued = fileInputHashes([path], new Map([[path, 'DIFFCTX']]), new Map([[path, '']]));
  const swapped = fileInputHashes([path], new Map([[path, 'CTX']]), new Map([[path, 'DIFF']]));
  assert.notEqual(a[path], glued[path], 'иначе правка контекста, склеившаяся с диффом, осталась бы незамеченной');
  assert.notEqual(a[path], swapped[path], 'перестановка дифф↔контекст обязана дать другой отпечаток');
});

test('configFingerprint: смена допуска verifier / порога маршрута / таймаута сбрасывает инкрементальность', () => {
  const base = fingerprint();
  assert.notEqual(fingerprint({ REVIEW_VERIFY_ADVISORY_MIN: '99' }), base,
    'иначе прошлые находки поедут со старым скором допуска');
  assert.notEqual(fingerprint({ REVIEW_ACTIONABILITY_MIN: '9' }), base,
    'иначе маршрут в строку считается по чужим порогам');
  assert.notEqual(fingerprint({ REVIEW_LLM_TIMEOUT_MS: '60000' }), base,
    'таймаут меняет, доезжает ли ответ роли');
});

test('configFingerprint: в MoA (дефолт) OLLAMA_MODELS тоже не сбрасывает инкрементальность', () => {
  // models.list в MoA — это метка жюри, не OLLAMA_MODELS. Существующий pipeline-тест
  // «смена конфигурации» меняет OLLAMA_MODELS при REVIEW_MODE=consensus и потому не ловит боевой путь.
  assert.equal(fingerprint(), fingerprint({ OLLAMA_MODELS: 'other-a,other-b' }));
});

test('configFingerprint: смена агрегатора — полный прогон (контроль, что отпечаток живой)', () => {
  assert.notEqual(fingerprint(), fingerprint({ MOA_AGGREGATOR: 'other-agg' }));
});

test('chunk-routing: critical lean-чанка без advisorClaims после strict не блокирует merge', async () => {
  const cfg = {
    reviewMode: 'moa',
    models: { list: [], fallback: [] },
    moa: { aggregator: 'agg', label: 'moa', references: ['r'], refMaxTokens: 100, refTemp: 0.6, aggTemp: 0.4 },
    limits: { concurrency: 2 },
    specialists: {
      enabled: true,
      assignments: [
        { role: 'security', model: 'sec-m' },
        { role: 'correctness', model: 'corr-m' },
        { role: 'architecture', model: 'arch-m' },
      ],
    },
    ollama: { temperature: 0.4 },
    gate: {},
  };
  const prompt = {
    buildMessages: () => [{ role: 'user', content: 'base' }],
    buildSpecialistMessages: (role, base) => [...base, { role: 'system', content: role }],
    buildSpecialistAggregatorMessages: (base, refs) => [
      ...base, { role: 'user', content: `jury refs=${refs.map((r) => r.role).join(',')}` },
    ],
    buildReferenceMessages: (base) => base,
    buildAggregatorMessages: (base) => base,
  };
  const engine = createReviewEngine(cfg, {
    ollama: {
      chatText: async () => { throw new Error('специалисты lean-чанка не вызываются'); },
      chatJson: async () => ({
        summary: 's',
        findings: [{
          file: 'docs/architecture.md', line: 1, severity: 'critical',
          title: 'секрет в плане', body: 'токен', confidence: 0.95,
        }],
      }),
    },
    prompt,
    log: () => {},
    observability: null,
  });
  const [res] = await engine.reviewAllModels(
    [{ annotated: 'd', fileContents: '', paths: ['docs/architecture.md'], partialPaths: [] }],
    { pr: {}, chunkRouting: { fileClass: () => 'docs', sensitive: null } },
  );
  assert.equal(res.findings.length, 1);
  assert.equal(res.findings[0].advisorClaims, undefined, 'агрегатор без жюри claims не сочиняет');
  const { findings: after } = applyRefutation(res.findings, () => {}, {
    strict: true, blockOn: ['critical', 'major'],
  });
  assert.equal(after[0].severity, 'minor');
  const { wouldBlock } = resolveBlockers(after, ['docs/architecture.md'], {
    blockOn: ['critical', 'major'], blockSingle: ['critical'], consensusMin: 1, confidenceMin: 0.8,
  }, () => {}, { skipFpFilter: true });
  assert.equal(wouldBlock, false);
});

test('specialist merge: сорванный формат — одна роль с полным текстом, не три копии', () => {
  const assignments = [
    { role: 'security', model: 'solo' },
    { role: 'correctness', model: 'solo' },
    { role: 'architecture', model: 'solo' },
  ];
  const refs = mergedTextToReferences('сплошной текст без заголовков секций', assignments);
  assert.equal(refs.length, 1, 'один советник: иначе refutation примет три claims из одного мнения за консенсус');
  assert.equal(refs[0].mergedFallback, true);
  assert.equal(refs[0].role, 'security');
  assert.equal(refs[0].text, 'сплошной текст без заголовков секций');
});
