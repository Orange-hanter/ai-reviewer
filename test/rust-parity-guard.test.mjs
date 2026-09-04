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
  const marks = [
    [`AT MOST ${cap} items`, 'потолок числа пунктов'],
    ['Do NOT report nits', 'запрет nit-ов'],
    ['quote the offending line verbatim', 'требование дословной цитаты для minor'],
  ];
  for (const [needle, what] of marks) {
    assert.ok(rustSrc.includes(needle), `rust/src/prompt.rs не содержит ${what} («${needle}»). `
      + 'Правка промпта только в JS-копии меняет поведение лишь там, где бинарь не собран.');
  }
  assert.ok(!/severity = critical\|major\|minor\|nit/.test(rustSrc),
    'rust/src/prompt.rs всё ещё разрешает роли репортить nit — шкала разошлась с JS.');
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
