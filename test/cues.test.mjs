/**
 * RFC 08: косвенные признаки и наряды.
 * Запуск: node --test test/cues.test.mjs
 *
 * Профиль здесь намеренно НЕ импортируется: lib/cues.mjs и lib/brief.mjs чистые и всё
 * внешнее принимают аргументом. Если когда-нибудь понадобится `getProfile()` внутри —
 * этот файл начнёт падать, и это правильный сигнал.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { collectSpans, collectCues, rankSpans, DEFAULT_WEIGHTS, DEFAULT_FACTORS } from '../lib/cues.mjs';
import { buildBriefs, rolesForHypotheses } from '../lib/brief.mjs';
import { extractFileDiffSections } from '../lib/router.mjs';

/** Хунк: `@@ -старт,длина +старт,длина @@` + строки с префиксами. */
const section = (file, body) => [
  `diff --git a/${file} b/${file}`,
  `--- a/${file}`,
  `+++ b/${file}`,
  body,
].join('\n');

const SRC_GUARD = section('src/api/team_details.py', [
  '@@ -107,6 +107,13 @@ def put(self, request, team):',
  '         """',
  '+',
  '+        if team.idp_provisioned:',
  '+            return Response(',
  '+                {"detail": "managed through your identity provider"},',
  '+                status=403,',
  '+            )',
  '+',
  '         serializer = TeamDetailsSerializer(team, data=request.data)',
].join('\n'));

const TEST_FILE = section('tests/api/test_team_details.py', [
  '@@ -203,6 +203,9 @@ def test_owner_without_team_membership(self):',
  '         assert team.name == "foo"',
  '+    def test_cannot_modify_idp_provisioned_teams(self):',
  '+        org = self.create_organization(owner=self.user)',
  '+        self.get_error_response(org.slug, status_code=403)',
  '     def tail(self):',
].join('\n'));

test('collectSpans: номера строк правой стороны, эффорт — протяжённость хунка', () => {
  const spans = collectSpans(new Map([['a.ts', section('a.ts', [
    '@@ -10,3 +10,5 @@',
    ' const before = 1;',
    '+const added = 2;',
    '+const more = 3;',
    ' const after = 4;',
  ].join('\n'))]]));
  assert.equal(spans.length, 1);
  const [s] = spans;
  assert.equal(s.startLine, 10, 'спан начинается со строки из заголовка хунка');
  assert.equal(s.endLine, 13, 'контекстные строки тоже двигают счётчик правой стороны');
  assert.equal(s.added, 2);
  assert.equal(s.effort, 4, 'эффорт — сколько строк новой версии придётся осмотреть');
  assert.match(s.addedText, /const added/);
});

test('collectSpans: хунк без добавленных строк выброшен — смотреть в новой версии не на что', () => {
  const spans = collectSpans(new Map([['a.ts', section('a.ts', [
    '@@ -10,3 +10,1 @@',
    ' const kept = 1;',
    '-const gone = 2;',
    '-const alsoGone = 3;',
  ].join('\n'))]]));
  assert.equal(spans.length, 0);
});

test('признак не поднимается без улики: у каждого cue непустой evidence', () => {
  const spans = collectCues(collectSpans(new Map([['src/a.py', SRC_GUARD]])), {});
  for (const span of spans) {
    for (const cue of span.cues) {
      assert.ok(cue.evidence.length > 0, `${cue.key} без evidence — это уже не признак, а мнение`);
      assert.ok(cue.kind === 'content' || cue.kind === 'surface', `${cue.key}: неизвестный род признака`);
    }
  }
});

test('деградация без фактов: нет графа, prescan и класса пути — нет признаков, но и нет исключения', () => {
  const spans = collectSpans(new Map([['src/a.py', SRC_GUARD]]));
  const withFacts = collectCues(spans, {
    fileClass: () => 'logic',
    prescan: [{ file: 'src/a.py', line: 110, title: 'eslint: no-x' }],
    blast: [{ file: 'src/a.py', line: 110, caller: 'x', crossCommunity: true }],
    priorPaths: ['src/a.py'],
  });
  const bare = collectCues(spans, {});
  const keys = (list) => list[0].cues.map((c) => c.key);
  assert.ok(keys(withFacts).includes('prescan:hit'));
  assert.ok(keys(withFacts).includes('graph:caller-outside'));
  assert.ok(keys(withFacts).includes('prior:seen'));
  for (const k of ['prescan:hit', 'graph:caller-outside', 'prior:seen']) {
    assert.ok(!keys(bare).includes(k), `${k} не может подняться без факта`);
  }
  assert.ok(bare[0].score >= 0);
});

