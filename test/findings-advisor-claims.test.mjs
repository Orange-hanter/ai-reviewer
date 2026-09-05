/**
 * Паритет `advisorClaims` в normalizeFinding: «claims нет» — это ОТСУТСТВУЮЩЕЕ поле,
 * а не `null`.
 *
 * Почему это отдельный тест, а не только parity-прогон. `lib/findings.mjs` — тонкая обёртка
 * над бинарём с JS-копией как fallback, и JS-копия собирает находку через
 * `normalizeAdvisorClaims` из `lib/specialists.mjs` — а это САМА обёртка над другим бинарём.
 * Пустой результат она отдаёт как `undefined` без бинаря и как JSON `null` с ним, поэтому
 * на машине с частично собранными бинарями рождалось третье поведение, которого нет ни у
 * одной из двух реализаций: `advisorClaims: null` в находке.
 *
 * Разница наблюдаема снаружи: `JSON.stringify` роняет `undefined`, но несёт `null`, а находки
 * уезжают в скрытый футер sticky и в версионированный артефакт экспорта. Читатели ждут именно
 * отсутствия: typedef Finding объявляет `[advisorClaims]` опциональным массивом, strict-refutation
 * (`lib/specialists.mjs`) понижает block-tier «без advisorClaims», actionability
 * (`lib/actionability.mjs`) считает claims только для массива, а `test/incremental.test.mjs`
 * прямо требует `assert.equal(f.advisorClaims, undefined)` на строгом assert.
 *
 * Тест проверяет ОБЕ стороны: JS-копию (форсированно) и бинарь, если он собран. В CI `npm test`
 * идёт раньше `cargo build`, поэтому Rust-ветка там пропускается — её держит `npm run check:parity`
 * (rust/parity/findings.parity.mjs), включённый в `check:rust` и в CI-шаг после сборки.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { normalizeFinding } from '../lib/findings.mjs';

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const FINDINGS_BIN = resolve(ROOT, 'rust/target/debug/findings');

const RAW_NO_CLAIMS = {
  file: 'server/api.ts', line: 11, severity: 'critical',
  title: 'IDOR на подписке', body: 'нет проверки userId', confidence: 0.92,
};
const RAW_WITH_CLAIMS = {
  ...RAW_NO_CLAIMS,
  advisorClaims: [{ role: 'security', status: 'verified', reason: 'подтверждено в коде' }],
};

/** Прогон через нужную ветку резолвинга: forceJs=true — JS-копия, false — бинарь. */
function normalize(raw, forceJs) {
  if (forceJs) process.env.AI_REVIEWER_FINDINGS_FORCE_JS = '1';
  else delete process.env.AI_REVIEWER_FINDINGS_FORCE_JS;
  try {
    return normalizeFinding(raw, 'm1', {});
  } finally {
    delete process.env.AI_REVIEWER_FINDINGS_FORCE_JS;
  }
}

/**
 * Проверка идёт по СЕРИАЛИЗОВАННОМУ виду: JS-копия, как и для `suggestion`/`quote`, кладёт ключ
 * со значением `undefined`, и это ровно то, что `JSON.stringify` роняет. Значимо здесь одно —
 * поле не должно доехать до JSON, то есть не должно стать `null`.
 */
function assertAbsent(out, side) {
  assert.equal(out.advisorClaims, undefined,
    `${side}: advisorClaims = ${JSON.stringify(out.advisorClaims)}; «жюри не высказывалось» `
    + 'обязано быть undefined/отсутствием, иначе значение доедет до JSON');
  assert.ok(!Object.prototype.hasOwnProperty.call(JSON.parse(JSON.stringify(out)), 'advisorClaims'),
    `${side}: advisorClaims доехало до сериализации — footer sticky и артефакт экспорта разойдутся `
    + 'между машиной с собранным бинарём и без него');
}

test('findings: пустые advisorClaims не превращаются в null (JS-копия)', () => {
  assertAbsent(normalize(RAW_NO_CLAIMS, true), 'JS-копия');
});

test('findings: пустые advisorClaims не превращаются в null (Rust-бинарь)', (t) => {
  if (!existsSync(FINDINGS_BIN)) {
    t.skip('rust/target/debug/findings не собран — сверять не с чем');
    return;
  }
  assertAbsent(normalize(RAW_NO_CLAIMS, false), 'Rust-бинарь');
});

test('findings: непустые advisorClaims сохраняются обеими сторонами', (t) => {
  const js = normalize(RAW_WITH_CLAIMS, true);
  assert.deepEqual(js.advisorClaims,
    [{ role: 'security', status: 'verified', reason: 'подтверждено в коде' }],
    'JS-копия потеряла claims — «нет поля» не должно стать «нет claims вообще»');
  if (!existsSync(FINDINGS_BIN)) {
    t.diagnostic('rust/target/debug/findings не собран — Rust-сторона пропущена');
    return;
  }
  // Сравниваем JSON-виды: JS-копия несёт ключи `suggestion`/`quote` со значением undefined,
  // Rust-порт их просто не кладёт — на сериализации это одно и то же (так же сверяет паритет).
  assert.deepEqual(
    JSON.parse(JSON.stringify(normalize(RAW_WITH_CLAIMS, false))),
    JSON.parse(JSON.stringify(js)),
    'Rust-бинарь и JS-копия разошлись на находке с claims');
});
