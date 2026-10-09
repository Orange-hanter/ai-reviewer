//! Порт чистых частей lib/engine.mjs (Фаза 3, шаг 2): скоуп prior findings
//! по чанку, демоут находок про невидимые чанку файлы, агрегация ответов
//! модели по чанкам (дедуп внутри модели).
//!
//! Оркестрация (параллелизм моделей×чанков, дедлайн-бюджет, fallback, MoA)
//! на время JSON-моста оставалась в обёртке lib/engine.mjs: бинарь-мост
//! одноразовый, у него нет ни общих часов с оркестратором, ни DI-точек, куда
//! тесты подменяют ollama/prompt. С задачей T12 (RFC 16) она портирована сюда
//! целиком — [`RealEngine`], реализация [`EngineApi`]: в едином бинаре
//! оркестратор и LLM-клиент живут в одном процессе, подменяется не «ollama»,
//! а трейт [`LlmApi`], а часы инжектятся ([`Clock`]) ради golden-прогонов.
//! Мост (`src/bin/engine.rs`) до cutover продолжает звать только чистые
//! функции ниже — [`scope_priors`], [`demote_out_of_chunk`], [`aggregate_model`].
//!
//! Паритет с JS, включая:
//! - chunkScope: состав чанка полон только если paths непуст И нет truthy
//!   unknownPaths; скоуп — Set исходных JSON-значений (строгое равенство);
//! - демоут: только findings с truthy file вне скоупа и severity !== 'nit';
//!   confidence = min(confidence ?? 0.5, 0.5); chunkIndex/demotedOutOfChunk
//!   дописываются ТОЛЬКО когда демоутилось > 0;
//! - aggregateModel: summary — первое truthy; все части с ошибкой →
//!   error parts[0] || 'нет ответа', skipped только если ВСЕ части skipped;
//!   дедуп — findings::dedupe_same_model_findings (копии, как {...f}).
//!   Окончательный выбор summary (из чанка с кодом) делает JS-обёртка
//!   pickAggregateSummary поверх обеих веток: ей нужна маршрутизация чанков,
//!   которая живёт только в lib/chunk-routing.mjs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::concurrency::run_limited;
use crate::diff::Chunk;
use crate::findings::{
    dedupe_same_model_findings, js_to_number, js_to_string, js_truthy, normalize_finding,
};
use crate::llm::{ChatJsonOpts, ChatTextOpts, LlmApi, UsageCallback};
use crate::profile::Profile;
use crate::prompt::{self, PromptCtx};
use crate::specialists::merge_specialist_references;
use crate::traits::{EngineApi, ModelReview, ReviewAllResult, ReviewContext, VerifyContext};
use crate::util::redact_secrets_for_log;
use crate::verifier::VerifierRunner;

/// Состав чанка известен, только если paths непуст и нет unknownPaths
/// (chunk?.paths?.length && !chunk.unknownPaths — JS truthiness).
fn chunk_scope(chunk: &Value) -> Option<Vec<Value>> {
    let paths = chunk.get("paths")?.as_array()?;
    if paths.is_empty() || js_truthy(chunk.get("unknownPaths").unwrap_or(&Value::Null)) {
        return None;
    }
    Some(paths.clone())
}

/// `!f.file || scope.has(f.file)` — file falsy → виден всем; иначе строгое
/// равенство значения одному из путей скоупа.
fn file_in_scope(scope: &[Value], f: &Value) -> bool {
    let file = f.get("file").unwrap_or(&Value::Null);
    !js_truthy(file) || scope.iter().any(|p| p == file)
}

/// Порт scopePriors: prior findings только по файлам ЭТОГО чанка.
/// Скоуп неполон/пуст или priors пусты — возвращаем вход как есть (null в
/// JSON-контракте батча planChunks — JS-обёртка подставляет оригинал).
pub fn scope_priors(prior_findings: &[Value], chunk: &Value) -> Option<Vec<Value>> {
    let scope = chunk_scope(chunk)?;
    if prior_findings.is_empty() {
        return None; // вход возвращается без изменений
    }
    Some(
        prior_findings
            .iter()
            .filter(|f| file_in_scope(&scope, f))
            .cloned()
            .collect(),
    )
}

/// Порт demoteOutOfChunk: находка про файл, которого чанк не видит, понижается
/// до nit и метится (слепой блокер merge невозможен). Файл, видимый хотя бы
/// частично, не трогается. Возвращает КОПИЮ res при демоуте > 0, иначе клон
/// входа (JS мутировал бы res — на результаты агрегации это не влияет, а
/// JSON-контракт не даёт отличить «та же ссылка» от копии).
pub fn demote_out_of_chunk(res: &Value, chunk: &Value, chunk_index: usize) -> Value {
    let Some(scope) = chunk_scope(chunk) else {
        return res.clone();
    };
    let findings = match res.get("findings").and_then(Value::as_array) {
        Some(f) if !f.is_empty() => f,
        _ => return res.clone(),
    };
    let mut demoted = 0usize;
    let mut out = Vec::with_capacity(findings.len());
    for f in findings {
        let sev_nit = f.get("severity").and_then(Value::as_str) == Some("nit");
        if file_in_scope(&scope, f) || sev_nit {
            out.push(f.clone());
            continue;
        }
        demoted += 1;
        let mut m = f.as_object().cloned().unwrap_or_default();
        m.insert("severity".into(), json!("nit"));
        m.insert("outOfChunk".into(), json!(true));
        // Math.min(f.confidence ?? 0.5, 0.5); после normalizeFinding confidence
        // всегда число 0..1 — не-число (Math.min → NaN) недостижимо.
        let conf = match f.get("confidence") {
            Some(c) => match js_to_number(c) {
                Some(n) if n.is_finite() => n.min(0.5),
                _ => 0.5,
            },
            None => 0.5,
        };
        m.insert("confidence".into(), json!(conf));
        out.push(Value::Object(m));
    }
    if demoted == 0 {
        return res.clone();
    }
    let mut r = res.as_object().cloned().unwrap_or_default();
    r.insert("findings".into(), Value::Array(out));
    // (res.demotedOutOfChunk || 0) + demoted — поле ставит сама эта функция,
    // поэтому число всегда (не-число в JS дало бы string-конкат — недостижимо).
    let prev = match res.get("demotedOutOfChunk").and_then(js_to_number) {
        Some(n) if n.is_finite() && n > 0.0 => n,
        _ => 0.0,
    };
    r.insert("demotedOutOfChunk".into(), json!(prev + demoted as f64));
    r.insert("chunkIndex".into(), json!(chunk_index));
    Value::Object(r)
}

/// Порт aggregateModel: склейка частей одной модели по всем чанкам.
/// Возвращает агрегат и число схлопнутых дублей (для лога обёртки —
/// в JS его печатал dedupeSameModelFindings через log).
pub fn aggregate_model(model: &str, done: &[Value]) -> (Value, usize) {
    let parts: Vec<&Value> = done
        .iter()
        .filter(|d| d.get("model").map(|m| m.as_str() == Some(model)).unwrap_or(false))
        .map(|d| d.get("res").unwrap_or(&Value::Null))
        .collect();
    let ok_parts: Vec<&Value> = parts
        .iter()
        .copied()
        .filter(|p| !js_truthy(p.get("error").unwrap_or(&Value::Null)))
        .collect();

    if ok_parts.is_empty() {
        let all_skipped = !parts.is_empty()
            && parts
                .iter()
                .all(|p| js_truthy(p.get("skipped").unwrap_or(&Value::Null)));
        let mut out = Map::new();
        out.insert("model".into(), Value::String(model.to_string()));
        let err = parts
            .first()
            .and_then(|p| p.get("error"))
            .filter(|e| js_truthy(e))
            .cloned()
            .unwrap_or_else(|| Value::String("нет ответа".to_string()));
        out.insert("error".into(), err);
        if all_skipped {
            out.insert("skipped".into(), json!(true));
        }
        return (Value::Object(out), 0);
    }

    let summary = ok_parts
        .iter()
        .map(|p| p.get("summary").unwrap_or(&Value::Null))
        .find(|s| js_truthy(s))
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));

    // flatMap(p => p.findings): у успешных частей findings всегда массив
    // (reviewWithModel/reviewChunkMoA) — не-массив трактуем как пусто.
    let all: Vec<Value> = ok_parts
        .iter()
        .flat_map(|p| match p.get("findings") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        })
        .collect();
    let (kept, dropped) = dedupe_same_model_findings(&all);

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model.to_string()));
    out.insert("summary".into(), summary);
    out.insert("findings".into(), Value::Array(kept));
    (Value::Object(out), dropped)
}

// ---------------------------------------------------------------------------
// Боевой движок (T12): порт createReviewEngine (lib/engine.mjs:160-412)
// ---------------------------------------------------------------------------

/// Маркер пропуска чанка по тайм-бюджету (`lib/engine.mjs:66`).
///
/// Не путать с «модель недоступна»: по нему `apply_fallback` понимает, что
/// резерв запускать НЕ надо (резерв упёрся бы в тот же истёкший бюджет), а
/// пайплайн — что ревью не отказало, а не поместилось в бюджет.
pub const TIME_SKIP: &str = "тайм-бюджет прогона исчерпан";

/// Часы прогона в epoch-мс. В JS это `Date.now()` в двух ролях сразу: проверка
/// дедлайна и `duration_ms` событий наблюдаемости.
///
/// Инъекция (требование T09) нужна golden-прогонам: с замороженными часами
/// детерминированы обе роли — и ветка дедлайна, и JSONL наблюдаемости, который
/// иначе отличался бы от эталона на каждый прогон.
pub type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

