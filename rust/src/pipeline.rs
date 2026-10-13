//! Порт `pipeline.mjs` (RFC 20, задача T26): `createReviewPipeline` → [`ReviewPipeline`],
//! `runDetailed` → [`ReviewPipeline::run_detailed`], `runGateResolution` →
//! [`run_gate_resolution`].
//!
//! Что изменилось против JS и почему:
//!
//! - **`die`/`process.exit` → [`PipelineOutcome`].** В JS мягкий отказ (нет ключа, пустой
//!   дифф, PR-черновик) — это `process.exit(0)` из середины стадии в CLI и исключение
//!   `PipelineSoftFail` в MCP. Здесь путь один — MCP-семантика: отказ возвращается
//!   значением с `soft_fail = true`, а `run_end` в observability пишется и в CLI (в JS
//!   `process.exit` обрывал прогон до `finally`, и событие терялось — молча).
//! - **Ссылки → индексы.** JS ходит по ОДНИМ И ТЕМ ЖЕ объектам находок: маршрут, refutation,
//!   verifier и гейт пишут поля прямо в них. Чистые Rust-стадии возвращают мутации по индексу
//!   (тот же контракт, что у JSON-бинарей), и пайплайн накладывает их на свой `Vec<Value>`.
//!   Блокеры поэтому держатся индексами до последнего момента: их значения
//!   материализуются ПОСЛЕ маршрута, иначе в артефакт ушли бы блокеры без
//!   `actionabilityScore` — в JS их несёт разделяемая ссылка.
//! - **Сотрудники, а не `deps`.** `repoFiles`, `readLines`, `headSha`, `costTracker`,
//!   часы и observability — [`PipelineOptions`], потому что `PipelineDeps` (трейты T08)
//!   заморожен и несёт только внешние эффекты.
//!
//! Инварианты, на которых держится гейт (CLAUDE.md), — в коде отмечены на месте:
//! порядок refutation → verifier → перенос инкрементальных находок → гейт; блок требует
//! жюри + verifier + confidence; fail-open на инфра-сбоях; отпечаток находки считается один
//! раз и уходит и в футер sticky, и в артефакт.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Map, Value};

use crate::absent_path::{build_repo_path_index, list_repo_files, RepoPathIndex};
use crate::actionability::{
    classify_findings, finding_location_key, route_findings, FilesMap, RouteCfg, ScoreCtx, ScoredItem, SIGNALS,
};
use crate::blast_findings::{emit_blast_radius_findings, BlastFindingsOpts};
use crate::config::load_review_config_parts;
use crate::context_window::{build_verifier_file_contents, window_chunk_file_contents};
use crate::cost::{efficiency_metrics, CostTracker};
use crate::diff::{
    build_chunks, collect_added_paths, collect_blast_symbols, collect_deleted_paths, parse_diff, read_changed_files_in, u16_len, Chunk,
    ChunkLimits,
};
use crate::engine::{prompt_ctx_from_profile, ChunkRouting};
use crate::export::build_review_json;
use crate::feedback::js_stringify_pretty;
use crate::findings::{
    filter_false_positive_findings, filter_stale_prior_findings, finding_fingerprint, js_to_number, js_to_string,
    js_trim, js_truthy, resolve_blockers, Gate, LineText, PriorFileRec, BLAST_FINDING_SOURCE,
};
use crate::fp_patterns::{default_fp_patterns, load_fp_patterns, FpPattern};
use crate::incremental::{
    build_next_state, config_fingerprint, file_input_hashes, format_incremental_note, plan_incremental,
    read_incremental_state, reuse_prior_cards_input, write_incremental_state, CarryOver, IncrementalPlan,
    unverified_paths, NextStateInput, PlanInput,
};
use crate::metrics::{count_by_severity, slim_findings};
use crate::observability::{iso_from_epoch_ms, Observability, ObservabilityConfig, PR_TITLE_LOG_MAX};
use crate::prescan::{dedupe_against_prescan, format_prescan_log, prescan_result_to_value};
use crate::profile::Profile;
use crate::prompt::build_changed_files_section;
use crate::report::{
    build_llm_unavailable_summary, build_router_summary, build_summary, js_slice_utf16, public_error_label,
};
use crate::router::{classify_file_path, extract_file_diff_sections, format_route_log, route_diff, RouteResult};
use crate::specialists::{apply_refutation, format_refutation_log};
use crate::symbol_cards::{
    build_symbol_cards, is_absence_claim, select_chunk_cards, symbol_cards_hash_input, symbol_cards_outcome, BuildOpts,
    LineReader, ReadLines, SymbolCards,
};
use crate::traits::{
    BlastMetrics, DeleteStaleOpts, InlineComment, ModelReview, PipelineDeps, PostReviewParams, PrescanResult, PullRequestRef,
    ReviewContext, StickyData, VerifyContext,
};
use crate::triage::{build_triage_context, parse_trusted_authors, run_auto_triage};
use crate::verifier::{apply_verifier_verdicts, select_verifier_candidates};

// ---------------------------------------------------------------------------
// Конфиг прогона
// ---------------------------------------------------------------------------

/// Разобранный CLI (`parseCli`, `lib/config.mjs:62-74`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliArgs {
    pub dry_run: bool,
    /// `--diff <файл>`; `None` — дифф берётся из GitHub.
    pub diff_file: Option<String>,
    /// `--head-sha` — SHA головы для file-diff прогона (MCP, локальный dry-run).
    pub head_sha: String,
    pub repo: String,
    pub pr_number: u64,
}

impl CliArgs {
    /// `cfg.cli` как JSON: отсутствующий `--diff` — ключа нет (`undefined` в JS).
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("dryRun".into(), json!(self.dry_run));
        if let Some(d) = &self.diff_file {
            m.insert("diffFile".into(), json!(d));
        }
        m.insert("headSha".into(), json!(self.head_sha));
        m.insert("repo".into(), json!(self.repo));
        m.insert("prNumber".into(), json!(self.pr_number));
        Value::Object(m)
    }
}

/// `assembleReviewConfig` (`lib/config.mjs:130`): части из `load_review_config_parts` +
/// CLI + ссылка на профиль. Профиль в JSON — только `name`/`source`: их читает
/// `configFingerprint`, остальное пайплайн берёт из `&Profile` напрямую.
pub fn assemble_review_config(parts: &Value, cli: &CliArgs, profile: &Profile) -> Value {
    let mut out = parts.as_object().cloned().unwrap_or_default();
    out.insert("cli".into(), cli.to_value());
    out.insert("profile".into(), json!({ "name": profile.name, "source": profile.source }));
    let github = out.entry("github").or_insert_with(|| json!({}));
    if let Some(g) = github.as_object_mut() {
        g.insert("repo".into(), json!(cli.repo));
        g.insert("prNumber".into(), json!(cli.pr_number));
    }
    Value::Object(out)
}

/// `loadReviewConfig(env, cli)` поверх уже разрешённого профиля.
pub fn load_review_config(env: &HashMap<String, String>, cli: &CliArgs, profile: &Profile) -> Value {
    let env_map: Map<String, Value> = env.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
    let parts = load_review_config_parts(&env_map, &profile.config_input());
    assemble_review_config(&parts, cli, profile)
}

// ---------------------------------------------------------------------------
// Исход прогона
// ---------------------------------------------------------------------------

/// Что записал экспорт R9 (`writeReviewExports`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportResult {
    pub dir: PathBuf,
    pub written: Vec<PathBuf>,
    pub sarif_results: usize,
}

/// Итог `runDetailed` — вместо `process.exit`/`die` (RFC 20, решение к T26).
///
/// `exit_code` — то, с чем выйдет `pr-review review`: `1` только на подтверждённых
/// блокерах (или осознанном fail-closed `REVIEW_LLM_REQUIRED=1`), всё остальное — `0`.
#[derive(Debug, Clone, Default)]
pub struct PipelineOutcome {
    pub exit_code: i32,
    /// Артефакт экспорта в памяти — MCP и тесты не зависят от записи на диск.
    pub artifact: Option<Value>,
    pub export_result: Option<ExportResult>,
    pub would_block: bool,
    /// Прогон не дошёл до полного ревью (router skip/light, LLM-аутедж, мягкий отказ).
    pub degraded: bool,
    pub reason: Option<String>,
    /// Мягкий отказ (`die` в JS): дальше первой проверки прогон не пошёл.
    pub soft_fail: bool,
}

/// Строка лога мягкого отказа CLI — `log('SOFT-FAIL:', msg)` дефолтного `die`.
pub fn soft_fail_line(reason: &str) -> String {
    format!("SOFT-FAIL: {reason}")
}

/// Мягкий отказ пайплайна — порт `PipelineSoftFail`. Живёт как тип ошибки, чтобы
/// `?` из глубины стадии выходил на верхний уровень, где его отличают от аварии.
#[derive(Debug)]
pub struct PipelineSoftFail(pub String);

impl std::fmt::Display for PipelineSoftFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PipelineSoftFail {}

fn die(msg: &str) -> anyhow::Error {
    anyhow::Error::new(PipelineSoftFail(msg.to_string()))
}

// ---------------------------------------------------------------------------
// Опции (сотрудники, которых нет в замороженном PipelineDeps)
// ---------------------------------------------------------------------------

/// Дерево репозитория для FP-семейства 7 (`deps.repoFiles`). `Err` и `Ok(None)` —
/// семейство выключено в этом прогоне (fail-open).
pub type RepoFilesFn = dyn Fn() -> Result<Option<Vec<String>>> + Send + Sync;
/// Часы прогона, эпоха в мс (дедлайн LLM-фазы, `generatedAt` артефакта).
pub type ClockFn = dyn Fn() -> u64 + Send + Sync;

/// Опциональные сотрудники прогона — остаток JS-`deps` (`pipeline.mjs:314-328`).
/// Всё `None` — боевые дефолты: `git ls-files`, чтение рабочего дерева, системные часы.
#[derive(Default)]
pub struct PipelineOptions<'a> {
    pub repo_files: Option<&'a RepoFilesFn>,
    /// Строки файла рабочего дерева для карточек символов (RFC 14).
    pub read_lines: Option<&'a ReadLines<'a>>,
    /// `deps.headSha` — перебивает `cli.headSha` на file-diff прогоне.
    pub head_sha: Option<String>,
    /// Готовый логгер прогона — тот же, что слушает движок (`with_events`).
    pub observability: Option<&'a Observability>,
    pub clock: Option<&'a ClockFn>,
    /// Корень рабочего дерева (чтение изменённых файлов, `git ls-files`, карточки).
    pub cwd: Option<PathBuf>,
}

/// Маршрутизация чанков движка (RFC 04, решение 2) из профиля: класс пути —
/// [`classify_file_path`], чувствительные пути — `layout.sensitive`.
///
/// Пайплайн получает движок готовым (`&dyn EngineApi`), поэтому связывает их тот, кто
/// строит `RealEngine`: `RealEngine::…with_chunk_routing(fns.routing())`.
pub struct ChunkRoutingFns<'p> {
    file_class: Box<dyn Fn(&str) -> String + Send + Sync + 'p>,
    sensitive: Option<Box<dyn Fn(&str) -> bool + Send + Sync + 'p>>,
}

impl<'p> ChunkRoutingFns<'p> {
    pub fn from_profile(profile: &'p Profile) -> Self {
        let sensitive: Option<Box<dyn Fn(&str) -> bool + Send + Sync + 'p>> =
            profile.layout().sensitive().map(|re| {
                // Ошибка исполнения backtracking-регекса потребителя — «не совпало».
                Box::new(move |p: &str| re.is_match(p).unwrap_or(false)) as Box<dyn Fn(&str) -> bool + Send + Sync>
            });
        Self { file_class: Box::new(|p: &str| classify_file_path(p).to_string()), sensitive }
    }

    pub fn routing(&self) -> ChunkRouting<'_> {
        ChunkRouting { file_class: Some(&*self.file_class), sensitive: self.sensitive.as_deref() }
    }
}

// ---------------------------------------------------------------------------
// Гейт
// ---------------------------------------------------------------------------

/// Вход [`run_gate_resolution`].
pub struct GateInput<'a> {
    pub findings: Vec<Value>,
    pub changed_paths: &'a [String],
    /// `cfg.gate` целиком.
    pub gate: &'a Value,
    /// `cfg.prescan` — политика блокировки prescan-находок.
    pub prescan_cfg: Option<&'a Value>,
    /// `cfg.feedback.patternsFile`; пусто — вшитый набор пакета.
    pub patterns_file: Option<&'a str>,
    pub repo_index: Option<&'a RepoPathIndex>,
    pub log: &'a (dyn Fn(&str) + Send + Sync),
}

/// Что вернул `beforeResolve`.
#[derive(Debug, Clone, Default)]
pub struct BeforeResolved {
    pub findings: Vec<Value>,
    pub refutation_stats: Option<Value>,
    pub verifier_stats: Option<Value>,
    /// `{reason, unverified}` — verifier недоступен (RFC 18).
    pub verifier_unavailable: Option<Value>,
}

/// Результат `runGateResolution`. Кластеры и блокеры — индексы в `findings`.
#[derive(Debug, Clone, Default)]
pub struct GateResolution {
    pub findings: Vec<Value>,
    pub clusters: Vec<Vec<usize>>,
    pub blockers: Vec<usize>,
    pub would_block: bool,
    /// Находки, снятые FP-паттернами и семейством 7 (sticky показывает их отдельно).
    pub fp_removed: Vec<Value>,
    pub before_fp_count: usize,
    pub after_fp_count: usize,
    pub refutation_stats: Option<Value>,
    pub verifier_stats: Option<Value>,
    pub verifier_unavailable: Option<Value>,
}

/// Хук между FP-фильтром и гейтом: refutation, verifier, перенос (`beforeResolve`).
pub type BeforeResolve<'b> = dyn FnMut(Vec<Value>) -> Result<BeforeResolved> + 'b;

fn fp_patterns(patterns_file: Option<&str>) -> FpPatterns {
    match patterns_file {
        Some(p) if !p.is_empty() => FpPatterns::Owned(load_fp_patterns(Path::new(p))),
        _ => FpPatterns::Embedded(default_fp_patterns()),
    }
}

