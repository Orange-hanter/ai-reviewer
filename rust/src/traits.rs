//! DI-контракт пайплайна: порт объекта `deps` из `pipeline.mjs:226-245` на трейты.
//!
//! Зачем трейты, а не структура с замыканиями: в JS `deps` — утиная типизация, и
//! форма каждого метода задокументирована только вызовом. Здесь она фиксируется
//! типами один раз (задача T08 RFC 16) и дальше **заморожена**: `pipeline.rs` (T26),
//! `github.rs`/`engine.rs`/`prescan.rs`/`graphify_client.rs` (волна 2) и
//! golden-harness (T25) пишутся против этих сигнатур.
//!
//! Соответствие JS-источникам:
//!
//! | трейт | JS-источник | дефолт в `pipeline.mjs` |
//! |---|---|---|
//! | [`GithubApi`] | `lib/github.mjs:271-275` (объект из `createGithubClient`) | `createGithubClient(cfg, log)` (`pipeline.mjs:235`) |
//! | [`EngineApi`] | `lib/engine.mjs:412` + `lib/verifier.mjs:166-203` | `createReviewEngine(...)` (`pipeline.mjs:262`) |
//! | [`PrescanApi`] | `lib/prescan-runner.mjs:35-40` | `createPrescanRunner(cfg, { log })` (`pipeline.mjs:237`) |
//! | [`GraphifyApi`] | `lib/graphify.mjs:400` | `createGraphifyClient(cfg)` (`pipeline.mjs:234`) |
//! | [`Notifier`] | `pipeline.mjs:189-205` (`notifyTelegram`) | `notifyTelegram` (`pipeline.mjs:238`) |
//!
//! Что из JS-`deps` СЮДА НЕ переезжает и почему:
//!
//! - `die` (`pipeline.mjs:227`) — в Rust нет `process.exit` из середины стадии;
//!   мягкий отказ возвращается значением (`PipelineOutcome`, решение RFC 16 к T26);
//! - `costTracker`, `ollama`, `prompt`, `findingsFmt` (`pipeline.mjs:230-236`) —
//!   не внешние эффекты, а внутренние сотрудники движка: их подменяли только
//!   ради удобства, ни один тест пайплайна этого не делает. Они становятся
//!   обычными полями `RealEngine`/`pipeline::run`.
//!
//! Стиль — блокирующий, без async (RFC 16, решение 2). Все трейты требуют
//! `Send + Sync`: MoA-параллелизм в `engine.rs` строится на `std::thread::scope`,
//! и `&dyn EngineApi`/`&dyn GithubApi` должны переживать передачу в scoped-поток.
//! Для фейков это ничего не стоит — журнал вызовов у них и так под `Mutex`.
//!
//! Форма данных: там, где JS гоняет открытый JSON (находки, вердикты, ответы
//! GitHub REST), тип — `serde_json::Value`. Это не лень, а требование паритета:
//! `findings.rs`, `report.rs`, `export.rs`, `verifier.rs` уже написаны против
//! `&Value` и никакой `struct Finding` в крейте не существует.
//!
//! Фейковые реализации всех пяти трейтов — в `crate::testutil` (порт `mkDeps`
//! из `test/pr-review.pipeline.test.mjs:41-88`). Свои фейки в тестах заводить
//! не нужно: журнал вызовов там общий, поэтому порядок стадий между разными
//! зависимостями наблюдаем так же, как в JS.

use anyhow::Result;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::Chunk;

/// `false` — маркер «поле в JSON не пишем» для `skip_serializing_if`.
fn is_false(b: &bool) -> bool {
    !*b
}

/// `0` — маркер «поле в JSON не пишем» для `skip_serializing_if`.
fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// `0` — маркер «поле в JSON не пишем» для `skip_serializing_if`.
fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

// ---------------------------------------------------------------------------
// Общие типы GitHub-контура
// ---------------------------------------------------------------------------

