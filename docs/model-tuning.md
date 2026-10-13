---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-10
---

# Выбор моделей и параметров

Провайдер по умолчанию — Ollama Cloud. Он не публикует внятной документации по каждой модели,
поэтому роли расставлены по собственным замерам: пробник `npx pr-review-probe-models`
(latency, reasoning effort, JSON-mode) и A/B-прогоны на замороженных диффах.

## Роли в пайплайне

| Роль | Дефолт | Ответ | Почему |
|------|--------|-------|--------|
| specialist correctness | `kimi-k2.7-code` | prose | лучший на локальной логике кода |
| specialist architecture | `glm-5.3` | prose | большое окно, устойчив на кросс-файловых связях |
| specialist security | `glm-5.3-flash` | prose | дёшево и быстро на типовых паттернах уязвимостей |
| jury aggregator | `minimax-m3` | **JSON** | синтез + refutation, вне пула advisors |
| verifier (Pass B) | `deepseek-v4-pro` | **JSON** | глубокий судья, независимый от аггрегатора |
| fallback | `deepseek-v4.1-flash` | JSON+prose | замена упавшей модели, чтобы прогон не терял роль целиком |

Колонка «Ответ» — не декорация, а ограничение при подборе (см. «JSON-роли» ниже).

Аггрегатор и verifier намеренно **не** входят в пул advisors: судья не должен подтверждать
собственные находки.

## JSON-роли: чем ограничен подбор

`chatJson` (aggregator, verifier) держит каскад ретраев, и первая ступень при пустом/оборванном
ответе — **повтор с `reasoning_effort: none`**. Отсюда жёсткое требование: модель на JSON-роли
обязана отдавать валидный JSON *и* на `low`, *и* на `none`. Advisors такому не подчиняются —
у них `jsonMode: false`, им проза штатна.

Замеры `pr-review-probe-models` (2026-09-03, Ollama Cloud):

| Модель | `low` | `none` | Годна на JSON-роль |
|--------|-------|--------|--------------------|
| `kimi-k2.7-code` | JSON ok (~2.0 с) | JSON ok (~0.9 с) | да |
| `deepseek-v4-pro` | JSON ok (~1.3 с) | JSON ok (~0.9 с) | да |
| `glm-5.3` | JSON ok (~1.2 с) | **проза** | **нет** — только prose-роль |
| `glm-5.3-flash` | JSON ok (~0.8 с) | **проза** | **нет** — только prose-роль |
| `minimax-m3` | JSON с обрезанным префиксом | JSON ok (~5.0 с) | да, но см. ниже |

Замена 2026-10-09: провайдер снял `deepseek-v4-flash` (`410 retired`, 2026-09-25) — он был
запасной моделью, и на исчерпанном лимите прогон уходил на неё и падал целиком. Замер
`pr-review-probe-models` (2026-10-09; `mistral-large-4` проверялся как кандидат на verifier,
но verifier остался `deepseek-v4-pro`):

| Модель | `low` | `none` | Годна на JSON-роль |
|--------|-------|--------|--------------------|
| `deepseek-v4.1-flash` | JSON ok (~0.9 с) | JSON ok (~0.6 с) | да |
| `mistral-large-4` | JSON ok, **медленно** (11–18 с, ~1000 токенов рассуждения) | JSON в ```` ```json ```` (~1.2–1.6 с) — `extractJsonObject` снимает обёртку | да |

Оба GLM 5.3 на `none` игнорируют `response_format: json_object` и уходят в развёрнутую прозу.
Поэтому они стоят на specialist-ролях (prose) и **не** назначаются агрегатором/верификатором:
на JSON-роли первый же ретрай каскада разложил бы прогон.

`minimax-m3` на `low` в пробнике терял ведущие токены (`": true, "desc": …}` вместо
`{"bug": true, …}`). На агрегаторе это гасится ретраем на `none` (там ответ валиден) и
восстановлением в `lib/json.mjs`, но при следующем пересмотре ролей это первый кандидат на замену.

## Температуры

| Параметр | Дефолт | Наблюдение |
|----------|--------|------------|
| `MOA_REFERENCE_TEMPERATURE` | `0.3` | ниже 0.6 — заметно меньше находок «по касательной», плотнее по существу |
| `MOA_AGGREGATOR_TEMPERATURE` | `0.4` | ниже — агрегатор начинает копировать advisors дословно |
| `REVIEW_VERIFIER_TEMPERATURE` | `0.2` | судье нужна воспроизводимость, а не разнообразие |

## Размер чанка

Окно контекста современных моделей — не ограничение: чанки можно увеличить в 3–4 раза.
Не делаем: на мелких чанках выше recall — узкое место не окно, а внимание модели к локальному
коду. `REVIEW_CHUNK_BYTES=56000` — компромисс между числом LLM-вызовов и качеством.

Обратная сторона мелких чанков — находки вида «этого файла нет в PR» из чанка, который его не
видел. Против этого в каждый чанк уходит полный список изменённых файлов с метками частей
(см. [false-positives.md](false-positives.md)).

## Reasoning effort

