<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 🔴 нужны правки

> ⛔ **Merge заблокирован:** 1 замечание. Правило блокировки: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока. Исправьте (или ослабьте `REVIEW_BLOCK_ON`/`REVIEW_BLOCK_SINGLE`/`REVIEW_CONSENSUS`). Инфра-сбои merge не блокируют.

### Walkthrough

нашёл баг

### Оценки моделей
- **m1** — 1 (🔴1 🟠0 🟡0 🔵0): нашёл баг

**Итого:** 1 замечаний — 🔴1 🟠0 🟡0 🔵0. Файлов в диффе: 1.

> 🔄 **С прошлого прогона (прошлый прогон `0123456`):** +0 новых, −1 закрыто, 1 без изменений, стабильность 50%.

<details><summary>🔄 Что изменилось</summary>

**Закрыты или сняты (1)**
- 🟡 `app/server/api.ts:2` магическое число

</details>

<details><summary>📈 История прогонов</summary>

| sha | гейт | 🔴 | 🟠 | 🟡 | 🔵 | Δ | стаб. |
|---|---|---|---|---|---|---|---|
| `0123456` | ⛔ | 1 | 0 | 1 | 0 | +0 / −0 | — |
| `deadbee` | ✅ | 0 | 0 | 1 | 0 | +0 / −0 | — |

⛔ Вердикт на `0123456` воспроизводим: блокировали все 2 прогонов на этом коммите.

</details>

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<details><summary>🔗 Model reasoning (consensus)</summary>

#### m1

нашёл баг

</details>

<sub>¹ Модели: m1, m2 · commit 0123456 · блокирует merge: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"app/server/api.ts","line":4,"severity":"critical","title":"IDOR в dropUser","model":"m1","fp":"4b4ac22414a5"}],"runs":[{"sha":"0123456","blocked":true,"counts":{"critical":1,"major":0,"minor":1,"nit":0}},{"sha":"deadbee","blocked":false,"counts":{"critical":0,"major":0,"minor":1,"nit":0}},{"sha":"0123456","counts":{"critical":1,"major":0,"minor":0,"nit":0},"added":0,"gone":1,"stability":0.5,"blocked":true}],"reasoning":[{"role":"model","model":"m1","excerpt":"нашёл баг","truncated":false}]} -->