/// PR, каким его видит пайплайн (`pipeline.mjs:263`, `264-275`).
///
/// Из полного объекта GitHub REST читаются ровно три поля — `number`, `title`,
/// `html_url` (`pipeline.mjs:275`, `pipeline.mjs:284-285`, `pipeline.mjs:640`;
/// `prompt.rs:479-481`; `export.rs:268-275`), поэтому остальные не сохраняем:
/// потери не наблюдаемы ни в sticky, ни в артефакте экспорта.
///
/// `html_url` намеренно **не** переименовано в camelCase: это ключ GitHub REST,
/// и `export.rs` ищет его именно так.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct PullRequestRef {
    pub number: u64,
    pub title: String,
    #[serde(rename = "html_url")]
    pub html_url: String,
}

impl PullRequestRef {
    /// Заглушка локального прогона: `{ number: cfg.github.prNumber, title: '(local diff)', html_url: '' }`
    /// (`pipeline.mjs:263`).
    pub fn local_diff(pr_number: u64) -> Self {
        Self { number: pr_number, title: "(local diff)".to_string(), html_url: String::new() }
    }
}

/// Результат `loadDiff` (`lib/github.mjs:105-114`).
///
/// В JS у метода три исхода: `null` (нет repo/pr/token или сбой API), `{draft: true}`
/// и `{pr, headSha, diff}`. В Rust первый исход — `None` у `Result<Option<LoadedDiff>>`,
/// остальные два различаются полем [`LoadedDiff::draft`] (`pipeline.mjs:270-271`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LoadedDiff {
    /// PR-черновик: пайплайн выходит мягко, LLM не зовётся (`pipeline.mjs:271`).
    #[serde(skip_serializing_if = "is_false")]
    pub draft: bool,
    pub pr: PullRequestRef,
    /// `pr.head?.sha || ''` — SHA головы; в sticky и экспорт уходит первые 7 символов.
    pub head_sha: String,
    /// Тело диффа в формате `application/vnd.github.v3.diff`.
    pub diff: String,
}

/// Разобранный скрытый футер прошлого sticky (`lib/github.mjs:116-142`).
///
/// Пайплайн нормализует форму сам (`pipeline.mjs:341-345`), потому что JS-клиент мог
/// вернуть неполный объект; в Rust неполнота выражена дефолтами полей, а не проверками
/// на месте использования.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StickyData {
    /// Находки прошлого прогона (в JS отфильтрованы `typeof f.file === 'string'` и обрезаны до 40).
    pub findings: Vec<Value>,
    /// История прогонов, последние 5: элементы вида `{sha, verdict, ...}`.
    pub runs: Vec<Value>,
    /// SHA прогона, которым написан футер (`''`, если футера не было).
    pub sha: String,
}

/// Инлайн-коммент GitHub review в том виде, в каком его строит
/// `createFindingsFormatter().classify` (`lib/findings.mjs:440-442`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct InlineComment {
    /// Путь как в диффе (`+++ b/...`).
    pub path: String,
    pub line: u64,
    /// Всегда `"RIGHT"`: комментируем новую сторону диффа.
    pub side: String,
    /// Markdown тела: маркер профиля + блоки находок группы.
    pub body: String,
}

impl InlineComment {
    /// `{ path, line, side: 'RIGHT', body }` — как в `lib/findings.mjs:440-442`.
    pub fn new(path: impl Into<String>, line: u64, body: impl Into<String>) -> Self {
        Self { path: path.into(), line, side: "RIGHT".to_string(), body: body.into() }
    }
}

/// Аргумент `postReview({ inline, headSha })` (`lib/github.mjs:183-193`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PostReviewParams {
    /// Уже обрезанный до `limits.maxInline` список (`pipeline.mjs:164-168`).
    pub inline: Vec<InlineComment>,
    /// Пустая строка — не передавать `commit_id` (`lib/github.mjs:190`).
    pub head_sha: String,
}

/// Результат `postReview`: `res?.id || null` (`lib/github.mjs:192`).
///
/// Собственный тип, а не голый `Option<u64>`, — чтобы значение читалось на месте
/// вызова: `None` означает «GitHub не вернул id», и тогда чистка старых инлайнов
/// ПРОПУСКАЕТСЯ, иначе сотрём только что запощенное (`pipeline.mjs:176-180`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct PostReviewResult {
    pub review_id: Option<u64>,
}

