/**
 * Страж паритета JS ↔ Rust для правок, которые иначе теряются молча.
 *
 * Модули `lib/router.mjs` и `lib/structure-lcov.mjs` переведены в тонкие обёртки над
 * Rust-бинарями с JS-копией как fallback. Правка, внесённая только в JS-копию, сливается
 * БЕЗ конфликта (стороны в разных файлах), проходит `npm test` (юнит-тесты идут раньше
 * `cargo build`, поэтому видят именно fallback) — и остаётся нерабочей на любой машине,
 * где бинарь собран. Ровно так возвращается уже исправленный дефект.
 *
 * Тест сравнивает ИСХОДНИКИ, ничего не собирая: пока `rust/` в дереве нет, он пропускается;
 * как только Rust-порт приезжает — начинает требовать, чтобы Rust покрывал не меньше JS.
 *
 * Это заведомо грубая проверка по тексту. Настоящее решение — прогонять
 * `rust/parity/*.parity.mjs` в CI после `cargo build`; страж нужен, пока их там нет
 * (см. docs/backlog.md, «Долг слияния»).
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { classifyFilePath } from '../lib/router.mjs';

const ROOT = fileURLToPath(new URL('..', import.meta.url));
const rustFile = (rel) => `${ROOT}rust/src/${rel}`;

/** Языки, ради которых чинилась классификация: не-JS исходники обязаны быть `logic`. */
const LANGUAGES = ['go', 'py', 'rb', 'java', 'rs', 'kt', 'php', 'cs', 'swift', 'gjs'];

test('router: не-JS исходники классифицируются как logic (JS-сторона)', () => {
  for (const ext of LANGUAGES) {
    assert.equal(classifyFilePath(`src/thing.${ext}`), 'logic', `.${ext}`);
  }
});

test('паритет: Rust-router знает те же языки, что и JS', (t) => {
  const file = rustFile('router.rs');
  if (!existsSync(file)) {
    t.skip('rust/src/router.rs ещё нет в дереве — сверять не с чем');
    return;
  }
  const source = readFileSync(file, 'utf8');
  // Ищем альтернацию расширений в объявлении `code:`-регекспа.
  const decl = /code:\s*regex::Regex::new\(\s*r?"([^"]+)"/.exec(source);
  assert.ok(decl, 'в rust/src/router.rs не найден code-регексп — проверьте, не переименован ли он');
  const missing = LANGUAGES.filter((ext) => !new RegExp(`\\b${ext}\\b`).test(decl[1]));
  assert.deepEqual(missing, [], `Rust-router не знает языков: ${missing.join(', ')}. `
    + 'Правка JS-fallback без переноса в Rust возвращает дефект «ревью не запускается на не-JS репозиториях».');
});

/**
 * Ограничения выхода ролей (RFC 04, решение 3) живут в тексте промпта, а текст продублирован
 * в rust/src/prompt.rs. Расхождение сторон не падает и не конфликтует — оно просто даёт
 * разные промпты на машине с собранным бинарём и без него, то есть разные находки при
 * одинаковом коде. Сверяем по смысловым маркерам, а не по всему тексту: полное равенство
 * потребовало бы поддерживать копию промпта ещё и в тесте.
 */
test('паритет: ограничения выхода ролей есть в обеих реализациях промпта', (t) => {
  const rust = rustFile('prompt.rs');
  if (!existsSync(rust)) {
    t.skip('rust/src/prompt.rs ещё нет в дереве — сверять не с чем');
    return;
  }
  const js = readFileSync(`${ROOT}lib/prompt.mjs`, 'utf8');
  const rustSrc = readFileSync(rust, 'utf8');
  // Величину потолка берём из JS-константы: тест не должен знать её сам.
  const cap = /SPECIALIST_MAX_ITEMS\s*=\s*(\d+)/.exec(js)?.[1];
  assert.ok(cap, 'в lib/prompt.mjs не найдена константа SPECIALIST_MAX_ITEMS');
  // Потолок жюри берём так же из JS-константы.
  const agg = /AGGREGATOR_MAX_ITEMS\s*=\s*(\d+)/.exec(js)?.[1];
  assert.ok(agg, 'в lib/prompt.mjs не найдена константа AGGREGATOR_MAX_ITEMS');
  const marks = [
    [`AT MOST ${cap} items`, 'потолок числа пунктов у роли'],
    ['Do NOT report nits', 'запрет nit-ов'],
    ['quote the offending line verbatim', 'требование дословной цитаты'],
    // A4: цитата обязана быть у КАЖДОГО пункта роли, а не только у minor — маркер по формату
    // строки в шаблоне пункта, его нельзя случайно оставить только в JS.
    ['  quote: <one line copied verbatim', 'строка `quote:` в шаблоне пункта роли'],
    // A3: потолок и длина полей выхода жюри — это оплаченные completion-токены.
    [`AT MOST ${agg} findings`, 'потолок числа находок жюри'],
    ['2–3 sentences: what breaks, why, and the fix', 'ограничение длины body'],
    ['"summary": "one sentence', 'ограничение длины summary'],
  ];
  for (const [needle, what] of marks) {
    assert.ok(rustSrc.includes(needle), `rust/src/prompt.rs не содержит ${what} («${needle}»). `
      + 'Правка промпта только в JS-копии меняет поведение лишь там, где бинарь не собран.');
  }
  assert.ok(!/severity = critical\|major\|minor\|nit/.test(rustSrc),
    'rust/src/prompt.rs всё ещё разрешает роли репортить nit — шкала разошлась с JS.');
});

