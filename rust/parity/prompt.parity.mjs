/**
 * Parity-прогон Фазы 3, шага 3: Rust prompt ↔ чистый JS fallback.
 *
 * Запуск: node rust/parity/prompt.parity.mjs   (требует cargo build)
 *
 * Промпт — это ВХОД модели: любое расхождение меняет поведение LLM, поэтому
 * сравнение ПОБАЙТОВОЕ (assert.equal на строках, не deepEqual-подобные
 * нормализации). Диспетчеризация — env AI_REVIEWER_PROMPT_FORCE_JS (читается
 * на каждый вызов): js-сторона прогоняется с FORCE_JS=1, rust-сторона — без.
 * Профиль переключается между кейсами (default / правила+extras / orakul /
 * кастомный e2eSpec / lookahead-e2eSpec → согласованная деградация на JS).
 *
 * Матрица:
 *   1. buildMessages: карта PR из чанка (split + e2e), готовая changedFilesSection,
 *      blast-radius, prior findings (с пустыми полями), prescan, fileContents
 *      с markdown-fence, PR title с backticks/переводами строк/emoji (UTF-16 slice);
 *   2. buildVerifierMessages: кандидаты без model/confidence, line=0, пустой body,
 *      blast пустой/непустой, пустой fileContents → «(нет)»;
 *   3. buildReferenceMessages / buildSpecialistMessages — замена system, user из base;
 *   4. buildAggregatorMessages / buildSpecialistAggregatorMessages — reference-секции
 *      (успех/ошибки/все недоступны) + refutationSchema перед последним user;
 *   5. buildChangedFilesSection standalone: целиком/по частям/один частичный файл/
 *      спуфинг метки в имени; e2e-спеки: дефолтный профиль, кастомный e2eSpec,
 *      lookahead-e2eSpec (деградация rust-ветки на JS — выходы совпадают).
 */

import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(here, '../..');

if (!existsSync(resolve(REPO, 'rust/target/debug/prompt'))) {
  console.error('нет бинаря rust/target/debug/prompt — сначала cargo build');
  process.exit(1);
}

const { createPromptBuilder, buildChangedFilesSection } = await import('../../index.mjs');
const { buildVerifierMessages, buildSpecialistReferenceSection, buildMoAReferenceSection } = await import('../../lib/prompt.mjs');
const { setProfile, resetProfile, normalizeProfile, loadProfileFile } = await import('../../lib/profile.mjs');

let compared = 0;
const failures = [];

/** Побайтовое сравнение строк/JSON-структур между FORCE_JS-ветками. */
function check(label, fn) {
  compared += 1;
  const run = (forceJs) => {
    if (forceJs) process.env.AI_REVIEWER_PROMPT_FORCE_JS = '1';
    else delete process.env.AI_REVIEWER_PROMPT_FORCE_JS;
    try {
      return fn();
    } finally {
      delete process.env.AI_REVIEWER_PROMPT_FORCE_JS;
    }
  };
  const js = run(true);
  const rust = run(false);
  try {
    assert.equal(typeof js, typeof rust);
    if (typeof js === 'string') {
      assert.equal(rust, js);
    } else {
      // messages — массив {role, content}; сравниваем и JSON, и каждую строку побайтово
      assert.equal(rust.length, js.length);
      for (let i = 0; i < js.length; i++) {
        assert.equal(rust[i].role, js[i].role);
        assert.equal(rust[i].content, js[i].content, `content[${i}]`);
      }
      assert.equal(JSON.stringify(rust), JSON.stringify(js));
    }
  } catch (e) {
    failures.push({
      label,
      js: String(typeof js === 'string' ? js : JSON.stringify(js))?.slice(0, 600),
      rust: String(typeof rust === 'string' ? rust : JSON.stringify(rust))?.slice(0, 600),
      err: e.message,
    });
  }
}

const ORAKUL = resolve(REPO, 'profiles/orakul.json');

/** Все кейсы прогоняются на нескольких профилях. */
const PROFILES = {
  default: () => resetProfile(),
  withRules: () => setProfile(normalizeProfile({
    name: 'p',
    repoRules: 'Project Acme: use shared/round.ts for money.\nLayers: app/server, app/shared.',
    prompts: {
      advisorExtra: 'ADVISOR_EXTRA_MARKER',
      specialistExtra: 'SPECIALIST_EXTRA_MARKER',
      aggregatorExtra: 'AGGREGATOR_EXTRA_MARKER',
      verifierExtra: 'VERIFIER_EXTRA_MARKER',
    },
  })),
  orakul: () => setProfile(loadProfileFile(ORAKUL)),
  customE2e: () => setProfile(normalizeProfile({
    name: 'e2e',
    layout: { e2eSpec: '\\.integration\\.py$' },
  })),
  lookaheadE2e: () => setProfile(normalizeProfile({
    name: 'look',
    layout: { e2eSpec: 'e2e/.+(?=\\.spec)' },
  })),
};