/// Аргумент `deleteStaleInline({ exceptReviewId })` (`lib/github.mjs:162`).
///
/// `None` — чистим все свои инлайны; `Some(id)` — кроме комментов только что
/// запощенного review (порядок post-then-delete, `pipeline.mjs:171-180`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct DeleteStaleOpts {
    pub except_review_id: Option<u64>,
}

/// Опции одиночного REST-вызова `gh(path, opts)` (`lib/github.mjs:99-103`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GhCallOpts {
    /// Пусто — `GET` (дефолт `ghRaw`).
    pub method: String,
    /// Тело запроса; `None` — не отправлять.
    pub body: Option<Value>,
    /// Пусто — `application/vnd.github+json`.
    pub accept: Option<String>,
}

/// Незакрытый review-тред PR (`lib/github.mjs:242-249`).
///
/// Отдаётся auto-triage (`lib/triage.mjs:73-108`). `triage::decide_auto_triage`
/// принимает `&Value`, поэтому на входе в него тред сериализуется
/// (`serde_json::to_value`) — форма ключей совпадает один в один.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ThreadRef {
    /// GraphQL node id треда (аргумент `replyReviewThread`/`resolveReviewThread`).
    pub id: String,
    pub path: String,
    /// `null` у тредов на удалённых строках — auto-triage это переживает.
    pub line: Option<u64>,
    /// Первые 10 комментариев треда (`comments(first: 10)`).
    pub comments: Vec<ThreadComment>,
}

/// Комментарий внутри review-треда (`lib/github.mjs:248`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ThreadComment {
    pub body: String,
    /// `author.login`; `None` — автор удалён/скрыт (в JS это `undefined`).
    pub author: Option<String>,
}

/// Транспорт и оркестрация GitHub — порт объекта, который возвращает
/// `createGithubClient` (`lib/github.mjs:271-275`, все 14 методов).
///
/// Fail-open/fail-closed поведение остаётся у ВЫЗЫВАЮЩЕГО: в JS часть методов
/// глотает ошибку внутри (`readPriorStickyData`, `deleteStaleInline`), часть —
/// бросает. Здесь ошибка всегда возвращается `Err`, а решение «продолжать или
/// нет» принимает `pipeline.rs`, где оно и задокументировано
/// (`pipeline.mjs:322-327`, `pipeline.mjs:301`). Реализация `RealGithubClient`
/// (T11) обязана сохранить именно те места глушения, что были в JS.
pub trait GithubApi: Send + Sync {
    /// Один REST-вызов с ретраями: `gh(path, opts)` (`lib/github.mjs:99-103`).
    ///
    /// Возврат: распарсенный JSON, а для `accept`, содержащего `diff`, — сырой
    /// текст в виде `Value::String` (`lib/github.mjs:83`). Пустое тело — `{}`.
    /// Нужен только `pr-review feedback-batch` для дискавери PR
    /// (`bin/pr-review-feedback-batch.mjs:54-56`); в самом ревью не используется.
    fn gh(&self, path: &str, opts: &GhCallOpts) -> Result<Value>;

    /// PR + его дифф (`lib/github.mjs:105-114`). `None` — нет repo/pr/token.
    fn load_diff(&self) -> Result<Option<LoadedDiff>>;

    /// Футер прошлого sticky целиком (`lib/github.mjs:116-142`).
    /// Основной путь carry-over: `pipeline.mjs:319-327`.
    fn read_prior_sticky_data(&self) -> Result<StickyData>;

    /// Совместимость: только находки из футера (`lib/github.mjs:144-147`).
    ///
    /// В `pipeline.mjs:320-322` это резервная ветка на случай клиента, у которого
    /// нет `readPriorStickyData`. В Rust оба метода обязательны, поэтому ветка
    /// исчезает; метод остаётся ради `feedback`-контура и внешних потребителей.
    fn read_prior_findings(&self) -> Result<Vec<Value>>;