`REVIEW_REASONING_EFFORT` (default `low`) применяется только к advisor-вызовам. На замерах
`none` даёт −50…−75% latency у kimi/glm/deepseek (thinking отключается, JSON остаётся
валидным), но качество находок на этом режиме не подтверждено — поэтому дефолт консервативный.

Пайплайн уже держит каскад ретраев: пустой `content` на `effort=low` → повтор с `none`
(reasoning-модели иногда тратят весь бюджет токенов на скрытое рассуждение).

### Замер A5: `REVIEW_AGGREGATOR_REASONING_EFFORT=none` на жюри — дефолт не меняется

`REVIEW_AGGREGATOR_REASONING_EFFORT` (default `low`) управляет effort первой попытки
JSON-вызовов — агрегатора жюри и verifier'а. Гипотеза была: провайдер считает рассуждение
completion-токенами, значит `none` срежет выход жюри. Gold-прогон на трёх кейсах
(`tp-real-bugs-unescape`, `fp-noise-no-real-blockers`, `tp-structure-real-defects`,
профиль `.ai-reviewer.json`, агрегатор `minimax-m3`) её не подтвердил:

| Метрика по трём кейсам | `low` (дефолт) | `none` |
|---|---|---|
| completion-токенов агрегатора | 95 563 | 100 754 (+5,4 %) |
| completion-токенов агрегатора на находку | 3 982 | 4 030 |
| токенов всего | 700 890 | 714 996 (+2,0 %) |
| время | 1 237 с | 1 212 с |
| inline | 12 | 8 |
| вердикты gold | ✅ ❌ ❌ | ❌ ❌ ❌ |

Экономии нет, а качество ниже: `fp-noise-no-real-blockers` перешёл ✅ → ❌ (два блокера на
кейсе без реальных блокеров), `tp-structure-real-defects` потерял найденную на `low`
размеченную локацию `lib/structure-drift.mjs:168-189`. **Дефолт остаётся `low`** — и в
`lib/config.mjs`, и в порте `rust/src/config.rs`. Переключатель сохраняется: на другом
агрегаторе (`sonnet` через шим, где рассуждение дешевле) замер надо повторить, а не переносить
вывод. Подробности прогона — в [RFC 04](../proposals/04-token-economy.md), раздел «Итог второго
прохода». Вопрос «менять ли дефолт на `none`» этим замером закрыт: не менять.

## Как менять модели безопасно

1. `npx pr-review-probe-models` — доступность, latency, JSON-mode новой модели.
2. `npm run eval` — гейт post-processing не должен поехать.
3. `OLLAMA_API_KEY=… npx pr-review-gold` — shadow-прогон с LLM на размеченных TP/FP диффах.
4. Первый боевой прогон сравнить по Δ-блоку sticky: что появилось и что ушло на том же коде.

## Локальный прогон с полными сигналами

Замер моделей на этом репозитории имеет смысл только тогда, когда детерминированный контур
жив. Прогон без code graph и без `ast-grep` — это не «движок слабый», а прогон, где сигналы
`prescan` (+3) и `blastCaller` (+2) выключены средой: разбор такого замера и его последствия —
в [review-quality.md](review-quality.md) §2.4.

**Профиль.** `.ai-reviewer.json` лежит в корне репозитория и подхватывается автопоиском —
никакого `REVIEW_PROFILE` для локального прогона задавать не нужно. Он задаёт чувствительную
зону (`lib/`, `bin/`, `rust/src/`, `pipeline.mjs`) и правила проекта в промпте.

**Code graph** — тем же пином, что и в `examples/ci/pr-review.yml`:

```bash
python3 -m venv .graphify/venv
.graphify/venv/bin/pip install --quiet --disable-pip-version-check graphifyy==0.9.87
.graphify/venv/bin/graphify --version          # ожидаем graphify 0.9.87

GRAPHIFY_OUT=.graphify/graphify-out GRAPHIFY_NO_TIPS=1 \
  .graphify/venv/bin/graphify update . --force
```

Дальше прогон видит граф через `GRAPHIFY_BIN` и `GRAPH`:

```bash
export GRAPHIFY_BIN=.graphify/venv/bin/graphify
export GRAPH=.graphify/graphify-out/graph.json
```

Проверка: в логе `blast-radius блоков: N` при N > 0 и ненулевой resolve символов вместо
`graph missing`.

**ast-grep** — отдельный бинарь, ставится пакетным менеджером:

```bash
brew install ast-grep     # macOS; иначе cargo install ast-grep --locked
ast-grep --version
```

Проверка: в логе `prescan R6: N issue(s)` вместо `ENOENT`.

**eslint здесь пуст — и это ожидаемо.** У пакета ноль runtime-зависимостей и нет
flat-config `eslint.config.js`, поэтому eslint-половина prescan на этом репозитории всегда
даёт `недоступен (нет node_modules)`. Сигнал `prescan` приходит только от ast-grep, и
поставляемый в пакете ruleset написан под TypeScript-потребителя — на `.mjs` он не срабатывает.
Смотреть надо на строку `blast-radius`; нулевой eslint здесь не диагноз.

`.graphify/` и `.ai-review/` — gitignored артефакты локальных прогонов, коммитить их не нужно.