/// Часы по умолчанию — системные (`Date.now()`).
pub fn system_clock() -> Clock {
    Box::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    })
}

/// Замороженные часы для тестов и golden-прогонов.
pub fn fixed_clock(ms: u64) -> Clock {
    Box::new(move || ms)
}

/// Назначение specialist'а (`cfg.specialists.assignments[i]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecialistAssignment {
    pub role: String,
    pub model: String,
}

/// Часть замороженного конфига, которую читает движок (`lib/engine.mjs:274`:
/// `const { reviewMode, models, moa, limits, specialists } = cfg`, плюс
/// `cfg.ollama.temperature`, `cfg.gate.requireQuote`, `cfg.verifier.*`).
///
/// Отдельная структура, а не `&Value`, по той же причине, что [`crate::llm::LlmConfig`]:
/// движок живёт весь прогон, и разбирать JSON на каждый вызов модели незачем.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// `cfg.reviewMode`: `"moa"` включает MoA-ветку, всё прочее — consensus.
    pub review_mode: String,
    /// `cfg.models.list` — основные модели consensus-режима.
    pub models: Vec<String>,
    /// `cfg.models.fallback` — очередь резервов (по одному на упавшую модель).
    pub fallback: Vec<String>,
    /// `cfg.moa.aggregator` — реальная модель-жюри.
    pub moa_aggregator: String,
    /// `cfg.moa.label` — под каким именем MoA-результат уходит наружу.
    pub moa_label: String,
    /// `cfg.moa.references` — advisor-модели (когда specialists выключены).
    pub moa_references: Vec<String>,
    /// `cfg.moa.aggTemp` — температура жюри.
    pub agg_temp: Option<f64>,
    /// `cfg.ollama.temperature` — дефолтная температура ревью-вызова.
    /// `None` = аргумент опущен, клиент подставит свой дефолт (как `undefined` в JS).
    pub temperature: Option<f64>,
    /// `cfg.limits.concurrency` — лимит пула «модель×чанк» (`mapLimit`).
    pub concurrency: usize,
    /// `cfg.specialists.enabled`.
    pub specialists_enabled: bool,
    /// `cfg.specialists.assignments`.
    pub assignments: Vec<SpecialistAssignment>,
    /// `cfg.gate.requireQuote` — передаётся в `normalize_finding`.
    pub require_quote: bool,
    /// `cfg.verifier.model` — модель Pass B (отдельная от пула advisors).
    pub verifier_model: String,
    /// `cfg.verifier.temperature`.
    pub verifier_temperature: Option<f64>,
}

impl Default for EngineConfig {
    /// Дефолты движка (`lib/config.mjs`), а не «пустой» объект: движок, собранный
    /// вручную в тесте, обязан вести себя как боевой.
    fn default() -> Self {
        EngineConfig {
            review_mode: "moa".to_string(),
            models: Vec::new(),
            fallback: Vec::new(),
            moa_aggregator: String::new(),
            moa_label: "moa".to_string(),
            moa_references: Vec::new(),
            agg_temp: None,
            temperature: None,
            concurrency: 10,
            specialists_enabled: false,
            assignments: Vec::new(),
            require_quote: false,
            verifier_model: String::new(),
            verifier_temperature: None,
        }
    }
}

/// `arr.map(String)` для списка моделей: JS кладёт сюда строки, но урезанный
/// конфиг переживаем так же мягко, как JS-шаблон (`${m}`).
fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(js_to_string).collect(),
        _ => Vec::new(),
    }
}

impl EngineConfig {
    /// Читает поля замороженного конфига (`load_review_config_parts`).
    ///
    /// Отсутствующие поля дают дефолты движка, а не панику — как `||` в JS.
    pub fn from_cfg(cfg: &Value) -> Self {
        let def = EngineConfig::default();
        let get = |root: &str, key: &str| cfg.get(root).and_then(|v| v.get(key));
        let moa = |key: &str| get("moa", key);
        EngineConfig {
            review_mode: cfg
                .get("reviewMode")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or(def.review_mode),
            models: str_list(get("models", "list")),
            fallback: str_list(get("models", "fallback")),
            moa_aggregator: moa("aggregator")
                .filter(|v| js_truthy(v))
                .map(js_to_string)
                .unwrap_or(def.moa_aggregator),
            moa_label: moa("label")
                .filter(|v| js_truthy(v))
                .map(js_to_string)
                .unwrap_or(def.moa_label),
            moa_references: str_list(moa("references")),
            agg_temp: moa("aggTemp").and_then(Value::as_f64),
            temperature: get("ollama", "temperature").and_then(Value::as_f64),
            // JS: mapLimit сам делает Math.max(1, …); 0/отсутствие → дефолт конфига.
            concurrency: get("limits", "concurrency")
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n >= 1.0)
                .map(|n| n as usize)
                .unwrap_or(def.concurrency),
            specialists_enabled: get("specialists", "enabled")
                .map(js_truthy)
                .unwrap_or(def.specialists_enabled),
            assignments: match get("specialists", "assignments") {
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|x| SpecialistAssignment {
                        role: js_to_string(x.get("role").unwrap_or(&Value::Null)),
                        model: js_to_string(x.get("model").unwrap_or(&Value::Null)),
                    })
                    .collect(),
                _ => Vec::new(),
            },
            require_quote: get("gate", "requireQuote")
                .map(js_truthy)
                .unwrap_or(def.require_quote),
            verifier_model: get("verifier", "model")
                .filter(|v| js_truthy(v))
                .map(js_to_string)
                .unwrap_or(def.verifier_model),
            verifier_temperature: get("verifier", "temperature").and_then(Value::as_f64),
        }
    }
}

/// Части профиля, нужные билдерам промптов. В JS движок получал готовый объект
/// `prompt` через DI; здесь билдеры — свободные функции `prompt.rs`, и весь
/// профиль сводится к [`PromptCtx`] (строки + исходник e2eSpec).
pub fn prompt_ctx_from_profile(profile: &Profile) -> PromptCtx {
    let p = profile.prompts();
    PromptCtx {
        repo_rules: profile.repo_rules.clone(),
        advisor_extra: p.advisor_extra.clone(),
        specialist_extra: p.specialist_extra.clone(),
        aggregator_extra: p.aggregator_extra.clone(),
        verifier_extra: p.verifier_extra.clone(),
        e2e_spec_source: profile.layout().e2e_spec_source().map(str::to_string),
    }
}

/// Колбэк наблюдаемости — порт `observability.logEvent(event, payload)`
/// (`lib/engine.mjs:186`). `None` = observability выключена (`obs` в JS равен
/// null, и все `obs?.logEvent` — no-op).
pub type EventSink<'a> = &'a (dyn Fn(&str, &Value) + Send + Sync);

/// Движок ревью: MoA/consensus-прогон моделей и verifier Pass B.
///
/// **Почему LLM-клиент взят по ссылке, а не `Box<dyn LlmApi>`**: клиент один на
/// весь прогон и принадлежит пайплайну — его же читают учёт стоимости
/// (`with_usage_sink`) и, после T13, swarm. Владение внутри движка заставило бы
/// пайплайн отдать клиента и потерять доступ к его семафорам/статистике, а тестам
/// — аллоцировать фейк в куче ради каждого сценария. Тот же выбор, что у
/// [`crate::traits::PipelineDeps`], который держит все зависимости по ссылке.
///
/// **Параллелизм** повторяет JS ровно там, где он в JS есть:
/// - `run_limited(модели×чанки, cfg.concurrency)` — `mapLimit` (`lib/engine.mjs:266`);
/// - `run_limited(чанки, cfg.concurrency)` в MoA — `mapLimit` (`lib/engine.mjs:340`);
/// - advisors/специалисты внутри чанка — `Promise.all` без лимита
///   (`lib/engine.mjs:305-311`), поэтому здесь `run_limited` с лимитом по числу
///   элементов. Реальный потолок запросов держат адаптивные семафоры внутри
///   [`crate::llm::RealLlmClient`] — второй раз ограничивать их здесь значило бы
///   ужесточить прогон против JS.
pub struct RealEngine<'a> {
    cfg: EngineConfig,
    llm: &'a dyn LlmApi,
    prompt_ctx: PromptCtx,
    log: &'a (dyn Fn(&str) + Send + Sync),
    on_event: Option<EventSink<'a>>,
    now: Clock,
}

impl<'a> RealEngine<'a> {
    /// Движок из уже разобранного конфига и готового [`PromptCtx`].
    pub fn new(
        cfg: EngineConfig,
        llm: &'a dyn LlmApi,
        prompt_ctx: PromptCtx,
        log: &'a (dyn Fn(&str) + Send + Sync),
    ) -> Self {
        RealEngine { cfg, llm, prompt_ctx, log, on_event: None, now: system_clock() }
    }

    /// Движок из замороженного конфига и профиля — путь пайплайна.
    pub fn from_config(
        cfg: &Value,
        llm: &'a dyn LlmApi,
        profile: &Profile,
        log: &'a (dyn Fn(&str) + Send + Sync),
    ) -> Self {
        RealEngine::new(
            EngineConfig::from_cfg(cfg),
            llm,
            prompt_ctx_from_profile(profile),
            log,
        )
    }

    /// Подключить observability (`deps.observability?.enabled ? … : null`).
    pub fn with_events(mut self, on_event: EventSink<'a>) -> Self {
        self.on_event = Some(on_event);
        self
    }