    /// Создать или обновить закреплённый комментарий (`lib/github.mjs:149-160`).
    /// Возврат — `"created"` или `"updated"` (попадает в лог `pipeline.mjs:610`).
    fn upsert_sticky(&self, body: &str) -> Result<String>;

    /// Удалить свои устаревшие инлайны; возврат — сколько удалено
    /// (`lib/github.mjs:162-181`). Треды с ответами не трогаются.
    fn delete_stale_inline(&self, opts: &DeleteStaleOpts) -> Result<usize>;

    /// Запостить review с инлайнами (`lib/github.mjs:183-193`).
    fn post_review(&self, params: &PostReviewParams) -> Result<PostReviewResult>;

    /// Все инлайн-комменты PR (`lib/github.mjs:195-200`); harvest фидбэка
    /// (`lib/feedback.mjs:355`). Элементы — сырой JSON GitHub REST.
    fn list_inline_comments(&self) -> Result<Vec<Value>>;

    /// Метки PR (`lib/github.mjs:202-207`); harvest фидбэка (`lib/feedback.mjs:354`).
    fn list_issue_labels(&self) -> Result<Vec<Value>>;

    /// Последний свой sticky-комментарий целиком (`lib/github.mjs:209-215`).
    /// `None` — sticky ещё нет. Harvest читает у него `body` и `user.login`
    /// (`lib/feedback.mjs:359-366`).
    fn read_sticky_review(&self) -> Result<Option<Value>>;

    /// Реакции 👍/👎 на инлайн-комменте (`lib/github.mjs:264-269`);
    /// harvest фидбэка (`lib/feedback.mjs:333`).
    fn list_pull_comment_reactions(&self, comment_id: u64) -> Result<Vec<Value>>;

    /// Незакрытые review-треды через GraphQL, с пагинацией до 50 страниц
    /// (`lib/github.mjs:223-250`). Пагинация сверх лимита — `Err`, не молчание.
    ///
    /// Вход auto-triage R11 (`lib/triage.mjs:105-108`): там `Err` глушится
    /// fail-open и стадия отдаёт `{resolved: 0, skipped: 0, errors: 1}`.
    fn list_unresolved_review_threads(&self) -> Result<Vec<ThreadRef>>;

    /// Ответ в тред (`lib/github.mjs:252-256`); auto-triage пишет им причину
    /// авто-резолва (`lib/triage.mjs:127`).
    fn reply_review_thread(&self, thread_id: &str, body: &str) -> Result<()>;

    /// Пометить тред resolved (`lib/github.mjs:258-262`, `lib/triage.mjs:128`).
    fn resolve_review_thread(&self, thread_id: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Движок ревью
// ---------------------------------------------------------------------------

/// Контекст прогона моделей — второй аргумент `engine.reviewAllModels(chunks, ctx)`
/// (`pipeline.mjs:409-415`, читается в `lib/engine.mjs:257-285`, `lib/engine.mjs:344-352`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ReviewContext {
    /// Готовые текстовые блоки blast-radius (`formatBlastBlock`, `lib/graphify.mjs:229`).
    pub blast_blocks: Vec<String>,
    pub pr: PullRequestRef,
    /// Находки прошлого прогона ПОСЛЕ инвалидации по отпечаткам (`pipeline.mjs:346`).
    /// Движок сам режет их по составу чанка (`scope_priors`, `engine.rs:57`).
    pub prior_findings: Vec<Value>,
    /// Все пути диффа: карта PR рисуется движком под каждый чанк (`pipeline.mjs:410-412`).
    pub changed_paths: Vec<String>,
    /// Секция prescan для промпта; подмешивается только в ПЕРВЫЙ чанк
    /// (`lib/engine.mjs:264`, `first: ci === 0`).
    pub prescan_section: String,
    /// Дедлайн LLM-фазы, epoch-мс; `0` — бюджет не задан (`pipeline.mjs:364`).
    /// После него чанки помечаются `skipped`, а не ревьюятся.
    pub deadline: u64,
}

/// Контекст verifier Pass B — второй аргумент `engine.verifyFindings(candidates, ctx)`
/// (`pipeline.mjs:139-141`, читается в `lib/verifier.mjs:169-176`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct VerifyContext {
    /// Аннотированный дифф, УЖЕ суженный до файлов кандидатов (`pipeline.mjs:128-131`).
    pub annotated: String,
    /// Содержимое тех же файлов, склеенное через `\n` (`pipeline.mjs:122-126`).
    pub file_contents: String,
    pub blast_blocks: Vec<String>,
    pub pr: PullRequestRef,
    /// Карта изменённых файлов ВСЕГО PR: у verifier-стадии контекст шире чанка
    /// (`pipeline.mjs:410-412`).
    pub changed_files_section: String,
    /// Карточки символов файлов кандидатов (RFC 14, решение 4) — готовые строки
    /// `symbol_cards::select_chunk_cards`; только при `verifier.symbolCards`
    /// (`pipeline.mjs:211-219`). Пусто — поля во входе промпта нет.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbol_cards: Vec<String>,
}

