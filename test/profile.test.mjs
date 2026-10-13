// Профиль проекта — то, чем движок адаптируется к репозиторию. Здесь проверяется ИМЕННО
// нейтральный (дефолтный) путь: репозиторий без профиля и загрузка чужого профиля из JSON.
// Регрессионный набор движка гоняется под reference-профилем (test/helpers/reference-profile.mjs),
// поэтому этот файл намеренно НЕ импортирует его: профиль process-wide, один файл — один процесс.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  DEFAULT_PROFILE, createMarkers, normalizeProfile, loadProfileFile,
  getProfile, setProfile, resetProfile, resolveProfile,
} from '../lib/profile.mjs';
import { MARKERS, repoRules } from '../lib/constants.mjs';
import { loadReviewConfig } from '../lib/config.mjs';
import { normalizeAppPath, isLintablePath } from '../lib/prescan-shared.mjs';
import { filterAstgrepFindings } from '../lib/astgrep.mjs';
import { buildSarif, toolName } from '../lib/export.mjs';
import { createPromptBuilder } from '../lib/prompt.mjs';

const ORAKUL_PROFILE = fileURLToPath(new URL('../profiles/orakul.json', import.meta.url));

test('дефолтный профиль: нейтральные маркеры и имя инструмента', () => {
  resetProfile();
  assert.equal(getProfile().name, 'default');
  assert.equal(MARKERS.review, '<!-- ai-review -->');
  assert.equal(MARKERS.inline, '<!-- ai-review-inline -->');
  assert.equal(MARKERS.dataKey, 'ai-review-data:');
  assert.equal(toolName(), 'ai-review');
});

test('маркеры читаются из АКТИВНОГО профиля, а не фиксируются при импорте', () => {
  resetProfile();
  assert.equal(MARKERS.review, '<!-- ai-review -->');
  setProfile(loadProfileFile(ORAKUL_PROFILE));
  // Модуль constants.mjs был импортирован ДО смены профиля: если бы MARKERS был снимком,
  // sticky ушёл бы с чужим маркером и потерял бы всю историю прогонов на живых PR.
  assert.equal(MARKERS.review, '<!-- orakul-ai-review -->');
  assert.equal(MARKERS.dataRe.exec('<!-- orakul-ai-review-data:{"a":1} -->')[1].trim(), '{"a":1}');
  resetProfile();
});

test('dataRe: на каждое чтение свой RegExp (общий lastIndex ломал бы разбор)', () => {
  resetProfile();
  const first = MARKERS.dataRe;
  const second = MARKERS.dataRe;
  assert.notEqual(first, second);
  assert.equal(first.source, second.source);
});

test('createMarkers: пустой namespace падает в дефолт, а не даёт «<!--  -->»', () => {
  assert.equal(createMarkers('').review, '<!-- ai-review -->');
  assert.equal(createMarkers('   ').inline, '<!-- ai-review-inline -->');
  assert.equal(createMarkers('acme-review').dataKey, 'acme-review-data:');
});

test('без профиля правил репозитория нет — промпт не выдумывает архитектуру', () => {
  resetProfile();
  assert.equal(repoRules(), '');
  const prompt = createPromptBuilder();
  const [system] = prompt.buildMessages({
    annotated: '+ const a = 1;', blastBlocks: [], pr: { number: 1, title: 't' },
    fileContents: '', priorFindings: [], changedPaths: ['src/a.ts'],
  });
  // Rust-промпт (0.6+) или deprecated JS-копия — без бинаря работает вторая.
  assert.match(system.content, /final reviewer \(jury aggregator\)|senior review engineer/);
  assert.doesNotMatch(system.content, /Orakul/);
});

test('профиль подставляет правила репозитория в системный промпт', () => {
  setProfile(loadProfileFile(ORAKUL_PROFILE));
  const prompt = createPromptBuilder();
  const [system] = prompt.buildMessages({
    annotated: '+ const a = 1;', blastBlocks: [], pr: { number: 1, title: 't' },
    fileContents: '', priorFindings: [], changedPaths: ['app/server/a.ts'],
  });
  assert.match(system.content, /Project Orakul/);
  assert.match(system.content, /shared\/round\.ts/);
  resetProfile();
});

test('раскладка по умолчанию — корень репозитория, без выдуманного app/', () => {
  resetProfile();
  assert.equal(normalizeAppPath('server/a.ts'), 'server/a.ts');
  assert.equal(normalizeAppPath('src/index.ts'), 'src/index.ts');
  assert.equal(isLintablePath('src/index.ts'), true);
  assert.equal(isLintablePath('packages/api/handler.tsx'), true);
  assert.equal(isLintablePath('README.md'), false);
});