    /// Подменить часы (golden-прогоны, тесты дедлайна).
    pub fn with_clock(mut self, now: Clock) -> Self {
        self.now = now;
        self
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    fn log(&self, message: &str) {
        (self.log)(message);
    }

    fn emit(&self, event: &str, payload: &Value) {
        if let Some(sink) = self.on_event {
            sink(event, payload);
        }
    }

    fn now_ms(&self) -> u64 {
        (self.now)()
    }

    /// `deadlineHit` (`lib/engine.mjs:66`): `Boolean(ctx?.deadline) && Date.now() > ctx.deadline`.
    /// `deadline == 0` — бюджет не задан.
    fn deadline_hit(&self, ctx: &ReviewContext) -> bool {
        ctx.deadline != 0 && self.now_ms() > ctx.deadline
    }

    // -- наблюдаемость -----------------------------------------------------

    /// `trackLlm` (`lib/engine.mjs:176-203`).
    ///
    /// Ветки `catch` у JS-версии здесь нет по построению: `review_with_model`
    /// ошибку модели возвращает результатом, а не пробрасывает, и в JS эта ветка
    /// тоже недостижима из движка. Порядок ключей события — как в JS-литерале.
    fn track_llm(&self, call_type: &str, meta: &[(&str, Value)], f: impl FnOnce() -> Value) -> Value {
        let t0 = self.now_ms();
        let result = f();
        if self.on_event.is_none() {
            return result;
        }
        let mut ev = Map::new();
        ev.insert("callType".into(), json!(call_type));
        for (k, v) in meta {
            ev.insert((*k).to_string(), v.clone());
        }
        ev.insert("duration_ms".into(), json!(self.now_ms().saturating_sub(t0)));
        let err = result.get("error").filter(|e| js_truthy(e));
        ev.insert("ok".into(), json!(err.is_none()));
        ev.insert(
            "findings".into(),
            json!(result.get("findings").and_then(Value::as_array).map_or(0, Vec::len)),
        );
        if let Some(e) = err {
            ev.insert("error".into(), json!(redact_secrets_for_log(&js_to_string(e), 200)));
        }
        // `...(usage ? {prompt_tokens, …} : {})`: undefined-поля JSON.stringify выбрасывает.
        if let Some(u) = result.get("__usage").filter(|u| js_truthy(u)) {
            for k in ["prompt_tokens", "completion_tokens", "total_tokens"] {
                if let Some(v) = u.get(k) {
                    ev.insert(k.into(), v.clone());
                }
            }
        }
        self.emit("llm_call", &Value::Object(ev));
        result
    }

    /// `trackText` (`lib/engine.mjs`). `text_chars` — длина в UTF-16
    /// юнитах (`String#length` в JS).
    ///
    /// `f` получает сборщик usage: prose-вызов возвращает строку (контракт `chat_text`),
    /// поэтому токены приходят боковым каналом. Фейк, который его не зовёт, оставляет
    /// usage пустым — событие пишется без токенов.
    fn track_text(
        &self,
        call_type: &str,
        meta: &[(&str, Value)],
        f: impl FnOnce(UsageCallback) -> Result<String, crate::llm::ChatError>,
    ) -> Result<String, crate::llm::ChatError> {
        let t0 = self.now_ms();
        let usage: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&usage);
        let result = f(UsageCallback(Arc::new(move |u: &Value| {
            *sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(u.clone());
        })));
        if self.on_event.is_none() {
            return result;
        }
        let mut ev = Map::new();
        ev.insert("callType".into(), json!(call_type));
        for (k, v) in meta {
            ev.insert((*k).to_string(), v.clone());
        }
        ev.insert("duration_ms".into(), json!(self.now_ms().saturating_sub(t0)));
        match &result {
            Ok(text) => {
                ev.insert("ok".into(), json!(true));
                ev.insert("text_chars".into(), json!(utf16_len(text)));
                let usage = usage.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(u) = usage.as_ref().filter(|u| js_truthy(u)) {
                    for k in ["prompt_tokens", "completion_tokens", "total_tokens"] {
                        // `usage.prompt_tokens` без поля — undefined, JSON.stringify его роняет.
                        if let Some(v) = u.get(k) {
                            ev.insert(k.into(), v.clone());
                        }
                    }
                }
            }
            Err(e) => {
                ev.insert("ok".into(), json!(false));
                ev.insert("error".into(), json!(redact_secrets_for_log(&e.message, 200)));
            }
        }
        self.emit("llm_call", &Value::Object(ev));
        result
    }

    // -- один вызов модели -------------------------------------------------

    /// `reviewWithModel` (`lib/engine.mjs:238-259`): JSON-ответ модели →
    /// `{model, summary, findings, __usage}`; сбой LLM → `{model, error}`.
    fn review_with_model(
        &self,
        model: &str,
        messages: &[Value],
        temperature: Option<f64>,
        chunk_index: usize,
        call_type: &str,
    ) -> Value {
        let meta = [
            ("model", json!(model)),
            ("chunkIndex", json!(chunk_index)),
            ("reviewMode", json!(self.cfg.review_mode)),
            prompt_meta(messages),
        ];
        self.track_llm(call_type, &meta, || {
            let opts = ChatJsonOpts { temperature, call_type: None };
            match self.llm.chat_json(&Value::Array(messages.to_vec()), model, &opts) {
                Ok(r) => {
                    if let Some(kind) = r.repaired {
                        self.emit(
                            "json_repair",
                            &json!({"kind": kind.as_str(), "model": model, "chunkIndex": chunk_index}),
                        );
                    }
                    let raw = match r.value.get("findings") {
                        Some(Value::Array(a)) => a.clone(),
                        _ => Vec::new(), // Array.isArray(...) ? ... : []
                    };
                    let findings: Vec<Value> = raw
                        .iter()
                        .map(|f| normalize_finding(f, model, self.cfg.require_quote))
                        .filter(|f| !f.is_null()) // .filter(Boolean)
                        .collect();
                    let mut out = Map::new();
                    out.insert("model".into(), json!(model));
                    // String(result.summary || '')
                    out.insert(
                        "summary".into(),
                        json!(match r.value.get("summary") {
                            Some(v) if js_truthy(v) => js_to_string(v),
                            _ => String::new(),
                        }),
                    );
                    // `verdict` модели больше не пробрасывается (lib/engine.mjs): гейт
                    // решает по находкам, агрегат вердикта не несёт.
                    out.insert("findings".into(), Value::Array(findings));
                    if let Some(u) = r.usage {
                        out.insert("__usage".into(), u);
                    }
                    Value::Object(out)
                }
                Err(e) => {
                    self.log(&format!("модель {model} недоступна: {}", e.message));
                    json!({"model": model, "error": e.message})
                }
            }
        })
    }

    /// `buildMessages({...})` для одного чанка (`lib/engine.mjs:270-275`).
    /// prescan-секция подмешивается только в первый чанк.
    fn build_chunk_messages(
        &self,
        chunk: &Value,
        first: bool,
        priors: &[Value],
        ctx: &ReviewContext,
    ) -> Result<Vec<Value>> {
        let args = json!({
            "annotated": chunk.get("annotated").cloned().unwrap_or(Value::Null),
            "blastBlocks": ctx.blast_blocks,
            "pr": ctx.pr,
            "fileContents": chunk.get("fileContents").cloned().unwrap_or(Value::Null),
            "priorFindings": priors,
            "changedPaths": ctx.changed_paths,
            "chunk": chunk,
            "prescanSection": if first { ctx.prescan_section.clone() } else { String::new() },
        });
        prompt::build_messages(&args, &self.prompt_ctx)
    }

    /// Скоуп priors по каждому чанку одним проходом (`planAllPriors`).
    fn scoped_priors(&self, chunks: &[Value], ctx: &ReviewContext) -> Vec<Vec<Value>> {
        chunks
            .iter()
            .map(|c| scope_priors(&ctx.prior_findings, c).unwrap_or_else(|| ctx.prior_findings.clone()))
            .collect()
    }

    /// Промпт не собрался (единственная причина — `E2ESPEC_UNSUPPORTED`: диалект
    /// `layout.e2eSpec` профиля не поддержан regex-крейтом). Это отказ движка, а
    /// не LLM, но наружу он идёт тем же результатом-с-ошибкой: иначе один битый
    /// паттерн в профиле превращал бы fail-open прогон в аварию, а пайплайн уже
    /// умеет показать «модель не ответила» в sticky и решить по `llmRequired`.
    fn prompt_failure(&self, model: &str, chunk_index: usize, e: &anyhow::Error) -> Value {
        self.log(&format!("промпт чанка {chunk_index} не собран: {e}"));
        json!({"model": model, "error": e.to_string()})
    }

    // -- прогон моделей по чанкам -----------------------------------------

    /// `runModelsOverChunks` (`lib/engine.mjs:261-281`): пул «модель×чанк» под
    /// `cfg.concurrency`, дедлайн-проверка ПЕРЕД сборкой промпта, демоут находок
    /// про невидимые чанку файлы. Возвращает элементы `{model, res}`.
    fn run_models_over_chunks(
        &self,
        model_list: &[String],
        chunks: &[Value],
        ctx: &ReviewContext,
    ) -> Vec<Value> {
        let scoped = self.scoped_priors(chunks, ctx);
        let mut tasks: Vec<(usize, usize)> = Vec::with_capacity(model_list.len() * chunks.len());
        for mi in 0..model_list.len() {
            for ci in 0..chunks.len() {
                tasks.push((mi, ci));
            }
        }
        run_limited(tasks, self.cfg.concurrency, |(mi, ci)| {
            let model = &model_list[mi];
            // Точка дедлайна 1/4 (lib/engine.mjs:269).
            let res = if self.deadline_hit(ctx) {
                json!({"model": model, "error": TIME_SKIP, "skipped": true})
            } else {
                match self.build_chunk_messages(&chunks[ci], ci == 0, &scoped[ci], ctx) {
                    Ok(messages) => self.review_with_model(
                        model,
                        &messages,
                        self.cfg.temperature,
                        ci,
                        "review_json",
                    ),
                    Err(e) => self.prompt_failure(model, ci, &e),
                }
            };
            json!({"model": model, "res": demote_out_of_chunk(&res, &chunks[ci], ci)})
        })
    }