/// Агрегат по одной модели — элемент массива, который отдаёт `reviewAllModels`
/// (`lib/engine.mjs:127-153` `aggregateModel`, декорации — `lib/engine.mjs:340-410`).
///
/// Все поля, кроме `model`, опциональны по сути: JS кладёт либо ветку успеха
/// (`summary`/`findings`; `verdict` из контракта агрегата удалён), либо ветку отказа (`error`). Отсюда `Default` +
/// `skip_serializing_if`: сериализация даёт ту же форму ключей, что JS-объект, и
/// `report.rs`/`export.rs`, читающие `&Value`, работают без адаптера.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelReview {
    /// Имя модели, а в MoA — `moa.label` (`lib/engine.mjs:333`, `lib/engine.mjs:390`).
    pub model: String,
    /// Первое непустое summary среди чанков (`lib/engine.mjs:145-147`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Находки после дедупа ВНУТРИ модели (`lib/engine.mjs:152`).
    pub findings: Vec<Value>,
    /// Модель недоступна. Пайплайн считает такие ревью «не ok»
    /// (`pipeline.mjs:427`, `ok = reviews.filter((r) => !r.error)`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Все части модели пропущены по тайм-бюджету, а не по отказу
    /// (`lib/engine.mjs:135`): такие ревью fallback НЕ вызывают (`lib/engine.mjs:361`).
    #[serde(skip_serializing_if = "is_false")]
    pub skipped: bool,
    /// Сколько чанков пропущено по тайм-бюджету. Пайплайн берёт **max** по моделям,
    /// а не сумму (`pipeline.mjs:417-418`).
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub skipped_chunks: u32,
    /// MoA: реальная модель-жюри (`lib/engine.mjs:334`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregator: Option<String>,
    /// MoA: список advisor-моделей (`lib/engine.mjs:335`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub moa_references: Vec<String>,
    /// Ответы advisors/специалистов: `{role, model, text}` или `{role, model, error}`.
    /// Из них `report.rs:291-360` строит секцию рассуждений.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<Value>,
    /// Резерв сработал вместо этой модели (`lib/engine.mjs:378`); в sticky
    /// печатается «_(резерв вместо …)_» (`report.rs:1222-1223`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_for: Option<String>,
    /// MoA-fallback без жюри специалистов: R5 refutation недоступен
    /// (`lib/engine.mjs:387`).
    #[serde(skip_serializing_if = "is_false")]
    pub jury_bypassed: bool,
    /// Сколько находок понижено до nit как «про файл вне чанка»
    /// (`lib/engine.mjs:98-104`). Живёт на результате чанка, в агрегат не переносится.
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub demoted_out_of_chunk: usize,
    /// Usage последнего LLM-вызова (`{prompt_tokens, completion_tokens, total_tokens}`).
    /// Ставится на результат ЧАНКА (`lib/engine.mjs:253`) и агрегатом отбрасывается;
    /// в поле здесь — ради того, чтобы форма чанк-результата тоже была выразима.
    #[serde(rename = "__usage", skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}