/**
 * RFC 07: с окнами контекста обещание «полный текст файла» становится ложью, а ложное
 * обещание хуже маленького контекста — оно приглашает модель утверждать ОТСУТСТВИЕ по
 * усечённому виду, то есть ровно тот класс FP, на подавление которого репозиторий тратит
 * больше всего сил. Держим обе копии промпта чистыми: правка только в JS оставила бы
 * враньё на каждой машине с собранным бинарём.
 */
test('паритет: ни одна копия промпта не обещает полный текст файла (RFC 07)', (t) => {
  const rust = rustFile('prompt.rs');
  if (!existsSync(rust)) {
    t.skip('rust/src/prompt.rs ещё нет в дереве — сверять не с чем');
    return;
  }
  const copies = [
    ['lib/prompt.mjs', readFileSync(`${ROOT}lib/prompt.mjs`, 'utf8')],
    ['rust/src/prompt.rs', readFileSync(rust, 'utf8')],
  ];
  // Формулировки, каждая из которых обещает модели то, чего ей не дают.
  const lies = [
    'full file context',
    'Full content of changed files',
    'FULL content of the changed files',
    'full file content is attached',
  ];
  for (const [file, src] of copies) {
    for (const lie of lies) {
      assert.ok(!src.includes(lie), `${file} снова обещает полный текст файла («${lie}»), `
        + 'хотя контекст едет окнами вокруг изменённых строк — это приглашение к absence-claim.');
    }
  }
  // …и обратная сторона того же решения: разметка усечения обязана быть в обеих копиях,
  // иначе метка есть, а объясняющего её абзаца нет (или наоборот).
  const required = [
    ['[THIS PART, CONTEXT WINDOWED]', 'метка оконного контекста в карте PR'],
    ['строки N–M пропущены', 'ссылка на маркер пропуска в тексте правила'],
    ['NO ABSENCE CLAIMS', 'запрет утверждений об отсутствии'],
  ];
  for (const [file, src] of copies) {
    for (const [needle, what] of required) {
      assert.ok(src.includes(needle), `${file} не содержит ${what} («${needle}»). `
        + 'Размер контекста и текст промпта меняются одним решением, а не двумя.');
    }
  }
});

/**
 * Поле `verdict` (approve|comment|changes_requested) жюри просили заполнять, а читать его
 * было некому: ни отчёт, ни export, ни swarm, ни гейт к нему не обращались (гейтовый
 * `verdict` — другой объект, он собирается из находок). Убрано из обеих копий схемы; тест
 * держит удаление симметричным — вернувшись в одну копию, поле снова оплачивалось бы
 * входными и completion-токенами на каждом вызове жюри там, где собран бинарь (или где нет).
 */
test('паритет: поле verdict убрано из схемы жюри в обеих реализациях промпта', (t) => {
  const rust = rustFile('prompt.rs');
  if (!existsSync(rust)) {
    t.skip('rust/src/prompt.rs ещё нет в дереве — сверять не с чем');
    return;
  }
  const needle = '"verdict":';
  for (const [file, src] of [
    ['lib/prompt.mjs', readFileSync(`${ROOT}lib/prompt.mjs`, 'utf8')],
    ['rust/src/prompt.rs', readFileSync(rust, 'utf8')],
  ]) {
    assert.ok(!src.includes(needle), `${file} снова просит у жюри поле verdict («${needle}»), `
      + 'но читателя у него нет — это оплаченные токены на каждом чанке.');
  }
});