    // -- MoA ---------------------------------------------------------------

    /// `reviewSpecialist` (`lib/engine.mjs:283-292`).
    fn review_specialist(&self, role: &str, model: &str, base: &[Value], chunk_index: usize) -> Value {
        let msgs = prompt::build_specialist_messages(role, base, &self.prompt_ctx);
        let meta = [
            ("model", json!(model)),
            ("role", json!(role)),
            ("chunkIndex", json!(chunk_index)),
            ("reviewMode", json!(self.cfg.review_mode)),
            prompt_meta(&msgs),
        ];
        let out = self.track_text("specialist", &meta, |on_usage| {
            let opts = ChatTextOpts { on_usage: Some(on_usage), ..ChatTextOpts::default() };
            self.llm.chat_text(&Value::Array(msgs), model, &opts)
        });
        match out {
            Ok(text) => json!({"role": role, "model": model, "text": text}),
            Err(e) => {
                self.log(&format!("specialist {role}/{model} недоступен: {}", e.message));
                json!({"role": role, "model": model, "error": e.message})
            }
        }
    }

    /// `reviewReferenceModel` (`lib/engine.mjs:294-303`). `role: 'advisor'`
    /// дописывается вызывающим — как `.then((r) => ({...r, role: 'advisor'}))`.
    fn review_reference_model(&self, model: &str, base: &[Value], chunk_index: usize) -> Value {
        let msgs = prompt::build_reference_messages(base, &self.prompt_ctx);
        let meta = [
            ("model", json!(model)),
            ("chunkIndex", json!(chunk_index)),
            ("reviewMode", json!(self.cfg.review_mode)),
            prompt_meta(&msgs),
        ];
        let out = self.track_text("advisor", &meta, |on_usage| {
            let opts = ChatTextOpts { on_usage: Some(on_usage), ..ChatTextOpts::default() };
            self.llm.chat_text(&Value::Array(msgs), model, &opts)
        });
        match out {
            Ok(text) => json!({"model": model, "text": text}),
            Err(e) => {
                self.log(&format!("advisor {model} недоступен: {}", e.message));
                json!({"model": model, "error": e.message})
            }
        }
    }

    /// `reviewChunkMoA` (`lib/engine.mjs:305-337`): advisors/специалисты →
    /// жюри-агрегатор. Ни одного успешного advisor'а — жюри не зовём вовсе.
    fn review_chunk_moa(&self, base: &[Value], chunk_index: usize, ctx: &ReviewContext) -> Value {
        let references: Vec<Value> = if self.cfg.specialists_enabled {
            let idx: Vec<usize> = (0..self.cfg.assignments.len()).collect();
            let limit = idx.len().max(1); // Promise.all — без лимита
            run_limited(idx, limit, |i| {
                let a = &self.cfg.assignments[i];
                self.review_specialist(&a.role, &a.model, base, chunk_index)
            })
        } else {
            let idx: Vec<usize> = (0..self.cfg.moa_references.len()).collect();
            let limit = idx.len().max(1);
            run_limited(idx, limit, |i| {
                let mut r = self.review_reference_model(&self.cfg.moa_references[i], base, chunk_index);
                if let Some(o) = r.as_object_mut() {
                    o.insert("role".into(), json!("advisor"));
                }
                r
            })
        };

        let any_ok = references.iter().any(|r| {
            js_truthy(r.get("text").unwrap_or(&Value::Null))
                && !js_truthy(r.get("error").unwrap_or(&Value::Null))
        });
        if !any_ok {
            let msg = references
                .iter()
                .map(|r| {
                    let err = r.get("error").filter(|e| js_truthy(e));
                    format!(
                        "{}/{}: {}",
                        js_to_string(r.get("role").unwrap_or(&Value::Null)),
                        js_to_string(r.get("model").unwrap_or(&Value::Null)),
                        err.map(js_to_string).unwrap_or_else(|| "пусто".to_string()),
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return json!({"model": self.cfg.moa_aggregator, "error": msg});
        }

        // Точка дедлайна 2/4 (lib/engine.mjs:318): advisors уже отработали, жюри
        // не запускаем, но их prose сохраняем — она уедет в sticky.
        if self.deadline_hit(ctx) {
            return json!({
                "model": self.cfg.moa_aggregator,
                "error": TIME_SKIP,
                "skipped": true,
                "references": references,
            });
        }

        let agg_messages = if self.cfg.specialists_enabled {
            prompt::build_specialist_aggregator_messages(base, &references)
        } else {
            prompt::build_aggregator_messages(base, &references)
        };
        let agg = self.review_with_model(
            &self.cfg.moa_aggregator,
            &agg_messages,
            self.cfg.agg_temp,
            chunk_index,
            "moa_aggregate",
        );
        if js_truthy(agg.get("error").unwrap_or(&Value::Null)) {
            return agg;
        }
        // {...agg, model: label, aggregator, references, findings: findings.map(model=label)}
        let mut out = agg.as_object().cloned().unwrap_or_default();
        out.insert("model".into(), json!(self.cfg.moa_label));
        let findings: Vec<Value> = match out.get("findings") {
            Some(Value::Array(a)) => a
                .iter()
                .map(|f| {
                    let mut m = f.as_object().cloned().unwrap_or_default();
                    m.insert("model".into(), json!(self.cfg.moa_label));
                    Value::Object(m)
                })
                .collect(),
            _ => Vec::new(),
        };
        out.insert("findings".into(), Value::Array(findings));
        out.insert("aggregator".into(), json!(self.cfg.moa_aggregator));
        out.insert("references".into(), Value::Array(references));
        Value::Object(out)
    }

    /// `runMoAOverChunks` (`lib/engine.mjs:339-354`).
    fn run_moa_over_chunks(&self, chunks: &[Value], ctx: &ReviewContext) -> Vec<Value> {
        let scoped = self.scoped_priors(chunks, ctx);
        let idx: Vec<usize> = (0..chunks.len()).collect();
        let agg_model = self.cfg.moa_aggregator.clone();
        run_limited(idx, self.cfg.concurrency, |ci| {
            // Точка дедлайна 3/4 (lib/engine.mjs:341).
            let res = if self.deadline_hit(ctx) {
                json!({"model": agg_model, "error": TIME_SKIP, "skipped": true})
            } else {
                match self.build_chunk_messages(&chunks[ci], ci == 0, &scoped[ci], ctx) {
                    Ok(messages) => self.review_chunk_moa(&messages, ci, ctx),
                    Err(e) => self.prompt_failure(&agg_model, ci, &e),
                }
            };
            json!({"model": agg_model, "res": demote_out_of_chunk(&res, &chunks[ci], ci)})
        })
    }

    /// `reviewAllMoA` (`lib/engine.mjs:356-366`): агрегат ровно один.
    fn review_all_moa(&self, chunks: &[Value], ctx: &ReviewContext) -> Vec<Value> {
        let done = self.run_moa_over_chunks(chunks, ctx);
        let mut agg = self.aggregate_one(&self.cfg.moa_aggregator, &done);
        let chunk_results: Vec<Value> =
            done.iter().map(|d| d.get("res").cloned().unwrap_or(Value::Null)).collect();
        let skipped = chunk_results
            .iter()
            .filter(|r| js_truthy(r.get("skipped").unwrap_or(&Value::Null)))
            .count();
        let o = agg.as_object_mut().expect("aggregate_model отдаёт объект");
        o.insert("model".into(), json!(self.cfg.moa_label));
        o.insert("aggregator".into(), json!(self.cfg.moa_aggregator));
        o.insert("moaReferences".into(), json!(self.cfg.moa_references));
        o.insert(
            "references".into(),
            Value::Array(merge_specialist_references(&chunk_results)),
        );
        o.insert("skippedChunks".into(), json!(skipped));
        vec![agg]
    }

    /// `aggregateAll` для одной модели: агрегат + лог дедупа той же строкой,
    /// что печатает `dedupeSameModelFindings` (`lib/engine.mjs:127`).
    fn aggregate_one(&self, model: &str, done: &[Value]) -> Value {
        let (agg, dropped) = aggregate_model(model, done);
        if dropped > 0 {
            self.log(&format!(
                "дедуп повторов внутри модели: {dropped} (одна тема из нескольких чанков)"
            ));
        }
        agg
    }

    // -- fallback ----------------------------------------------------------

    /// `applyFallback` (`lib/engine.mjs:369-390`): по одному резерву на упавшую
    /// модель; `skipped` (тайм-бюджет) резерв НЕ вызывает.
    fn apply_fallback(
        &self,
        results: &mut [Value],
        chunks: &[Value],
        ctx: &ReviewContext,
        moa_mode: bool,
    ) {
        let mut spare: VecDeque<String> = self.cfg.fallback.iter().cloned().collect();
        for i in 0..results.len() {
            let err = results[i].get("error").cloned().unwrap_or(Value::Null);
            if !js_truthy(&err) || js_truthy(results[i].get("skipped").unwrap_or(&Value::Null)) {
                continue;
            }
            // Точка дедлайна 4/4 (lib/engine.mjs:370).
            if self.deadline_hit(ctx) {
                self.log("тайм-бюджет исчерпан — fallback не запускаем");
                break;
            }
            let Some(fb) = spare.pop_front() else { break };
            let prev_model = js_to_string(results[i].get("model").unwrap_or(&Value::Null));
            let who = if moa_mode {
                "MoA aggregator".to_string()
            } else {
                format!("primary {prev_model}")
            };
            self.log(&format!("{who} недоступен ({}) → резерв {fb}", js_to_string(&err)));
            if self.cfg.review_mode == "moa" && self.cfg.specialists_enabled {
                self.log(
                    "MoA fallback: plain model без specialist jury — R5 refutation/advisorClaims недоступны",
                );
            }
            let plain = self.run_models_over_chunks(std::slice::from_ref(&fb), chunks, ctx);
            let mut fb_res = self.aggregate_one(&fb, &plain);
            let o = fb_res.as_object_mut().expect("aggregate_model отдаёт объект");
            o.insert(
                "fallbackFor".into(),
                json!(if moa_mode { self.cfg.moa_aggregator.clone() } else { prev_model }),
            );
            if moa_mode {
                // decorate MoA (lib/engine.mjs:385-390)
                if self.cfg.specialists_enabled {
                    o.insert("juryBypassed".into(), json!(true));
                }
                o.insert("model".into(), json!(self.cfg.moa_label));
                let relabel: Option<Vec<Value>> = match o.get("findings") {
                    Some(Value::Array(a)) if !a.is_empty() => Some(
                        a.iter()
                            .map(|f| {
                                let mut m = f.as_object().cloned().unwrap_or_default();
                                m.insert("model".into(), json!(self.cfg.moa_label));
                                Value::Object(m)
                            })
                            .collect(),
                    ),
                    _ => None,
                };
                if let Some(fs) = relabel {
                    o.insert("findings".into(), Value::Array(fs));
                }
            }
            results[i] = fb_res;
        }
    }

    /// `reviewAllModels` (`lib/engine.mjs:392-410`) во внутреннем JSON-виде.
    /// Публичен, потому что на нём удобно писать паритетные и golden-прогоны:
    /// форма один в один с JS-массивом до превращения в [`ModelReview`].
    pub fn review_all_values(&self, chunks: &[Value], ctx: &ReviewContext) -> Vec<Value> {
        if self.cfg.review_mode == "moa" {
            let mut results = self.review_all_moa(chunks, ctx);
            self.apply_fallback(&mut results, chunks, ctx, true);
            return results;
        }
        let done = self.run_models_over_chunks(&self.cfg.models, chunks, ctx);
        let mut results: Vec<Value> = self
            .cfg
            .models
            .iter()
            .map(|model| {
                let mut agg = self.aggregate_one(model, &done);
                let skipped = done
                    .iter()
                    .filter(|d| {
                        d.get("model").and_then(Value::as_str) == Some(model.as_str())
                            && js_truthy(
                                d.get("res")
                                    .and_then(|r| r.get("skipped"))
                                    .unwrap_or(&Value::Null),
                            )
                    })
                    .count();
                agg.as_object_mut()
                    .expect("aggregate_model отдаёт объект")
                    .insert("skippedChunks".into(), json!(skipped));
                agg
            })
            .collect();
        self.apply_fallback(&mut results, chunks, ctx, false);
        results
    }

    /// Verifier Pass B с текущими конфигом/клиентом.
    fn verifier(&self) -> VerifierRunner<'_> {
        VerifierRunner {
            model: &self.cfg.verifier_model,
            temperature: self.cfg.verifier_temperature,
            llm: self.llm,
            prompt_ctx: &self.prompt_ctx,
            on_event: self.on_event,
            now: &*self.now,
        }
    }
}

impl EngineApi for RealEngine<'_> {
    fn review_all_models(&self, chunks: &[Chunk], ctx: &ReviewContext) -> Result<ReviewAllResult> {
        let values: Vec<Value> = chunks
            .iter()
            .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
            .collect();
        Ok(self
            .review_all_values(&values, ctx)
            .iter()
            .map(model_review_from_value)
            .collect())
    }

    fn verify_findings(&self, candidates: &[Value], ctx: &VerifyContext) -> Result<Vec<Value>> {
        self.verifier().verify_findings(candidates, ctx)
    }
}

/// `String#length` в JS — число UTF-16 кодовых юнитов.
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `messagesChars` (`lib/engine.mjs`): размер промпта в символах — детерминированный
/// прокси токенов, доступный ДО вызова и независимый от того, вернул ли провайдер usage.
/// `String(m?.content ?? '')`: отсутствие и `null` — пустая строка.
pub(crate) fn messages_chars(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|m| match m.get("content") {
            None | Some(Value::Null) => 0,
            Some(Value::String(s)) => utf16_len(s),
            Some(v) => utf16_len(&js_to_string(v)),
        })
        .sum()
}