test('профиль с appDir возвращает путь из diff в каталог приложения — только для своих слоёв', () => {
  setProfile(loadProfileFile(ORAKUL_PROFILE));
  assert.equal(normalizeAppPath('server/a.ts'), 'app/server/a.ts');
  assert.equal(normalizeAppPath('app/server/a.ts'), 'app/server/a.ts');
  // Каталог, которого нет в layerDirs, не должен уезжать под app/ — иначе eslint пошёл бы
  // линтить несуществующий путь и prescan молча схлопнулся бы в ноль находок.
  assert.equal(normalizeAppPath('docs/a.ts'), 'docs/a.ts');
  assert.equal(isLintablePath('shared/round.ts'), true);
  assert.equal(isLintablePath('docs/a.ts'), false);
  resetProfile();
});

test('scope и allowlist ast-grep берутся из профиля', () => {
  resetProfile();
  const findings = [
    { ruleId: 'raw-x-forwarded-for', file: 'app/server/net.ts', line: 1 },
    { ruleId: 'raw-x-forwarded-for', file: 'app/server/auth.ts', line: 2 },
    { ruleId: 'client-imports-server', file: 'app/shared/x.ts', line: 3 },
  ];
  // Без профиля правило действует ровно там, где его нашёл ast-grep.
  assert.equal(filterAstgrepFindings(findings).length, 3);

  setProfile(loadProfileFile(ORAKUL_PROFILE));
  const kept = filterAstgrepFindings(findings);
  assert.deepEqual(kept.map((f) => f.file), ['app/server/auth.ts']);
  resetProfile();
});

test('SARIF: имя инструмента и ruleId из профиля', () => {
  resetProfile();
  const payload = {
    pr: { number: 7 },
    findings: [{ file: 'src/a.ts', line: 3, severity: 'critical', title: 'boom', consensusBlock: true }],
    blockers: [{ file: 'src/a.ts', line: 3, severity: 'critical', title: 'boom' }],
  };
  const def = buildSarif(payload, { repositoryUri: 'https://example.com/acme/widgets' });
  assert.equal(def.runs[0].tool.driver.name, 'ai-review');
  assert.equal(def.runs[0].results[0].ruleId, 'ai-review/critical');

  setProfile(loadProfileFile(ORAKUL_PROFILE));
  const orakul = buildSarif(payload, { repositoryUri: 'https://example.com/acme/widgets' });
  assert.equal(orakul.runs[0].tool.driver.name, 'orakul-ai-review');
  assert.equal(orakul.runs[0].results[0].ruleId, 'orakul/critical');
  assert.equal(orakul.runs[0].tool.driver.informationUri, 'https://github.com/Orange-hanter/Orakul');
  resetProfile();
});

test('конфиг берёт пути из профиля, env перебивает профиль', () => {
  resetProfile();
  const bare = loadReviewConfig({ REVIEW_PROFILE: ORAKUL_PROFILE }, { dryRun: true, repo: '', prNumber: 0 });
  assert.equal(bare.profile.name, 'orakul');
  assert.equal(bare.feedback.logFile, 'ops/pr-review/feedback/fp-feedback.jsonl');
  assert.equal(bare.export.dir, '.graphify/graphify-out/review-export');
  assert.equal(bare.prescan.appDir, 'app');

  const overridden = loadReviewConfig({
    REVIEW_PROFILE: ORAKUL_PROFILE,
    REVIEW_EXPORT_DIR: 'out/review',
    REVIEW_APP_DIR: 'packages/api',
  }, { dryRun: true, repo: '', prNumber: 0 });
  assert.equal(overridden.export.dir, 'out/review');
  assert.equal(overridden.prescan.appDir, 'packages/api');
  resetProfile();
});

test('дефолтный конфиг не тянет чужие пути проекта', async () => {
  resetProfile();
  // loadReviewConfig ищет профиль автопоиском в cwd, а у самого движка теперь есть свой
  // .ai-reviewer.json в корне — «репозиторий без профиля» приходится собирать явно, пустым
  // каталогом. Проверяется ровно тот же нейтральный путь, что и раньше.
  const cwd = process.cwd();
  const dir = await mkdtemp(join(tmpdir(), 'ai-reviewer-noprofile-'));
  try {
    process.chdir(dir);
    const cfg = loadReviewConfig({}, { dryRun: true, repo: '', prNumber: 0 });
    assert.equal(cfg.profile.name, 'default');
    assert.equal(cfg.feedback.logFile, '.ai-review/feedback/fp-feedback.jsonl');
    assert.equal(cfg.observability.dir, '.ai-review/observability');
    assert.equal(cfg.prescan.appDir, '');
    // patternsFile пуст → движок берёт стартовый набор из пакета (см. lib/fp-patterns.mjs).
    assert.equal(cfg.feedback.patternsFile, '');
  } finally {
    process.chdir(cwd);
    await rm(dir, { recursive: true, force: true });
    resetProfile();
  }
});

