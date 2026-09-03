# Бэклог: фичи и идеи из анализа рынка AI-ревью (2026-08-27)

Источник — сравнительное исследование открытых (PR-Agent, Kodus, Red Hat ai-code-review,
Semgrep/reviewdog) и коммерческих (CodeRabbit, Greptile, Graphite, BugBot, Qodo) решений
плюс бенчмарки (Entelligence 2026, arXiv CRAB) и статьи о галлюцинациях (diffray).
Ключевой вывод исследования: ниша «доказательный merge-гейт с MoA-жюри + verifier +
FP-learning-loop» пуста — конкурентов в ней нет; риски проекта не алгоритмические,
а эмпирические и инфраструктурные.

Статус пунктов сверен с кодом и CHANGELOG 2026-09-03. «Сделано» значит, что механизм
живёт в пакете и покрыт тестами; «частично» — что готова инфраструктура, но не сделан
сам замер или эксперимент, ради которого пункт заводился.

## P0 — устраняют экзистенциальные риски

1. **Абстракция LLM-провайдера** (A1). Заменить привязку `lib/ollama.mjs` → Ollama Cloud
   тонким слоем «OpenAI-совместимый endpoint + конфиг ролей» (подход LiteLLM). Урок PR-Agent:
   их тихий fallback на OpenAI при сбое конфига — грех, который нам запрещён: подмена модели
   в merge-гейте обязана падать громко.
   → **сделано** (0.2.2): `lib/llm.mjs` `createLlmClient`, `lib/ollama.mjs` под ним.
2. **Публичный бенчмарк precision/recall** (B6). Методология Entelligence воспроизводима:
   Cal.com / Sentry / Discourse / Keycloak / Grafana, golden set, скоринг семантической
   близости. `pr-review-gold` — уже пол-инфраструктуры. Двойная отдача: доказательство
   «precision выше CodeRabbit» (готовый питч) + калибровка эвристических весов actionability.
   Отдельным срезом — оценка severity-ranking (постобработка отдельно от recall, как в arXiv).
   → **сделано** (Unreleased): публичный набор `gold-public/` — 13 tp + 5 clean из
   grafana / sentry / discourse, разметка выведена по SZZ-lite из фиксов самих проектов
   (`pr-review-gold-harvest`, `--validate`), скоринг по локации (`mustFindLocations`),
   прогон одной командой `npm run benchmark:public`, результат — [benchmark-public.md](benchmark-public.md).
   Решения зафиксированы в [RFC 03](../proposals/03-public-benchmark-dataset.md).
   Побочный результат замера: router не считал кодом ничего, кроме JS/TS, и PR на Go/Python
   уходил в `light` без ревью — см. CHANGELOG.
   Остаётся открытым **сравнение с конкурентами**: их прогон на том же наборе не делался,
   поэтому формулировка «точнее, чем X» пока не обеспечена.
3. **Учёт стоимости прогона** (A2). MoA = 5+ вызовов × чанки; metrics считает находки, не
   деньги. Токены/роли/модель в JSONL-лог, строку $ в sticky-футер (параллель с cost-audit
   ZAIrgRush).
   → **сделано** (0.2.2): `lib/cost.mjs`, строка стоимости в sticky, usage в JSONL.

## A. Устойчивость и операционка

4. Расширить contract-тесты на все CLI-команды (сейчас — workflow-шаблоны).
5. **RFC-процесс** как в cod-doc: actionability-веса, смена namespace, strict-режим —
   фиксировать решения до реализации.
6. Политика ретраев `effort=none` вынести в конфиг (замер: −50–75% latency на reasoning-моделях);
   качество режима не подтверждено → прогнать как эксперимент через gold-набор.
   → **частично**: `REVIEW_REASONING_EFFORT` в конфиге, каскад ретраев в пайплайне;
   эксперимент на gold не прогнан, дефолт остаётся консервативным (`low`).

## B. Качество вердикта

7. **Whole-repo retrieval каскадом**: только когда blast-radius пуст, а suspicious-зона
   высокая (выигрыш Greptile; свой embedding-индекс не строим).
8. **Детекция cross-service breaking changes** — дыра всей категории (обзор на 450K-монорепо:
   не ловит ни один открытый инструмент). Наш graph-подход — уникальный задел: правило
   «изменённый символ используется вне диффа в другом сервисе» почти уже есть в blast-radius,
   не хватает маршрута находки.
9. **Strict-режим профиля**: prescan + verifier-confirmed, без advisory (точность Graphite
   без их провала recall 7.5% — наша фильтрация по сигналам, а не молчанием).
10. Дифференциация температур по типу чанка (security — глубже, docs — легче).

## C. Анти-FP контур

11. **Аудит по чеклисту diffray** (−96% галлюцинаций): обязательные grounding-цитаты кода
    в находках; self-consistency (повторный вопрос при другой температуре → совпадение =
    ещё один вес actionability); enforcement structured output.