/// `promptMeta(messages, layout)` для раскладки `legacy`: только `prompt_chars`.
/// Раскладка `shared-prefix` (`cache_prefix_chars`, `lib/prompt-layout.mjs`) — задача T37.
fn prompt_meta(messages: &[Value]) -> (&'static str, Value) {
    ("prompt_chars", json!(messages_chars(messages)))
}

/// Внутренний JSON-агрегат → [`ModelReview`].
///
/// Своя функция, а не `serde_json::from_value`: агрегат собирается как JS-объект
/// (ключ отсутствует ⇔ поля не было), и явное сопоставление сохраняет разницу
/// «нет ключа» / `null`, которую дефолтная десериализация стирает.
pub fn model_review_from_value(v: &Value) -> ModelReview {
    let opt_str = |key: &str| match v.get(key) {
        Some(x) if !x.is_null() => Some(js_to_string(x)),
        _ => None,
    };
    let flag = |key: &str| js_truthy(v.get(key).unwrap_or(&Value::Null));
    let count = |key: &str| match v.get(key).and_then(js_to_number) {
        Some(n) if n.is_finite() && n > 0.0 => n as usize,
        _ => 0,
    };
    let arr = |key: &str| match v.get(key) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    ModelReview {
        model: opt_str("model").unwrap_or_default(),
        summary: opt_str("summary"),
        findings: arr("findings"),
        error: opt_str("error"),
        skipped: flag("skipped"),
        skipped_chunks: count("skippedChunks") as u32,
        aggregator: opt_str("aggregator"),
        moa_references: arr("moaReferences").iter().map(js_to_string).collect(),
        references: arr("references"),
        fallback_for: opt_str("fallbackFor"),
        jury_bypassed: flag("juryBypassed"),
        demoted_out_of_chunk: count("demotedOutOfChunk"),
        usage: v.get("__usage").filter(|u| !u.is_null()).cloned(),
    }
}

