//! Фейковые реализации DI-контракта из `crate::traits` — порт `mkDeps`
//! (`test/pr-review.pipeline.test.mjs:41-88`).
//!
//! Модуль **публичный, а не `#[cfg(test)]`**: те же фейки нужны трём разным
//! потребителям — юнит-тестам `pipeline.rs` (T26), golden-harness (T25) и
//! интеграционным тестам в `rust/tests/`, которые видят крейт как внешнюю
//! библиотеку и до `#[cfg(test)]`-модулей не дотягиваются.
//!
//! Что перенесено из JS дословно:
//!
//! - **общий журнал в порядке вызовов.** В `mkDeps` объект `calls` один на все
//!   зависимости, и тесты проверяют по нему порядок `post → delete → sticky`
//!   (post-then-delete, `pipeline.mjs:171-180`). Здесь ту же роль играет
//!   [`CallLog`], который фейки держат через `Arc` — поэтому порядок между
//!   РАЗНЫМИ фейками (github и notifier) наблюдаем так же, как в JS;
//! - **дефолты ответов.** `postReview` отдаёт `555`, `upsertSticky` — `'created'`,
//!   `deleteStaleInline` — `0`, `readPriorStickyData` — пустой объект формы
//!   `{findings: [], runs: [], sha: ''}` (эта форма в JS-тесте отмечена как
//!   регрессия ревью #534: первый прогон PR не должен падать на `.findings`).
//!
//! Чего в JS не было и что добавлено осознанно: инъекция ошибок ([`ErrorHook`]).
//! В JS отказ подмешивали переписыванием метода через `mutateDeps`
//! (`test/pr-review.pipeline.test.mjs:75`), в Rust трейт так не переопределить.
//! Хук — одна точка на фейк: он видит уже записанную в журнал запись и решает,
//! вернуть ли `Err`. Это позволяет проверять fail-open ветки пайплайна
//! (`pipeline.mjs:322-327` prior sticky, `pipeline.mjs:501-506` постинг,
//! `lib/triage.mjs:106` auto-triage), не заводя по флагу на метод.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::diff::Chunk;
use crate::traits::{
    BlastRadius, DeleteStaleOpts, EngineApi, GhCallOpts, GithubApi, GraphifyApi, InlineComment,
    LoadedDiff, Notifier, PipelineDeps, PostReviewParams, PostReviewResult, PrescanApi,
    PrescanResult, ReviewAllResult, ReviewContext, StickyData, ThreadRef, VerifyContext,
};

// ---------------------------------------------------------------------------
// Журнал вызовов
// ---------------------------------------------------------------------------

/// Одна запись журнала — вызов метода любой из зависимостей.
///
/// Поля хранятся значениями, а не ссылками на аргументы: журнал переживает
/// прогон, и после `run()` тест читает его уже без вызывающего кадра стека.
///
/// В варианты попали только `Eq`-типы (строки, числа, [`InlineComment`]), чтобы
/// запись целиком сравнивалась с ожидаемой в `assert_eq!`. Аргументы, у которых
/// формы `serde_json::Value` (контекст движка, опции REST-вызова), в запись не
/// кладутся — их фейки складывают отдельными полями (`FakeEngine::review_contexts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallRecord {
    /// `gh(path, opts)` — `path` и `opts.method` (пусто → `GET`).
    Gh { path: String, method: String },
    /// `loadDiff()`.
    LoadDiff,
    /// `readPriorStickyData()`.
    ReadPriorStickyData,
    /// `readPriorFindings()`.
    ReadPriorFindings,
    /// `upsertSticky(body)`.
    UpsertSticky { body: String },
    /// `deleteStaleInline({exceptReviewId})`.
    DeleteStaleInline { except_review_id: Option<u64> },
    /// `postReview({inline, headSha})`.
    PostReview { inline: Vec<InlineComment>, head_sha: String },
    /// `listInlineComments()`.
    ListInlineComments,
    /// `listIssueLabels()`.
    ListIssueLabels,
    /// `readStickyReview()`.
    ReadStickyReview,
    /// `listPullCommentReactions(commentId)`.
    ListPullCommentReactions { comment_id: u64 },
    /// `listUnresolvedReviewThreads()`.
    ListUnresolvedReviewThreads,
    /// `replyReviewThread(threadId, body)`.
    ReplyReviewThread { thread_id: String, body: String },
    /// `resolveReviewThread(threadId)`.
    ResolveReviewThread { thread_id: String },
    /// `engine.reviewAllModels(chunks, ctx)` — число чанков; сам контекст
    /// складывается в `FakeEngine::review_contexts`.
    ReviewAllModels { chunks: usize },
    /// `engine.verifyFindings(candidates, ctx)` — число кандидатов.
    VerifyFindings { candidates: usize },
    /// `prescan.run(changedPaths)`.
    PrescanRun { changed_paths: Vec<String> },
    /// `graphify.buildBlastRadius(symbols)`.
    BuildBlastRadius { symbols: Vec<String> },
    /// Отправка уведомления дежурному.
    Notify { text: String },
}

