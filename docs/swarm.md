---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-29
---

# Контракт swarm (E22 / E23)

Машиночитаемый мост между **ai-reviewer** (merge-гейт) и оркестратором PR-swarm
(ZAIrgRush и подобные). Тот же принцип, что `strong=verdict` в ZAIrgRush: два независимых
судьи — swarm не может сам себе сертифицировать merge.

## Где появляется

Каждый JSON-экспорт (`REVIEW_EXPORT=1`) содержит объект `swarm`. Рядом с основным
артефактом пишется sidecar `<toolName>-swarm.json` — для потребителей, которым нужен
только контракт.

Контракт задаёт `buildSwarmVerdict` в [`lib/swarm.mjs`](../lib/swarm.mjs). Экспорт
прикладывает его в [`buildReviewJson`](../lib/export.mjs) через Rust-порт или его эталонную
JS-копию `buildSwarmVerdictJs`; обе сверяются с `lib/swarm.mjs` parity-тестами.

## Форма (`version: 1`)

| Поле | Значение |
|-------|---------|
| `role` | Всегда `external-verdict` |
| `wouldBlock` | То же, что намерение гейта по exit-коду в этом прогоне |
| `strong` | `true` только если `wouldBlock` **и** каждый блокер независимо подтверждён (`prescan` или `verifierStatus === 'verified'`) |
| `blockers` | Урезанный список блокеров (file/line/severity/source/prescan/verifierStatus/blastCaller) |
| `blastScope` | Метрики Graphify + `affected_files` |
| `suggestedScopeFiles` | **E23**: пути из `affected_files`, которых **нет** в диффе PR, — расширение scope задачи swarm |
| `verdict` | Blob воспроизводимости из sticky, если есть |
| `notes` | Подсказки для логов в человекочитаемом виде |

Soft blast-majors без verifier никогда не выставляют `strong` (и `wouldBlock` при
soft-гейте тоже не выставляют). Шум уровня advisory во внешний вердикт не попадает.

## Использование оркестратором

1. Прочитать `<tool>-swarm.json` (или `swarm` из основного JSON).
2. Если `strong === true` → считать это **блоком** второго судьи (не мержить / не
   помечать вердикт swarm как pass).
3. Если `wouldBlock && !strong` → для swarm это advisory; не приравнивать к жёсткому
   внешнему блоку.
4. Объединить `suggestedScopeFiles` со scope задачи сверх диффа PR.

Публичные хелперы из корневого экспорта пакета: `buildSwarmVerdict`, `isStrongBlocker`,
`suggestedScopeFiles`, `SWARM_CONTRACT_VERSION`.
