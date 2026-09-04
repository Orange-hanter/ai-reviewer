<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 💬 есть замечания

> ✅ Блокеров нет: major есть лишь у одной модели — показаны как замечания (для блока нужен консенсус ≥2); critical не найдено.

### Оценки моделей
- **m1** — 0 (🔴0 🟠0 🟡0 🔵0): 

**Итого:** 1 замечаний — 🔴0 🟠1 🟡0 🔵0. Файлов в диффе: 1.

### Замечания вне строк диффа
- 🟠 **Breaking change: caller вне диффа не обновлён (blast-radius)**¹ (`app/client/src/page.tsx`:42) — Graphify blast-radius: зависимый файл `app/client/src/page.tsx` (`renderUser`) не входит в изменённые файлы PR (другая community в AST-графе; другой слой/сервис относительно изменений) [EXTRACTED]. Проверьте совместимость контракта символа `getUser` с этим caller.

<details><summary>📡 Blast-radius (Graphify) · 1 блоков · symbols 1/2 · graph fresh</summary>

- `getUser` (app/server/api.ts)
  - `renderUser` @ app/client/src/page.tsx:42 [calls][EXTRACTED] cross-community


<sub>AST code graph: caller @ path [relation][EXTRACTED|INFERRED]; cross-community — сильнее слой-эвристик. Callers вне диффа — сигнал регрессии для LLM/verifier.</sub>

</details>

<sub>¹ Модели: m1, m2 · commit 0123456 · блокирует merge: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"app/client/src/page.tsx","line":42,"severity":"major","title":"Breaking change: caller вне диффа не обновлён (blast-radius)","model":"blast-radius"}],"runs":[{"sha":"0123456","counts":{"critical":0,"major":1,"minor":0,"nit":0},"added":1,"gone":0,"blocked":false}]} -->