/// Результат `reviewAllModels` — по одному агрегату на модель
/// (в MoA-режиме массив всегда длины 1, `lib/engine.mjs:330-338`).
///
/// Алиас, а не обёртка: пайплайн работает с ним как со списком —
/// `reviews.filter`, `reviews.reduce(max)`, `reviews.map` (`pipeline.mjs:417-436`).
pub type ReviewAllResult = Vec<ModelReview>;

/// Движок ревью: MoA/consensus-прогон и verifier Pass B
/// (`lib/engine.mjs:412` — возвращаемый объект `createReviewEngine`).
///
/// Ключевая семантика, которую обязана сохранить реализация (T12): **ошибки
/// моделей возвращаются результатами, а не `Err`**. На этом стоит вся развилка
/// fail-open/fail-closed в `pipeline.mjs:437-441`: `Err` из `review_all_models`
/// означал бы аварию самого движка, а не недоступность LLM.
pub trait EngineApi: Send + Sync {
    /// Прогон всех моделей по всем чанкам с fallback-каскадом
    /// (`lib/engine.mjs:399-410`).
    fn review_all_models(&self, chunks: &[Chunk], ctx: &ReviewContext) -> Result<ReviewAllResult>;

    /// Verifier Pass B (`lib/verifier.mjs:167-203`): второй проход отдельной моделью.
    ///
    /// Возврат — массив вердиктов `{id|index, verdict, reason, ...}` как их отдала
    /// модель; сопоставление с кандидатами и мутации делает
    /// `verifier::apply_verifier_verdicts`. Пустой `candidates` → пустой ответ без
    /// вызова LLM (`lib/verifier.mjs:168`). Сбой LLM здесь — `Err`, и стадия
    /// глушится fail-open на стороне пайплайна (`pipeline.mjs:154-158`).
    fn verify_findings(&self, candidates: &[Value], ctx: &VerifyContext) -> Result<Vec<Value>>;
}

// ---------------------------------------------------------------------------
// Prescan
// ---------------------------------------------------------------------------

/// Результат одного инструмента prescan — eslint (`lib/prescan.mjs:135`) или
/// ast-grep (`lib/astgrep.mjs:110`, `lib/astgrep.mjs:127`).
///
/// Пайплайн читает у него только `error` (`pipeline.mjs:396-397`), но целиком он
/// нужен `runCombinedPrescan` для склейки и логов.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolPrescanResult {
    pub findings: Vec<Value>,
    pub prompt_section: String,
    pub scanned: usize,
    /// Причина, по которой инструмент не запускался: `"no-eslint"`, `"no-ast-grep"`,
    /// `"no-config"`. Это **строка-тег**, а не счётчик (`lib/prescan-shared.mjs:87-90`
    /// склеивает теги через `+`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// Сколько lintable-файлов было до обрезки капом.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lintable_total: Option<usize>,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Мягкий сбой инструмента: prescan fail-open, прогон продолжается.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Счётчики находок по инструментам (`lib/prescan-runner.mjs:27-30`).
/// Читаются sticky-рендером (`report.rs:1371-1374`) и observability.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct PrescanByTool {
    pub eslint: usize,
    pub astgrep: usize,
}

/// Результат `prescan.run(changedPaths)` (`lib/prescan-runner.mjs:8-32`,
/// метод — `lib/prescan-runner.mjs:35-40`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PrescanResult {
    /// Детерминированные находки eslint + ast-grep; у каждой стоит флаг `prescan`.
    pub findings: Vec<Value>,
    /// Секция для промпта первого чанка (`pipeline.mjs:413`).
    pub prompt_section: String,
    pub scanned: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lintable_total: Option<usize>,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Склейка тегов пропуска обоих инструментов через `+`
    /// (`lib/prescan-shared.mjs:87-90`). `None` — оба отработали.
    ///
    /// Неоднозначность JS: фейк в `test/pr-review.pipeline.test.mjs:47` кладёт сюда
    /// число `0`. Реальные реализации кладут только строку или `undefined`, а
    /// потребители (`obs.logEvent('prescan')`) значение не интерпретируют — поэтому
    /// контракт зафиксирован по реализации, а не по фейку.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// Подрезультат eslint целиком (`lib/prescan-runner.mjs:25`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eslint: Option<ToolPrescanResult>,
    /// Подрезультат ast-grep целиком (`lib/prescan-runner.mjs:26`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub astgrep: Option<ToolPrescanResult>,
    /// `None` — инструменты не отчитались (в JS `byTool` тогда `null`, и sticky
    /// печатает общее число находок вместо разбивки, `report.rs:1371-1374`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_tool: Option<PrescanByTool>,
    /// Склейка ошибок обоих инструментов через `; ` (`lib/prescan-runner.mjs:31`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Детерминированный прескан до LLM (`lib/prescan-runner.mjs:35-40`).