impl CallRecord {
    /// Короткий тег записи — то, что JS-тест клал в `calls.order`
    /// (`test/pr-review.pipeline.test.mjs:56-64`: `post`, `delete`, `sticky`,
    /// `telegram`). Остальные теги подобраны по тому же принципу «глагол без
    /// аргументов» и используются в утверждениях о порядке стадий.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Gh { .. } => "gh",
            Self::LoadDiff => "loadDiff",
            Self::ReadPriorStickyData => "priorSticky",
            Self::ReadPriorFindings => "priorFindings",
            Self::UpsertSticky { .. } => "sticky",
            Self::DeleteStaleInline { .. } => "delete",
            Self::PostReview { .. } => "post",
            Self::ListInlineComments => "inlineComments",
            Self::ListIssueLabels => "issueLabels",
            Self::ReadStickyReview => "stickyReview",
            Self::ListPullCommentReactions { .. } => "reactions",
            Self::ListUnresolvedReviewThreads => "threads",
            Self::ReplyReviewThread { .. } => "reply",
            Self::ResolveReviewThread { .. } => "resolve",
            Self::ReviewAllModels { .. } => "review",
            Self::VerifyFindings { .. } => "verify",
            Self::PrescanRun { .. } => "prescan",
            Self::BuildBlastRadius { .. } => "blast",
            Self::Notify { .. } => "telegram",
        }
    }
}

/// Общий журнал вызовов всех фейков одного прогона — порт объекта `calls`
/// из `mkDeps` (`test/pr-review.pipeline.test.mjs:42`).
///
/// Один на набор зависимостей и разделяется через `Arc`, поэтому порядок
/// наблюдаем сквозь границы фейков; это и есть та величина, которую проверяет
/// тест post-then-delete.
#[derive(Debug, Default)]
pub struct CallLog {
    /// Записи в порядке вызова.
    pub calls: Mutex<Vec<CallRecord>>,
}

impl CallLog {
    /// Пустой журнал.
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавить запись в конец.
    ///
    /// Отравленный `Mutex` (паника в другом потоке) здесь не считается ошибкой
    /// теста: журнал — диагностика, и его недоступность не должна маскировать
    /// исходную панику. Поэтому `into_inner` вместо `unwrap`.
    pub fn record(&self, rec: CallRecord) {
        match self.calls.lock() {
            Ok(mut v) => v.push(rec),
            Err(poisoned) => poisoned.into_inner().push(rec),
        }
    }