12. **Корреляция «фикс в соседнем коммите»** — наше семейство FP #3, механизма до сих пор нет:
    на повторном прогоне того же PR считать diff от merge-base, а не head; отпечатки кода
    решают между прогонами, но не между коммитами одного прогона.
13. FP-паттерны с confidence, влияющим на actionability, вместо бинарного «погасить»
    (подход Semgrep).

## D. Внедрение и поверхность

14. **AGENTS.md как дефолтный repoRulesFile** (стандарт de facto: Cursor, PR-Agent, OpenAI).
    → **сделано** (0.2.3): `lib/profile.mjs` берёт `AGENTS.md`, если профиль не задал правил.
15. **Режимы `/ask` и `/improve`** поверх того же движка (без гейта / с пониженным
    actionability).
16. **Walkthrough-summary** в начало sticky (массовый продукт CodeRabbit; один вызов
    агрегатора).
    → **сделано** (0.2.3): `buildWalkthroughSection` из summary агрегатора, без лишнего вызова.
17. Кастомизируемая часть промптов — в конфиг потребителя (TOML-подход PR-Agent), сейчас
    они код в `lib/prompt.mjs`.
    → **сделано** (0.2.3): `prompts.advisorExtra|specialistExtra|aggregatorExtra|verifierExtra`
    в профиле (JSON, не TOML — свой формат профиля уже был).
18. **Мультифордж**, первый кандидат Forgejo (ниша, которую community пилит прямо сейчас);
    абстракция над `lib/github.mjs` — только под реального второго потребителя.
19. Self-hosted профиль деплоя: docker-compose + webhook-режим (как PR-Agent), сейчас
    только CI-workflow.
20. Per-PR срез audit log в артефакт (enterprise-гейт: без audit log не платят; JSONL уже есть).

## E. Пересечения с тройкой (ZAIrgRush — руки, cod-doc — память, ai-reviewer — иммунитет)

21. Feedback-журнал ai-reviewer → cod-doc: 👎-журнал потребителя как источник findings
    для дедупа/промоута.
    → **частично**: артефакт экспорта v2 отдаёт `fp` (дедуп по коду), `verifierStatus` и
    `actionabilityScore` — потребитель считает по ним `times_seen` и стабильность набора
    (PR #5, поля описаны в [configuration.md](configuration.md)). Сам 👎-журнал
    (`fp-feedback.jsonl`) в cod-doc пока не уезжает.
22. **ai-reviewer как внешний verdict для PR swarm'а**: тот же принцип «двух независимых
    судей», что и правило strong=verdict в ZAIrgRush.
    → **сделано** в пакете: `lib/swarm.mjs` + поле `swarm` / `*-swarm.json` (см. [swarm.md](swarm.md)).
23. Blast-radius graphify → scope задач swarm'а: метрика `blast_radius.affected_files`
    → **сделано** в пакете: `swarm.suggestedScopeFiles` (вне диффа PR). Оркестратор ZAIrgRush
    ещё должен начать читать артефакт.
24. **Structure protocol / contracts / scenarios**: versioned facts + assessment +
    obligations, EvidenceReceipt, Graphify/SCIP/LCOV, mutation/property evidence и pinned
    `structure_context` MCP.
    → **сделано в producer-пакете**: `lib/structure*.mjs`, `pr-review-structure`,
    `pr-review-structure-mcp`, schemas и fixtures. Глубокое snapshot-хранилище и obligations
    export остаются consumer-частью cod-doc.

## Отклонено сознательно

- embedding-индекс всего репо как фундамент (graph-подход точнее и уже работает);
- фиксы в один клик из ревью (бизнес BugBot/Cursor, замыкает на IDE);
- SOC 2 / SSO / enterprise-compliance (не наш рынок на этом этапе).

## Приоритет внутри списка

P0 закрыт целиком: A1, A2 и B6. Публичный замер проведён — набор, разметка, прогон и отчёт
в [benchmark-public.md](benchmark-public.md). Остаток B6 сузился до одного пункта: прогона
конкурентов на том же наборе; без него сравнительная формулировка не обеспечена.

Дальше по силе: **B8** (единственная дыра, признанная всей категорией, и у нас есть задел
её закрыть — blast-radius уже находит вызывающих вне диффа, не хватает маршрута находки),
**C12** (задокументированное собственное FP-семейство без механизма). E22–E23 сделаны в
пакете; чтобы они дали эффект, артефакт должен начать читать оркестратор ZAIrgRush.

**A5 (RFC до реализации)** перестал быть теорией: крупные контуры приезжают готовыми PR
(structure-платформа, #6), и решение «это часть пакета merge-гейта или соседний пакет»
дешевле принять до кода, а не после пяти тысяч строк.