// ---------------------------------------------------------------------------
// Тесты — ключевые случаи из test/pr-review.engine.test.mjs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(paths: serde_json::Value, extra: serde_json::Value) -> Value {
        let mut m = Map::new();
        if let Some(p) = paths.as_array() {
            m.insert("paths".into(), Value::Array(p.clone()));
        }
        if let Some(o) = extra.as_object() {
            for (k, v) in o {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }

    #[test]
    fn scope_priors_filters_by_chunk_files() {
        let priors = vec![
            json!({"file": "a.ts", "title": "a"}),
            json!({"file": "b.ts", "title": "b"}),
            json!({"file": "", "title": "no file"}),
        ];
        let c = chunk(json!(["a.ts"]), json!({}));
        let scoped = scope_priors(&priors, &c).unwrap();
        assert_eq!(scoped.len(), 2);
        assert_eq!(scoped[0]["title"], "a");
        assert_eq!(scoped[1]["title"], "no file");

        // пустые paths / unknownPaths → None (скоуп неполон)
        assert!(scope_priors(&priors, &chunk(json!([]), json!({}))).is_none());
        assert!(scope_priors(&priors, &chunk(json!(["a.ts"]), json!({"unknownPaths": true}))).is_none());
        assert!(scope_priors(&priors, &chunk(serde_json::Value::Null, json!({}))).is_none());
        // пустые priors → None (вход без изменений)
        assert!(scope_priors(&[], &c).is_none());
    }

    #[test]
    fn demote_out_of_chunk_lowers_invisible_blockers() {
        let res = json!({
            "model": "m1",
            "findings": [
                {"file": "b.ts", "line": 3, "severity": "critical", "title": "слепой", "confidence": 0.9},
                {"file": "a.ts", "line": 1, "severity": "major", "title": "виден", "confidence": 0.9},
                {"file": "", "line": 0, "severity": "minor", "title": "без файла", "confidence": 0.5},
                {"file": "b.ts", "line": 4, "severity": "nit", "title": "уже nit", "confidence": 0.3},
            ],
        });
        let c = chunk(json!(["a.ts"]), json!({}));
        let out = demote_out_of_chunk(&res, &c, 0);
        let f = out["findings"].as_array().unwrap();
        assert_eq!(f[0]["severity"], "nit");
        assert_eq!(f[0]["outOfChunk"], true);
        assert_eq!(f[0]["confidence"], 0.5);
        assert_eq!(f[1]["severity"], "major", "видимый файл не тронут");
        assert!(!f[1].as_object().unwrap().contains_key("outOfChunk"));
        assert_eq!(f[2]["severity"], "minor", "без файла не демоутится");
        assert_eq!(f[3]["severity"], "nit", "nit не демоутится повторно");
        assert_eq!(out["demotedOutOfChunk"], 1.0);
        assert_eq!(out["chunkIndex"], 0.0);

        // без демоута — никаких меток
        let clean = demote_out_of_chunk(&res, &chunk(json!(["a.ts", "b.ts"]), json!({})), 1);
        assert!(clean.as_object().unwrap().get("demotedOutOfChunk").is_none());
        assert!(clean.as_object().unwrap().get("chunkIndex").is_none());
        // скоуп неполон — как есть
        let untouched = demote_out_of_chunk(&res, &chunk(json!([]), json!({})), 0);
        assert_eq!(untouched["findings"].as_array().unwrap().len(), 4);
    }

    fn done_item(model: &str, res: serde_json::Value) -> Value {
        json!({ "model": model, "res": res })
    }

    #[test]
    fn aggregate_merges_chunks_and_dedupes() {
        let done = vec![
            done_item("m1", json!({
                "model": "m1", "summary": "s1", "verdict": "comment",
                "findings": [{"file": "a.ts", "line": 1, "severity": "minor", "title": "X", "confidence": 0.5, "model": "m1"}],
            })),
            done_item("m1", json!({
                "model": "m1", "summary": "", "verdict": "changes_requested",
                // та же находка из второго чанка — схлопывается
                "findings": [{"file": "a.ts", "line": 1, "severity": "minor", "title": "x", "confidence": 0.5, "model": "m1"}],
            })),
        ];
        let (agg, dropped) = aggregate_model("m1", &done);
        assert_eq!(dropped, 1);
        assert!(agg.get("verdict").is_none(), "поле verdict удалено из контракта агрегата");
        assert_eq!(agg["summary"], "s1", "первое непустое summary");
        assert_eq!(agg["findings"].as_array().unwrap().len(), 1);
        assert_eq!(agg["findings"][0]["dupCount"], 2.0);
    }

    #[test]
    fn aggregate_all_failed_and_all_skipped() {
        let done = vec![
            done_item("m1", json!({"model": "m1", "error": "down"})),
            done_item("m1", json!({"model": "m1", "error": "down2"})),
        ];
        let (agg, _) = aggregate_model("m1", &done);
        assert_eq!(agg["error"], "down", "первая ошибка");
        assert!(agg.get("skipped").is_none());

        let skipped_done = vec![
            done_item("m1", json!({"model": "m1", "error": "тайм-бюджет", "skipped": true})),
            done_item("m1", json!({"model": "m1", "error": "тайм-бюджет", "skipped": true})),
        ];
        let (agg, _) = aggregate_model("m1", &skipped_done);
        assert_eq!(agg["skipped"], true);

        // одна часть skipped, другая упала по ошибке — skipped НЕ выставляется
        let mixed = vec![
            done_item("m1", json!({"model": "m1", "error": "тайм", "skipped": true})),
            done_item("m1", json!({"model": "m1", "error": "boom"})),
        ];
        let (agg, _) = aggregate_model("m1", &mixed);
        assert_eq!(agg["error"], "тайм");
        assert!(agg.get("skipped").is_none(), "не все части skipped");

        let empty: Vec<Value> = vec![];
        let (agg, _) = aggregate_model("m1", &empty);
        assert_eq!(agg["error"], "нет ответа");
    }

    // -----------------------------------------------------------------------
    // RealEngine — порт test/pr-review.engine.test.mjs (блоки engine / engine MoA /
    // engine verifier). Блоки `ollama:` и `semaphore:` того же файла портированы
    // не сюда, а в llm.rs и concurrency.rs — там живёт их предмет.
    // -----------------------------------------------------------------------

    use std::sync::Mutex;

    use crate::llm::{ChatError, ChatJsonResult};

    type JsonFn = Box<dyn Fn(&Value, &str) -> Result<Value, String> + Send + Sync>;
    type TextFn = Box<dyn Fn(&Value, &str) -> Result<String, String> + Send + Sync>;

    /// Подмена `ollama` из JS-тестов (`{ chatJson, chatText }`): трейт [`LlmApi`] —
    /// ровно та же точка подмены, только типизированная.
    struct FakeLlm {
        on_json: JsonFn,
        on_text: TextFn,
        /// (модель, content последнего сообщения) — аналог `msgs.at(-1)?.content`.
        json_calls: Mutex<Vec<(String, String)>>,
        text_calls: Mutex<Vec<(String, String)>>,
        /// usage, которое фейк отдаёт через `on_usage` prose-вызова (`None` — не зовёт,
        /// как DI-мок, ничего не знающий про usage).
        text_usage: Option<Value>,
    }

    fn chat_err(message: String) -> ChatError {
        ChatError {
            message,
            status: None,
            empty: false,
            truncated: false,
            bad_escape: false,
            networky: false,
        }
    }

    fn last_content(messages: &Value) -> String {
        messages
            .as_array()
            .and_then(|a| a.last())
            .map(|m| js_to_string(m.get("content").unwrap_or(&Value::Null)))
            .unwrap_or_default()
    }

    impl FakeLlm {
        fn json(f: impl Fn(&Value, &str) -> Result<Value, String> + Send + Sync + 'static) -> Self {
            FakeLlm {
                on_json: Box::new(f),
                on_text: Box::new(|_, m| Err(format!("chatText не ожидался: {m}"))),
                json_calls: Mutex::new(Vec::new()),
                text_calls: Mutex::new(Vec::new()),
                text_usage: None,
            }
        }

        fn with_text(
            mut self,
            f: impl Fn(&Value, &str) -> Result<String, String> + Send + Sync + 'static,
        ) -> Self {
            self.on_text = Box::new(f);
            self
        }

        fn json_models(&self) -> Vec<String> {
            self.json_calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
        }

        fn json_prompts(&self) -> Vec<String> {
            self.json_calls.lock().unwrap().iter().map(|(_, c)| c.clone()).collect()
        }

        /// Модели prose-вызовов, отсортированные: порядок внутри `Promise.all`
        /// (и внутри пула потоков) не определён — в JS-тесте тоже `.sort()`.
        fn text_models_sorted(&self) -> Vec<String> {
            let mut v: Vec<String> =
                self.text_calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect();
            v.sort();
            v
        }
    }

    impl LlmApi for FakeLlm {
        fn chat_json(
            &self,
            messages: &Value,
            model: &str,
            _opts: &ChatJsonOpts,
        ) -> std::result::Result<ChatJsonResult, ChatError> {
            self.json_calls.lock().unwrap().push((model.to_string(), last_content(messages)));
            (self.on_json)(messages, model)
                .map(|value| ChatJsonResult { value, repaired: None, usage: None })
                .map_err(chat_err)
        }

        fn chat_text(
            &self,
            messages: &Value,
            model: &str,
            opts: &ChatTextOpts,
        ) -> std::result::Result<String, ChatError> {
            self.text_calls.lock().unwrap().push((model.to_string(), last_content(messages)));
            let out = (self.on_text)(messages, model).map_err(chat_err)?;
            if let (Some(u), Some(cb)) = (&self.text_usage, &opts.on_usage) {
                (cb.0)(u);
            }
            Ok(out)
        }
    }

    /// Замороженные часы всех тестов: дедлайн и duration_ms детерминированы.
    const NOW: u64 = 1_700_000_000_000;

    /// ENGINE_CFG из JS-теста.
    fn engine_cfg() -> EngineConfig {
        EngineConfig {
            review_mode: "consensus".to_string(),
            models: vec!["m1".to_string()],
            fallback: Vec::new(),
            moa_aggregator: "agg".to_string(),
            moa_label: "moa".to_string(),
            moa_references: Vec::new(),
            agg_temp: Some(0.4),
            temperature: Some(0.4),
            concurrency: 2,
            specialists_enabled: false,
            assignments: Vec::new(),
            require_quote: false,
            verifier_model: String::new(),
            verifier_temperature: None,
        }
    }

    /// MOA_CFG из JS-теста.
    fn moa_cfg() -> EngineConfig {
        EngineConfig {
            review_mode: "moa".to_string(),
            models: Vec::new(),
            fallback: Vec::new(),
            moa_aggregator: "agg-model".to_string(),
            moa_label: "moa".to_string(),
            moa_references: vec!["ref-a".to_string(), "ref-b".to_string()],
            agg_temp: Some(0.4),
            temperature: Some(0.4),
            concurrency: 4,
            specialists_enabled: true,
            assignments: vec![
                SpecialistAssignment { role: "security".into(), model: "sec-m".into() },
                SpecialistAssignment { role: "correctness".into(), model: "corr-m".into() },
                SpecialistAssignment { role: "architecture".into(), model: "arch-m".into() },
            ],
            require_quote: false,
            verifier_model: "ver-m".to_string(),
            verifier_temperature: Some(0.2),
        }
    }

    fn mk_engine<'a>(
        llm: &'a FakeLlm,
        cfg: EngineConfig,
        log: &'a (dyn Fn(&str) + Send + Sync),
    ) -> RealEngine<'a> {
        RealEngine::new(cfg, llm, PromptCtx::default_ctx(), log).with_clock(fixed_clock(NOW))
    }

    fn chunk_of(annotated: &str, paths: &[&str], partial: &[&str], unknown: bool) -> Chunk {
        Chunk {
            annotated: annotated.to_string(),
            file_contents: String::new(),
            unknown_paths: unknown,
            paths: paths.iter().map(|s| s.to_string()).collect(),
            partial_paths: partial.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// CHUNKS из JS-теста: состав неизвестен (paths пуст) — скоуп и демоут выключены.
    fn plain_chunks() -> Vec<Chunk> {
        vec![chunk_of("d1", &[], &[], false), chunk_of("d2", &[], &[], false)]
    }

    /// SCOPED_CHUNKS из JS-теста.
    fn scoped_chunks() -> Vec<Chunk> {
        vec![chunk_of("d1", &["a.ts"], &[], false), chunk_of("d2", &["b.ts"], &[], false)]
    }

    fn ok_json(_: &Value, _: &str) -> Result<Value, String> {
        Ok(json!({"summary": "s", "verdict": "approve", "findings": []}))
    }

    fn priors() -> Vec<Value> {
        vec![
            json!({"file": "a.ts", "line": 1, "severity": "major", "title": "по файлу первого чанка"}),
            json!({"file": "b.ts", "line": 2, "severity": "major", "title": "по файлу второго чанка"}),
            json!({"file": "", "line": 0, "severity": "minor", "title": "без файла — видна всем"}),
        ]
    }

    /// Промпты, доехавшие до модели, для заданных чанков и контекста.
    fn prompts_for(chunks: &[Chunk], ctx: &ReviewContext) -> Vec<String> {
        let llm = FakeLlm::json(ok_json);
        let log = |_: &str| {};
        let engine = mk_engine(&llm, engine_cfg(), &log);
        engine.review_all_models(chunks, ctx).expect("движок не бросает");
        llm.json_prompts()
    }

    #[test]
    fn deadline_skips_chunks_llm_not_called_fallback_not_started() {
        let llm = FakeLlm::json(|_, _| panic!("LLM не должен вызываться после дедлайна"));
        let log = |_: &str| {};
        let mut cfg = engine_cfg();
        cfg.fallback = vec!["m2".to_string()];
        let engine = mk_engine(&llm, cfg, &log);
        let ctx = ReviewContext { deadline: NOW - 1000, ..Default::default() };

        let res = engine.review_all_models(&plain_chunks(), &ctx).unwrap();
        assert!(llm.json_models().is_empty(), "ни одного LLM-вызова после дедлайна");
        assert_eq!(res[0].skipped_chunks, 2);
        assert!(res[0].skipped, "агрегат помечен skipped, а не «модель недоступна»");
        assert!(res[0].error.as_deref().unwrap().contains("тайм-бюджет"));
        assert_eq!(res[0].model, "m1", "резерв не подставлен вместо skipped");
    }

    #[test]
    fn primary_down_falls_back_and_marks_fallback_for() {
        let llm = FakeLlm::json(|_, model| {
            if model == "m1" {
                Err("модель лежит".to_string())
            } else {
                Ok(json!({"summary": "ok", "verdict": "approve", "findings": []}))
            }
        });
        let log = |_: &str| {};
        let mut cfg = engine_cfg();
        cfg.fallback = vec!["m2".to_string()];
        let engine = mk_engine(&llm, cfg, &log);

        let res = engine
            .review_all_models(&[chunk_of("d1", &[], &[], false)], &ReviewContext::default())
            .unwrap();
        assert_eq!(res[0].model, "m2");
        assert_eq!(res[0].fallback_for.as_deref(), Some("m1"));
        assert!(res[0].error.is_none());
    }

    #[test]
    fn findings_normalized_and_aggregated_over_chunks() {
        // Разные находки по чанкам: иначе тест не отличить от проверки дедупа.
        let llm = FakeLlm::json(|messages, _| {
            let user = last_content(messages);
            let file = if user.contains("d1") { "d1.ts" } else { "d2.ts" };
            Ok(json!({
                "summary": "s", "verdict": "comment",
                "findings": [{"file": file, "line": 1, "severity": "minor", "title": "t", "confidence": 0.5}],
            }))
        });
        let log = |_: &str| {};
        let engine = mk_engine(&llm, engine_cfg(), &log);

        let res = engine.review_all_models(&plain_chunks(), &ReviewContext::default()).unwrap();
        assert_eq!(res[0].findings.len(), 2, "по находке с каждого из двух чанков");
        assert_eq!(res[0].findings[0]["model"], "m1");
        // `verdict` модели из агрегата удалён (lib/engine.mjs): гейт решает по находкам.
        let v = serde_json::to_value(&res[0]).unwrap();
        assert!(v.get("verdict").is_none(), "verdict модели не протекает в агрегат");
    }

    // Регрессия PR #520: одна тема из нескольких чанков попадала в sticky N раз.
    // Между МОДЕЛЯМИ дедупа быть не должно: там повтор — сигнал консенсуса.
    #[test]
    fn same_finding_from_different_chunks_collapses_within_model() {
        let same = |_: &Value, _: &str| {
            Ok(json!({
                "summary": "s", "verdict": "comment",
                "findings": [{"file": "a.ts", "line": 1, "severity": "minor", "title": "Один и тот же дефект", "confidence": 0.5}],
            }))
        };
        let log = |_: &str| {};

        let llm = FakeLlm::json(same);
        let one = mk_engine(&llm, engine_cfg(), &log)
            .review_all_models(&plain_chunks(), &ReviewContext::default())
            .unwrap();
        assert_eq!(one[0].findings.len(), 1, "дубль из второго чанка схлопнут");
        assert_eq!(one[0].findings[0]["dupCount"], 2.0, "число повторов сохранено");

        let llm2 = FakeLlm::json(same);
        let mut two_models = engine_cfg();
        two_models.models = vec!["m1".to_string(), "m2".to_string()];
        let both = mk_engine(&llm2, two_models, &log)
            .review_all_models(&plain_chunks(), &ReviewContext::default())
            .unwrap();
        assert_eq!(both.iter().map(|r| r.findings.len()).collect::<Vec<_>>(), vec![1, 1]);
        // Атрибуция, а не только счёт: кластеризация считает консенсус по НАБОРУ
        // моделей, и подмена model схлопнула бы две модели в одну.
        assert_eq!(
            both.iter().map(|r| js_to_string(&r.findings[0]["model"])).collect::<Vec<_>>(),
            vec!["m1", "m2"],
        );
    }

    // Корень несходящегося ревью на PR #520: карту PR получал только чанк 0,
    // а prior findings — все чанки.
    #[test]
    fn pr_map_in_every_chunk_priors_scoped_to_chunk_files() {
        let ctx = ReviewContext {
            changed_paths: vec!["a.ts".to_string(), "b.ts".to_string()],
            prior_findings: priors(),
            ..Default::default()
        };
        let prompts = prompts_for(&scoped_chunks(), &ctx);
        assert_eq!(prompts.len(), 2, "по вызову на чанк");
        for p in &prompts {
            assert!(p.contains("## All changed files in the PR (2)"), "карта PR в каждом чанке");
            assert!(p.contains("`a.ts`") && p.contains("`b.ts`"), "полный список файлов PR");
        }
        let first = prompts.iter().find(|p| p.contains("d1")).expect("промпт чанка 0");
        let second = prompts.iter().find(|p| p.contains("d2")).expect("промпт чанка 1");
        assert!(first.contains("по файлу первого чанка"));
        assert!(first.contains("без файла — видна всем"));
        assert!(!first.contains("по файлу второго чанка"), "чужой prior не доехал");
        assert!(second.contains("по файлу второго чанка"));
        assert!(second.contains("без файла — видна всем"));
        assert!(!second.contains("по файлу первого чанка"));

        // Частично видимый файл — свой: prior по нему приходит.
        let partial = prompts_for(&[chunk_of("d", &["a.ts"], &["a.ts"], false)], &ctx);
        assert!(partial[0].contains("по файлу первого чанка"));
        assert!(partial[0].contains("без файла — видна всем"));
        assert!(!partial[0].contains("по файлу второго чанка"));

        // Состав чанка неполный — скоуп и понижение отключаются.
        let unknown = prompts_for(&[chunk_of("d", &["a.ts"], &[], true)], &ctx);
        assert!(unknown[0].contains("по файлу второго чанка"), "при неизвестном составе priors не режем");
    }

    #[test]
    fn out_of_chunk_finding_demoted_to_nit_and_marked() {
        let llm = FakeLlm::json(|_, _| {
            Ok(json!({
                "summary": "s", "verdict": "changes_requested",
                "findings": [
                    {"file": "b.ts", "line": 3, "severity": "critical", "title": "слепой блокер", "confidence": 0.9},
                    {"file": "a.ts", "line": 1, "severity": "major", "title": "виден в этом чанке", "confidence": 0.9},
                ],
            }))
        });
        let log = |_: &str| {};
        let engine = mk_engine(&llm, engine_cfg(), &log);
        let ctx = ReviewContext {
            changed_paths: vec!["a.ts".to_string(), "b.ts".to_string()],
            ..Default::default()
        };

        let res = engine.review_all_models(&scoped_chunks()[..1], &ctx).unwrap();
        let invisible: Vec<&Value> =
            res[0].findings.iter().filter(|f| f["file"] == "b.ts").collect();
        let visible: Vec<&Value> = res[0].findings.iter().filter(|f| f["file"] == "a.ts").collect();
        assert_eq!(invisible.len(), 1);
        assert_eq!(invisible[0]["severity"], "nit", "блокер по невидимому файлу не блокирует merge");
        assert_eq!(invisible[0]["outOfChunk"], true);
        assert!(js_to_number(&invisible[0]["confidence"]).unwrap() <= 0.5);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0]["severity"], "major", "находка по видимому файлу не тронута");
    }

    #[test]
    fn moa_specialists_feed_aggregator_and_relabel_findings() {
        let llm = FakeLlm::json(|_, _| {
            Ok(json!({
                "summary": "jury",
                "verdict": "changes_requested",
                "findings": [{
                    "file": "a.ts", "line": 3, "severity": "critical", "title": "IDOR",
                    "body": "open", "confidence": 0.9,
                    "advisorClaims": [
                        {"role": "security", "status": "verified", "reason": "real"},
                        {"role": "correctness", "status": "rejected", "reason": "n/a"},
                        {"role": "architecture", "status": "downgraded", "severity": "minor", "reason": "layer"},
                    ],
                }],
            }))
        })
        .with_text(|_, model| Ok(format!("prose from {model}")));
        let log = |_: &str| {};
        let engine = mk_engine(&llm, moa_cfg(), &log);

        let res = engine
            .review_all_models(&[chunk_of("d1", &["a.ts"], &[], false)], &ReviewContext::default())
            .unwrap();
        let res = &res[0];
        assert_eq!(llm.text_models_sorted(), vec!["arch-m", "corr-m", "sec-m"]);
        assert_eq!(llm.json_models(), vec!["agg-model"], "ровно один вызов жюри");
        assert_eq!(res.model, "moa");
        assert_eq!(res.aggregator.as_deref(), Some("agg-model"));
        assert_eq!(res.moa_references, vec!["ref-a", "ref-b"]);
        assert_eq!(res.findings.len(), 1);
        assert_eq!(res.findings[0]["model"], "moa", "атрибуция — label MoA, не agg-model");
        assert_eq!(res.findings[0]["advisorClaims"].as_array().unwrap().len(), 3);
        assert_eq!(res.findings[0]["advisorClaims"][0]["status"], "verified");
        assert!(res.references.len() >= 3, "specialist references смержены в результат");
        assert!(res
            .references
            .iter()
            .all(|r| js_to_string(&r["text"]).contains("prose from")));
    }

    #[test]
    fn moa_all_specialists_down_skips_aggregator() {
        let llm = FakeLlm::json(|_, _| Ok(json!({"findings": []})))
            .with_text(|_, _| Err("down".to_string()));
        let log = |_: &str| {};
        let engine = mk_engine(&llm, moa_cfg(), &log);

        let res = engine
            .review_all_models(&[chunk_of("d1", &["a.ts"], &[], false)], &ReviewContext::default())
            .unwrap();
        assert!(llm.json_models().is_empty(), "жюри не зовём без единого advisor'а");
        let err = res[0].error.as_deref().expect("ошибка без успешного жюри");
        assert!(err.contains("security/sec-m"), "в ошибке роль и модель: {err}");
    }

    // -- verifier Pass B ----------------------------------------------------

    fn verify_ctx() -> VerifyContext {
        VerifyContext { annotated: "d".to_string(), ..Default::default() }
    }

    fn candidates() -> Vec<Value> {
        vec![
            json!({"file": "a.ts", "line": 1, "severity": "critical", "title": "x"}),
            json!({"file": "b.ts", "line": 2, "severity": "major", "title": "y"}),
        ]
    }

    #[test]
    fn verifier_verdicts_reach_caller() {
        let llm = FakeLlm::json(|_, _| {
            Ok(json!({"verdicts": [
                {"candidateIndex": 0, "decision": "rejected", "reason": "fp"},
                {"candidateIndex": 1, "decision": "verified", "reason": "ok"},
            ]}))
        });
        let log = |_: &str| {};
        let engine = mk_engine(&llm, moa_cfg(), &log);

        let verdicts = engine.verify_findings(&candidates(), &verify_ctx()).unwrap();
        assert_eq!(llm.json_models(), vec!["ver-m"], "Pass B идёт отдельной моделью");
        assert_eq!(verdicts.len(), 2);
        assert_eq!(verdicts[0]["candidateIndex"], 0);
        assert_eq!(verdicts[0]["decision"], "rejected");
        assert_eq!(verdicts[1]["candidateIndex"], 1);
        assert_eq!(verdicts[1]["decision"], "verified");
    }

    #[test]
    fn verifier_event_carries_prompt_chars_on_ok_and_error() {
        // lib/verifier.mjs: prompt_chars пишется и в ok-, и в error-событие.
        let log = |_: &str| {};
        for fail in [false, true] {
            let llm = FakeLlm::json(move |_, _| {
                if fail { Err("verifier timeout".to_string()) } else { Ok(json!({"verdicts": []})) }
            });
            let events: Mutex<Vec<Value>> = Mutex::new(Vec::new());
            let sink = |_: &str, payload: &Value| events.lock().unwrap().push(payload.clone());
            let engine = mk_engine(&llm, moa_cfg(), &log).with_events(&sink);
            let _ = engine.verify_findings(&candidates(), &verify_ctx());
            let evs = events.lock().unwrap();
            assert_eq!(evs.len(), 1);
            assert_eq!(evs[0]["ok"], !fail);
            assert!(evs[0]["prompt_chars"].as_u64().unwrap() > 0);
            assert!(evs[0].get("total_tokens").is_none(), "usage нет — полей токенов нет");
        }
    }

    #[test]
    fn verifier_without_verdicts_field_returns_empty() {
        let llm = FakeLlm::json(|_, _| Ok(json!({"summary": "oops no verdicts"})));
        let log = |_: &str| {};
        let engine = mk_engine(&llm, moa_cfg(), &log);

        let verdicts = engine.verify_findings(&candidates()[..1], &verify_ctx()).unwrap();
        assert!(verdicts.is_empty(), "strict apply понизит находку снаружи");

        // Пустой список кандидатов — LLM не зовём вовсе.
        assert!(engine.verify_findings(&[], &verify_ctx()).unwrap().is_empty());
        assert_eq!(llm.json_models().len(), 1, "второй вызов без кандидатов не состоялся");
    }

    #[test]
    fn verifier_llm_error_propagates() {
        let llm = FakeLlm::json(|_, _| Err("verifier timeout".to_string()));
        let log = |_: &str| {};
        let engine = mk_engine(&llm, moa_cfg(), &log);

        let err = engine.verify_findings(&candidates()[..1], &verify_ctx()).unwrap_err();
        assert!(err.to_string().contains("verifier timeout"), "пайплайн решает fail-closed");
    }

    // -- наблюдаемость и часы ----------------------------------------------

    #[test]
    fn observability_events_carry_meta_and_frozen_duration() {
        let llm = FakeLlm::json(|_, _| {
            Ok(json!({"summary": "s", "verdict": "comment", "findings": [
                {"file": "a.ts", "line": 1, "severity": "minor", "title": "t", "confidence": 0.5},
            ]}))
        });
        let log = |_: &str| {};
        let events: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());
        let sink = |name: &str, payload: &Value| {
            events.lock().unwrap().push((name.to_string(), payload.clone()));
        };
        let engine = mk_engine(&llm, engine_cfg(), &log).with_events(&sink);

        engine
            .review_all_models(&[chunk_of("d1", &[], &[], false)], &ReviewContext::default())
            .unwrap();
        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 1);
        let (name, ev) = &evs[0];
        assert_eq!(name, "llm_call");
        assert_eq!(ev["callType"], "review_json");
        assert_eq!(ev["model"], "m1");
        assert_eq!(ev["chunkIndex"], 0);
        assert_eq!(ev["reviewMode"], "consensus");
        assert_eq!(ev["ok"], true);
        assert_eq!(ev["findings"], 1);
        // Часы заморожены — длительность детерминирована (требование golden-прогонов).
        assert_eq!(ev["duration_ms"], 0);
        assert!(ev.get("error").is_none(), "ok-событие без ключа error");
        // promptMeta (legacy): размер входа — детерминированный прокси токенов.
        assert!(ev["prompt_chars"].as_u64().unwrap() > 0);
        let keys: Vec<&String> = ev.as_object().unwrap().keys().collect();
        assert_eq!(
            keys[..5],
            ["callType", "model", "chunkIndex", "reviewMode", "prompt_chars"],
            "порядок ключей как в JS-литерале"
        );
    }

    #[test]
    fn specialist_events_carry_usage_from_on_usage() {
        // lib/engine.mjs trackText: токены prose-вызова приходят боковым каналом onUsage.
        let mut llm = FakeLlm::json(|_, _| Ok(json!({"findings": []})))
            .with_text(|_, model| Ok(format!("prose from {model}")));
        llm.text_usage = Some(json!({"prompt_tokens": 11, "completion_tokens": 2, "total_tokens": 13}));
        let log = |_: &str| {};
        let events: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());
        let sink = |name: &str, payload: &Value| {
            events.lock().unwrap().push((name.to_string(), payload.clone()));
        };
        let engine = mk_engine(&llm, moa_cfg(), &log).with_events(&sink);
        engine
            .review_all_models(&[chunk_of("d1", &["a.ts"], &[], false)], &ReviewContext::default())
            .unwrap();
        let evs = events.lock().unwrap();
        let specs: Vec<&Value> =
            evs.iter().filter(|(_, e)| e["callType"] == "specialist").map(|(_, e)| e).collect();
        assert_eq!(specs.len(), 3);
        for e in specs {
            assert_eq!(e["total_tokens"], 13);
            assert_eq!(e["prompt_tokens"], 11);
            assert!(e["prompt_chars"].as_u64().unwrap() > 0);
            assert!(e["text_chars"].as_u64().unwrap() > 0);
        }
    }

    #[test]
    fn messages_chars_counts_utf16_and_skips_missing_content() {
        let msgs = vec![
            json!({"role": "system", "content": "аб"}),
            json!({"role": "user", "content": "😀"}),
            json!({"role": "user"}),
            json!({"role": "user", "content": null}),
        ];
        assert_eq!(messages_chars(&msgs), 4);
    }
}