/**
 * A1: два самых дорогих абзаца системного промпта жюри (BLAST-RADIUS RULE, CONVERGENCE) и абзац
 * verifier'а про blast-radius стали УСЛОВНЫМИ — едут только вместе со своей секцией в user-части.
 * Условие невозможно проверить по одному тексту промпта, поэтому сверяем структуру: с каждой
 * стороны абзац обязан быть отдельной именованной константой (в JS — вставляемой тернарником,
 * в Rust — push_str под if). Если Rust-копия снова склеит их в один литерал, промпт с бинарём
 * станет длиннее промпта без него на ~1100 символов в каждом вызове, и никакой тест этого не
 * заметит: обе стороны по отдельности валидны.
 */
test('паритет: дорогие абзацы промпта условны с обеих сторон', (t) => {
  const rust = rustFile('prompt.rs');
  if (!existsSync(rust)) {
    t.skip('rust/src/prompt.rs ещё нет в дереве — сверять не с чем');
    return;
  }
  const js = readFileSync(`${ROOT}lib/prompt.mjs`, 'utf8');
  const rustSrc = readFileSync(rust, 'utf8');

  const jsMarks = [
    [/const AGGREGATOR_BLAST_RULE\s*=/, 'абзац BLAST-RADIUS RULE вынесен в константу'],
    [/const AGGREGATOR_CONVERGENCE\s*=/, 'абзац CONVERGENCE вынесен в константу'],
    [/const VERIFIER_BLAST_RULE\s*=/, 'абзац verifier\'а про blast-radius вынесен в константу'],
    [/blastBlocks\.length \? AGGREGATOR_BLAST_RULE/, 'BLAST-RADIUS RULE едет только с непустым blast-radius'],
    [/priorFindings\.length\) \? AGGREGATOR_CONVERGENCE/, 'CONVERGENCE едет только с непустыми priors'],
    [/blastBlocks\.length \? VERIFIER_BLAST_RULE/, 'абзац verifier\'а едет только с непустым blast-radius'],
  ];
  for (const [re, what] of jsMarks) {
    assert.match(js, re, `в lib/prompt.mjs не найдено: ${what}`);
  }

  const rustMarks = [
    [/const AGGREGATOR_BLAST_RULE: &str/, 'абзац BLAST-RADIUS RULE вынесен в константу'],
    [/const AGGREGATOR_CONVERGENCE: &str/, 'абзац CONVERGENCE вынесен в константу'],
    [/const VERIFIER_BLAST_RULE: &str/, 'абзац verifier\'а про blast-radius вынесен в константу'],
    [/if !blast_blocks\.is_empty\(\)\s*\{\s*system\.push_str\(AGGREGATOR_BLAST_RULE\)/, 'BLAST-RADIUS RULE под условием непустого blast-radius'],
    [/if !priors\.is_empty\(\)\s*\{\s*system\.push_str\(AGGREGATOR_CONVERGENCE\)/, 'CONVERGENCE под условием непустых priors'],
    [/if !blast_blocks\.is_empty\(\)\s*\{\s*system\.push_str\(VERIFIER_BLAST_RULE\)/, 'абзац verifier\'а под условием непустого blast-radius'],
  ];
  for (const [re, what] of rustMarks) {
    assert.match(rustSrc, re, `в rust/src/prompt.rs не найдено: ${what}. `
      + 'Условность абзаца, реализованная только в JS-копии, экономит вход лишь там, где бинарь не собран.');
  }

  // A2: компактная карта PR — одна строка на все чужие части вместо пункта на путь. Метка
  // [ANOTHER PART] на ней обязательна: блок NO ABSENCE CLAIMS ссылается на неё дословно, и
  // сторона, потерявшая метку, оставит две фразы запрета указывающими в пустоту.
  assert.match(js, /Reviewed in other parts \(\$\{elsewhere\.length\}\) — \[ANOTHER PART\]/,
    'в lib/prompt.mjs нет компактной строки чужих частей с меткой [ANOTHER PART]');
  assert.match(rustSrc, /Reviewed in other parts \(\{\}\) — \[ANOTHER PART\]/,
    'в rust/src/prompt.rs нет компактной строки чужих частей с меткой [ANOTHER PART]');
});

test('паритет: Rust-парсер LCOV разбирает объявленные счётчики', (t) => {
  const file = rustFile('structure_lcov.rs');
  if (!existsSync(file)) {
    t.skip('rust/src/structure_lcov.rs ещё нет в дереве — сверять не с чем');
    return;
  }
  const source = readFileSync(file, 'utf8');
  const missing = ['FNF', 'FNH', 'LF', 'LH'].filter((key) => !source.includes(`${key}:`));
  assert.deepEqual(missing, [], `Rust-парсер LCOV не разбирает: ${missing.join(', ')}. `
    + 'Сверка объявленных и посчитанных счётчиков есть только в JS-копии и не работает.');
});