test('автопоиск: .ai-reviewer.json в корне рабочего каталога подхватывается сам', () => {
  resetProfile();
  const profile = resolveProfile({}, { cwd: fileURLToPath(new URL('..', import.meta.url)) });
  assert.equal(profile.name, 'ai-reviewer', 'профиль самого движка найден без REVIEW_PROFILE');
  assert.ok(profile.layout.sensitive.test('lib/pipeline-x.mjs'), 'lib/ — чувствительная зона');
  assert.ok(profile.layout.sensitive.test('rust/src/report.rs'), 'rust/src/ — чувствительная зона');
  assert.ok(profile.layout.sensitive.test('pipeline.mjs'));
  assert.ok(!profile.layout.sensitive.test('docs/architecture.md'));
  assert.match(profile.repoRules, /ноль runtime-зависимостей/, 'правила проекта доехали в промпт');
  resetProfile();
});

test('битый профиль не роняет ревью — fail-open в дефолт с записью в лог', async () => {
  resetProfile();
  const dir = await mkdtemp(join(tmpdir(), 'ai-reviewer-profile-'));
  try {
    const broken = join(dir, 'broken.json');
    await writeFile(broken, '{ this is not json', 'utf8');
    const logs = [];
    const profile = resolveProfile({ REVIEW_PROFILE: broken }, { log: (m) => logs.push(m) });
    assert.equal(profile.name, 'default');
    assert.match(logs.join('\n'), /дефолтном профиле/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('нормализация профиля: битый regex не роняет загрузку, allow — множество путей', () => {
  const profile = normalizeProfile({
    name: 'acme',
    markerNamespace: 'acme-review',
    layout: { appDir: 'app/', layerDirs: ['server'], lintable: '([unclosed' },
    astgrep: { scopes: { 'r1': '^app/server/' }, allow: { 'r1': 'app/server/ok.ts' } },
  });
  assert.equal(profile.markers.review, '<!-- acme-review -->');
  assert.equal(profile.toolName, 'acme-review', 'toolName по умолчанию = namespace');
  assert.equal(profile.layout.appDir, 'app', 'хвостовой слэш срезается');
  assert.equal(profile.layout.lintable.source, DEFAULT_PROFILE.layout.lintable);
  assert.ok(profile.astgrep.allow.get('r1').has('app/server/ok.ts'), 'строка допустима вместо массива');
});

test('явно назначенный профиль не перетирается автопоиском', () => {
  const explicit = setProfile(loadProfileFile(ORAKUL_PROFILE));
  const resolved = resolveProfile({});
  assert.equal(resolved, explicit);
  // …но env-указание сильнее явного: прогон вправе выбрать профиль сам.
  const other = resolveProfile({ REVIEW_PROFILE: ORAKUL_PROFILE });
  assert.equal(other.name, 'orakul');
  resetProfile();
});

test('без repoRulesFile подхватывается AGENTS.md рядом с профилем', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'ai-review-agents-'));
  try {
    await writeFile(join(dir, 'AGENTS.md'), '# Agent rules\nPrefer shared/round.ts\n', 'utf8');
    await writeFile(join(dir, 'profile.json'), JSON.stringify({ name: 'with-agents', markerNamespace: 't' }), 'utf8');
    const profile = loadProfileFile(join(dir, 'profile.json'));
    assert.match(profile.repoRules, /Prefer shared\/round\.ts/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('prompts.* дописываются в системные промпты', () => {
  setProfile(normalizeProfile({
    name: 'p',
    prompts: {
      aggregatorExtra: 'CUSTOM_AGG_MARKER',
      verifierExtra: 'CUSTOM_VER_MARKER',
      advisorExtra: 'CUSTOM_ADV_MARKER',
    },
  }));
  const prompt = createPromptBuilder();
  const [system] = prompt.buildMessages({
    annotated: '+ const a = 1;', blastBlocks: [], pr: { number: 1, title: 't' },
    fileContents: '', priorFindings: [], changedPaths: ['src/a.ts'],
  });
  assert.match(system.content, /CUSTOM_AGG_MARKER/);
  const [vSystem] = prompt.buildVerifierMessages({
    candidates: [{ severity: 'major', file: 'a.ts', line: 1, title: 't', body: '' }],
    annotated: '', fileContents: '', blastBlocks: [], pr: { number: 1, title: 't' },
  });
  assert.match(vSystem.content, /CUSTOM_VER_MARKER/);
  const [refSystem] = prompt.buildReferenceMessages([
    { role: 'user', content: 'diff' },
  ]);
  assert.match(refSystem.content, /CUSTOM_ADV_MARKER/);
  resetProfile();
});
