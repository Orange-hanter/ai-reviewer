---
type: proposal
status: accepted
owner: ai-reviewer
source_of_truth: true
created: 2026-11-02
last_updated: 2026-11-02
---

# RFC 22. Пониженная находка не блокирует merge

> Категория: 🎯 Прямое · Риск: низкий · Зависимости: RFC 17 (решение — свойство находки), RFC 20 (новое поведение — только в Rust)

## Контекст

Понижение — вердикт «дефект реален, но не merge-blocker». Его выносят оба независимых
судьи: verifier (`decision: downgrade`) и refutation жюри (все claims `downgraded`, либо
strict-refutation для находки уровня блока без `advisorClaims`). Оба понижают severity ровно
в `minor`/`nit` и этим снимают блок — но только пока `minor`/`nit` не входят в
`REVIEW_BLOCK_ON`. С `REVIEW_BLOCK_ON=critical,major,minor` пониженная находка с confidence ≥
порога блокирует так же, как до понижения: вердикт судьи гейт молча игнорирует.

Найдено при переносе порога в промпт (PR #78): промпт обещал «downgrade — real, but not
merge-blocking», и там это обещание сделали условным. Правильнее сдержать обещание в гейте.

## Текущее состояние

- verifier: `rust/src/verifier.rs:194-205` — `downgrade` → severity minor/nit (иначе minor),
  `verifierStatus: "downgraded"`.
- refutation: `rust/src/specialists.rs:252` (`refutationStatus: "downgraded_no_claims"`,
  strict) и `:298` (`"downgraded"`); обёртка `lib/specialists.mjs:136` пишет поле в находку.
- гейт: `rust/src/findings.rs` `cluster_findings` — `in_block_on` смотрит только severity;
  вне strict `consensus_block = tier_block && confidence_block`, статусы понижения не читаются.
  Strict (`gate.strict`) требует `verified || prescan`, поэтому verifier-понижение там уже не
  блокирует, а refutation-понижение с последующим `verified` — блокирует.
- отбор кандидатов verifier'а: `rust/src/verifier.rs:56` — по severity из blockOn, без учёта
  `refutationStatus`.

## Предложение

1. **Определение.** Находка понижена, если `verifierStatus == "downgraded"` или
   `refutationStatus ∈ {"downgraded", "downgraded_no_claims"}`.
2. **Гейт** (`cluster_findings`): для пониженной находки `in_block_on = false`. Она не
   блокирует и не даёт голос консенсуса кластера — ровно то, что при дефолтном конфиге уже
   делает её новая severity. Поведение не зависит от `REVIEW_BLOCK_ON`, `strict`, blast.
3. **Отбор verifier'а**: находка, пониженная refutation'ом, кандидатом не становится — гейт
   её вердикт не применит, вызов модели был бы оплачен впустую.
4. **Промпт**: «downgrade — real, but not merge-blocking» снова безусловен (H8/H9 из PR #78),
   потому что теперь это верно при любом конфиге.

Дефолтный конфиг (`critical,major`) этим не меняется: пониженная находка там уже minor/nit
вне blockOn. Проверка — `npm run eval` без правок существующих fixtures.

## Миграция

Нет: формат находки, футер sticky, export не меняются; поля `verifierStatus` и
`refutationStatus` уже пишутся. Потребитель с `minor` в `REVIEW_BLOCK_ON` перестаёт получать
блок от находок, которые судья понизил, — это и есть исправление.

## Риски и что не делаем

- JS-копии (`lib/findings.mjs`, `lib/verifier.mjs`) не правим (RFC 20): без Rust-бинарей
  гейт ведёт себя по-старому. Расхождение в parity помечается rust-owned.
- Не трогаем strict-ветку и blast soft-gate: правило применяется до них и только снимает блок.
- Не меняем цель понижения (minor/nit) и не вводим «понижение в severity ниже блока»: это
  связало бы verifier с конфигом гейта ради того же результата.

## Оценка

Две строки логики в Rust, фильтр в отборе кандидатов, правка двух фраз промпта; три eval
fixture (две на понижение при `minor` в blockOn и контрольная «непониженный minor блокирует»).