enum FpPatterns {
    Embedded(&'static [FpPattern]),
    Owned(Vec<FpPattern>),
}

impl FpPatterns {
    fn as_slice(&self) -> &[FpPattern] {
        match self {
            FpPatterns::Embedded(p) => p,
            FpPatterns::Owned(v) => v,
        }
    }
}

/// `runGateResolution` (`pipeline.mjs:299-311`): FP-фильтр → `before_resolve` →
/// `resolveBlockers`. Единый путь для ветки LLM-аутеджа и основного прогона.
///
/// FP-фильтр идёт ДО LLM-стадий: verifier не тратит токены на известные ложные находки,
/// а refutation не понижает уже отфильтрованное.
pub fn run_gate_resolution(input: GateInput<'_>, before_resolve: Option<&mut BeforeResolve<'_>>) -> Result<GateResolution> {
    let before_fp = input.findings;
    let before_fp_count = before_fp.len();
    let patterns = fp_patterns(input.patterns_file);
    let fp = filter_false_positive_findings(&before_fp, input.changed_paths, patterns.as_slice(), input.repo_index);
    for line in &fp.logs {
        (input.log)(line);
    }
    let kept: HashSet<usize> = fp.kept.iter().copied().collect();
    let mut after_fp = Vec::with_capacity(fp.kept.len());
    let mut fp_removed = Vec::new();
    for (i, f) in before_fp.into_iter().enumerate() {
        if kept.contains(&i) {
            after_fp.push(f);
        } else {
            fp_removed.push(f);
        }
    }
    let after_fp_count = after_fp.len();
    let intermediate = match before_resolve {
        Some(hook) => hook(after_fp)?,
        None => BeforeResolved { findings: after_fp, ..BeforeResolved::default() },
    };
    let mut findings = intermediate.findings;
    let gate = Gate::from_value(input.gate);
    let resolved = resolve_blockers(&findings, &gate, input.prescan_cfg);
    // Мутации гейта — на свои находки (в JS — на разделяемые объекты).
    for m in &resolved.mutations {
        if let Some(o) = findings.get_mut(m.index).and_then(Value::as_object_mut) {
            o.insert("confidence".into(), num(m.confidence));
            o.insert("consensus".into(), json!(m.consensus));
            o.insert("clusterModels".into(), Value::Array(m.cluster_models.clone()));
            o.insert("confidenceBlock".into(), json!(m.confidence_block));
            o.insert("consensusBlock".into(), json!(m.consensus_block));
        }
    }
    for &(i, b) in &resolved.prescan_mutations {
        if let Some(o) = findings.get_mut(i).and_then(Value::as_object_mut) {
            o.insert("consensusBlock".into(), json!(b));
        }
    }
    Ok(GateResolution {
        findings,
        clusters: resolved.clusters,
        blockers: resolved.blockers,
        would_block: resolved.would_block,
        fp_removed,
        before_fp_count,
        after_fp_count,
        refutation_stats: intermediate.refutation_stats,
        verifier_stats: intermediate.verifier_stats,
        verifier_unavailable: intermediate.verifier_unavailable,
    })
}

// ---------------------------------------------------------------------------
// Мелкие помощники
// ---------------------------------------------------------------------------

/// JS Number → JSON: целое печатается без `.0`, как `JSON.stringify`.
fn num(x: f64) -> Value {
    if x.is_finite() && x == x.trunc() && x.abs() < 9.0e15 {
        json!(x as i64)
    } else if x.is_finite() {
        json!(x)
    } else {
        Value::Null
    }
}

fn get<'v>(v: &'v Value, path: &[&str]) -> Option<&'v Value> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    Some(cur)
}

fn truthy(v: &Value, path: &[&str]) -> bool {
    get(v, path).is_some_and(js_truthy)
}

/// `Number(x)`; отсутствие — NaN.
fn number(v: &Value, path: &[&str]) -> f64 {
    get(v, path).and_then(js_to_number).unwrap_or(f64::NAN)
}

fn usize_of(x: f64) -> usize {
    if x.is_finite() && x > 0.0 {
        x as usize
    } else {
        0
    }
}

fn string(v: &Value, path: &[&str]) -> String {
    match get(v, path) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(x) => js_to_string(x),
    }
}

fn str_list(v: &Value, path: &[&str]) -> Vec<String> {
    get(v, path)
        .and_then(Value::as_array)
        .map(|a| a.iter().map(js_to_string).collect())
        .unwrap_or_default()
}

fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Наложить мутации маршрута на находки (`applyRouteMutations`): скор и причины пишутся
/// В САМУ находку — их читают рендер инлайна и auto-triage.
fn apply_route_mutations(findings: &mut [Value], scored: &[ScoredItem]) {
    for s in scored {
        let Some(o) = findings.get_mut(s.index).and_then(Value::as_object_mut) else { continue };
        o.insert("actionabilityScore".into(), json!(s.score));
        o.insert("actionabilityReasons".into(), json!(s.reasons.iter().map(|r| r.why).collect::<Vec<_>>()));
        if s.promoted {
            o.insert("actionabilityPromoted".into(), Value::Bool(true));
        } else {
            // `= undefined` в JS: в JSON поля нет. shift_remove — порядок прочих ключей цел.
            o.shift_remove("actionabilityPromoted");
        }
    }
}

/// `attachFingerprints`: отпечаток кода под находкой. Считается ОДИН раз на прогон —
/// до того, как набор разойдётся по футеру sticky, Δ-блоку и артефакту экспорта.
fn attach_fingerprints(findings: &mut [Value], texts: &IndexMap<String, LineText>) {
    for f in findings.iter_mut() {
        let rec = f.get("file").and_then(Value::as_str).and_then(|p| texts.get(p));
        let fp = finding_fingerprint(rec, f.get("line").unwrap_or(&Value::Null));
        if !fp.is_empty() {
            if let Some(o) = f.as_object_mut() {
                o.insert("fp".into(), json!(fp));
            }
        }
    }
}

/// Тексты строк RIGHT-стороны по файлу из записи `parse_diff`.
fn line_texts(files: &Map<String, Value>) -> IndexMap<String, LineText> {
    files
        .iter()
        .map(|(p, rec)| {
            let text: LineText = rec
                .get("text")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.parse::<u64>().ok()?, v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            (p.clone(), text)
        })
        .collect()
}

fn valid_lines(rec: &Value) -> Vec<u64> {
    rec.get("validLines")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

fn files_map(files: &Map<String, Value>) -> FilesMap {
    files.iter().map(|(p, rec)| (p.clone(), valid_lines(rec).into_iter().collect::<BTreeSet<u64>>())).collect()
}

/// Обратная карта «формулировка сигнала → ключ» (`REASON_KEY_BY_WHY`): в находку маршрут
/// пишет формулировки, метрике нужен стабильный ключ.
fn reason_key(why: &str) -> String {
    SIGNALS.iter().find(|(_, _, w)| *w == why).map(|(k, _, _)| k.to_string()).unwrap_or_else(|| why.to_string())
}

fn review_errored(r: &ModelReview) -> bool {
    r.error.as_deref().is_some_and(|e| !e.is_empty())
}

fn error_text(r: &ModelReview) -> String {
    r.error.clone().unwrap_or_else(|| "undefined".to_string())
}

/// Состояние прогона, которое переживает ранний выход: артефакт и итог для `run_end`.
#[derive(Default)]
struct RunState {
    last_artifact: Option<Value>,
    last_export: Option<ExportResult>,
    final_obs: Value,
}

/// Вход стадии verifier Pass B — зеркало деструктуризации `runVerifierStage`.
struct VerifierStageCtx<'s> {
    deadline: u64,
    content_map: &'s IndexMap<String, String>,
    annotated: &'s str,
    blast_blocks: &'s [String],
    pr: &'s PullRequestRef,
    changed_files_section: &'s str,
    files: &'s FilesMap,
    prior_keys: &'s HashSet<String>,
    prescan_keys: &'s HashSet<String>,
    symbol_cards: Option<&'s SymbolCards>,
}

/// Сбои запроса к графу (`QUERY_FAILURE_REASONS`, `lib/graphify.mjs`).
const QUERY_FAILURE_REASONS: [&str; 3] = ["graphify_unavailable", "graphify_timeout", "graphify_error"];

/// `blastUnavailableReason` (`lib/graphify.mjs`, RFC 16): причина, по которой blast-radius
/// НЕ посчитан (инфра-сбой), или `None`. Пустой радиус при живом графе — не сбой.
fn blast_unavailable_reason(m: &BlastMetrics) -> Option<&'static str> {
    if m.skip_reason == "client_did_not_report" {
        return None;
    }
    if m.graph_status == "missing" {
        return Some("graph_missing");
    }
    let r = |k: &str| m.skip_reasons.get(k).copied().unwrap_or(0);
    if r("graph_enrichment_unavailable") > 0 {
        return Some("graph_unreadable");
    }
    let failed: u64 = QUERY_FAILURE_REASONS.iter().map(|k| r(k)).sum();
    if m.symbols_total > 0 && failed >= m.symbols_total as u64 {
        return Some(QUERY_FAILURE_REASONS.iter().copied().find(|k| r(k) > 0).unwrap_or("graphify_error"));
    }
    None
}

/// `buildBlastUnavailableSummary`: sticky при `REVIEW_BLAST_REQUIRED=1` и непосчитанном
/// blast-radius (LLM не вызывался).
fn build_blast_unavailable_summary(review_marker: &str, reason: &str) -> String {
    format!(
        "{review_marker}\n## 🤖 Авто код-ревью\n\n⛔ Blast-radius недоступен (`{reason}`): code graph Graphify не построен или не читается.\n\n\
         <sub>Без графа гейт не видит callers вне диффа — merge заблокирован до LLM-ревью \
         (fail-closed, `REVIEW_BLAST_REQUIRED=1`). Проверьте шаг сборки графа (`graphify update`), \
         `GRAPH` и `GRAPHIFY_BIN`; осознанное ослабление — `REVIEW_BLAST_REQUIRED=0`. Детали — в логе CI job.</sub>"
    )
}

/// Итог стадии verifier — `{ findings, stats, unavailable }` JS плюс снятые находки.
struct VerifierStageOut {
    findings: Vec<Value>,
    stats: Option<Value>,
    /// Находки, снятые verifier'ом (rejected), в их последнем состоянии.
    rejected: Vec<Value>,
    /// `{reason, unverified}` — verifier недоступен (RFC 18), только в strict-режиме.
    unavailable: Option<Value>,
}

impl VerifierStageOut {
    fn untouched(findings: Vec<Value>) -> Self {
        Self { findings, stats: None, rejected: Vec::new(), unavailable: None }
    }
}

// ---------------------------------------------------------------------------
// Пайплайн
// ---------------------------------------------------------------------------

/// Собранный пайплайн — порт объекта `{ run, runDetailed }` из `createReviewPipeline`.
pub struct ReviewPipeline<'a> {
    cfg: &'a Value,
    profile: &'a Profile,
    deps: PipelineDeps<'a>,
    opts: PipelineOptions<'a>,
}

/// `createReviewPipeline(cfg, deps)`.
pub fn create_review_pipeline<'a>(
    cfg: &'a Value,
    profile: &'a Profile,
    deps: PipelineDeps<'a>,
    opts: PipelineOptions<'a>,
) -> ReviewPipeline<'a> {
    ReviewPipeline { cfg, profile, deps, opts }
}

impl<'a> ReviewPipeline<'a> {
    fn log(&self, msg: &str) {
        (self.deps.log)(msg);
    }

    fn now(&self) -> u64 {
        self.opts.clock.map(|c| c()).unwrap_or_else(system_now)
    }

    fn cost(&self) -> &CostTracker {
        self.deps.cost
    }