///
/// Контракт fail-open: сбой инструмента приходит полем `error`, а не `Err`.
/// `Err` здесь означает аварию самого раннера — пайплайн такую не глушит.
pub trait PrescanApi: Send + Sync {
    /// `run(changedPaths)`: пути ровно в том виде, в каком они пришли из диффа
    /// (`+++ b/...`), нормализацию делает раннер.
    fn run(&self, changed_paths: &[String]) -> Result<PrescanResult>;
}

// ---------------------------------------------------------------------------
// Graphify / blast-radius
// ---------------------------------------------------------------------------

/// Распределение уверенности рёбер графа (`lib/graphify.mjs:42`, `lib/graphify.mjs:370`).
///
/// Ключи — в ВЕРХНЕМ регистре, как их отдаёт graphify; `unknown` — нижним.
/// `rename_all` здесь неприменим, поэтому каждое поле переименовано явно.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BlastConfidence {
    #[serde(rename = "EXTRACTED")]
    pub extracted: u64,
    #[serde(rename = "INFERRED")]
    pub inferred: u64,
    #[serde(rename = "AMBIGUOUS")]
    pub ambiguous: u64,
    pub unknown: u64,
}

/// Метрики blast-radius (`lib/graphify.mjs:383-397`, пустой случай — `lib/graphify.mjs:33-43`).
///
/// **Ключи snake_case, а не camelCase** — единственное исключение среди типов этого
/// модуля. Так их пишет JS, так их читают потребители артефакта экспорта и
/// observability (`blast_radius` в событии `plan`, `pipeline.mjs:390`), поэтому
/// `rename_all = "camelCase"` тут был бы поломкой контракта, а не унификацией.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BlastMetrics {
    pub symbols_total: usize,
    /// `None` — клиент не отчитался (старый API, `pipeline.mjs:41`).
    pub symbols_resolved: Option<usize>,
    pub affected_nonempty: usize,
    /// Свежесть графа относительно HEAD; `None` — измерить не удалось.
    pub graph_fresh: Option<bool>,
    /// `fresh` / `stale` / `missing` / `unknown`.
    pub graph_status: String,
    /// `"none"`, единственная причина, либо `"mixed"` (`lib/graphify.mjs:391`).
    pub skip_reason: String,
    /// Причина → сколько раз встретилась. Порядок вставки значим: он определяет,
    /// какая причина попадёт в `skip_reason` при единственной причине.
    pub skip_reasons: IndexMap<String, u64>,
    /// Отсортированный список файлов, попавших в радиус.
    pub affected_files: Vec<String>,
    pub confidence: BlastConfidence,
    pub cross_community: u64,
}

/// Результат `buildBlastRadius(symbols)` (`lib/graphify.mjs:382-398`).
///
/// JS-пайплайн дополнительно умеет принять голый массив блоков от старых клиентов
/// (`normalizeBlastRadius`, `pipeline.mjs:35-61`). В Rust этой ветки нет: тип
/// один, и «клиент не отчитался» выражается через `symbols_resolved: None`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BlastRadius {
    /// Готовые текстовые блоки для промпта (`formatBlastBlock`, `lib/graphify.mjs:229-236`).
    pub blocks: Vec<String>,
    pub metrics: BlastMetrics,
    /// Индекс уже прочитанного `graph.json` (`graphIndex`, `lib/graphify.mjs`) — для
    /// карточек символов (RFC 14): второй раз граф с диска не читаем. `None` — обогащение
    /// не загрузилось. В артефакты не сериализуется: это рабочее состояние прогона.
    #[serde(skip)]
    pub graph_index: Option<std::sync::Arc<crate::graphify_client::GraphIndex>>,
}