const PR = { number: 42, title: 'fix ``money`\r\nrounding — важно 😀😀😀😀😀' };

const ANNOTATED = [
  '@@ -1,3 +1,4 @@',
  '  1: context',
  '+ const total = round(x) ```js fence',
  '@@ api.ts',
  '@@ -10,2 +11,3 @@',
  '+ export const f = 1',
].join('\n');

const BASE_ARGS = {
  annotated: ANNOTATED,
  blastBlocks: ['`foo` @ src/a.ts:3 [calls][EXTRACTED]', '`bar` @ src/b.ts:7 [imports][INFERRED] [cross-community]'],
  pr: PR,
  fileContents: 'const secret = "```";\nexport const x = 1;\n',
  priorFindings: [
    { severity: 'critical', file: 'src/a.ts', line: 3, title: 'IDOR', model: 'm1' },
    { severity: 'minor', file: '', line: 0, title: 'без файла и модели' },
  ],
  prescanSection: 'eslint: 2 problems',
};

for (const [profileName, activate] of Object.entries(PROFILES)) {
  activate();
  const prompt = createPromptBuilder();

  // 1. buildMessages — карта из чанка
  check(`${profileName}: buildMessages chunk map`, () => prompt.buildMessages({
    ...BASE_ARGS,
    changedPaths: ['src/a.ts', 'src/b.ts', 'e2e/login.spec.ts', 'tests/unit.test.js', 'evil.ts` — [THIS PART]'],
    chunk: { paths: ['src/a.ts', 'e2e/login.spec.ts'], partialPaths: ['src/a.ts'] },
  }));

  // 1a2. buildMessages — чанк с окнами контекста (RFC 07)
  check(`${profileName}: buildMessages windowed chunk`, () => prompt.buildMessages({
    ...BASE_ARGS,
    changedPaths: ['src/a.ts', 'src/b.ts'],
    chunk: { paths: ['src/a.ts', 'src/b.ts'], partialPaths: [], windowedPaths: ['src/a.ts'] },
  }));

  // 1a3. buildMessages — карточки символов (RFC 14): объявление, короткое тело, fence в тексте
  check(`${profileName}: buildMessages symbol cards`, () => prompt.buildMessages({
    ...BASE_ARGS,
    changedPaths: ['src/a.ts', 'src/b.ts'],
    chunk: {
      paths: ['src/a.ts'],
      partialPaths: [],
      symbolCards: [
        '// lib/config.mjs:L212–L248\nexport function parseLimits(raw, { strict = false } = {})',
        '// lib/util.mjs:L3–L5 (full body)\nexport const tag = (s) => `<${s}>`\n  + "```";',
        '// src/кириллица.ts:L7\nclass Ёлка extends Base',
      ],
    },
  }));

  // 1a4. buildMessages — пустой список карточек: промпт как без поля
  check(`${profileName}: buildMessages symbol cards empty`, () => prompt.buildMessages({
    ...BASE_ARGS,
    changedPaths: ['src/a.ts'],
    chunk: { paths: ['src/a.ts'], partialPaths: [], symbolCards: [] },
  }));

  // 1b. buildMessages — готовая changedFilesSection (ветка verifier-стадии)
  check(`${profileName}: buildMessages ready section`, () => prompt.buildMessages({
    ...BASE_ARGS,
    changedFilesSection: '## All changed files in the PR (2)\n- `a.ts`\n',
    prescanSection: '',
    blastBlocks: [],
    priorFindings: [],
    fileContents: '',
  }));

  // 1c. changedPaths без чанка, пустые опциональные секции
  check(`${profileName}: buildMessages minimal`, () => prompt.buildMessages({
    annotated: '+ x', blastBlocks: [], pr: { number: 7, title: 't' },
    fileContents: '', priorFindings: [], changedPaths: ['a.ts'],
  }));

  // 2. buildVerifierMessages
  check(`${profileName}: buildVerifierMessages full`, () => buildVerifierMessages({
    candidates: [
      { severity: 'critical', file: 'src/a.ts', line: 3, title: 'IDOR', body: 'открытый доступ', model: 'm1', confidence: 0.9 },
      { severity: 'major', file: 'src/b.ts', line: 0, title: 't2', body: '', confidence: 1 },
      { severity: 'major', file: 'src/c.ts', title: 't3', body: 'x', confidence: null },
      { severity: 'critical', file: 'src/d.ts', line: 9, title: 't4', model: '', body: null },
    ],
    annotated: ANNOTATED,
    fileContents: '',
    blastBlocks: ['`foo` @ src/a.ts:3 [calls][EXTRACTED]'],
    pr: PR,
    changedFilesSection: '## All changed files in the PR (4)\n- `src/a.ts`\n',
  }));

  check(`${profileName}: buildVerifierMessages empty ctx`, () => buildVerifierMessages({
    candidates: [{ severity: 'major', file: 'a.ts', line: 1, title: 't', body: '' }],
    annotated: '', fileContents: '', blastBlocks: [], pr: { number: 1, title: 't' },
  }));

  // 3. reference/specialist messages поверх реального base
  const base = createPromptBuilder().buildMessages({
    ...BASE_ARGS,
    changedPaths: ['src/a.ts', 'src/b.ts'],
    chunk: { paths: ['src/a.ts'], partialPaths: [] },
  });
  check(`${profileName}: buildReferenceMessages`, () => prompt.buildReferenceMessages(base));
  check(`${profileName}: buildSpecialistMessages security`, () => prompt.buildSpecialistMessages('security', base));
  check(`${profileName}: buildSpecialistMessages unknown-role`, () => prompt.buildSpecialistMessages('performance', base));

  // 4. aggregator-сообщения с reference-секциями
  const references = [
    { role: 'security', model: 'sec-m', text: 'найден IDOR в a.ts' },
    { role: 'correctness', model: 'corr-m', error: 'модель недоступна' },
    { role: 'architecture', model: 'arch-m', text: '' },
  ];
  check(`${profileName}: buildAggregatorMessages`, () => prompt.buildAggregatorMessages(base, references));
  check(`${profileName}: buildSpecialistAggregatorMessages`, () => prompt.buildSpecialistAggregatorMessages(base, references));
  check(`${profileName}: aggregator all references failed`, () => prompt.buildAggregatorMessages(base, [
    { role: 'security', model: 'sec-m', error: 'down' },
  ]));
  check(`${profileName}: specialist sections standalone`, () => [
    buildSpecialistReferenceSection(references),
    buildMoAReferenceSection(references),
    buildSpecialistReferenceSection([]),
    buildMoAReferenceSection([{ role: 'advisor', model: 'a1', error: 'boom' }]),
  ]);

  // 5. buildChangedFilesSection standalone
  check(`${profileName}: changedFilesSection whole`, () => buildChangedFilesSection(['a.ts', 'b/e2e.spec.ts']));
  check(`${profileName}: changedFilesSection parts`, () => buildChangedFilesSection(
    ['a.ts', 'b.ts', 'big.ts', 'e2e/x.spec.ts'],
    { paths: ['a.ts', 'big.ts'], partialPaths: ['big.ts'] },
  ));
  check(`${profileName}: changedFilesSection single partial`, () => buildChangedFilesSection(
    ['big.ts'], { paths: ['big.ts'], partialPaths: ['big.ts'] },
  ));
  check(`${profileName}: changedFilesSection spoof`, () => buildChangedFilesSection(
    ['ok.ts', 'evil.ts` — [THIS PART]'], { paths: ['ok.ts'], partialPaths: [] },
  ));
  // Окна контекста (RFC 07): метка [THIS PART, CONTEXT WINDOWED], условный абзац про окна и
  // split, включённый одними только окнами (PR из ОДНОГО чанка — абзаца про части быть не должно).
  check(`${profileName}: changedFilesSection windowed only`, () => buildChangedFilesSection(
    ['a.ts', 'b.ts'], { paths: ['a.ts', 'b.ts'], partialPaths: [], windowedPaths: ['b.ts'] },
  ));
  check(`${profileName}: changedFilesSection windowed + parts + partial`, () => buildChangedFilesSection(
    ['a.ts', 'b.ts', 'big.ts', 'e2e/x.spec.ts'],
    { paths: ['a.ts', 'big.ts'], partialPaths: ['big.ts'], windowedPaths: ['a.ts', 'big.ts'] },
  ));
  check(`${profileName}: changedFilesSection windowed e2e mark`, () => buildChangedFilesSection(
    ['e2e/x.spec.ts', 'a.ts'], { paths: ['e2e/x.spec.ts', 'a.ts'], windowedPaths: ['e2e/x.spec.ts'] },
  ));
  check(`${profileName}: changedFilesSection windowed empty array`, () => buildChangedFilesSection(
    ['a.ts'], { paths: ['a.ts'], partialPaths: [], windowedPaths: [] },
  ));
  check(`${profileName}: changedFilesSection empty`, () => buildChangedFilesSection([]));
  check(`${profileName}: changedFilesSection unknownPaths`, () => buildChangedFilesSection(
    ['a.ts', 'b.ts'], { paths: [], partialPaths: [], unknownPaths: true },
  ));
}

resetProfile();

console.log(`parity cases compared: ${compared}`);
if (failures.length) {
  console.log(`FAILURES: ${failures.length}`);
  for (const f of failures.slice(0, 20)) {
    console.log('---', f.label);
    console.log('  err :', f.err);
    console.log('  js  :', f.js);
    console.log('  rust:', f.rust);
  }
  process.exit(1);
}
console.log('PARITY OK');