    fn cwd(&self) -> PathBuf {
        self.opts
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// Код возврата CLI (`run`). `Err` — авария самого пайплайна; CLI её глушит в `0`.
    /// Мягкий отказ логируется `SOFT-FAIL: …` — как дефолтный `die` в JS (`pipeline.mjs:316`).
    pub fn run(&self) -> Result<i32> {
        let o = self.run_detailed()?;
        if o.soft_fail {
            self.log(&soft_fail_line(o.reason.as_deref().unwrap_or("")));
        }
        Ok(o.exit_code)
    }

    /// `runDetailed`: тот же прогон, но с артефактом в памяти.
    pub fn run_detailed(&self) -> Result<PipelineOutcome> {
        let own_obs;
        let obs: &Observability = match self.opts.observability {
            Some(o) => o,
            None => {
                // Каталог лога — от корня прогона (в JS — от process.cwd()).
                let mut oc = ObservabilityConfig::from_config_with_profile(self.cfg, self.profile);
                oc.dir = self.cwd().join(&oc.dir).to_string_lossy().into_owned();
                own_obs = Observability::new(&oc, system_now, None);
                &own_obs
            }
        };
        let mut st = RunState { final_obs: json!({ "exitCode": 0 }), ..RunState::default() };
        let res = self.body(obs, &mut st);
        let out = match res {
            Ok(o) => Ok(o),
            Err(e) => match e.downcast_ref::<PipelineSoftFail>() {
                // Без лога: `SOFT-FAIL: …` печатает только CLI-путь ([`Self::run`],
                // `pr-review review`) — в JS это дефолтный `die`. MCP подменяет `die` на
                // `throw PipelineSoftFail` без лога, и лишней notification клиент не видит.
                Some(soft) => {
                    st.final_obs = json!({
                        "exitCode": 0,
                        "degraded": true,
                        "error": js_slice_utf16(&soft.0, 200),
                    });
                    Ok(PipelineOutcome {
                        exit_code: 0,
                        artifact: st.last_artifact.clone(),
                        export_result: st.last_export.clone(),
                        would_block: false,
                        degraded: true,
                        reason: Some(soft.0.clone()),
                        soft_fail: true,
                    })
                }
                None => {
                    st.final_obs = json!({
                        "exitCode": 0,
                        "crashed": true,
                        "error": js_slice_utf16(&format!("{e:#}"), 200),
                    });
                    Err(e)
                }
            },
        };
        self.finish_obs(obs, &st.final_obs);
        out
    }

    /// `finishObs`: единственная точка выхода прогона — здесь же `efficiencyMetrics`.
    fn finish_obs(&self, obs: &Observability, data: &Value) {
        if !obs.enabled {
            return;
        }
        let merged = match efficiency_metrics(&self.cost().snapshot(), data) {
            Some(Value::Object(eff)) => {
                let mut m = data.as_object().cloned().unwrap_or_default();
                for (k, v) in eff {
                    m.insert(k, v);
                }
                Value::Object(m)
            }
            _ => data.clone(),
        };
        let _ = obs.flush(Some(&merged));
    }

    fn outcome(&self, st: &RunState, exit_code: i32, would_block: bool, degraded: bool, reason: Option<String>) -> PipelineOutcome {
        PipelineOutcome {
            exit_code,
            artifact: st.last_artifact.clone(),
            export_result: st.last_export.clone(),
            would_block,
            degraded,
            reason,
            soft_fail: false,
        }
    }

    fn log_stage(&self, obs: &Observability, stage: &str, findings: &[Value]) {
        if !obs.enabled {
            return;
        }
        let arr = Value::Array(findings.to_vec());
        obs.log_event(
            "findings_stage",
            &json!({
                "stage": stage,
                "count": findings.len(),
                "bySeverity": count_by_severity(Some(&arr)),
                "sample": slim_findings(Some(&arr), 40),
            }),
        );
    }

    fn persist_and_capture(&self, st: &mut RunState, obs: &Observability, payload: Value) {
        st.last_export = self.persist_exports(&payload, obs);
        st.last_artifact = Some(payload);
    }

    /// `persistExports`: экспорт R9 fail-open — сбой записи не роняет прогон.
    fn persist_exports(&self, payload: &Value, obs: &Observability) -> Option<ExportResult> {
        if !truthy(self.cfg, &["export", "enabled"]) {
            return None;
        }
        if truthy(self.cfg, &["cli", "dryRun"]) && !truthy(self.cfg, &["export", "dryRun"]) {
            return None;
        }
        match write_review_exports(self.cfg, self.profile, payload, &self.cwd(), self.deps.log) {
            Ok(out) => {
                obs.log_event(
                    "export",
                    &json!({ "ok": true, "files": out.written.len(), "sarifResults": out.sarif_results }),
                );
                Some(out)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.log(&format!("export R9 failed (fail-open): {msg}"));
                obs.log_event("export", &json!({ "ok": false, "error": js_slice_utf16(&msg, 200) }));
                None
            }
        }
    }

    fn review_json(&self, input: Map<String, Value>) -> Result<Value> {
        build_review_json(Some(&Value::Object(input)), &iso_from_epoch_ms(self.now() as i64))
    }

    fn score_ctx(&self, findings: &[Value], prior_keys: &HashSet<String>, prescan_keys: &HashSet<String>, with_classes: bool) -> ScoreCtx {
        let mut classes = HashMap::new();
        if with_classes {
            for f in findings {
                if let Some(p) = f.get("file").filter(|v| js_truthy(v)).map(js_to_string) {
                    let c = classify_file_path(&p).to_string();
                    classes.insert(p, c);
                }
            }
        }
        let sensitive = self.profile.layout().sensitive_source();
        ScoreCtx::new(prior_keys.clone(), prescan_keys.clone(), classes.clone(), sensitive)
            .or_else(|_| ScoreCtx::new(prior_keys.clone(), prescan_keys.clone(), classes, None))
            .expect("ScoreCtx без регекса профиля собирается всегда")
    }

    fn classify_cfg(&self) -> RouteCfg {
        let inline_on = str_list(self.cfg, &["inlineOn"]);
        let d = RouteCfg::default();
        RouteCfg {
            inline_on: if inline_on.is_empty() { d.inline_on } else { inline_on },
            threshold: finite_or(number(self.cfg, &["actionability", "threshold"]), d.threshold),
            max_advisory_inline: get(self.cfg, &["actionability", "maxAdvisoryInline"])
                .and_then(js_to_number)
                .map(usize_of)
                .unwrap_or(d.max_advisory_inline),
            strict: truthy(self.cfg, &["gate", "strict"]),
        }
    }

    /// Уведомление дежурного: текст собирает пайплайн, доставку — [`crate::traits::Notifier`].
    fn notify(&self, pr: &PullRequestRef, counts: &Value, total: usize, reason: Option<&str>) {
        let Some(n) = self.deps.notifier else { return };
        let c = |k: &str| counts.get(k).map(js_to_string).unwrap_or_else(|| "undefined".into());
        let text = format!(
            "🔍 Авто-ревью PR #{}: «{}»\n{} замечаний — 🔴{} 🟠{} 🟡{} 🔵{}{}\n{}",
            pr.number,
            pr.title,
            total,
            c("critical"),
            c("major"),
            c("minor"),
            c("nit"),
            reason.map(|r| format!("\n⚠️ {r}")).unwrap_or_default(),
            pr.html_url,
        );
        if let Err(e) = n.notify(&text) {
            self.log(&format!("telegram ping failed (не критично): {e:#}"));
        }
    }

    /// `publishInline`: post-then-delete — старые инлайны чистятся только после успешного
    /// постинга нового review, иначе при сбое постинга комментарии потерялись бы.
    fn publish_inline(&self, inline: &[InlineComment], head_sha: &str, max_inline: usize, cleanup_without_post: bool) -> Result<(usize, usize, usize)> {
        let to_post: Vec<InlineComment> = inline.iter().take(max_inline).cloned().collect();
        if inline.len() > to_post.len() {
            self.log(&format!(
                "inline-комментов {} > лимита {max_inline} — постим первые {max_inline} (полные счётчики в sticky)",
                inline.len()
            ));
        }
        let gh = self.deps.github;
        if to_post.is_empty() && cleanup_without_post {
            let cleared = gh.delete_stale_inline(&DeleteStaleOpts::default())?;
            return Ok((0, inline.len(), cleared));
        }
        let posted = gh.post_review(&PostReviewParams { inline: to_post.clone(), head_sha: head_sha.to_string() })?;
        // review id не пришёл → чистку пропускаем: иначе сотрём только что запощенное.
        let cleared = match posted.review_id {
            Some(id) => gh.delete_stale_inline(&DeleteStaleOpts { except_review_id: Some(id) })?,
            None => {
                self.log("review id не получен — пропускаем чистку старых инлайнов");
                0
            }
        };
        Ok((to_post.len(), inline.len(), cleared))
    }

    fn body(&self, obs: &Observability, st: &mut RunState) -> Result<PipelineOutcome> {
        let cfg = self.cfg;
        let profile = self.profile;
        let markers = profile.markers();
        let layout = profile.layout();
        let dry_run = truthy(cfg, &["cli", "dryRun"]);
        let diff_file = string(cfg, &["cli", "diffFile"]);
        let post_creds = truthy(cfg, &["github", "repo"])
            && truthy(cfg, &["github", "prNumber"])
            && truthy(cfg, &["github", "token"]);
        let review_mode = string(cfg, &["reviewMode"]);
        let gate_v = get(cfg, &["gate"]).cloned().unwrap_or_else(|| json!({}));
        let block_on = str_list(cfg, &["gate", "blockOn"]);
        let block_on_v: Vec<Value> = block_on.iter().map(|s| json!(s)).collect();
        let llm_required = truthy(cfg, &["gate", "llmRequired"]);
        let lim = |k: &str| number(cfg, &["limits", k]);
        let models_list = str_list(cfg, &["models", "list"]);

        if string(cfg, &["ollama", "key"]).is_empty() {
            return Err(die("нет LLM API key (задайте LLM_API_KEY или OLLAMA_API_KEY)"));
        }
        if review_mode == "moa" && str_list(cfg, &["moa", "references"]).is_empty() {
            return Err(die("MOA_REFERENCE_MODELS пуст — задай хотя бы один advisor"));
        }
        if diff_file.is_empty() && !post_creds {
            return Err(die("нет REPO/PR_NUMBER/GITHUB_TOKEN для загрузки диффа"));
        }
        if !dry_run && !post_creds {
            return Err(die("постинг требует REPO/PR_NUMBER/GITHUB_TOKEN — для локального прогона добавь --dry-run"));
        }

        let pr_number = get(cfg, &["github", "prNumber"]).and_then(js_to_number).map(usize_of).unwrap_or(0) as u64;
        let mut pr = PullRequestRef::local_diff(pr_number);
        let head_sha: String;
        let diff: String;
        if !diff_file.is_empty() {
            let bytes = std::fs::read(&diff_file).with_context(|| format!("не читается дифф {diff_file}"))?;
            diff = String::from_utf8_lossy(&bytes).into_owned();
            head_sha = js_trim(
                self.opts
                    .head_sha
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&string(cfg, &["cli", "headSha"])),
            )
            .to_string();
            let short = if head_sha.is_empty() { String::new() } else { format!(" · head {}", js_slice_utf16(&head_sha, 7)) };
            self.log(&format!("дифф из файла {diff_file} ({} байт){short}", u16_len(&diff)));
        } else {
            // Сбой GitHub API при загрузке диффа — тот же мягкий отказ, что «нет диффа»:
            // инфра-сбой не превращается в блок.
            let loaded = match self.deps.github.load_diff() {
                Ok(Some(l)) => l,
                Ok(None) | Err(_) => {
                    return Err(die("не удалось загрузить дифф PR (нет REPO/PR/token или сбой GitHub API)"));
                }
            };
            if loaded.draft {
                return Err(die("PR — черновик, пропускаем"));
            }
            pr = loaded.pr;
            head_sha = loaded.head_sha;
            diff = loaded.diff;
            self.log(&format!(
                "PR #{} «{}» · дифф {} байт · head {}",
                pr.number,
                pr.title,
                u16_len(&diff),
                js_slice_utf16(&head_sha, 7)
            ));
        }
        if js_trim(&diff).is_empty() {
            return Err(die("пустой дифф"));
        }

        let parsed = parse_diff(&diff);
        let files = parsed.files;
        let annotated = parsed.annotated;
        let changed_text = parsed.changed_text;
        let changed_paths: Vec<String> = files.keys().cloned().collect();
        let changed_paths_v = json!(changed_paths);
        let texts = line_texts(&files);
        let fmap = files_map(&files);
        let route = route_diff(&changed_paths, &diff, get(cfg, &["router", "enabled"]).map(js_truthy).unwrap_or(true));
        let route_v = serde_json::to_value(&route).unwrap_or(Value::Null);
        self.log(&format!("router R4: {}", format_route_log(&route)));
        obs.log_event(
            "run_start",
            &json!({
                "pr": pr.number,
                "prTitle": js_slice_utf16(&pr.title, PR_TITLE_LOG_MAX),
                "headSha": js_slice_utf16(&head_sha, 7),
                "reviewMode": review_mode,
                "dryRun": dry_run,
                "files": changed_paths.len(),
                "diffBytes": u16_len(&diff),
            }),
        );
        obs.log_event(
            "router",
            &json!({
                "route": route.route,
                "reason": route.reason,
                "files": route.files.iter().map(|f| f.class.clone()).collect::<Vec<_>>(),
            }),
        );

        if route.route == "skip" || route.route == "light" {
            return self.router_shortcut(obs, st, &route, &route_v, &pr, &head_sha, &changed_paths_v, &review_mode, dry_run, post_creds);
        }

        let prep_t0 = self.now();
        let cwd = self.cwd();
        // Добавленные файлы контекстом не прикладываем: их дифф И ЕСТЬ полный текст, второй
        // экземпляр оплачивался бы в каждом вызове каждой роли. Символы blast-radius не
        // теряются: collect_blast_symbols разбирает changed_text, где строки нового файла есть.
        let added_paths =
            if truthy(cfg, &["limits", "skipAddedFileContext"]) { collect_added_paths(&diff) } else { Vec::new() };
        if !added_paths.is_empty() {
            self.log(&format!(
                "контекст: {} добавленных файл(ов) — дифф уже содержит их целиком, полный текст не прикладываем",
                added_paths.len()
            ));
        }
        // Сначала файлы: collect_blast_symbols добирает экспорты из полного текста.
        let content_map = {
            let mut log = |s: &str| self.log(s);
            read_changed_files_in(&cwd, &changed_paths, lim("maxFileLines"), &mut log, Some(&added_paths))
        };
        let prior_sticky = match self.deps.github.read_prior_sticky_data() {
            Ok(s) => s,
            Err(e) => {
                // Молчаливый провал стоил бы сходимости: без prior findings прогон с нуля.
                self.log(&format!("prior sticky недоступен ({e:#}) — прогон без carry-over"));
                StickyData::default()
            }
        };
        // Удалённым файлам линтер не нужен: их нет в рабочем дереве (RFC 17).
        let deleted_paths = collect_deleted_paths(&diff);
        let prescan_paths: Vec<String> = if deleted_paths.is_empty() {
            changed_paths.clone()
        } else {
            changed_paths.iter().filter(|p| !deleted_paths.contains(p)).cloned().collect()
        };
        let prescan_result: PrescanResult = self.deps.prescan.run(&prescan_paths)?;
        let symbols = collect_blast_symbols(&changed_text, Some(&content_map), usize_of(lim("maxSymbols")));
        self.log(&format!(
            "файлов: {} · символов под blast-radius: {} {}",
            files.len(),
            symbols.len(),
            serde_json::to_string(&symbols).unwrap_or_default()
        ));
        // Окна контекста (RFC 07). ПОСЛЕ collect_blast_symbols (он читает полный текст) и ДО
        // file_input_hashes (хэш — от входа модели, т.е. от окна). Verifier получает полную
        // content_map: он режет сам, вокруг строки кандидата.
        let ctx_windows =
            window_chunk_file_contents(&content_map, Some(&files), lim("contextMargin"), lim("contextWindowMinChars"));
        if !ctx_windows.windowed.is_empty() {
            self.log(&format!(
                "контекст: окна ±{} вокруг изменённых строк в {} файл(ах) из {} — {}→{} симв.",
                js_to_string(&num(lim("contextMargin"))),
                ctx_windows.windowed.len(),
                content_map.len(),
                ctx_windows.before,
                ctx_windows.after
            ));
        }
        // Свежесть графа меряем даже без экспортов в диффе.
        let blast = self.deps.graphify.build_blast_radius(&symbols)?;
        let blast_blocks = blast.blocks.clone();
        let blast_metrics = blast.metrics.clone();
        let blast_metrics_v = serde_json::to_value(&blast_metrics).unwrap_or(Value::Null);
        let graph_index = blast.graph_index.clone();
        self.log(&format!("prep parallel: {}мс (graphify+files+prior+prescan)", self.now().saturating_sub(prep_t0)));
        // REVIEW_BLAST_REQUIRED (RFC 16): без code graph гейт не видит callers вне диффа →
        // fail-closed ДО LLM (тратить токены на ревью, которое всё равно заблокируется, незачем).
        let blast_failure =
            if truthy(cfg, &["gate", "blastRequired"]) { blast_unavailable_reason(&blast_metrics) } else { None };
        if let Some(reason) = blast_failure {
            self.log(&format!("blast-radius недоступен ({reason}) — fail-closed (REVIEW_BLAST_REQUIRED)"));
            obs.log_event("blast_failed", &json!({ "reason": reason, "required": true, "blast_radius": blast_metrics_v }));
            let mut input = Map::new();
            input.insert("pr".into(), serde_json::to_value(&pr)?);
            input.insert("headSha".into(), json!(head_sha));
            input.insert("reviewMode".into(), json!(review_mode));
            input.insert("route".into(), route_v.clone());
            input.insert("wouldBlock".into(), json!(true));
            input.insert("counts".into(), json!({ "critical": 0, "major": 0, "minor": 0, "nit": 0 }));
            input.insert("total".into(), json!(0));
            input.insert("findings".into(), json!([]));
            input.insert("blockers".into(), json!([]));
            input.insert("reviews".into(), json!([]));
            input.insert("blastMetrics".into(), blast_metrics_v.clone());
            input.insert("changedPaths".into(), changed_paths_v.clone());
            let payload = self.review_json(input)?;
            self.persist_and_capture(st, obs, payload);
            st.final_obs = json!({ "exitCode": 1, "wouldBlock": true, "blastFailed": reason, "degraded": true });
            if !dry_run && post_creds {
                if let Err(e) = self.deps.github.upsert_sticky(&build_blast_unavailable_summary(&markers.review, reason)) {
                    self.log(&format!("заметку не запостили: {}", js_slice_utf16(&format!("{e:#}"), 200)));
                }
            }
            return Ok(self.outcome(st, 1, true, true, Some(format!("blast-radius unavailable: {reason}"))));
        }

        // Инвалидация carry-over по отпечаткам ДО LLM: устаревшие prior findings не подмешиваем.
        let prior_files: IndexMap<String, PriorFileRec> = files
            .iter()
            .map(|(p, rec)| {
                (
                    p.clone(),
                    PriorFileRec {
                        valid_lines: valid_lines(rec).into_iter().map(|n| n as f64).collect(),
                        text: texts.get(p).cloned(),
                    },
                )
            })
            .collect();
        let stale = filter_stale_prior_findings(&prior_sticky.findings, &prior_files);
        let prior_findings: Vec<Value> = stale
            .kept
            .iter()
            .map(|&(i, line)| {
                let p = &prior_sticky.findings[i];
                let old = p.get("line").and_then(js_to_number).filter(|n| !n.is_nan()).unwrap_or(0.0);
                if line == old {
                    p.clone()
                } else {
                    let mut m = p.as_object().cloned().unwrap_or_default();
                    m.insert("line".into(), num(line));
                    Value::Object(m)
                }
            })
            .collect();
        let mut reasons = Vec::new();
        if stale.code_changed > 0 {
            reasons.push(format!("код изменён: {}", stale.code_changed));
        }
        if stale.out_of_diff > 0 {
            reasons.push(format!("файл вне диффа: {}", stale.out_of_diff));
        }
        if stale.stale_legacy > 0 {
            reasons.push(format!("правило ±3 (без fingerprint): {}", stale.stale_legacy));
        }
        if !reasons.is_empty() {
            self.log(&format!("prior findings: снято {}", reasons.join(", ")));
        }
        if stale.legacy > 0 {
            self.log(&format!(
                "prior findings: {} из старого sticky без fingerprint — сравнение по ±3 строки",
                stale.legacy
            ));
        }
        if prior_findings.len() != prior_sticky.findings.len() {
            obs.log_event("prior_findings", &json!({ "loaded": prior_sticky.findings.len(), "kept": prior_findings.len() }));
        }

        // Контекст доказуемости для маршрута advisory — один на прогон: его читают и
        // verifier-стадия (кого дополнительно проверить), и classify (кого поднять в строку).
        let prior_keys: HashSet<String> = prior_findings.iter().map(finding_location_key).collect();
        let prescan_keys: HashSet<String> = prescan_result.findings.iter().map(finding_location_key).collect();
        let prompt_ctx = prompt_ctx_from_profile(profile);
        let changed_files_section = build_changed_files_section(
            &changed_paths.iter().map(|p| json!(p)).collect::<Vec<_>>(),
            &json!({}),
            &prompt_ctx,
        )?;

        let diff_sections = extract_file_diff_sections(&annotated);
        // Карточки символов (RFC 14): ПОСЛЕ blast-radius (индекс графа), ДО file_input_hashes
        // (карточки входят в хэш входа файла) и ДО build_chunks (чанк собирает свои).
        let default_reader;
        let read_lines: &ReadLines<'_> = match self.opts.read_lines {
            Some(r) => r,
            None => {
                default_reader = LineReader::new(&cwd);
                &|p: &str| default_reader.read(p)
            }
        };
        let symbol_cards: Option<SymbolCards> = if truthy(cfg, &["limits", "symbolCards"]) {
            let sections: HashMap<String, String> = diff_sections.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let windows: HashMap<String, String> = ctx_windows.map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            Some(build_symbol_cards(
                &changed_paths,
                &BuildOpts {
                    graph_index: graph_index.as_deref(),
                    sections: &sections,
                    windows: &windows,
                    read_lines,
                    body_max_lines: lim("symbolCardsBodyMaxLines"),
                    root: &cwd,
                    app_dir: &layout.app_dir,
                },
            ))
        } else {
            None
        };
        if let Some(sc) = &symbol_cards {
            self.log(&format!(
                "карточки символов: {} из {} вызванных имён {}",
                sc.metrics.get("cards").map(js_to_string).unwrap_or_default(),
                sc.metrics.get("candidates").map(js_to_string).unwrap_or_default(),
                sc.metrics.get("skip_reasons").map(|v| v.to_string()).unwrap_or_default()
            ));
        }
        let incremental_on = truthy(cfg, &["incremental", "enabled"]);
        // Относительные пути конфига — от корня прогона (в JS — от process.cwd()).
        let state_file = match string(cfg, &["incremental", "stateFile"]) {
            f if f.is_empty() => f,
            f => cwd.join(f).to_string_lossy().into_owned(),
        };
        let prior_incremental: Option<Value> =
            if incremental_on { read_incremental_state(Some(Path::new(&state_file))) } else { None };
        // Вход карточек в хэш. Граф недоступен, а фича включена: берём вход карточек из
        // прошлого прогона (аудит RFC 14, п. 1.9), сужая так, чтобы не перенести находку
        // мимо реального изменения (см. reuse_prior_cards_input).
        let mut cards_input: Option<IndexMap<String, String>> = symbol_cards.as_ref().map(|sc| {
            sc.by_path.iter().map(|(p, cards)| (p.clone(), symbol_cards_hash_input(cards))).collect()
        });
        let mut cards_targets: Option<Map<String, Value>> = symbol_cards.as_ref().map(|sc| {
            sc.by_path
                .iter()
                .map(|(p, cards)| {
                    let uniq: IndexSet<&str> = cards.iter().map(|c| c.path.as_str()).collect();
                    (p.clone(), json!(uniq.into_iter().collect::<Vec<_>>()))
                })
                .collect()
        });
        let mut cards_input_reused = 0usize;
        if symbol_cards.is_some() && graph_index.is_none() {
            if let Some(r) = reuse_prior_cards_input(prior_incremental.as_ref(), &changed_paths) {
                cards_input_reused = r.cards_input.len();
                cards_input = Some(r.cards_input);
                cards_targets = Some(r.cards_targets);
                if cards_input_reused > 0 {
                    self.log(&format!("карточки: граф недоступен — вход карточек {cards_input_reused} файл(ов) взят из прошлого прогона (их карточки не указывают на файлы PR)"));
                }
            }
        }
        let input_hashes = file_input_hashes(&changed_paths, &diff_sections, &ctx_windows.map, cards_input.as_ref());
        let config_fp = config_fingerprint(cfg);
        let plan: IncrementalPlan = if incremental_on {
            plan_incremental(PlanInput {
                state: prior_incremental.as_ref(),
                hashes: &input_hashes,
                changed_paths: &changed_paths,
                config_fp: &config_fp,
                full_every: number(cfg, &["incremental", "fullEvery"]),
            })
        } else {
            IncrementalPlan::disabled(&changed_paths)
        };
        let mode_str = if plan.is_incremental() { "incremental" } else { "full" };
        if incremental_on {
            self.log(&format!("инкрементальность: {mode_str} — {}", plan.reason));
            obs.log_event(
                "incremental",
                &json!({
                    "mode": mode_str,
                    "reason": plan.reason,
                    "reviewed": plan.review_paths.len(),
                    "unchanged": plan.unchanged.len(),
                    "carried": plan.carried.len(),
                    "symbolCardsReused": cards_input_reused,
                }),
            );
        }
        // Дифф для LLM сужаем до ревьюируемых файлов; files/changed_paths остаются полными —
        // на них держатся карта PR, отпечатки, гейт и Δ-блок.
        let annotated_for_llm = if plan.is_incremental() {
            diff_sections
                .iter()
                .filter(|(p, _)| plan.review_paths.contains(p.as_str()))
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            annotated.clone()
        };
        let context_for_llm: IndexMap<String, String> = if plan.is_incremental() {
            ctx_windows.map.iter().filter(|(p, _)| plan.review_paths.contains(p.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
        } else {
            ctx_windows.map.clone()
        };
        let carry_from = prior_incremental.as_ref().and_then(|s| s.get("headSha")).filter(|v| js_truthy(v)).map(js_to_string).unwrap_or_default();
        let mut carry: Option<CarryOver> = Some(CarryOver::from_plan(&plan, &carry_from));
        let carried_n = carry.as_ref().map_or(0, CarryOver::len);
        if carried_n > 0 {
            self.log(&format!("перенесено находок по неизменившимся файлам: {carried_n}"));
        }

        let mut chunks: Vec<Chunk> = build_chunks(
            &annotated_for_llm,
            &context_for_llm,
            &ChunkLimits {
                max_diff_bytes: usize_of(lim("maxDiffBytes")),
                max_context_bytes: usize_of(lim("maxContextBytes")),
                chunk_bytes: usize_of(lim("chunkBytes")),
                max_chunks: usize_of(lim("maxChunks")),
            },
        );
        // windowedPaths — ПОСЛЕ нарезки: окна ставит пайплайн, он и знает список.
        if !ctx_windows.windowed.is_empty() {
            for c in &mut chunks {
                c.windowed_paths = Some(c.paths.iter().filter(|p| ctx_windows.windowed.contains(p.as_str())).cloned().collect());
            }
        }
        let mut symbol_cards_dropped = 0usize;
        if let Some(sc) = symbol_cards.as_ref().filter(|sc| !sc.by_path.is_empty()) {
            for c in &mut chunks {
                let sel = select_chunk_cards(
                    &sc.by_path,
                    &c.paths,
                    lim("symbolCardsMax"),
                    lim("symbolCardsMaxChars"),
                    &format!("{}\n{}", c.annotated, c.file_contents),
                );
                if !sel.cards.is_empty() {
                    c.symbol_cards = sel.cards;
                    c.symbol_card_names = sel.names;
                }
                c.symbol_cards_chars = Some(sel.chars);
                c.symbol_cards_dropped = Some(sel.dropped);
                symbol_cards_dropped += sel.dropped;
            }
        }
        let note0 = [
            if chunks.len() > 1 {
                format!("PR большой — разбит на {} частей для полного охвата (каждая ревьюится отдельно).", chunks.len())
            } else {
                String::new()
            },
            format_incremental_note(Some(&plan)),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
        // Дедлайн LLM-фазы: после него чанки пропускаются, отчёт частичный, но штатный.
        let budget = lim("timeBudgetMs");
        // Насыщение: `as u64` уже насыщает огромный бюджет до u64::MAX, и голое `+` в debug
        // паникует (exit 101), а в release заворачивает дедлайн в прошлое — прогон без LLM.
        // В JS это конечное число далеко в будущем.
        let deadline = if budget > 0.0 { self.now().saturating_add(budget as u64) } else { 0 };
        self.log(&format!(
            "blast-radius блоков: {} · symbols {}/{} · affected {} · graph {} · файлов с контекстом: {} · чанков: {}",
            blast_blocks.len(),
            blast_metrics.symbols_resolved.map(|n| n.to_string()).unwrap_or_else(|| "—".into()),
            blast_metrics.symbols_total,
            blast_metrics.affected_nonempty,
            blast_metrics.graph_status,
            content_map.len(),
            chunks.len()
        ));
        let blast_findings = emit_blast_radius_findings(
            &blast_blocks,
            &changed_paths,
            &BlastFindingsOpts {
                symbols: &symbols,
                model: Some(BLAST_FINDING_SOURCE),
                drop_ambiguous: get(cfg, &["graphify", "dropAmbiguous"]) != Some(&Value::Bool(false)),
                layout,
            },
        );
        if !blast_findings.is_empty() {
            self.log(&format!("blast-radius: +{} детерминированных cross-layer/community caller(s)", blast_findings.len()));
        }
        let chunk_classes = |c: &Chunk| -> Vec<String> {
            let set: BTreeSet<String> = c.paths.iter().map(|p| classify_file_path(p).to_string()).collect();
            set.into_iter().collect()
        };
        obs.log_event(
            "plan",
            &json!({
                "chunks": chunks.len(),
                "symbols": symbols.len(),
                "blastBlocks": blast_blocks.len(),
                "models": models_list,
                "llmConcurrency": get(cfg, &["limits", "concurrency"]).cloned().unwrap_or(Value::Null),
                "chunkBytes": get(cfg, &["limits", "chunkBytes"]).cloned().unwrap_or(Value::Null),
                "blast_radius": blast_metrics_v,
                "symbolCards": symbol_cards.as_ref().map(|sc| {
                    let mut m = sc.metrics.as_object().cloned().unwrap_or_default();
                    m.insert("dropped".into(), json!(symbol_cards_dropped));
                    m.insert("hash_input_reused".into(), json!(cards_input_reused));
                    m.insert("chunks_over_limit".into(), json!(chunks.iter().filter(|c| c.symbol_cards_dropped.unwrap_or(0) > 0).count()));
                    Value::Object(m)
                }),
                "contextWindows": {
                    "margin": num(lim("contextMargin")),
                    "minChars": num(lim("contextWindowMinChars")),
                    "files": ctx_windows.windowed.len(),
                    "before": ctx_windows.before,
                    "after": ctx_windows.after,
                },
                "chunkPlan": chunks.iter().enumerate().map(|(i, c)| json!({
                    "i": i,
                    "diff_chars": u16_len(&c.annotated),
                    "context_chars": u16_len(&c.file_contents),
                    "windowed_paths": c.windowed_paths.as_ref().map_or(0, Vec::len),
                    "symbol_cards": c.symbol_cards.len(),
                    "symbol_cards_chars": c.symbol_cards_chars.unwrap_or(0),
                    "symbol_card_names": c.symbol_card_names,
                    "paths": c.paths.len(),
                    "classes": chunk_classes(c),
                })).collect::<Vec<_>>(),
            }),
        );

        if !prior_findings.is_empty() {
            self.log(&format!("прошлых находок подгружено: {} (сходимость)", prior_findings.len()));
        }
        self.log(&format!("prescan R6: {}", format_prescan_log(&prescan_result)));
        {
            let mut ev = Map::new();
            ev.insert("findings".into(), json!(prescan_result.findings.len()));
            ev.insert("scanned".into(), json!(prescan_result.scanned));
            if let Some(s) = &prescan_result.skipped {
                ev.insert("skipped".into(), json!(s));
            }
            ev.insert("truncated".into(), json!(prescan_result.truncated));
            if let Some(n) = prescan_result.lintable_total {
                ev.insert("lintableTotal".into(), json!(n));
            }
            ev.insert("byTool".into(), serde_json::to_value(prescan_result.by_tool).unwrap_or(Value::Null));
            if let Some(e) = &prescan_result.error {
                ev.insert("error".into(), json!(e));
            }
            if let Some(e) = prescan_result.eslint.as_ref().and_then(|t| t.error.as_ref()) {
                ev.insert("eslintError".into(), json!(e));
            }
            if let Some(e) = prescan_result.astgrep.as_ref().and_then(|t| t.error.as_ref()) {
                ev.insert("astgrepError".into(), json!(e));
            }
            ev.insert("sample".into(), slim_findings(Some(&Value::Array(prescan_result.findings.clone())), 40));
            obs.log_event("prescan", &Value::Object(ev));
        }

        let specialists_on = truthy(cfg, &["specialists", "enabled"]);
        let mode_tail = if review_mode == "moa" {
            let who = if specialists_on {
                let a = get(cfg, &["specialists", "assignments"]).and_then(Value::as_array).cloned().unwrap_or_default();
                format!(
                    "specialists: {}",
                    a.iter().map(|x| format!("{}={}", string(x, &["role"]), string(x, &["model"]))).collect::<Vec<_>>().join(", ")
                )
            } else {
                format!("advisors: {}", str_list(cfg, &["moa", "references"]).join(", "))
            };
            format!(" · {who} · aggregator: {}", string(cfg, &["moa", "aggregator"]))
        } else {
            String::new()
        };
        self.log(&format!(
            "режим: {review_mode}{mode_tail} · модели: {} · порог: {}",
            models_list.join(", "),
            js_to_string(&num(number(cfg, &["gate", "consensusMin"])))
        ));

        // Инкрементальный прогон без изменившихся файлов: ревьюить нечего. Это не аутедж
        // LLM — fail-closed ниже он не поднимает, иначе «ничего не поменялось» блокировало бы.
        let nothing_to_review = plan.is_incremental() && plan.review_paths.is_empty();
        if nothing_to_review {
            self.log("инкрементальность: изменившихся файлов нет — LLM не вызывается");
        }
        let reviews: Vec<ModelReview> = if nothing_to_review {
            Vec::new()
        } else {
            self.deps.engine.review_all_models(
                &chunks,
                &ReviewContext {
                    blast_blocks: blast_blocks.clone(),
                    pr: pr.clone(),
                    prior_findings: prior_findings.clone(),
                    changed_paths: changed_paths.clone(),
                    prescan_section: prescan_result.prompt_section.clone(),
                    deadline,
                },
            )?
        };
        // max, а не сумма: в consensus пропущенный чанк считается у каждой модели.
        let skipped_chunks = reviews.iter().map(|r| r.skipped_chunks).max().unwrap_or(0);
        if skipped_chunks > 0 {
            self.log(&format!("⏱ тайм-бюджет: {skipped_chunks} из {} чанков пропущены — отчёт частичный", chunks.len()));
            obs.log_event(
                "time_budget",
                &json!({ "skippedChunks": skipped_chunks, "totalChunks": chunks.len(), "budgetMs": num(budget) }),
            );
        }
        let note = [
            note0,
            if skipped_chunks > 0 {
                format!("⏱ Тайм-бюджет исчерпан: {skipped_chunks} из {} частей не ревьюились — охват частичный, перезапустите ревью.", chunks.len())
            } else {
                String::new()
            },
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
        let ok: Vec<String> = reviews.iter().filter(|r| !review_errored(r)).map(|r| r.model.clone()).collect();
        // Покрытие по чанкам (RFC 19): чанк покрыт, если его отревьюила хоть одна ok-модель.
        // Агрегат собирается из успешных чанков, и без этого счёта «7 из 10 частей» выглядело
        // как полный прогон, а файлы упавших частей попадали в инкрементальный state.
        let ok_reviews: Vec<&ModelReview> = reviews.iter().filter(|r| !review_errored(r)).collect();
        let chunk_covered = |ci: usize| {
            ok_reviews.iter().any(|r| !r.failed_chunks.contains(&ci) && !r.skipped_chunk_indexes.contains(&ci))
        };
        let uncovered_chunks: Vec<usize> =
            if ok_reviews.is_empty() { Vec::new() } else { (0..chunks.len()).filter(|&ci| !chunk_covered(ci)).collect() };
        // Сбой, а не тайм-бюджет: хоть одна модель упала на этом чанке — degraded по
        // REVIEW_LLM_REQUIRED; пропуск по тайм-бюджету — заметка (note).
        let failed_uncovered: Vec<usize> = uncovered_chunks
            .iter()
            .copied()
            .filter(|ci| ok_reviews.iter().any(|r| r.failed_chunks.contains(ci)))
            .collect();
        let uncovered_paths: IndexSet<String> =
            uncovered_chunks.iter().flat_map(|&ci| chunks.get(ci).map(|c| c.paths.clone()).unwrap_or_default()).collect();
        if !failed_uncovered.is_empty() {
            let failed_paths: IndexSet<String> = failed_uncovered
                .iter()
                .flat_map(|&ci| chunks.get(ci).map(|c| c.paths.clone()).unwrap_or_default())
                .collect();
            self.log(&format!(
                "покрытие: {} из {} частей не отревьюены из-за сбоя модели {}",
                failed_uncovered.len(),
                chunks.len(),
                serde_json::to_string(&failed_paths).unwrap_or_default()
            ));
            obs.log_event(
                "coverage_gap",
                &json!({
                    "failed": failed_uncovered.len(),
                    "total": chunks.len(),
                    "paths": failed_paths.iter().take(20).collect::<Vec<_>>(),
                }),
            );
        }
        // Находки моделей метятся происхождением: в JS гейт мутирует ТЕ ЖЕ объекты, что лежат
        // в `reviews[].findings`, и walkthrough sticky видит severity после verifier. Метка
        // снимается до любого вывода (write_back_to_reviews).
        let llm_findings: Vec<Value> = reviews
            .iter()
            .enumerate()
            .filter(|(_, r)| !review_errored(r))
            .flat_map(|(ri, r)| r.findings.iter().enumerate().map(move |(fi, f)| with_origin(f, ri, fi)))
            .collect();
        let mut all_findings: Vec<Value> = prescan_result.findings.clone();
        all_findings.extend(blast_findings.iter().cloned());
        all_findings.extend(dedupe_against_prescan(&llm_findings, &prescan_result.findings, layout));
        self.log_stage(obs, "after_llm_merge", &all_findings);
        // Дерево репозитория для FP-семейства 7. None — не прочитано или чужое: фильтр тогда
        // только по паттернам, и это видно в логе.
        let repo_index: Option<RepoPathIndex> = if all_findings.is_empty() {
            None
        } else {
            let tree = match self.opts.repo_files {
                Some(f) => f().unwrap_or(None),
                None => list_repo_files(&cwd),
            };
            build_repo_path_index(tree.as_deref(), &changed_paths)
        };
        if !all_findings.is_empty() && repo_index.is_none() {
            self.log("FP-семейство 7 выключено: дерево репозитория не прочитано или чужое");
        }
        let patterns_file = match string(cfg, &["feedback", "patternsFile"]) {
            f if f.is_empty() => f,
            f => cwd.join(f).to_string_lossy().into_owned(),
        };

        if ok.is_empty() && !nothing_to_review && (llm_required || all_findings.is_empty()) {
            return self.llm_outage(
                obs,
                st,
                OutageCtx {
                    reviews: &reviews,
                    all_findings,
                    changed_paths: &changed_paths,
                    gate_v: &gate_v,
                    patterns_file: &patterns_file,
                    repo_index: repo_index.as_ref(),
                    texts: &texts,
                    fmap: &fmap,
                    pr: &pr,
                    head_sha: &head_sha,
                    review_mode: &review_mode,
                    route_v: &route_v,
                    blast_metrics_v: &blast_metrics_v,
                    changed_paths_v: &changed_paths_v,
                    models_list: &models_list,
                    dry_run,
                },
            );
        }
        if ok.is_empty() && !all_findings.is_empty() {
            self.log("LLM недоступен — продолжаем с prescan-only findings (R6), REVIEW_LLM_REQUIRED=0");
            obs.log_event("llm_failed", &json!({ "prescanOnly": true, "required": llm_required }));
        }

        let any_ok = !ok.is_empty();
        let vctx = VerifierStageCtx {
            deadline,
            content_map: &content_map,
            annotated: &annotated,
            blast_blocks: &blast_blocks,
            pr: &pr,
            changed_files_section: &changed_files_section,
            files: &fmap,
            prior_keys: &prior_keys,
            prescan_keys: &prescan_keys,
            symbol_cards: symbol_cards.as_ref(),
        };
        let mut removed_llm: Vec<Value> = Vec::new();
        let mut before_resolve = |fp_filtered: Vec<Value>| -> Result<BeforeResolved> {
            let mut current = fp_filtered;
            // Refutation и verifier — LLM-стадии; при полном аутедже пропускаем, чтобы ветки
            // fail-closed и fall-through вели себя симметрично.
            let mut refutation_stats = None;
            if specialists_on && any_ok {
                let out = apply_refutation(&current, true, &block_on_v);
                for m in &out.mutations {
                    if let Some(o) = current.get_mut(m.index).and_then(Value::as_object_mut) {
                        if let Some(sev) = &m.severity {
                            o.insert("severity".into(), sev.clone());
                        }
                        if let Some(s) = &m.refutation_status {
                            o.insert("refutationStatus".into(), json!(s));
                        }
                    }
                }
                for line in &out.logs {
                    self.log(line);
                }
                let stats = serde_json::to_value(&out.stats).unwrap_or(Value::Null);
                let kept: HashSet<usize> = out.kept.iter().copied().collect();
                let (keep, dropped): (Vec<_>, Vec<_>) = current.into_iter().enumerate().partition(|(i, _)| kept.contains(i));
                removed_llm.extend(dropped.into_iter().map(|(_, f)| f));
                current = keep.into_iter().map(|(_, f)| f).collect();
                self.log(&format!("refutation R5: {}", format_refutation_log(&stats)));
                obs.log_event("refutation", &stats);
                self.log_stage(obs, "after_refutation", &current);
                refutation_stats = Some(stats);
            }
            let (verifier_stats, verifier_unavailable) = if any_ok {
                let out = self.run_verifier_stage(current, &vctx, obs);
                removed_llm.extend(out.rejected);
                current = out.findings;
                (out.stats, out.unavailable)
            } else {
                (None, None)
            };
            // Перенесённые находки — ПОСЛЕ refutation и verifier, намеренно: обе стадии они
            // прошли в том прогоне, где были найдены. Пропусти их здесь ещё раз — и strict-
            // refutation понизила бы подтверждённый блокер до minor только потому, что в ЭТОМ
            // прогоне жюри по неизменившемуся файлу не высказывалось.
            if let Some(c) = carry.take().filter(|c| !c.is_empty()) {
                current = c.append_after_verifier(current);
                self.log_stage(obs, "after_carry_over", &current);
            }
            Ok(BeforeResolved { findings: current, refutation_stats, verifier_stats, verifier_unavailable })
        };
        let gate_res = run_gate_resolution(
            GateInput {
                findings: all_findings,
                changed_paths: &changed_paths,
                gate: &gate_v,
                prescan_cfg: get(cfg, &["prescan"]),
                patterns_file: Some(&patterns_file),
                repo_index: repo_index.as_ref(),
                log: self.deps.log,
            },
            Some(&mut before_resolve),
        )?;
        let mut gate_res = gate_res;
        let mut all_findings = std::mem::take(&mut gate_res.findings);
        // Один расчёт отпечатков на прогон: футер sticky, Δ-блок и артефакт — одна величина.
        attach_fingerprints(&mut all_findings, &texts);
        // Кластер — блокер, если блокирует ЛЮБАЯ его находка (RFC 17): решение — свойство
        // находки, а не первой в кластере.
        let blocker_clusters = gate_res
            .clusters
            .iter()
            .filter(|g| g.iter().any(|&i| all_findings.get(i).is_some_and(|f| truthy(f, &["consensusBlock"]))))
            .count();
        let blocker_idx = gate_res.blockers.clone();
        let refutation_stats = gate_res.refutation_stats.clone().unwrap_or(Value::Null);
        let verifier_stats = gate_res.verifier_stats.clone().unwrap_or(Value::Null);
        // Verifier не дал вердикта по находкам уровня блока из-за сбоя (RFC 18): находки уже
        // понижены; блокирует ли прогон — как при аутедже LLM, по REVIEW_LLM_REQUIRED.
        let verifier_unavailable: Option<Value> = gate_res.verifier_unavailable.clone().map(|v| {
            let mut m = v.as_object().cloned().unwrap_or_default();
            m.insert("blocked".into(), json!(llm_required));
            Value::Object(m)
        });
        // Части PR, не отревьюенные из-за сбоя модели (RFC 19), — тот же рычаг.
        let coverage_gap: Option<Value> = (!failed_uncovered.is_empty())
            .then(|| json!({ "failed": failed_uncovered.len(), "total": chunks.len(), "blocked": llm_required }));
        let degraded_block = verifier_unavailable.as_ref().is_some_and(|v| truthy(v, &["blocked"]))
            || coverage_gap.as_ref().is_some_and(|v| truthy(v, &["blocked"]));
        let degraded_why = [
            verifier_unavailable
                .as_ref()
                .map(|v| {
                    format!(
                        "verifier недоступен ({}), не проверено {}",
                        string(v, &["reason"]),
                        get(v, &["unverified"]).map(js_to_string).unwrap_or_default()
                    )
                })
                .unwrap_or_default(),
            coverage_gap
                .as_ref()
                .map(|_| format!("не отревьюено {} из {} частей PR (сбой модели)", failed_uncovered.len(), chunks.len()))
                .unwrap_or_default(),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
        let would_block = !blocker_idx.is_empty() || degraded_block;
        if !gate_res.fp_removed.is_empty() {
            self.log(&format!("ложных находок отфильтровано: {}", gate_res.fp_removed.len()));
        }
        obs.log_event(
            "fp_filter",
            &json!({ "before": gate_res.before_fp_count, "after": gate_res.after_fp_count, "removed": gate_res.fp_removed.len() }),
        );
        self.log_stage(obs, "after_fp_filter", &all_findings);
        {
            let b: Vec<Value> = blocker_idx.iter().map(|&i| all_findings[i].clone()).collect();
            let mut ev = Map::new();
            ev.insert("wouldBlock".into(), json!(would_block));
            ev.insert("blockers".into(), json!(b.len()));
            if let Some(v) = &verifier_unavailable {
                ev.insert("verifierUnavailable".into(), v.clone());
            }
            if let Some(v) = &coverage_gap {
                ev.insert("coverageGap".into(), v.clone());
            }
            ev.insert("sample".into(), slim_findings(Some(&Value::Array(b)), 40));
            obs.log_event("gate", &Value::Object(ev));
        }
        self.log_stage(obs, "after_gate", &all_findings);

        // Маршрут (actionability) пишет скор В САМИ находки — блокеры материализуем после.
        let ctx = self.score_ctx(&all_findings, &prior_keys, &prescan_keys, true);
        let classified = classify_findings(&all_findings, &fmap, &self.classify_cfg(), &ctx, &markers.inline);
        apply_route_mutations(&mut all_findings, &classified.scored);
        let inline: Vec<InlineComment> = classified.inline.iter().map(to_inline).collect();
        let mut reviews = reviews;
        write_back_to_reviews(&mut reviews, &[&gate_res.fp_removed, &removed_llm, &all_findings]);
        strip_origin(&mut all_findings);
        strip_origin(&mut gate_res.fp_removed);
        let reviews_v: Vec<Value> = reviews.iter().map(|r| serde_json::to_value(r).unwrap_or(Value::Null)).collect();
        let outside: Vec<Value> = classified.outside.iter().map(|&i| all_findings[i].clone()).collect();
        let blockers: Vec<Value> = blocker_idx.iter().map(|&i| all_findings[i].clone()).collect();

        // Карточки → находки (аудит RFC 14, п. 1.5). `inline` пустой — паритет с JS: там во
        // множество кладутся объекты инлайн-КОММЕНТАРИЕВ, а сверяются с находками, и счётчик
        // `final.inline` всегда 0. Чинить — вместе с JS-стороной.
        if symbol_cards.is_some() {
            let chunks_v: Vec<Value> = chunks.iter().map(|c| serde_json::to_value(c).unwrap_or(Value::Null)).collect();
            let mut ev = Map::new();
            ev.insert("enabled".into(), json!(true));
            if let Value::Object(o) = symbol_cards_outcome(&chunks_v, &llm_findings, &all_findings, &[]) {
                ev.extend(o);
            }
            obs.log_event("symbol_cards_outcome", &Value::Object(ev));
        } else {
            obs.log_event(
                "symbol_cards_outcome",
                &json!({ "enabled": false, "llm": {
                    "findings": llm_findings.len(),
                    "absenceClaims": llm_findings.iter().filter(|f| is_absence_claim(f)).count(),
                }}),
            );
        }
        self.log_actionability(obs, &all_findings, inline.len(), &outside);

        let summary = build_summary(
            Some(cfg),
            Some(&json!({
                "reviews": reviews_v,
                "allFindings": all_findings,
                "outside": outside,
                "blastBlocks": blast_blocks,
                "blastMetrics": blast_metrics_v,
                "fileCount": files.len(),
                "headSha": head_sha,
                "note": note,
                "verifierStats": verifier_stats,
                "refutationStats": refutation_stats,
                "prescanResult": prescan_result_to_value(&prescan_result),
                "fpRemoved": gate_res.fp_removed,
                "verifierUnavailable": verifier_unavailable,
                "coverageGap": coverage_gap,
                // Δ — против ПОЛНОГО набора прошлого прогона: «закрыто» — всё, что ушло из
                // отчёта, включая снятое инвалидацией.
                "priorFindings": prior_sticky.findings,
                "priorRuns": prior_sticky.runs,
                "priorSha": prior_sticky.sha,
                "costSnapshot": self.cost().snapshot(),
            })),
            Some(&Value::Object(texts.iter().map(|(p, t)| (p.clone(), json!(t))).collect())),
            blocker_clusters,
            &markers.review,
            &markers.data_key,
        )?;
        let total = summary.total;
        let counts = summary.counts.clone();
        // Дисперсия набора находок и вердикт со sha — отдельными событиями (бэклог P1 #8).
        obs.log_event("stability", &summary.delta);
        obs.log_event("verdict", &summary.verdict);

        let mut input = Map::new();
        input.insert("pr".into(), serde_json::to_value(&pr)?);
        input.insert("headSha".into(), json!(head_sha));
        input.insert("reviewMode".into(), json!(review_mode));
        input.insert("route".into(), route_v.clone());
        input.insert("wouldBlock".into(), json!(would_block));
        input.insert("verdict".into(), summary.verdict.clone());
        input.insert("counts".into(), counts.clone());
        input.insert("total".into(), json!(total));
        input.insert("findings".into(), json!(all_findings));
        input.insert("blockers".into(), json!(blockers));
        input.insert("reviews".into(), json!(reviews_v));
        // `null` стадии, которая не работала, в артефакт не пишется (в JS `?? undefined`).
        if !verifier_stats.is_null() {
            input.insert("verifierStats".into(), verifier_stats);
        }
        if !refutation_stats.is_null() {
            input.insert("refutationStats".into(), refutation_stats);
        }
        input.insert("prescanResult".into(), prescan_result_to_value(&prescan_result));
        input.insert("blastMetrics".into(), blast_metrics_v.clone());
        input.insert("changedPaths".into(), changed_paths_v.clone());
        let payload = self.review_json(input)?;
        self.persist_and_capture(st, obs, payload);

        // Состояние следующего прогона — итоговый набор, видимый человеку (без prescan и
        // blast: они детерминированы и пересчитываются каждый раз).
        if incremental_on {
            // Находка, до которой verifier не дошёл (RFC 17/18), и файлы непокрытых чанков
            // (RFC 19) не переносятся, а их файлы не получают хэш: иначе «перезапустите ревью»
            // после degraded-блока перенёс бы непроверенный critical как minor без LLM.
            let unverified = unverified_paths(&all_findings, &uncovered_paths);
            if !unverified.is_empty() {
                self.log(&format!(
                    "инкрементальность: {} файл(ов) с непроверенными verifier'ом находками — в следующем прогоне ревьюятся заново",
                    unverified.len()
                ));
            }
            let next = build_next_state(NextStateInput {
                pr_number: &json!(pr.number),
                head_sha: &head_sha,
                config_fp: &config_fp,
                plan: &plan,
                prior: prior_incremental.as_ref(),
                hashes: &input_hashes,
                cards_input: cards_input.as_ref(),
                cards_targets: cards_targets.as_ref(),
                cards_input_reused,
                findings: &all_findings,
                unverified_paths: Some(&unverified),
            });
            let mut log = |s: &str| self.log(s);
            write_incremental_state(Some(Path::new(&state_file)), &next, &mut log);
        }

        if dry_run {
            self.log("=== DRY-RUN: вывод не постится ===");
            self.log(&format!(
                "модели-ок: {} · findings={total} · inline={} · outside={}",
                ok.join(", "),
                inline.len(),
                outside.len()
            ));
            self.log(&if !blockers.is_empty() {
                format!("⛔ WOULD BLOCK merge: {} кластер(ов) × {}", blockers.len(), block_on.join("/"))
            } else if degraded_block {
                format!("⛔ WOULD BLOCK merge (degraded): {degraded_why}")
            } else if review_mode == "moa" {
                "✅ не блокирует (MoA-aggregator)".to_string()
            } else {
                format!("✅ не блокирует (консенсус ≥{})", js_to_string(&num(number(cfg, &["gate", "consensusMin"]))))
            });
            self.log("\n--- INLINE COMMENTS ---");
            for c in &inline {
                let head: Vec<&str> = c.body.split('\n').take(2).collect();
                self.log(&format!("{}:{}\n  {}", c.path, c.line, head.join(" / ")));
            }
            self.log(&format!("\n--- SUMMARY MARKDOWN ---\n{}", summary.md));
            let exit = i32::from(would_block);
            st.final_obs = json!({
                "exitCode": exit, "dryRun": true, "wouldBlock": would_block, "blockers": blockers.len(),
                "total": total, "counts": counts, "inline": inline.len(),
            });
            if degraded_block {
                if let Some(o) = st.final_obs.as_object_mut() {
                    o.insert("degraded".into(), json!(true));
                }
                return Ok(self.outcome(st, exit, would_block, true, Some(degraded_why)));
            }
            return Ok(self.outcome(st, exit, would_block, false, None));
        }

        let max_inline = usize_of(lim("maxInline"));
        let (posted, all_inline, cleared) = self.publish_inline(&inline, &head_sha, max_inline, false)?;
        let op = self.deps.github.upsert_sticky(&summary.md)?;
        self.log(&format!("review запощен ({posted}/{all_inline} inline, очищено старых {cleared}) · summary {op}"));

        if truthy(cfg, &["triage", "enabled"]) {
            let trusted = parse_trusted_authors(get(cfg, &["feedback", "trustedAuthors"]));
            let tctx = build_triage_context(Some(&json!({
                "allFindings": all_findings,
                "blockers": blockers,
                "changedPaths": changed_paths,
                "headSha": head_sha,
                "blockOn": block_on,
                "patternsFile": if patterns_file.is_empty() { Value::Null } else { json!(patterns_file) },
                "trustedAuthors": trusted,
            })));
            let log: &dyn Fn(&str) = self.deps.log;
            let stats = run_auto_triage(self.deps.github, &tctx, &markers.inline, log);
            obs.log_event("auto_triage", &serde_json::to_value(stats).unwrap_or(Value::Null));
            if stats.resolved > 0 {
                self.log(&format!("auto-triage: resolved {} thread(s)", stats.resolved));
            }
        }

        self.notify(&pr, &counts, total, None);

        if !blockers.is_empty() {
            self.log(&format!("⛔ merge ЗАБЛОКИРОВАН: {} кластер(ов) уровня {}", blockers.len(), block_on.join("/")));
            st.final_obs = json!({
                "exitCode": 1, "wouldBlock": true, "blockers": blockers.len(), "total": total,
                "counts": counts, "inline": inline.len(),
            });
            return Ok(self.outcome(st, 1, true, false, None));
        }
        if degraded_block {
            self.log(&format!("⛔ merge ЗАБЛОКИРОВАН (degraded): {degraded_why} — перезапустите ревью"));
            st.final_obs = json!({
                "exitCode": 1, "wouldBlock": true, "degraded": true, "blockers": 0, "total": total,
                "counts": counts, "inline": inline.len(),
            });
            return Ok(self.outcome(st, 1, true, true, Some(degraded_why)));
        }
        self.log(&format!("готово — блокеров нет ({review_mode})."));
        st.final_obs = json!({
            "exitCode": 0, "wouldBlock": false, "blockers": 0, "total": total, "counts": counts, "inline": inline.len(),
        });
        Ok(self.outcome(st, 0, false, false, None))
    }

    /// Router skip/light: LLM не зовётся, merge не блокируется.
    #[allow(clippy::too_many_arguments)]
    fn router_shortcut(
        &self,
        obs: &Observability,
        st: &mut RunState,
        route: &RouteResult,
        route_v: &Value,
        pr: &PullRequestRef,
        head_sha: &str,
        changed_paths_v: &Value,
        review_mode: &str,
        dry_run: bool,
        post_creds: bool,
    ) -> Result<PipelineOutcome> {
        let md = build_router_summary(
            route_v,
            Some(&json!(head_sha)),
            Some(changed_paths_v),
            truthy(self.cfg, &["router", "enabled"]),
            &self.profile.markers().review,
        )?;
        st.final_obs = json!({ "exitCode": 0, "route": route.route, "wouldBlock": false });
        let mut input = Map::new();
        input.insert("pr".into(), serde_json::to_value(pr)?);
        input.insert("headSha".into(), json!(head_sha));
        input.insert("reviewMode".into(), json!(review_mode));
        input.insert("route".into(), route_v.clone());
        input.insert("wouldBlock".into(), json!(false));
        input.insert("counts".into(), json!({ "critical": 0, "major": 0, "minor": 0, "nit": 0 }));
        input.insert("total".into(), json!(0));
        input.insert("findings".into(), json!([]));
        input.insert("blockers".into(), json!([]));
        input.insert("reviews".into(), json!([]));
        input.insert("changedPaths".into(), changed_paths_v.clone());
        let payload = self.review_json(input)?;
        self.persist_and_capture(st, obs, payload);
        let reason = Some(format!("router {}", route.route));
        if dry_run {
            self.log(&format!("=== DRY-RUN: router {} — LLM пропущен ===", route.route));
            self.log(&format!("\n--- SUMMARY MARKDOWN ---\n{md}"));
        } else if post_creds {
            let _ = self.deps.github.delete_stale_inline(&DeleteStaleOpts::default());
            self.deps.github.upsert_sticky(&md)?;
            self.log(&format!("router {}: sticky обновлён, LLM не вызывался", route.route));
        }
        st.final_obs = json!({ "exitCode": 0, "route": route.route, "wouldBlock": false, "degraded": true });
        Ok(self.outcome(st, 0, false, true, reason))
    }

    /// Все модели недоступны: FP-фильтр и гейт по prescan-находкам (они от LLM не зависят),
    /// sticky с диагнозом. Блок — только осознанный: `REVIEW_LLM_REQUIRED=1` или
    /// подтверждённый prescan-блокер.
    fn llm_outage(&self, obs: &Observability, st: &mut RunState, o: OutageCtx<'_>) -> Result<PipelineOutcome> {
        let llm_required = truthy(self.cfg, &["gate", "llmRequired"]);
        let why = o.reviews.iter().map(|r| format!("{}: {}", r.model, error_text(r))).collect::<Vec<_>>().join("; ");
        let why_public = o
            .reviews
            .iter()
            .map(|r| format!("{}: {}", r.model, public_error_label(r.error.as_ref().map(|e| json!(e)).as_ref())))
            .collect::<Vec<_>>()
            .join("; ");
        self.log(&format!(
            "все модели недоступны: {why} {}",
            if llm_required { "— fail-closed (REVIEW_LLM_REQUIRED)" } else { "— fail-open" }
        ));
        // Refutation/verifier при полном аутедже недоступны по определению — гейт без них.
        let gate_res = run_gate_resolution(
            GateInput {
                findings: o.all_findings,
                changed_paths: o.changed_paths,
                gate: o.gate_v,
                prescan_cfg: get(self.cfg, &["prescan"]),
                patterns_file: Some(o.patterns_file),
                repo_index: o.repo_index,
                log: self.deps.log,
            },
            None,
        )?;
        let mut all = gate_res.findings;
        // Отпечатки и здесь: артефакт этой ветки тоже дедуплицируется потребителем по коду.
        attach_fingerprints(&mut all, o.texts);
        let blockers: Vec<Value> = gate_res.blockers.iter().map(|&i| all[i].clone()).collect();
        if !gate_res.fp_removed.is_empty() {
            self.log(&format!("ложных находок отфильтровано: {}", gate_res.fp_removed.len()));
        }
        obs.log_event(
            "fp_filter",
            &json!({ "before": gate_res.before_fp_count, "after": gate_res.after_fp_count, "removed": gate_res.fp_removed.len() }),
        );
        self.log_stage(obs, "after_fp_filter", &all);
        let all_v = Value::Array(all.clone());
        let counts = count_by_severity(Some(&all_v));
        let gate_blocked = llm_required || !blockers.is_empty();
        obs.log_event("llm_failed", &json!({ "prescanOnly": true, "required": llm_required }));
        obs.log_event(
            "gate",
            &json!({
                "wouldBlock": gate_blocked,
                "blockers": blockers.len(),
                "sample": slim_findings(Some(&Value::Array(blockers.clone())), 40),
            }),
        );
        self.log_stage(obs, "after_gate", &all);
        let mut input = Map::new();
        input.insert("pr".into(), serde_json::to_value(o.pr)?);
        input.insert("headSha".into(), json!(o.head_sha));
        input.insert("reviewMode".into(), json!(o.review_mode));
        input.insert("route".into(), o.route_v.clone());
        input.insert("wouldBlock".into(), json!(gate_blocked));
        input.insert("counts".into(), counts.clone());
        input.insert("total".into(), json!(all.len()));
        input.insert("findings".into(), all_v.clone());
        input.insert("blockers".into(), json!(blockers));
        input.insert("reviews".into(), json!([]));
        input.insert("llmFailed".into(), json!(true));
        input.insert(
            "llmErrors".into(),
            json!(o
                .reviews
                .iter()
                .map(|r| json!({ "model": r.model, "error": js_slice_utf16(r.error.as_deref().unwrap_or(""), 200) }))
                .collect::<Vec<_>>()),
        );
        input.insert("blastMetrics".into(), o.blast_metrics_v.clone());
        input.insert("changedPaths".into(), o.changed_paths_v.clone());
        let payload = self.review_json(input)?;
        self.persist_and_capture(st, obs, payload);
        let exit = i32::from(gate_blocked);
        let final_obs = json!({
            "exitCode": exit, "llmFailed": true, "wouldBlock": gate_blocked, "blockers": blockers.len(),
            "total": all.len(), "counts": counts, "degraded": true,
        });
        if o.dry_run {
            let mut f = final_obs.as_object().cloned().unwrap_or_default();
            f.insert("dryRun".into(), json!(true));
            st.final_obs = Value::Object(f);
            return Ok(self.outcome(st, exit, gate_blocked, true, Some("llm unavailable".into())));
        }
        let blocker_idx: HashSet<usize> = gate_res.blockers.iter().copied().collect();
        let advisory: Vec<Value> = all
            .iter()
            .enumerate()
            .filter(|(i, f)| truthy(f, &["prescan"]) && !blocker_idx.contains(i))
            .map(|(_, f)| f.clone())
            .collect();
        let body = build_llm_unavailable_summary(
            llm_required,
            Some(&json!(why_public)),
            Some(&json!(o.models_list)),
            Some(&Value::Array(blockers.clone())),
            Some(&Value::Array(advisory)),
            &self.profile.markers().review,
        )?;
        let ctx = self.score_ctx(&blockers, &HashSet::new(), &HashSet::new(), false);
        let classified = classify_findings(&blockers, o.fmap, &self.classify_cfg(), &ctx, &self.profile.markers().inline);
        let inline: Vec<InlineComment> = classified.inline.iter().map(to_inline).collect();
        match self.publish_inline(&inline, o.head_sha, usize_of(number(self.cfg, &["limits", "maxInline"])), true) {
            Ok((p, t, c)) => self.log(&format!("fail-closed: запощено {p}/{t} prescan inline, очищено старых {c}")),
            Err(e) => self.log(&format!(
                "fail-closed: постинг/чистка inline упали (не критично): {}",
                js_slice_utf16(&format!("{e:#}"), 200)
            )),
        }
        if let Err(e) = self.deps.github.upsert_sticky(&body) {
            self.log(&format!("и заметку не запостили: {}", js_slice_utf16(&format!("{e:#}"), 200)));
        }
        // Неблокирующий fail-open дежурного не будит: диагноз остаётся в sticky.
        if gate_blocked {
            self.notify(o.pr, &counts, all.len(), Some("fail-closed: LLM outage"));
        }
        st.final_obs = final_obs;
        Ok(self.outcome(st, exit, gate_blocked, true, Some("llm unavailable".into())))
    }

    /// `markVerifierUnavailable` (RFC 18): verifier не дал вердикта по находкам уровня блока
    /// из-за сбоя (исключение первого вызова, ни одного валидного вердикта, тайм-бюджет).
    /// Такая находка не может считаться подтверждённым блокером — понижается до minor с
    /// `verifierStatus: unavailable`; судьбу прогона решает REVIEW_LLM_REQUIRED в гейте.
    fn mark_verifier_unavailable(&self, all: &mut [Value], pending: &[usize], reason: &str, obs: &Observability) -> Value {
        for &i in pending {
            let Some(o) = all.get_mut(i).and_then(Value::as_object_mut) else { continue };
            let was = o.get("severity").map(js_to_string).unwrap_or_else(|| "undefined".into());
            o.insert(
                "verifierReason".into(),
                json!(format!("verifier недоступен ({reason}) — было {was}, понижено до minor")),
            );
            o.insert("severity".into(), json!("minor"));
            o.insert("verifierStatus".into(), json!("unavailable"));
        }
        self.log(&format!(
            "verifier недоступен ({reason}): {} находок уровня блока не проверены → minor",
            pending.len()
        ));
        obs.log_event("verifier_unavailable", &json!({ "reason": reason, "unverified": pending.len() }));
        json!({ "reason": reason, "unverified": pending.len() })
    }

    /// Verifier Pass B (`runVerifierStage`): кандидаты critical/major + доказуемые advisory,
    /// батчами (RFC 17). Сбой судит только strict-режим ([`Self::mark_verifier_unavailable`]);
    /// при REVIEW_VERIFIER_STRICT=0 находки без вердикта остаются как есть.
    fn run_verifier_stage(&self, mut all: Vec<Value>, c: &VerifierStageCtx<'_>, obs: &Observability) -> VerifierStageOut {
        let cfg = self.cfg;
        if !truthy(cfg, &["verifier", "enabled"]) {
            return VerifierStageOut::untouched(all);
        }
        let block_on = str_list(cfg, &["gate", "blockOn"]);
        let block_on_v: Vec<Value> = block_on.iter().map(|s| json!(s)).collect();
        let strict = truthy(cfg, &["verifier", "strictBlocks"]);
        let is_block_tier = |f: &Value| {
            let sev = f.get("severity").and_then(Value::as_str).unwrap_or("");
            block_on.iter().any(|b| b == sev) && !truthy(f, &["prescan"])
        };
        let budget_low = |now: u64| c.deadline > 0 && (c.deadline as i128 - now as i128) < 120_000;
        if budget_low(self.now()) {
            self.log("verifier pass B пропущен: тайм-бюджет на исходе (<2 мин в запасе)");
            obs.log_event("verifier_skip", &json!({ "reason": "time_budget" }));
            let pending: Vec<usize> = (0..all.len()).filter(|&i| is_block_tier(&all[i])).collect();
            let unavailable = (strict && !pending.is_empty())
                .then(|| self.mark_verifier_unavailable(&mut all, &pending, "time_budget", obs));
            return VerifierStageOut { unavailable, ..VerifierStageOut::untouched(all) };
        }
        let threshold = number(cfg, &["actionability", "threshold"]);
        // Порог ДОПУСКА к verifier'у ниже порога маршрута (docs/review-quality.md §2.2, P1):
        // кандидатом становится то, что СТАНЕТ доказуемым после подтверждения. Порог
        // маршрута пересчитывается в classify уже с вердиктом на руках.
        let verify_min = number(cfg, &["actionability", "verifyMin"]);
        let verify_threshold = if verify_min.is_finite() { verify_min } else { threshold };
        let verify_advisory = number(cfg, &["actionability", "verifyAdvisory"]);
        let advisory_max = usize_of(verify_advisory);
        let pool: Vec<usize> = if advisory_max > 0 {
            let ctx = self.score_ctx(&all, c.prior_keys, c.prescan_keys, true);
            let scored = route_findings(
                &all,
                c.files,
                &RouteCfg {
                    inline_on: block_on.clone(),
                    threshold: finite_or(verify_threshold, RouteCfg::default().threshold),
                    max_advisory_inline: advisory_max,
                    strict: truthy(cfg, &["gate", "strict"]),
                },
                &ctx,
            );
            apply_route_mutations(&mut all, &scored);
            scored.iter().filter(|s| s.promoted).map(|s| s.index).collect()
        } else {
            Vec::new()
        };
        // Блокеры сверх капа одного вызова идут следующими батчами (RFC 17): кап одного
        // вызова держит размер промпта, число батчей — цену прогона.
        let batch_cap = usize_of(number(cfg, &["verifier", "maxCandidates"])).max(1);
        let max_batches_n = number(cfg, &["verifier", "maxBatches"]);
        // `verifier.maxBatches ?? 1`: нет поля — один батч; NaN/0 — `Math.max(1, …)`.
        let max_batches = if get(cfg, &["verifier", "maxBatches"]).is_none_or(Value::is_null) {
            1
        } else {
            usize_of(max_batches_n).max(1)
        };
        let candidates =
            select_verifier_candidates(&all, &block_on_v, batch_cap.saturating_mul(max_batches), &pool, advisory_max);
        if !pool.is_empty() {
            // Насколько пониженный порог расширил допуск — измеримо, а не вера в P1.
            let admitted = pool
                .iter()
                .filter(|&&i| all[i].get("actionabilityScore").and_then(js_to_number).unwrap_or(0.0) < finite_or(threshold, 0.0))
                .count();
            self.log(&format!(
                "verifier: +{} advisory-кандидат(ов) по доказуемости (порог допуска {} vs маршрут {}; из них {admitted} прошли только по пониженному порогу)",
                pool.len(),
                js_to_string(&num(verify_threshold)),
                js_to_string(&num(threshold))
            ));
            obs.log_event(
                "verifier_advisory",
                &json!({
                    "pool": pool.len(),
                    "admittedByLowerBar": admitted,
                    "verifyThreshold": num(verify_threshold),
                    "routeThreshold": num(threshold),
                }),
            );
        }
        if candidates.is_empty() {
            obs.log_event("verifier_skip", &json!({ "reason": "no_candidates" }));
            return VerifierStageOut::untouched(all);
        }
        // Первый батч — как прежний единственный вызов: до капа блокеров плюс advisory.
        // Остальные — только блокеры: advisory не стоят лишнего вызова.
        let block_tier: Vec<usize> = candidates.iter().copied().filter(|&i| is_block_tier(&all[i])).collect();
        let advisory: Vec<usize> = candidates.iter().copied().filter(|&i| !is_block_tier(&all[i])).collect();
        let mut batches: Vec<Vec<usize>> = vec![block_tier.iter().take(batch_cap).copied().chain(advisory.iter().copied()).collect()];
        for chunk in block_tier.get(batch_cap..).unwrap_or(&[]).chunks(batch_cap) {
            batches.push(chunk.to_vec());
        }
        self.log(&format!(
            "verifier pass B: {} кандидат(ов) {} · батчей {} · модель {}",
            candidates.len(),
            block_on.join("/"),
            batches.len(),
            string(cfg, &["verifier", "model"])
        ));
        let margin = number(cfg, &["verifier", "contextMargin"]);
        let verify_batch = |all: &[Value], batch: &[usize], bi: usize| -> Result<Vec<Value>> {
            let mut cand_values: Vec<Value> = batch.iter().map(|&i| all[i].clone()).collect();
            // Метка происхождения — служебная, в промпт verifier'а она не уходит.
            strip_origin(&mut cand_values);
            // Verifier получает только файлы/секции диффа кандидатов и окна вокруг их строк.
            let cand_files: IndexSet<String> = cand_values
                .iter()
                .filter_map(|f| f.get("file").filter(|v| js_truthy(v)).map(js_to_string))
                .collect();
            let ctx_window = build_verifier_file_contents(c.content_map, &cand_values, margin);
            let sections: Vec<String> = extract_file_diff_sections(c.annotated)
                .into_iter()
                .filter(|(p, _)| cand_files.contains(p))
                .map(|(_, s)| s)
                .collect();
            let annotated_scoped = if sections.is_empty() { c.annotated.to_string() } else { sections.join("\n") };
            let tag = if batches.len() > 1 { format!(" [батч {}/{}]", bi + 1, batches.len()) } else { String::new() };
            self.log(&format!(
                "verifier ctx{tag}: {} файл(ов) с кандидатами из {}, контекст {}→{} симв. (окна ±{}), дифф {}/{} симв.",
                cand_files.len(),
                c.content_map.len(),
                ctx_window.before,
                ctx_window.after,
                js_to_string(&num(margin)),
                u16_len(&annotated_scoped),
                u16_len(c.annotated)
            ));
            // Карточки символов verifier'у (RFC 14, решение 4) — только под флагом.
            let verifier_cards: Vec<String> = match c.symbol_cards {
                Some(sc) if truthy(cfg, &["verifier", "symbolCards"]) && !sc.by_path.is_empty() => {
                    select_chunk_cards(
                        &sc.by_path,
                        &cand_files.iter().cloned().collect::<Vec<_>>(),
                        number(cfg, &["limits", "symbolCardsMax"]),
                        number(cfg, &["limits", "symbolCardsMaxChars"]),
                        &format!("{annotated_scoped}\n{}", ctx_window.text),
                    )
                    .cards
                }
                _ => Vec::new(),
            };
            if !verifier_cards.is_empty() {
                self.log(&format!("verifier: карточек символов {}", verifier_cards.len()));
            }
            self.deps.engine.verify_findings(
                &cand_values,
                &VerifyContext {
                    annotated: annotated_scoped,
                    file_contents: ctx_window.text,
                    blast_blocks: c.blast_blocks.to_vec(),
                    pr: c.pr.clone(),
                    changed_files_section: c.changed_files_section.to_string(),
                    symbol_cards: verifier_cards,
                },
            )
        };

        // Сначала все вызовы, потом одно применение: сбой посреди батчей не должен оставить
        // набор наполовину применённым.
        let mut sent: Vec<usize> = Vec::new();
        let mut verdicts: Vec<Value> = Vec::new();
        let mut batches_sent = 0usize;
        let mut stop_reason = "";
        for (bi, batch) in batches.iter().enumerate() {
            if bi > 0 && budget_low(self.now()) {
                stop_reason = "time_budget";
                self.log(&format!("verifier: тайм-бюджет — проверено {} из {} кандидатов", sent.len(), candidates.len()));
                break;
            }
            let batch_verdicts = match verify_batch(&all, batch, bi) {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("{e:#}");
                    if bi == 0 {
                        self.log(&format!("verifier pass failed: {msg}"));
                        obs.log_event("verifier_error", &json!({ "error": js_slice_utf16(&msg, 200) }));
                        let unavailable = (strict && !block_tier.is_empty())
                            .then(|| self.mark_verifier_unavailable(&mut all, &block_tier, "error", obs));
                        return VerifierStageOut { unavailable, ..VerifierStageOut::untouched(all) };
                    }
                    // Первый батч прошёл: его вердикты не выбрасываем, непроверенный хвост —
                    // недоступность (ниже).
                    self.log(&format!("verifier: батч {}/{} упал ({msg}) — хвост не проверен", bi + 1, batches.len()));
                    obs.log_event("verifier_error", &json!({ "batch": bi, "error": js_slice_utf16(&msg, 200) }));
                    stop_reason = "batch_error";
                    break;
                }
            };
            for v in batch_verdicts {
                // `Number.isInteger(idx) && 0 <= idx < batch.length` — чужие индексы отсеиваются
                // до сдвига, иначе попали бы в соседний батч.
                let Some(idx) = v.get("candidateIndex").and_then(Value::as_f64).filter(|x| x.fract() == 0.0) else {
                    continue;
                };
                if idx < 0.0 || idx >= batch.len() as f64 {
                    continue;
                }
                let mut m = v.as_object().cloned().unwrap_or_default();
                m.insert("candidateIndex".into(), json!(idx as usize + sent.len()));
                verdicts.push(Value::Object(m));
            }
            sent.extend(batch.iter().copied());
            batches_sent += 1;
        }
        // Ни одного валидного вердикта — сбой судьи, а не его решение: тот же исход, что у
        // исключения.
        if verdicts.is_empty() && sent.iter().any(|&i| is_block_tier(&all[i])) {
            obs.log_event("verifier_error", &json!({ "error": "no valid verdicts" }));
            let unavailable =
                (strict && !block_tier.is_empty()).then(|| self.mark_verifier_unavailable(&mut all, &block_tier, "empty", obs));
            return VerifierStageOut { unavailable, ..VerifierStageOut::untouched(all) };
        }
        let applied = apply_verifier_verdicts(&all, &sent, &verdicts, strict);
        for m in &applied.mutations {
            if let Some(o) = all.get_mut(m.index).and_then(Value::as_object_mut) {
                for (k, v) in &m.set {
                    o.insert(k.clone(), v.clone());
                }
            }
        }
        for line in &applied.logs {
            self.log(line);
        }
        let mut stats = applied.stats.clone();
        let sent_set: HashSet<usize> = sent.iter().copied().collect();
        // Хвост, не дошедший из-за сбоя или тайм-бюджета, — недоступность, а не кап.
        let mut unavailable = None;
        if !stop_reason.is_empty() {
            let tail: Vec<usize> =
                applied.kept.iter().copied().filter(|&i| is_block_tier(&all[i]) && !sent_set.contains(&i)).collect();
            if strict && !tail.is_empty() {
                unavailable = Some(self.mark_verifier_unavailable(&mut all, &tail, stop_reason, obs));
            }
        }
        // Strict: eligible-находки, не дошедшие до verifier'а (сверх капа × батчей), без
        // верификации не блокируют (бэклог #7) → minor.
        if strict {
            let why = format!("сверх капа {batch_cap} × {max_batches}");
            let mut overflow = 0usize;
            for &i in &applied.kept {
                if is_block_tier(&all[i]) && !sent_set.contains(&i) {
                    if let Some(o) = all[i].as_object_mut() {
                        o.insert("severity".into(), json!("minor"));
                        o.insert("verifierStatus".into(), json!("overflow"));
                        o.insert(
                            "verifierReason".into(),
                            json!(format!("не проверено verifier'ом ({why}) — понижено до minor")),
                        );
                    }
                    overflow += 1;
                }
            }
            if overflow > 0 {
                stats.insert("overflow".into(), json!(overflow));
                self.log(&format!(
                    "verifier strict: {overflow} находок не проверено ({why}) → minor (без верификации не блокируют)"
                ));
            }
        }
        stats.insert("batches".into(), json!(batches_sent));
        let n = |k: &str| stats.get(k).map(js_to_string).unwrap_or_default();
        self.log(&format!(
            "verifier итог: verified={} rejected={} downgraded={} missing={}",
            n("verified"),
            n("rejected"),
            n("downgraded"),
            n("missing")
        ));
        let stats = Value::Object(stats);
        obs.log_event("verifier", &stats);
        let kept: HashSet<usize> = applied.kept.iter().copied().collect();
        let (out, rejected): (Vec<_>, Vec<_>) = all.into_iter().enumerate().partition(|(i, _)| kept.contains(i));
        let out: Vec<Value> = out.into_iter().map(|(_, f)| f).collect();
        self.log_stage(obs, "after_verifier", &out);
        VerifierStageOut {
            findings: out,
            stats: Some(stats),
            rejected: rejected.into_iter().map(|(_, f)| f).collect(),
            unavailable,
        }
    }

    /// Распределение actionability-скоров (docs/review-quality.md §3, P3.2): пик у порога —
    /// мешает порог, пик в нуле — нет сигналов. Топ причин — по находкам свода.
    fn log_actionability(&self, obs: &Observability, findings: &[Value], inline_count: usize, outside: &[Value]) {
        if !obs.enabled {
            return;
        }
        // Ключи гистограммы — как у JS-объекта с числовыми ключами: по возрастанию.
        let mut hist: std::collections::BTreeMap<i64, u64> = std::collections::BTreeMap::new();
        for f in findings {
            if let Some(s) = f.get("actionabilityScore").and_then(Value::as_f64).filter(|s| s.is_finite()) {
                *hist.entry(s as i64).or_insert(0) += 1;
            }
        }
        let mut reasons: IndexMap<String, u64> = IndexMap::new();
        for f in outside {
            for why in f.get("actionabilityReasons").and_then(Value::as_array).into_iter().flatten() {
                *reasons.entry(reason_key(&js_to_string(why))).or_insert(0) += 1;
            }
        }
        let mut top: Vec<(String, u64)> = reasons.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top.truncate(5);
        obs.log_event(
            "actionability",
            &json!({
                "threshold": get(self.cfg, &["actionability", "threshold"]).cloned().unwrap_or(Value::Null),
                "inline": inline_count,
                "outside": outside.len(),
                "histogram": hist.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<_, _>>(),
                "topOutsideReasons": top.iter().map(|(k, c)| json!({ "key": k, "count": c })).collect::<Vec<_>>(),
            }),
        );
    }
}

/// Служебная метка происхождения LLM-находки: `"<review>:<finding>"`.
const ORIGIN_KEY: &str = "__pipelineOrigin";

fn with_origin(f: &Value, ri: usize, fi: usize) -> Value {
    let mut f = f.clone();
    if let Some(o) = f.as_object_mut() {
        o.insert(ORIGIN_KEY.into(), json!(format!("{ri}:{fi}")));
    }
    f
}

fn strip_origin(findings: &mut [Value]) {
    for f in findings {
        if let Some(o) = f.as_object_mut() {
            o.shift_remove(ORIGIN_KEY);
        }
    }
}

/// Последнее состояние каждой LLM-находки — обратно в `reviews[].findings`: в JS это одни
/// и те же объекты (verifier, refutation, гейт и маршрут пишут в них), и walkthrough sticky
/// со счётчиками по моделям читает уже итоговую severity. Наборы — от ранних к поздним:
/// снятые FP-фильтром, снятые жюри/verifier'ом, итоговые.
fn write_back_to_reviews(reviews: &mut [ModelReview], sets: &[&Vec<Value>]) {
    for set in sets {
        for f in set.iter() {
            let Some(origin) = f.get(ORIGIN_KEY).and_then(Value::as_str) else { continue };
            let Some((ri, fi)) = origin.split_once(':') else { continue };
            let (Ok(ri), Ok(fi)) = (ri.parse::<usize>(), fi.parse::<usize>()) else { continue };
            if let Some(slot) = reviews.get_mut(ri).and_then(|r| r.findings.get_mut(fi)) {
                let mut v = f.clone();
                if let Some(o) = v.as_object_mut() {
                    o.shift_remove(ORIGIN_KEY);
                }
                *slot = v;
            }
        }
    }
}

fn finite_or(x: f64, d: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        d
    }
}

fn to_inline(c: &crate::actionability::InlineComment) -> InlineComment {
    let line = js_to_number(&c.line).filter(|n| n.is_finite() && *n >= 0.0).unwrap_or(0.0) as u64;
    InlineComment::new(c.path.clone(), line, c.body.clone())
}

/// Вход ветки LLM-аутеджа — срез локальных переменных `body`.
struct OutageCtx<'o> {
    reviews: &'o [ModelReview],
    all_findings: Vec<Value>,
    changed_paths: &'o [String],
    gate_v: &'o Value,
    patterns_file: &'o str,
    repo_index: Option<&'o RepoPathIndex>,
    texts: &'o IndexMap<String, LineText>,
    fmap: &'o FilesMap,
    pr: &'o PullRequestRef,
    head_sha: &'o str,
    review_mode: &'o str,
    route_v: &'o Value,
    blast_metrics_v: &'o Value,
    changed_paths_v: &'o Value,
    models_list: &'o [String],
    dry_run: bool,
}

// ---------------------------------------------------------------------------
// Экспорт R9
// ---------------------------------------------------------------------------

/// `writeReviewExports` (`lib/export.mjs:478-512`): JSON-артефакт, swarm-вердикт и SARIF
/// в `cfg.export.dir` (иначе каталог профиля), относительно корня прогона.
pub fn write_review_exports(
    cfg: &Value,
    profile: &Profile,
    payload: &Value,
    cwd: &Path,
    log: &(dyn Fn(&str) + Send + Sync),
) -> Result<ExportResult> {
    let tool = profile.tool_name.clone();
    let dir_cfg = string(cfg, &["export", "dir"]);
    let dir_rel = if dir_cfg.is_empty() { profile.paths().export_dir().to_string() } else { dir_cfg };
    let dir = cwd.join(dir_rel);
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let repo = string(cfg, &["github", "repo"]);
    let repository_uri = if repo.is_empty() { None } else { Some(format!("https://github.com/{repo}")) };
    let sarif_all = get(cfg, &["export", "sarifAll"]) == Some(&Value::Bool(true));
    let sarif_findings = crate::export::select_sarif_findings(Some(payload), sarif_all);
    let mut written = Vec::new();
    if truthy(cfg, &["export", "json"]) {
        let json_path = dir.join(format!("{tool}.json"));
        std::fs::write(&json_path, format!("{}\n", js_stringify_pretty(payload)))
            .with_context(|| format!("write {}", json_path.display()))?;
        log(&format!("export R9: JSON → {}", json_path.display()));
        written.push(json_path);
        if let Some(swarm) = payload.get("swarm").filter(|v| js_truthy(v)) {
            let swarm_path = dir.join(format!("{tool}-swarm.json"));
            std::fs::write(&swarm_path, format!("{}\n", js_stringify_pretty(swarm)))
                .with_context(|| format!("write {}", swarm_path.display()))?;
            log(&format!("export E22: swarm → {}", swarm_path.display()));
            written.push(swarm_path);
        }
    }
    if truthy(cfg, &["export", "sarif"]) {
        let sarif_path = dir.join(format!("{tool}.sarif.json"));
        let profile_json = json!({
            "toolName": profile.tool_name,
            "sarifRulePrefix": profile.sarif_rule_prefix,
            "repositoryUri": if profile.repository_uri.is_empty() { Value::Null } else { json!(profile.repository_uri) },
        });
        let sarif = crate::export::build_sarif(Some(payload), repository_uri.as_deref(), sarif_all, Some(&profile_json));
        std::fs::write(&sarif_path, format!("{}\n", js_stringify_pretty(&sarif)))
            .with_context(|| format!("write {}", sarif_path.display()))?;
        log(&format!(
            "export R9: SARIF → {} ({} results{})",
            sarif_path.display(),
            sarif_findings.len(),
            if sarif_all { ", all" } else { ", blockers" }
        ));
        written.push(sarif_path);
    }
    Ok(ExportResult { dir, written, sarif_results: sarif_findings.len() })
}

/// Строки рабочего дерева как `Arc` — сигнатура [`ReadLines`] для тестов.
pub fn lines_of(text: &str) -> Arc<Vec<String>> {
    Arc::new(text.split('\n').map(str::to_string).collect())
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