    /// Копия журнала целиком.
    pub fn records(&self) -> Vec<CallRecord> {
        match self.calls.lock() {
            Ok(v) => v.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Теги записей в порядке вызова — аналог `calls.order`.
    pub fn tags(&self) -> Vec<&'static str> {
        self.records().iter().map(CallRecord::tag).collect()
    }

    /// Сколько раз встретился тег.
    pub fn count(&self, tag: &str) -> usize {
        self.records().iter().filter(|r| r.tag() == tag).count()
    }

    /// Тела всех `upsertSticky` — аналог `calls.upsertSticky`.
    pub fn sticky_bodies(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter_map(|r| match r {
                CallRecord::UpsertSticky { body } => Some(body),
                _ => None,
            })
            .collect()
    }

    /// Аргументы всех `postReview` — аналог `calls.postReview`.
    pub fn posted_reviews(&self) -> Vec<(Vec<InlineComment>, String)> {
        self.records()
            .into_iter()
            .filter_map(|r| match r {
                CallRecord::PostReview { inline, head_sha } => Some((inline, head_sha)),
                _ => None,
            })
            .collect()
    }

    /// Аргументы всех `deleteStaleInline` — аналог `calls.deleteStaleInline`.
    pub fn delete_stale_opts(&self) -> Vec<Option<u64>> {
        self.records()
            .into_iter()
            .filter_map(|r| match r {
                CallRecord::DeleteStaleInline { except_review_id } => Some(except_review_id),
                _ => None,
            })
            .collect()
    }

    /// Тексты всех уведомлений — аналог `calls.telegram`.
    pub fn notifications(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter_map(|r| match r {
                CallRecord::Notify { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    /// Очистить журнал (для тестов, гоняющих несколько прогонов подряд).
    pub fn clear(&self) {
        match self.calls.lock() {
            Ok(mut v) => v.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
    }
}

/// Инъекция отказов: видит уже записанный вызов и решает, вернуть ли `Err`.
///
/// `Some(msg)` → метод отдаёт `Err(anyhow!(msg))`, `None` → штатный ответ.
/// Хук вызывается ПОСЛЕ записи в журнал: тест должен видеть, что метод дёрнули,
/// даже если он упал.
pub type ErrorHook = Box<dyn Fn(&CallRecord) -> Option<String> + Send + Sync>;

/// Общая часть каждого фейка: журнал + необязательный хук отказов.
///
/// Вынесена в отдельный тип, а не продублирована пять раз, потому что правило
/// «сначала записать, потом решить про ошибку» обязано быть одинаковым у всех
/// фейков — иначе тесты порядка вызовов начнут зависеть от того, какой фейк
/// упал.
#[derive(Default)]
pub struct FakeCore {
    /// Журнал; по умолчанию собственный, через `with_log` — общий с другими фейками.
    pub calls: Arc<CallLog>,
    fail: Option<ErrorHook>,
}

impl FakeCore {
    /// Фейк с собственным журналом.
    pub fn new() -> Self {
        Self::default()
    }

    /// Подключить общий журнал набора.
    pub fn with_log(log: Arc<CallLog>) -> Self {
        Self { calls: log, fail: None }
    }

    /// Установить хук отказов.
    pub fn failing(
        mut self,
        hook: impl Fn(&CallRecord) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.fail = Some(Box::new(hook));
        self
    }

    /// Записать вызов и применить хук отказов.
    pub fn check(&self, rec: CallRecord) -> Result<()> {
        self.calls.record(rec.clone());
        if let Some(hook) = &self.fail {
            if let Some(msg) = hook(&rec) {
                return Err(anyhow!(msg));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

/// Фейк [`GithubApi`]: ответы задаются полями, вызовы пишутся в общий журнал.
///
/// Дефолты повторяют `mkDeps` (`test/pr-review.pipeline.test.mjs:53-63`), чтобы
/// портированный тест собирался без настройки.
pub struct FakeGithub {
    /// Журнал + хук отказов.
    pub core: FakeCore,
    /// Ответ `gh(path, opts)`; по умолчанию `{}` — как пустое тело в JS.
    pub gh_response: Value,
    /// Ответ `loadDiff()`; `None` — «нет repo/pr/token», пайплайн мягко выходит.
    pub loaded_diff: Option<LoadedDiff>,
    /// Ответ `readPriorStickyData()`; дефолт — пустая форма (регрессия ревью #534).
    pub prior_sticky: StickyData,
    /// Ответ `upsertSticky(body)`; дефолт — `"created"`.
    pub upsert_result: String,
    /// Ответ `deleteStaleInline(...)` — сколько инлайнов «удалено»; дефолт `0`.
    pub deleted_inline: usize,
    /// Ответ `postReview(...)`; дефолт `Some(555)` — id из JS-фейка.
    /// `None` заставляет пайплайн пропустить чистку старых инлайнов
    /// (`pipeline.mjs:176-180`).
    pub post_review_id: Option<u64>,
    /// Ответ `listInlineComments()`.
    pub inline_comments: Vec<Value>,
    /// Ответ `listIssueLabels()`.
    pub issue_labels: Vec<Value>,
    /// Ответ `readStickyReview()`.
    pub sticky_review: Option<Value>,
    /// Ответ `listPullCommentReactions(id)`.
    pub reactions: Vec<Value>,
    /// Ответ `listUnresolvedReviewThreads()` — вход auto-triage.
    pub threads: Vec<ThreadRef>,
}

impl Default for FakeGithub {
    fn default() -> Self {
        Self {
            core: FakeCore::new(),
            gh_response: Value::Object(Default::default()),
            loaded_diff: None,
            prior_sticky: StickyData::default(),
            upsert_result: "created".to_string(),
            deleted_inline: 0,
            post_review_id: Some(555),
            inline_comments: Vec::new(),
            issue_labels: Vec::new(),
            sticky_review: None,
            reactions: Vec::new(),
            threads: Vec::new(),
        }
    }
}

impl FakeGithub {
    /// Фейк с дефолтами `mkDeps` и собственным журналом.
    pub fn new() -> Self {
        Self::default()
    }

    /// Тот же фейк, но пишущий в общий журнал набора.
    pub fn with_log(log: Arc<CallLog>) -> Self {
        Self { core: FakeCore::with_log(log), ..Self::default() }
    }

    /// Установить хук отказов (см. [`ErrorHook`]).
    pub fn failing(
        mut self,
        hook: impl Fn(&CallRecord) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.core = self.core.failing(hook);
        self
    }

    /// Ответ `loadDiff()` — PR с готовым телом диффа.
    pub fn with_loaded_diff(mut self, loaded: LoadedDiff) -> Self {
        self.loaded_diff = Some(loaded);
        self
    }

    /// Ответ `readPriorStickyData()` — прошлый sticky для проверки carry-over.
    pub fn with_prior_sticky(mut self, sticky: StickyData) -> Self {
        self.prior_sticky = sticky;
        self
    }

    /// Ответ `listUnresolvedReviewThreads()` — вход auto-triage.
    pub fn with_threads(mut self, threads: Vec<ThreadRef>) -> Self {
        self.threads = threads;
        self
    }
}

impl GithubApi for FakeGithub {
    fn gh(&self, path: &str, opts: &GhCallOpts) -> Result<Value> {
        self.core.check(CallRecord::Gh {
            path: path.to_string(),
            method: if opts.method.is_empty() { "GET".to_string() } else { opts.method.clone() },
        })?;
        Ok(self.gh_response.clone())
    }

    fn load_diff(&self) -> Result<Option<LoadedDiff>> {
        self.core.check(CallRecord::LoadDiff)?;
        Ok(self.loaded_diff.clone())
    }

    fn read_prior_sticky_data(&self) -> Result<StickyData> {
        self.core.check(CallRecord::ReadPriorStickyData)?;
        Ok(self.prior_sticky.clone())
    }

    fn read_prior_findings(&self) -> Result<Vec<Value>> {
        self.core.check(CallRecord::ReadPriorFindings)?;
        Ok(self.prior_sticky.findings.clone())
    }

    fn upsert_sticky(&self, body: &str) -> Result<String> {
        self.core.check(CallRecord::UpsertSticky { body: body.to_string() })?;
        Ok(self.upsert_result.clone())
    }

    fn delete_stale_inline(&self, opts: &DeleteStaleOpts) -> Result<usize> {
        self.core
            .check(CallRecord::DeleteStaleInline { except_review_id: opts.except_review_id })?;
        Ok(self.deleted_inline)
    }

    fn post_review(&self, params: &PostReviewParams) -> Result<PostReviewResult> {
        self.core.check(CallRecord::PostReview {
            inline: params.inline.clone(),
            head_sha: params.head_sha.clone(),
        })?;
        Ok(PostReviewResult { review_id: self.post_review_id })
    }

    fn list_inline_comments(&self) -> Result<Vec<Value>> {
        self.core.check(CallRecord::ListInlineComments)?;
        Ok(self.inline_comments.clone())
    }

    fn list_issue_labels(&self) -> Result<Vec<Value>> {
        self.core.check(CallRecord::ListIssueLabels)?;
        Ok(self.issue_labels.clone())
    }

    fn read_sticky_review(&self) -> Result<Option<Value>> {
        self.core.check(CallRecord::ReadStickyReview)?;
        Ok(self.sticky_review.clone())
    }

    fn list_pull_comment_reactions(&self, comment_id: u64) -> Result<Vec<Value>> {
        self.core.check(CallRecord::ListPullCommentReactions { comment_id })?;
        Ok(self.reactions.clone())
    }

    fn list_unresolved_review_threads(&self) -> Result<Vec<ThreadRef>> {
        self.core.check(CallRecord::ListUnresolvedReviewThreads)?;
        Ok(self.threads.clone())
    }

    fn reply_review_thread(&self, thread_id: &str, body: &str) -> Result<()> {
        self.core.check(CallRecord::ReplyReviewThread {
            thread_id: thread_id.to_string(),
            body: body.to_string(),
        })
    }

    fn resolve_review_thread(&self, thread_id: &str) -> Result<()> {
        self.core.check(CallRecord::ResolveReviewThread { thread_id: thread_id.to_string() })
    }
}

// ---------------------------------------------------------------------------
// Движок
// ---------------------------------------------------------------------------

/// Фейк [`EngineApi`] — порт `deps.engine` (`test/pr-review.pipeline.test.mjs:49-52`).
///
/// Контексты стадий складываются целиком: golden-harness сверяет по ним, что
/// пайплайн собрал промпт-вход именно из того, что обещает контракт (например,
/// что `prescanSection` ушёл, а `changedPaths` — не готовая секция).
#[derive(Default)]
pub struct FakeEngine {
    /// Журнал + хук отказов.
    pub core: FakeCore,
    /// Ответ `reviewAllModels`. Пустой список означает «моделей-ок нет» —
    /// ветка LLM-аутеджа (`pipeline.mjs:437-441`).
    pub reviews: ReviewAllResult,
    /// Ответ `verifyFindings` — вердикты как их отдала бы модель.
    pub verdicts: Vec<Value>,
    /// Контексты всех вызовов `reviewAllModels` в порядке вызова.
    pub review_contexts: Mutex<Vec<ReviewContext>>,
    /// Контексты всех вызовов `verifyFindings` в порядке вызова.
    pub verify_contexts: Mutex<Vec<VerifyContext>>,
}

impl FakeEngine {
    /// Фейк без ответов: ноль ревью, ноль вердиктов.
    pub fn new() -> Self {
        Self::default()
    }

    /// Тот же фейк, но пишущий в общий журнал набора.
    pub fn with_log(log: Arc<CallLog>) -> Self {
        Self { core: FakeCore::with_log(log), ..Self::default() }
    }

    /// Установить хук отказов (см. [`ErrorHook`]).
    pub fn failing(
        mut self,
        hook: impl Fn(&CallRecord) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.core = self.core.failing(hook);
        self
    }

    /// Ответ `reviewAllModels`.
    pub fn with_reviews(mut self, reviews: ReviewAllResult) -> Self {
        self.reviews = reviews;
        self
    }

    /// Ответ `verifyFindings`.
    pub fn with_verdicts(mut self, verdicts: Vec<Value>) -> Self {
        self.verdicts = verdicts;
        self
    }
}

impl EngineApi for FakeEngine {
    fn review_all_models(&self, chunks: &[Chunk], ctx: &ReviewContext) -> Result<ReviewAllResult> {
        self.core.check(CallRecord::ReviewAllModels { chunks: chunks.len() })?;
        push(&self.review_contexts, ctx.clone());
        Ok(self.reviews.clone())
    }

    fn verify_findings(&self, candidates: &[Value], ctx: &VerifyContext) -> Result<Vec<Value>> {
        self.core.check(CallRecord::VerifyFindings { candidates: candidates.len() })?;
        push(&self.verify_contexts, ctx.clone());
        // Пустой список кандидатов LLM не зовёт (`lib/verifier.mjs:168`) —
        // повторяем это и в фейке, чтобы тест не увидел вердиктов из ниоткуда.
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.verdicts.clone())
    }
}

// ---------------------------------------------------------------------------
// Prescan и graphify
// ---------------------------------------------------------------------------

/// Фейк [`PrescanApi`] — порт `deps.prescan` (`test/pr-review.pipeline.test.mjs:44-48`).
#[derive(Default)]
pub struct FakePrescan {
    /// Журнал + хук отказов.
    pub core: FakeCore,
    /// Ответ `run(changedPaths)`; дефолт — пустой прескан.
    pub result: PrescanResult,
}

impl FakePrescan {
    /// Пустой прескан: ни находок, ни секции промпта.
    pub fn new() -> Self {
        Self::default()
    }

    /// Тот же фейк, но пишущий в общий журнал набора.
    pub fn with_log(log: Arc<CallLog>) -> Self {
        Self { core: FakeCore::with_log(log), ..Self::default() }
    }

    /// Установить хук отказов (см. [`ErrorHook`]).
    pub fn failing(
        mut self,
        hook: impl Fn(&CallRecord) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.core = self.core.failing(hook);
        self
    }

    /// Ответ `run` целиком.
    pub fn with_result(mut self, result: PrescanResult) -> Self {
        self.result = result;
        self
    }

    /// Ответ `run` только по находкам — как `prescanFindings` в JS-хелпере.
    pub fn with_findings(mut self, findings: Vec<Value>) -> Self {
        self.result.findings = findings;
        self
    }
}

impl PrescanApi for FakePrescan {
    fn run(&self, changed_paths: &[String]) -> Result<PrescanResult> {
        self.core.check(CallRecord::PrescanRun { changed_paths: changed_paths.to_vec() })?;
        Ok(self.result.clone())
    }
}

/// Фейк [`GraphifyApi`] — порт `deps.graphify` (дефолт JS-хелпера отдавал `[]`,
/// `test/pr-review.pipeline.test.mjs:41`).
#[derive(Default)]
pub struct FakeGraphify {
    /// Журнал + хук отказов.
    pub core: FakeCore,
    /// Ответ `buildBlastRadius(symbols)`; дефолт — пустой радиус.
    pub radius: BlastRadius,
}

impl FakeGraphify {
    /// Пустой blast-radius.
    pub fn new() -> Self {
        Self::default()
    }

    /// Тот же фейк, но пишущий в общий журнал набора.
    pub fn with_log(log: Arc<CallLog>) -> Self {
        Self { core: FakeCore::with_log(log), ..Self::default() }
    }

    /// Установить хук отказов (см. [`ErrorHook`]).
    pub fn failing(
        mut self,
        hook: impl Fn(&CallRecord) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.core = self.core.failing(hook);
        self
    }

    /// Ответ `buildBlastRadius`.
    pub fn with_radius(mut self, radius: BlastRadius) -> Self {
        self.radius = radius;
        self
    }
}

impl GraphifyApi for FakeGraphify {
    fn build_blast_radius(&self, symbols: &[String]) -> Result<BlastRadius> {
        self.core.check(CallRecord::BuildBlastRadius { symbols: symbols.to_vec() })?;
        Ok(self.radius.clone())
    }
}

// ---------------------------------------------------------------------------
// Уведомления
// ---------------------------------------------------------------------------

/// Фейк [`Notifier`]: тексты уведомлений видны через журнал
/// ([`CallLog::notifications`]) — так же, как `calls.telegram` в JS-тесте.
#[derive(Default)]
pub struct FakeNotifier {
    /// Журнал + хук отказов.
    pub core: FakeCore,
}

impl FakeNotifier {
    /// Фейк с собственным журналом.
    pub fn new() -> Self {
        Self::default()
    }

    /// Тот же фейк, но пишущий в общий журнал набора.
    pub fn with_log(log: Arc<CallLog>) -> Self {
        Self { core: FakeCore::with_log(log) }
    }

    /// Установить хук отказов (см. [`ErrorHook`]).
    ///
    /// Ошибку отсюда пайплайн НЕ ждёт: реальный нотификатор глушит сеть сам
    /// (`pipeline.mjs:203-204`). Хук нужен, чтобы проверить именно это.
    pub fn failing(
        mut self,
        hook: impl Fn(&CallRecord) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.core = self.core.failing(hook);
        self
    }
}

impl Notifier for FakeNotifier {
    fn notify(&self, text: &str) -> Result<()> {
        self.core.check(CallRecord::Notify { text: text.to_string() })
    }
}

// ---------------------------------------------------------------------------
// Минимальный набор зависимостей
// ---------------------------------------------------------------------------

/// Готовый набор фейков с ОДНИМ общим журналом — порт возвращаемого значения
/// `mkDeps` (`{deps, calls}`).
///
/// Владеет фейками, поэтому [`PipelineDeps`] с его заимствованиями строится
/// одной строкой, а после прогона тест читает и журнал, и конкретный фейк:
///
/// ```
/// use ai_reviewer::testutil::FakeDeps;
///
/// let fakes = FakeDeps::new();
/// let deps = fakes.deps();
/// deps.log("прогон начался");
/// assert!(fakes.calls.records().is_empty());
/// assert_eq!(fakes.logs(), vec!["прогон начался".to_string()]);
/// ```
pub struct FakeDeps {
    /// Общий журнал всех фейков набора.
    pub calls: Arc<CallLog>,
    /// Фейк GitHub.
    pub github: FakeGithub,
    /// Фейк движка.
    pub engine: FakeEngine,
    /// Фейк прескана.
    pub prescan: FakePrescan,
    /// Фейк graphify.
    pub graphify: FakeGraphify,
    /// Фейк уведомлений. В [`FakeDeps::deps`] он подключён всегда: «телеграм не
    /// настроен» проверяется отдельно, через [`FakeDeps::deps_without_notifier`].
    pub notifier: FakeNotifier,
    /// Строки, записанные в лог прогона.
    log_lines: Arc<Mutex<Vec<String>>>,
    /// Логгер, который отдаётся в `PipelineDeps::log`; пишет в `log_lines`.
    log_fn: Box<dyn Fn(&str) + Send + Sync>,
}

impl Default for FakeDeps {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDeps {
    /// Минимальный набор: все пять фейков на дефолтах и общий журнал.
    pub fn new() -> Self {
        let calls = Arc::new(CallLog::new());
        let log_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log_lines);
        Self {
            github: FakeGithub::with_log(Arc::clone(&calls)),
            engine: FakeEngine::with_log(Arc::clone(&calls)),
            prescan: FakePrescan::with_log(Arc::clone(&calls)),
            graphify: FakeGraphify::with_log(Arc::clone(&calls)),
            notifier: FakeNotifier::with_log(Arc::clone(&calls)),
            calls,
            log_lines,
            log_fn: Box::new(move |line: &str| push(&sink, line.to_string())),
        }
    }

    /// Заимствования для `pipeline::run`.
    pub fn deps(&self) -> PipelineDeps<'_> {
        PipelineDeps {
            github: &self.github,
            engine: &self.engine,
            prescan: &self.prescan,
            graphify: &self.graphify,
            notifier: Some(&self.notifier),
            log: &*self.log_fn,
        }
    }

    /// То же, но без уведомлений: «телеграм не сконфигурирован»
    /// (ранний `return` в `pipeline.mjs:190`).
    pub fn deps_without_notifier(&self) -> PipelineDeps<'_> {
        PipelineDeps { notifier: None, ..self.deps() }
    }

    /// Копия строк лога прогона в порядке записи.
    pub fn logs(&self) -> Vec<String> {
        match self.log_lines.lock() {
            Ok(v) => v.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// Дописать значение в `Mutex<Vec<_>>`, переживая отравление мьютекса.
///
/// Отравление означает панику в другом потоке; ронять на нём ещё и запись
/// диагностики бессмысленно — исходную панику это только замаскирует.
fn push<T>(slot: &Mutex<Vec<T>>, value: T) {
    match slot.lock() {
        Ok(mut v) => v.push(value),
        Err(poisoned) => poisoned.into_inner().push(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_github_records_calls_in_order() {
        let fakes = FakeDeps::new();
        let gh = &fakes.github;

        // Порядок стадии публикации: post-then-delete → sticky (`pipeline.mjs:171-186`).
        let posted = gh
            .post_review(&PostReviewParams {
                inline: vec![InlineComment::new("foo.ts", 3, "тело")],
                head_sha: "deadbeef".to_string(),
            })
            .unwrap();
        assert_eq!(posted.review_id, Some(555), "дефолт id повторяет JS-фейк");
        gh.delete_stale_inline(&DeleteStaleOpts { except_review_id: posted.review_id }).unwrap();
        gh.upsert_sticky("свод").unwrap();
        fakes.notifier.notify("🔍 Авто-ревью PR #1").unwrap();

        assert_eq!(fakes.calls.tags(), vec!["post", "delete", "sticky", "telegram"]);
        assert_eq!(fakes.calls.posted_reviews()[0].1, "deadbeef");
        assert_eq!(fakes.calls.delete_stale_opts(), vec![Some(555)]);
        assert_eq!(fakes.calls.sticky_bodies(), vec!["свод".to_string()]);
        assert_eq!(fakes.calls.notifications(), vec!["🔍 Авто-ревью PR #1".to_string()]);
        assert_eq!(fakes.calls.count("post"), 1);
    }

    #[test]
    fn shared_log_keeps_order_across_fakes() {
        let fakes = FakeDeps::new();
        fakes.prescan.run(&["foo.ts".to_string()]).unwrap();
        fakes.graphify.build_blast_radius(&["foo".to_string()]).unwrap();
        fakes.github.read_prior_sticky_data().unwrap();
        fakes.engine.review_all_models(&[], &ReviewContext::default()).unwrap();

        assert_eq!(fakes.calls.tags(), vec!["prescan", "blast", "priorSticky", "review"]);
        assert_eq!(
            fakes.calls.records()[0],
            CallRecord::PrescanRun { changed_paths: vec!["foo.ts".to_string()] }
        );
        assert_eq!(fakes.engine.review_contexts.lock().unwrap().len(), 1);
    }

    #[test]
    fn error_hook_fires_after_recording() {
        let fakes = FakeDeps::new();
        let gh = FakeGithub::with_log(Arc::clone(&fakes.calls))
            .failing(|rec| matches!(rec, CallRecord::UpsertSticky { .. }).then(|| "boom".into()));

        assert_eq!(gh.delete_stale_inline(&DeleteStaleOpts::default()).unwrap(), 0);
        let err = gh.upsert_sticky("свод").unwrap_err();

        assert_eq!(err.to_string(), "boom");
        // Упавший вызов всё равно в журнале: тест обязан видеть, что метод дёрнули.
        assert_eq!(fakes.calls.tags(), vec!["delete", "sticky"]);
    }

    #[test]
    fn verify_findings_skips_llm_on_empty_candidates() {
        let fakes = FakeDeps::new();
        let engine = FakeEngine::with_log(Arc::clone(&fakes.calls))
            .with_verdicts(vec![serde_json::json!({"index": 0, "verdict": "confirmed"})]);

        assert!(engine.verify_findings(&[], &VerifyContext::default()).unwrap().is_empty());
        let one = vec![serde_json::json!({"file": "foo.ts"})];
        assert_eq!(engine.verify_findings(&one, &VerifyContext::default()).unwrap().len(), 1);
        assert_eq!(fakes.calls.tags(), vec!["verify", "verify"]);
    }
}
