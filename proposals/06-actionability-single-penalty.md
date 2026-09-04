# 06 — Один вердикт жюри — один штраф в actionability

> Категория: 🎯 Прямое · Риск: низкий · Зависимости: [RFC 04](04-token-economy.md) (измеримость), `docs/review-quality.md` (разбор)

## Контекст

Разбор прогона PR #6 в [`docs/review-quality.md`](../docs/review-quality.md) §2.3 показал:
из 34 находок 30 были отсечены арифметикой маршрута ещё до того, как кто-либо посмотрел на их
суть. Одна из двух причин — понижение жюри вычитается дважды.

Правило репозитория: веса actionability меняются только через RFC. Этот RFC короткий, потому
что доказательная часть уже написана в review-quality.md; здесь — решение и его границы.

## Текущее состояние

Сверено по коду:

- [`lib/specialists.mjs`](../lib/specialists.mjs) `applyRefutation(..., { strict: true })`:
  находка, у которой хоть один claim `downgraded`, получает `severity` из `decideRefutation`
  (minor/nit) и `refutationStatus: 'downgraded'`;
- [`lib/actionability.mjs`](../lib/actionability.mjs) `scoreActionabilityJs` (и порт
  `rust/src/actionability.rs::score_actionability`): для той же находки
  `claims.some(rejected|downgraded)` → сигнал `juryDowngraded` весом **−2**;
- итог по группам из R2: «хоть один claim понижен» — 18 находок, потолок баллов **1** при
  пороге 4; «все verified» — 4 находки, потолок 5; «без claims» — 12, потолок 3.

Один и тот же факт — «жюри понизило» — применяется в двух независимых местах: сначала
снимает severity (и с ней право на инлайн по тиру), затем отнимает два балла из доказуемости.
Единственная понижённая претензия из пяти обнуляет находку целиком.

## Предложение

`juryDowngraded` не применяется, если `refutationStatus === 'downgraded'` — понижение уже
отработало на severity. Сигнал остаётся для случая, когда refutation не меняла severity, но
часть claims отклонена (`rejected` при наличии `verified` — `decideRefutation` даёт `keep`).

Не делаем «пропорциональный jury» (+2/0/−2 по доле claims): это второй параметр там, где
проблема — в двойном учёте одного.

## Миграция

Меняется только скоринг; `resolveBlockers`, `REVIEW_BLOCK_ON`, порог маршрута не
затрагиваются. Веса в `ACTIONABILITY_SIGNALS` те же. Обе реализации (JS и Rust) — синхронно,
parity `rust/parity/actionability.parity.mjs`.

## Риски и что не делаем

- **Риск:** больше advisory-находок наберёт порог → больше инлайнов. Ограничитель уже есть:
  `REVIEW_MAX_ADVISORY_INLINE` (5). Не поднимаем.
- **Не делаем:** не трогаем вес `verified`, `prescan`, `repeated`; не меняем `strict` в
  `applyRefutation`.

## Оценка

Одна задача: правка в двух файлах + тест «одна понижённая претензия из пяти не обнуляет
находку» (JS) + parity. Ожидание на R2: группа «хоть один claim понижен» получает потолок 3
вместо 1 — те же права, что у находок без claims, а не хуже.