/// Индекс графа кода и радиус поражения (`lib/graphify.mjs:400`).
pub trait GraphifyApi: Send + Sync {
    /// `buildBlastRadius(symbols)`: символы, затронутые диффом
    /// (`collectBlastSymbols`, `pipeline.mjs:333`).
    ///
    /// Fail-open: отсутствующий/устаревший граф — это пустые `blocks` и метрики с
    /// причиной пропуска, а не `Err` (`lib/graphify.mjs:341-347`).
    fn build_blast_radius(&self, symbols: &[String]) -> Result<BlastRadius>;
}

// ---------------------------------------------------------------------------
// Уведомления
// ---------------------------------------------------------------------------

/// Пинг дежурного в Telegram (`pipeline.mjs:189-205`, дефолт — `pipeline.mjs:238`).
///
/// Границу провели по «отправить», а не по «составить»: текст
/// (`🔍 Авто-ревью PR #…` + счётчики + причина + ссылка, `pipeline.mjs:191-194`)
/// собирает `pipeline.rs` детерминированно, а трейт отвечает только за доставку.
/// Так golden-тест проверяет ровно то, что увидит дежурный, а не промежуточную
/// структуру; в JS-тесте (`test/pr-review.pipeline.test.mjs:66-72`) для этого
/// приходилось лезть в третий аргумент замыкания.
///
/// Fail-open: реализация обязана глушить сетевой сбой сама и возвращать `Ok`
/// (`pipeline.mjs:203-204` — «telegram ping failed (не критично)»).
pub trait Notifier: Send + Sync {
    fn notify(&self, text: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Сборка зависимостей
// ---------------------------------------------------------------------------

/// Инжектируемые зависимости пайплайна — порт объекта `deps` (`pipeline.mjs:226-238`).
///
/// **Заимствования, а не `Box<dyn …>`** — сознательно:
///
/// - `RealEngine` держит внутри LLM-клиент и адаптивные семафоры, а `RealGithubClient` —
///   HTTP-клиент; всё это создаётся один раз в `cli::review::run` и живёт там на стеке.
///   Владение через `Box` заставило бы либо клонировать состояние, либо заворачивать
///   его в `Arc` без нужды;
/// - тесту и golden-harness нужен доступ к КОНКРЕТНОМУ фейку ПОСЛЕ прогона — прочитать
///   журнал вызовов. С `Box<dyn …>` фейк уезжает внутрь `PipelineDeps`, и журнал
///   пришлось бы доставать через `Arc` в обход трейта;
/// - структура из одних ссылок получается `Copy`, поэтому её дёшево передавать
///   в scoped-потоки и вложенные стадии.
///
/// Цена решения: время жизни `'a` протекает в сигнатуру `pipeline::run`. Это
/// приемлемо — `run` вызывается ровно из одного места.
#[derive(Clone, Copy)]
pub struct PipelineDeps<'a> {
    pub github: &'a dyn GithubApi,
    pub engine: &'a dyn EngineApi,
    pub prescan: &'a dyn PrescanApi,
    pub graphify: &'a dyn GraphifyApi,
    /// `None` — телеграм не сконфигурирован; стадия уведомления пропускается целиком
    /// (в JS это ранний `return` внутри `notifyTelegram`, `pipeline.mjs:190`).
    pub notifier: Option<&'a dyn Notifier>,
    /// Логгер прогона (`deps.log ?? createLogger()`, `pipeline.mjs:227`).
    /// `Send + Sync`, потому что в лог пишут и стадии, идущие в scoped-потоках.
    pub log: &'a (dyn Fn(&str) + Send + Sync),
}

impl<'a> PipelineDeps<'a> {
    /// Записать строку в лог прогона.
    pub fn log(&self, message: &str) {
        (self.log)(message);
    }
}