test('поверхность УМНОЖАЕТ, а не вычитает: тестовый файл не вытесняет исходник (регрессия sentry-78722)', () => {
  // Первый замер RFC 08 провалился именно здесь: тестовые хунки набирали лексикон
  // (`owner`, `def test_…`, `403`) и обгоняли исходник, где лежал настоящий дефект.
  // Слагаемым штрафом это не чинится — сумма лексикона не ограничена сверху.
  const spans = collectCues(collectSpans(new Map([
    ['src/api/team_details.py', SRC_GUARD],
    ['tests/api/test_team_details.py', TEST_FILE],
  ])), { fileClass: () => 'logic' });
  const ranked = rankSpans(spans);
  assert.match(ranked[0].file, /^src\//, 'исходник обязан стоять выше теста');

  const src = spans.find((s) => s.file.startsWith('src/'));
  const tst = spans.find((s) => s.file.startsWith('tests/'));
  assert.equal(tst.factor, DEFAULT_FACTORS['path:test'], 'множитель теста применён');
  assert.equal(src.factor, 1, 'у исходника множителей нет');
  assert.ok(src.cues.some((c) => c.key === 'shape:guard-insert'),
    'ранний выход, вставленный в существующий поток управления, — структурный признак');
});

test('shape:guard-insert не поднимается на новом коде: там нет «существующего потока»', () => {
  const wholeNew = section('src/new.py', [
    '@@ -0,0 +1,4 @@',
    '+def handler(request):',
    '+    if request.blocked:',
    '+        return Response(status=403)',
    '+    return Response(status=200)',
  ].join('\n'));
  const [span] = collectCues(collectSpans(new Map([['src/new.py', wholeNew]])), {});
  assert.ok(!span.cues.some((c) => c.key === 'shape:guard-insert'),
    'все строки добавлены — охранять нечего, это просто новая функция');
});

test('ранжирование детерминировано, тай-брейк — порядок диффа', () => {
  const spans = collectCues(collectSpans(new Map([
    ['src/a.py', SRC_GUARD],
    ['tests/api/test_team_details.py', TEST_FILE],
  ])), {});
  const once = rankSpans(spans).map((s) => s.id ?? `${s.file}:${s.startLine}`);
  const twice = rankSpans(spans).map((s) => s.id ?? `${s.file}:${s.startLine}`);
  assert.deepEqual(once, twice, 'два вызова — один порядок, иначе замер невоспроизводим');

  const flat = [
    { file: 'a.ts', startLine: 1, endLine: 2, effort: 2, score: 0, index: 5, cues: [] },
    { file: 'b.ts', startLine: 1, endLine: 2, effort: 2, score: 0, index: 2, cues: [] },
  ];
  assert.equal(rankSpans(flat)[0].file, 'b.ts', 'при равных очках выигрывает порядок диффа');
});

test('профиль может погасить признак весом 0', () => {
  const facts = { weights: { ...DEFAULT_WEIGHTS, 'shape:guard-insert': 0 } };
  const [span] = collectCues(collectSpans(new Map([['src/a.py', SRC_GUARD]])), facts);
  assert.ok(!span.cues.some((c) => c.key === 'shape:guard-insert'));
});

test('наряды: бюджет перераспределяется, но не растёт, и никто не остаётся без окна', () => {
  const spans = collectCues(collectSpans(new Map([
    ['src/api/team_details.py', SRC_GUARD],
    ['tests/api/test_team_details.py', TEST_FILE],
  ])), { fileClass: () => 'logic' });
  const ranked = rankSpans(spans);
  const baseMargin = 40;
  const briefs = buildBriefs(ranked, { baseMargin, minMargin: 8, fileClass: () => 'logic' });

  const spent = briefs.reduce((s, b) => s + b.contextMargin, 0);
  assert.ok(spent <= baseMargin * ranked.length,
    `суммарное окно ${spent} не может превысить сегодняшнюю трату ${baseMargin * ranked.length}`);
  for (const b of briefs) {
    assert.ok(b.contextMargin >= 8, `${b.id}: ни один спан не остаётся без окна — иначе тихая потеря recall`);
    assert.ok(b.contextMargin <= baseMargin * 2, `${b.id}: потолок окна`);
  }
  assert.ok(briefs[0].contextMargin > briefs[briefs.length - 1].contextMargin,
    'верх ранжирования получает больше контекста, чем низ — в этом весь смысл');
});

test('наряды: добавленный файл окна не получает — его содержимое и есть дифф (решение 1 RFC 04)', () => {
  const spans = collectCues(collectSpans(new Map([['src/new.py', section('src/new.py', [
    '@@ -0,0 +1,2 @@',
    '+def a():',
    '+    return 1',
  ].join('\n'))]])), { addedPaths: ['src/new.py'] });
  const [brief] = buildBriefs(rankSpans(spans), { baseMargin: 40 });
  assert.equal(brief.contextMargin, 0);
});

test('роли выводятся из гипотез таблицей, а не эвристикой', () => {
  assert.deepEqual(rolesForHypotheses(['injection', 'off-by-one']), ['security', 'correctness']);
  assert.deepEqual(rolesForHypotheses(['api-contract']), ['architecture']);
  assert.deepEqual(rolesForHypotheses([]), [], 'нет гипотезы — нет адресной роли, а не «все роли»');
  assert.deepEqual(rolesForHypotheses(['неизвестная']), []);
});

test('наряд не является находкой: ни severity, ни confidence в нём нет', () => {
  const spans = collectCues(collectSpans(new Map([['src/a.py', SRC_GUARD]])), {});
  const [brief] = buildBriefs(rankSpans(spans), {});
  for (const forbidden of ['severity', 'confidence', 'title', 'body']) {
    assert.ok(!(forbidden in brief),
      `поле ${forbidden} превратило бы наряд в улику — гейт обязан их различать`);
  }
});

test('разбор реального diff-текста через extractFileDiffSections', () => {
  const diff = `${SRC_GUARD}\n${TEST_FILE}`;
  const spans = collectSpans(extractFileDiffSections(diff));
  assert.equal(spans.length, 2);
  assert.deepEqual(spans.map((s) => s.file).sort(),
    ['src/api/team_details.py', 'tests/api/test_team_details.py']);
});
