//! Ревью-MCP — порт `lib/review-mcp.mjs`, `lib/review-mcp-handshake.mjs` и
//! `lib/review-mcp-tools.mjs` (RFC 16, задача T41; RFC 08–12).
//!
//! Превью для агента, не merge-гейт: GitHub не трогается (немой клиент вместо
//! настоящего), exit по блокерам не бывает, structure не импортируется.
//!
//! Инварианты:
//! - пин: `headSha` обязателен на живом диффе, пути — только внутри корня MCP
//!   (`resolve_contained_path`), sha сверяется с HEAD и чистотой дерева, если корень —
//!   git-дерево и не передан `uncommitted: true`;
//! - `review_run` — единственный вызов LLM и единственный слот: проверка «слот
//!   свободен» и его захват идут под одним мьютексом состояния (в JS — очередь
//!   `withClaimLock`), поэтому второй прогон с другим пином получает
//!   `review already running`, а тот же пин — stub бегущего прогона;
//! - `wait: false` — stub с `jobId` до MoA, сам прогон — в фоновом потоке, результат
//!   читается поллингом `review_findings` / `review_artifact` по `jobId`; sidecar
//!   `.ai-review/mcp/runs/<jobId>.json` переживает перезапуск сервера.
//!
//! Внешние эффекты — через [`McpDeps`] (git, прогон пайплайна, дешёвые стадии), как
//! DI `extra` в JS: тесты гоняют весь домен без сети и без git.
//!
//! Отклонения от JS (осознанные):
//! - корень — абсолютный путь, вычисленный подкомандой один раз (в JS `mcpRoot`
//!   берёт сырое `REVIEW_MCP_ROOT` на каждый вызов: относительный корень после
//!   `chdir` резолвится второй раз, неподставленный `${…}` ломает каждый путь);
//! - любая ошибка `review_run` после захвата слота освобождает слот и пишет degraded
//!   sidecar. В JS ошибки до `try` (дифф вне корня, битый `priorExportPath`) оставляли
//!   слот занятым до перезапуска, а sidecar — вечным `running`;
//! - каталог sidecar-ов проверяется на containment (symlink `.ai-review` наружу корня
//!   не даёт писать вне него);
//! - тексты системных ошибок — от Rust std (`No such file or directory (os error 2)`),
//!   ошибки разбора JSON — `invalid JSON: …` от serde_json; коды ответов те же;
//! - `startedAt` превью `review_run` входит в `tokenBudget` (JS дописывал его после
//!   подгонки, и превью выходило за бюджет на ~10 токенов);
//! - `fp` при равных severity и score сравнивается побайтно, а не `localeCompare`
//!   (отпечатки — hex в нижнем регистре, порядок совпадает).

use std::collections::HashMap;
use std::io::Read;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::actionability::{score_actionability, ScoreCtx};
use crate::diff::{collect_blast_symbols, parse_diff};
use crate::findings::{
    filter_stale_prior_findings, finding_fingerprint, js_to_number, js_to_string, js_trim, js_truthy, LineText,
    PriorFileRec,
};
use crate::fp_patterns::{default_fp_patterns, match_fp_patterns};
use crate::metrics::{aggregate_metrics, parse_jsonl};
use crate::observability::iso_from_epoch_ms;
use crate::pipeline::{load_review_config, CliArgs, PipelineOutcome};
use crate::profile::Profile;
use crate::report::{js_json_stringify, js_slice_utf16, utf16_len};
use crate::router::route_diff;
use crate::traits::{
    BlastRadius, DeleteStaleOpts, GhCallOpts, GithubApi, LoadedDiff, PostReviewParams, PostReviewResult,
    PrescanResult, StickyData, ThreadRef,
};
use crate::util::{canonical_json, env_flag, resolve_contained_path, sha256_str};

pub const REVIEW_MCP_PROTOCOL_VERSION: u64 = 1;

const STORE_CAP: usize = 8;
const DISK_CAP: usize = 32;
const TEXT_MAX: usize = 8192;
const ARTIFACT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const BLAST_MAX_FILES: usize = 50;
const EXTRA_PATHS_MAX: usize = 20;
const DEFAULT_TOKEN_BUDGET: i64 = 4000;
const TOKEN_BUDGET_MIN: i64 = 256;
const TOKEN_BUDGET_MAX: i64 = 32000;
const SIDECAR_DIR: &str = ".ai-review/mcp/runs";
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// `maxBuffer` git-вызовов (`createDefaultGit`): больший вывод — ошибка, как в execFile.
const GIT_MAX_BUFFER: usize = 1024 * 1024;

/// Логгер прогона (тот же тип, что `cli::wiring::RunLog`).
pub type McpLog = Arc<dyn Fn(&str) + Send + Sync>;
/// notification без id: `(method, params)`. Пишется и из фонового потока.
pub type McpNotify = Arc<dyn Fn(&str, Value) + Send + Sync>;

// ---------------------------------------------------------------------------
// Каталог тулзов (lib/review-mcp-tools.mjs)
// ---------------------------------------------------------------------------

fn head_sha_prop() -> Value {
    json!({ "type": "string", "pattern": "^[0-9a-fA-F]{7,64}$" })
}
fn uncommitted_prop() -> Value {
    json!({ "type": "boolean" })
}
fn diff_path_prop() -> Value {
    json!({ "type": "string", "minLength": 1 })
}
fn token_budget_prop() -> Value {
    json!({ "type": "integer", "minimum": TOKEN_BUDGET_MIN, "maximum": TOKEN_BUDGET_MAX })
}
fn run_id_prop() -> Value {
    json!({ "type": "string", "pattern": "^[0-9a-f]{64}$" })
}

pub const REVIEW_RUN: &str = "review_run";
pub const REVIEW_FINDINGS: &str = "review_findings";
pub const REVIEW_ROUTE: &str = "review_route";
pub const REVIEW_PRESCAN: &str = "review_prescan";
pub const FP_CHECK: &str = "fp_check";
pub const FINGERPRINT_CHECK: &str = "fingerprint_check";
pub const REVIEW_ARTIFACT: &str = "review_artifact";
pub const BLAST_SCOPE: &str = "blast_scope";
pub const ACTIONABILITY_EXPLAIN: &str = "actionability_explain";
pub const REVIEW_METRICS: &str = "review_metrics";

fn default_tools() -> Vec<Value> {
    vec![
        json!({
            "name": REVIEW_RUN,
            "description": "Run a pinned dry-run review (no GitHub). Default wait=true blocks until MoA. wait=false returns a running stub with jobId; page via review_findings.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["diffPath", "headSha"],
                "properties": {
                    "diffPath": diff_path_prop(),
                    "headSha": head_sha_prop(),
                    "tokenBudget": token_budget_prop(),
                    "priorExportPath": { "type": "string", "minLength": 1 },
                    "uncommitted": uncommitted_prop(),
                    "wait": { "type": "boolean" },
                },
            },
        }),
        json!({
            "name": REVIEW_FINDINGS,
            "description": "Page findings of a previous review_run by jobId or runFingerprint. Does not invoke the LLM. Running jobs return empty findings.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "jobId": run_id_prop(),
                    "runFingerprint": run_id_prop(),
                    "headSha": head_sha_prop(),
                    "file": { "type": "string", "maxLength": 1000 },
                    "severity": { "type": "string", "enum": ["critical", "major", "minor", "nit"] },
                    "offset": { "type": "integer", "minimum": 0, "maximum": 10000 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                },
            },
        }),
        json!({
            "name": REVIEW_ROUTE,
            "description": "Classify a pinned diff as skip/light/full without invoking the LLM.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["diffPath", "headSha"],
                "properties": {
                    "diffPath": diff_path_prop(),
                    "headSha": head_sha_prop(),
                    "uncommitted": uncommitted_prop(),
                },
            },
        }),
        json!({
            "name": REVIEW_PRESCAN,
            "description": "Run eslint + ast-grep prescan on a pinned diff. Does not invoke the LLM.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["diffPath", "headSha"],
                "properties": {
                    "diffPath": diff_path_prop(),
                    "headSha": head_sha_prop(),
                    "uncommitted": uncommitted_prop(),
                    "tokenBudget": token_budget_prop(),
                },
            },
        }),
        json!({
            "name": FP_CHECK,
            "description": "Check whether a finding title+body matches a known false-positive pattern for a file.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["title", "body", "file"],
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "file": { "type": "string" },
                    "extraPaths": { "type": "array", "items": { "type": "string" }, "maxItems": EXTRA_PATHS_MAX },
                },
            },
        }),
        json!({
            "name": FINGERPRINT_CHECK,
            "description": "Check whether the code under a prior finding fingerprint is still present in a pinned diff.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["file", "line", "priorFp", "diffPath"],
                "properties": {
                    "file": { "type": "string", "minLength": 1 },
                    "line": { "type": "integer", "minimum": 1 },
                    "priorFp": { "type": "string", "minLength": 1, "maxLength": 64 },
                    "diffPath": diff_path_prop(),
                    "headSha": head_sha_prop(),
                    "uncommitted": uncommitted_prop(),
                },
            },
        }),
        json!({
            "name": REVIEW_ARTIFACT,
            "description": "Read a bounded preview of a pinned review JSON artifact, or a sidecar by jobId/runFingerprint. Does not invoke the LLM.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "minLength": 1 },
                    "jobId": run_id_prop(),
                    "runFingerprint": run_id_prop(),
                    "headSha": head_sha_prop(),
                    "tokenBudget": token_budget_prop(),
                },
            },
        }),
    ]
}

fn extra_tools() -> Vec<Value> {
    vec![
        json!({
            "name": BLAST_SCOPE,
            "description": "List files outside the diff affected by changed symbols (Graphify). Does not invoke the LLM.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["diffPath", "headSha"],
                "properties": {
                    "diffPath": diff_path_prop(),
                    "headSha": head_sha_prop(),
                    "uncommitted": uncommitted_prop(),
                },
            },
        }),
        json!({
            "name": ACTIONABILITY_EXPLAIN,
            "description": "Explain actionability score and reasons for one finding. Does not mutate the finding or invoke the LLM.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["finding"],
                "properties": {
                    "finding": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["file", "line", "severity", "title", "body"],
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer" },
                            "severity": { "type": "string" },
                            "title": { "type": "string" },
                            "body": { "type": "string" },
                            "verifierStatus": { "type": "string" },
                            "prescan": {},
                            "advisorClaims": { "type": "array" },
                            "refutationStatus": { "type": "string" },
                        },
                    },
                },
            },
        }),
        json!({
            "name": REVIEW_METRICS,
            "description": "Aggregate a confined observability JSONL file. Does not invoke the LLM.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "minLength": 1 },
                },
            },
        }),
    ]
}

/// `extraEnabled`: фаза C (blast_scope, actionability_explain, review_metrics).
pub fn extra_enabled(env: &HashMap<String, String>) -> bool {
    env_flag(env, "REVIEW_MCP_EXTRA", false)
}

/// `listedTools`.
pub fn listed_tools(env: &HashMap<String, String>) -> Vec<Value> {
    let mut tools = default_tools();
    if extra_enabled(env) {
        tools.extend(extra_tools());
    }
    tools
}

fn request_id(request: &Value) -> Value {
    match request.get("id") {
        Some(Value::Null) | None => Value::Null,
        Some(v) => v.clone(),
    }
}

/// `handleReviewMcpHandshake`: `None` — не handshake (идёт в домен), `Some(None)` —
/// notification без ответа. Домен для handshake не нужен (RFC 10).
pub fn handshake(request: &Value, env: &HashMap<String, String>) -> Option<Option<Value>> {
    let id = request_id(request);
    let method = request.get("method").and_then(Value::as_str)?;
    let result = match method {
        "initialize" => {
            let pv = request
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .filter(|v| js_truthy(v))
                .cloned()
                .unwrap_or_else(|| json!("2025-06-18"));
            json!({
                "protocolVersion": pv,
                "capabilities": { "tools": {}, "logging": {} },
                "serverInfo": { "name": "ai-reviewer", "version": "1.0.0" },
            })
        }
        "notifications/initialized" => return Some(None),
        "ping" | "logging/setLevel" => json!({}),
        "tools/list" => json!({ "tools": listed_tools(env) }),
        "resources/list" => json!({ "resources": [] }),
        "prompts/list" => json!({ "prompts": [] }),
        _ => return None,
    };
    Some(Some(json!({ "jsonrpc": "2.0", "id": id, "result": result })))
}

// ---------------------------------------------------------------------------
// Зависимости домена
// ---------------------------------------------------------------------------

/// git-проба пина (`createDefaultGit`).
pub trait GitProbe: Send + Sync {
    fn is_inside_work_tree(&self, root: &Path) -> bool;
    fn rev_parse_head(&self, root: &Path) -> Result<String>;
    fn status_porcelain(&self, root: &Path) -> Result<String>;
}

/// Боевой git: `git -C <root> …` с таймаутом 5 с. Аргументы фиксированы,
/// значения от клиента в argv не попадают.
#[derive(Debug, Default)]
pub struct SystemGit;

fn git_output(root: &Path, args: &[&str], max_buffer: usize) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("git {}: {e}", args.join(" ")))?;
    // stdout читаем в отдельном потоке: большой вывод не должен упереться в буфер пайпа,
    // пока мы ждём завершения с таймаутом.
    let mut stdout = child.stdout.take().expect("stdout piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(max_buffer as u64 + 1).read_to_end(&mut buf);
        buf
    });
    let mut stderr = child.stderr.take().expect("stderr piped");
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {}: timeout", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    if out.len() > max_buffer {
        bail!("git {}: stdout maxBuffer exceeded", args.join(" "));
    }
    if !status.success() {
        bail!("Command failed: git {}\n{}", args.join(" "), String::from_utf8_lossy(&err).trim_end());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

impl GitProbe for SystemGit {
    fn is_inside_work_tree(&self, root: &Path) -> bool {
        git_output(root, &["rev-parse", "--is-inside-work-tree"], GIT_MAX_BUFFER)
            .map(|s| s.trim() == "true")
            .unwrap_or(false)
    }
    fn rev_parse_head(&self, root: &Path) -> Result<String> {
        Ok(git_output(root, &["rev-parse", "HEAD"], 1024)?.trim().to_string())
    }
    fn status_porcelain(&self, root: &Path) -> Result<String> {
        git_output(root, &["status", "--porcelain"], GIT_MAX_BUFFER)
    }
}

/// Вход превью-прогона: всё, из чего JS собирает `loadReviewConfig` + `createReviewPipeline`.
pub struct RunRequest {
    /// env прогона (с `REVIEW_EXPORT_DRY_RUN`, по умолчанию `1`).
    pub env: HashMap<String, String>,
    pub cli: CliArgs,
    pub root: PathBuf,
    /// `deps.headSha`.
    pub head_sha: String,
    /// Prior из `priorExportPath` — его отдаёт немой GitHub-клиент.
    pub prior: StickyData,
    pub log: McpLog,
}

/// То из `runDetailed()`, что читает MCP.
#[derive(Debug, Clone, Default)]
pub struct RunReport {
    pub artifact: Option<Value>,
    pub degraded: bool,
    pub reason: Option<String>,
    pub would_block: bool,
    /// `exportResult.written`.
    pub written: Vec<String>,
}

impl From<PipelineOutcome> for RunReport {
    fn from(o: PipelineOutcome) -> Self {
        RunReport {
            artifact: o.artifact,
            degraded: o.degraded,
            reason: o.reason,
            would_block: o.would_block,
            written: o
                .export_result
                .map(|r| r.written.iter().map(|p| p.to_string_lossy().into_owned()).collect())
                .unwrap_or_default(),
        }
    }
}

/// Pinned dry-run MoA (`createReviewPipeline(cfg, deps).runDetailed()`).
/// `Err` — авария прогона; мягкий отказ — `Ok` с `degraded`.
pub trait ReviewRunner: Send + Sync {
    fn run(&self, req: RunRequest) -> Result<RunReport>;
}

/// Дешёвые стадии без LLM: prescan и blast-radius.
pub trait CheapStages: Send + Sync {
    fn prescan(&self, cfg: &Value, profile: &Profile, root: &Path, log: McpLog, paths: &[String])
        -> Result<PrescanResult>;
    fn blast(&self, cfg: &Value, symbols: &[String]) -> Result<BlastRadius>;
}

#[derive(Clone)]
pub struct McpDeps {
    pub git: Arc<dyn GitProbe>,
    pub runner: Arc<dyn ReviewRunner>,
    pub stages: Arc<dyn CheapStages>,
    /// notifications (`notifications/message`): логи прогона и ошибка фонового прогона.
    pub notify: McpNotify,
}

/// Немой GitHub (`createSilentGithub`): prior из экспорта, запись — no-op.
#[derive(Debug, Default)]
pub struct SilentGithub {
    prior: StickyData,
}

impl SilentGithub {
    pub fn new(prior: StickyData) -> Self {
        Self { prior }
    }
}

impl GithubApi for SilentGithub {
    fn gh(&self, _path: &str, _opts: &GhCallOpts) -> Result<Value> {
        Ok(json!({}))
    }
    fn load_diff(&self) -> Result<Option<LoadedDiff>> {
        Ok(None)
    }
    fn read_prior_sticky_data(&self) -> Result<StickyData> {
        Ok(self.prior.clone())
    }
    fn read_prior_findings(&self) -> Result<Vec<Value>> {
        Ok(self.prior.findings.clone())
    }
    fn upsert_sticky(&self, _body: &str) -> Result<String> {
        Ok("skipped".into())
    }
    fn delete_stale_inline(&self, _opts: &DeleteStaleOpts) -> Result<usize> {
        Ok(0)
    }
    fn post_review(&self, _params: &PostReviewParams) -> Result<PostReviewResult> {
        Ok(PostReviewResult::default())
    }
    fn list_inline_comments(&self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
    fn list_issue_labels(&self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
    fn read_sticky_review(&self) -> Result<Option<Value>> {
        Ok(None)
    }
    fn list_pull_comment_reactions(&self, _comment_id: u64) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
    fn list_unresolved_review_threads(&self) -> Result<Vec<ThreadRef>> {
        Ok(Vec::new())
    }
    fn reply_review_thread(&self, _thread_id: &str, _body: &str) -> Result<()> {
        Ok(())
    }
    fn resolve_review_thread(&self, _thread_id: &str) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// JS-примитивы
// ---------------------------------------------------------------------------

fn truthy(v: Option<&Value>) -> bool {
    v.is_some_and(js_truthy)
}

/// Значение, если truthy (`x || undefined`).
fn truthy_val(v: Option<&Value>) -> Option<Value> {
    v.filter(|v| js_truthy(v)).cloned()
}

/// `String(v)`; отсутствующее — `"undefined"`.
fn js_string(v: Option<&Value>) -> String {
    v.map(js_to_string).unwrap_or_else(|| "undefined".into())
}

/// Число в JSON так, как его печатает `JSON.stringify` (3, а не 3.0).
fn num_value(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        Value::from(n as i64)
    } else if n.is_finite() {
        Value::from(n)
    } else {
        Value::Null
    }
}

/// `boundedInt(value, schema, fallback)`.
fn bounded_int(v: Option<&Value>, min: i64, max: i64, fallback: i64) -> i64 {
    match v.and_then(js_to_number) {
        Some(n) if n.is_finite() && n.fract() == 0.0 => (n.max(min as f64).min(max as f64)) as i64,
        _ => fallback,
    }
}

fn token_budget(args: &Map<String, Value>) -> i64 {
    bounded_int(args.get("tokenBudget"), TOKEN_BUDGET_MIN, TOKEN_BUDGET_MAX, DEFAULT_TOKEN_BUDGET)
}

fn json_text(v: &Value) -> String {
    let mut s = String::new();
    js_json_stringify(v, &mut s);
    s
}

/// `Math.ceil(JSON.stringify(value).length / 4)`.
fn estimate_tokens(v: &Value) -> i64 {
    utf16_len(&json_text(v)).div_ceil(4) as i64
}

fn is_head_sha(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_run_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `HEAD_SHA_RE.test(v)` (с приведением к строке).
fn head_sha_ok(v: Option<&Value>) -> bool {
    v.is_some_and(|v| is_head_sha(&js_to_string(v)))
}

fn run_id_ok(v: Option<&Value>) -> bool {
    truthy(v) && v.is_some_and(|v| is_run_id(&js_to_string(v)))
}

/// `headShaPrefixMatch`.
pub fn head_sha_prefix_match(a: &str, b: &str) -> bool {
    let x = a.to_lowercase();
    let y = b.to_lowercase();
    if x.is_empty() || y.is_empty() {
        return false;
    }
    let (shorter, longer) = if x.len() <= y.len() { (&x, &y) } else { (&y, &x) };
    shorter.len() >= 7 && longer.starts_with(shorter.as_str())
}

/// `headShaPrefixMatch(a, b)` над значениями (`String(a || '')`).
fn prefix_match_v(a: Option<&Value>, b: Option<&Value>) -> bool {
    let s = |v: Option<&Value>| if truthy(v) { js_string(v) } else { String::new() };
    head_sha_prefix_match(&s(a), &s(b))
}

/// `computeReviewJobId`.
pub fn compute_review_job_id(head_sha: &Value, uncommitted: bool, diff_path: &Value, diff_hash: &str) -> String {
    sha256_str(&canonical_json(&json!({
        "diffHash": diff_hash,
        "diffPath": diff_path,
        "headSha": head_sha,
        "uncommitted": uncommitted,
    })))
}

fn now_iso() -> String {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    iso_from_epoch_ms(ms)
}

fn parse_json(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|e| anyhow!("invalid JSON: {e}"))
}

// ---------------------------------------------------------------------------
// Находки превью
// ---------------------------------------------------------------------------

/// `slimFinding`.
fn slim_finding(f: &Value) -> Value {
    let mut m = Map::new();
    m.insert("file".into(), truthy_val(f.get("file")).unwrap_or_else(|| json!("")));
    let line = f.get("line").and_then(js_to_number).filter(|n| *n != 0.0).unwrap_or(0.0);
    m.insert("line".into(), num_value(line));
    m.insert("severity".into(), truthy_val(f.get("severity")).unwrap_or_else(|| json!("")));
    let title = if truthy(f.get("title")) { js_string(f.get("title")) } else { String::new() };
    m.insert("title".into(), Value::String(js_slice_utf16(&title, 200)));
    for key in ["fp", "verifierStatus"] {
        if let Some(v) = truthy_val(f.get(key)) {
            m.insert(key.into(), v);
        }
    }
    if let Some(score) = f.get("actionabilityScore").filter(|v| v.as_f64().is_some_and(f64::is_finite)) {
        m.insert("actionabilityScore".into(), score.clone());
    }
    for key in ["prescan", "consensusBlock"] {
        if let Some(v) = truthy_val(f.get(key)) {
            m.insert(key.into(), v);
        }
    }
    Value::Object(m)
}

fn sev_rank(f: &Value) -> u8 {
    match f.get("severity").and_then(Value::as_str) {
        Some("critical") => 0,
        Some("major") => 1,
        Some("minor") => 2,
        Some("nit") => 3,
        _ => 9,
    }
}

/// `sortFindings`: severity, затем actionabilityScore по убыванию, затем fp.
fn sort_findings(list: &mut [Value]) {
    let score = |f: &Value| f.get("actionabilityScore").and_then(js_to_number).filter(|n| *n != 0.0).unwrap_or(0.0);
    let fp = |f: &Value| if truthy(f.get("fp")) { js_string(f.get("fp")) } else { String::new() };
    list.sort_by(|a, b| {
        sev_rank(a)
            .cmp(&sev_rank(b))
            .then_with(|| score(b).partial_cmp(&score(a)).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| fp(a).cmp(&fp(b)))
    });
}

fn slim_sorted(list: Option<&Value>) -> Vec<Value> {
    let mut out: Vec<Value> = list.and_then(Value::as_array).map(|a| a.iter().map(slim_finding).collect()).unwrap_or_default();
    sort_findings(&mut out);
    out
}

fn finding_key(f: &Value) -> String {
    let fp = if truthy(f.get("fp")) { js_string(f.get("fp")) } else { String::new() };
    format!("{}|{}|{}", js_string(f.get("file")), js_string(f.get("line")), fp)
}

/// Что `fitPreview` читает из результата прогона.
#[derive(Debug, Default)]
struct PreviewSource<'a> {
    artifact: Option<&'a Value>,
    degraded: bool,
    reason: Option<Value>,
    would_block: Option<Value>,
    written: Vec<String>,
}

/// `fitPreview`: превью в рамках `tokenBudget`, хвост — через `review_findings`.
///
/// `started_at` — хвостовое поле `startedAt`. JS дописывает его после подгонки, и
/// превью может выйти за бюджет; здесь оно учитывается в бюджете (место в объекте
/// то же — последним ключом).
fn fit_preview(
    src: &PreviewSource<'_>,
    head_sha: Value,
    token_budget: i64,
    kind: &str,
    job_id: &str,
    started_at: Option<&str>,
) -> Map<String, Value> {
    let artifact = src.artifact.unwrap_or(&Value::Null);
    let blockers = slim_sorted(artifact.get("blockers"));
    let findings = slim_sorted(artifact.get("findings"));

    let mut base = Map::new();
    base.insert("kind".into(), json!(kind));
    base.insert("protocolVersion".into(), json!(REVIEW_MCP_PROTOCOL_VERSION));
    base.insert("status".into(), json!(if src.degraded { "degraded" } else { "completed" }));
    if let Some(reason) = src.reason.as_ref().filter(|v| js_truthy(v)) {
        base.insert("reason".into(), reason.clone());
    }
    base.insert("headSha".into(), head_sha.clone());
    base.insert("jobId".into(), json!(job_id));
    base.insert("runFingerprint".into(), json!(""));
    let artifact_path = src.written.iter().find(|p| p.ends_with(".json") && !p.ends_with("-swarm.json"));
    base.insert("artifactPath".into(), json!(artifact_path.cloned().unwrap_or_default()));
    let would_block = artifact.get("wouldBlock").filter(|v| !v.is_null()).or(src.would_block.as_ref());
    base.insert("previewWouldBlock".into(), json!(truthy(would_block)));
    let strong = artifact.get("swarm").and_then(|s| s.get("strong"));
    base.insert("strong".into(), json!(truthy(strong)));
    if let Some(route) = truthy_val(artifact.get("router").and_then(|r| r.get("route"))) {
        base.insert("route".into(), route);
    }
    base.insert("counts".into(), truthy_val(artifact.get("counts")).unwrap_or_else(|| json!({})));
    base.insert("blockers".into(), json!([]));
    base.insert("findingsPreview".into(), json!([]));
    base.insert("truncated".into(), json!(false));
    if let Some(s) = started_at {
        base.insert("startedAt".into(), json!(s));
    }

    let fingerprint_source = match src.artifact.filter(|v| js_truthy(v)) {
        Some(a) => a.clone(),
        None => {
            let reason = src.reason.clone().filter(js_truthy).unwrap_or_else(|| json!("degraded"));
            json!({ "headSha": head_sha, "reason": reason })
        }
    };
    base.insert("runFingerprint".into(), json!(sha256_str(&canonical_json(&fingerprint_source))));

    let fill = |base: &mut Map<String, Value>, key: &str, items: &[Value]| {
        let mut out: Vec<Value> = Vec::new();
        for item in items {
            let mut trial = base.clone();
            let mut next = out.clone();
            next.push(item.clone());
            trial.insert(key.into(), Value::Array(next));
            trial.insert("truncated".into(), json!(true));
            if estimate_tokens(&Value::Object(trial)) > token_budget {
                base.insert("truncated".into(), json!(true));
                break;
            }
            out.push(item.clone());
        }
        base.insert(key.into(), Value::Array(out));
    };

    fill(&mut base, "blockers", &blockers);
    let blocker_keys: std::collections::HashSet<String> =
        base["blockers"].as_array().map(|a| a.iter().map(finding_key).collect()).unwrap_or_default();
    let rest: Vec<Value> = findings.into_iter().filter(|f| !blocker_keys.contains(&finding_key(f))).collect();
    fill(&mut base, "findingsPreview", &rest);
    let len = |base: &Map<String, Value>, k: &str| base[k].as_array().map_or(0, Vec::len);
    if len(&base, "blockers") < blockers.len() || len(&base, "findingsPreview") < rest.len() {
        base.insert("truncated".into(), json!(true));
    }
    if estimate_tokens(&Value::Object(base.clone())) > token_budget {
        base.insert("findingsPreview".into(), json!([]));
        base.insert("truncated".into(), json!(true));
        loop {
            let empty = base["blockers"].as_array().is_none_or(Vec::is_empty);
            if empty || estimate_tokens(&Value::Object(base.clone())) <= token_budget {
                break;
            }
            if let Some(Value::Array(a)) = base.get_mut("blockers") {
                a.pop();
            }
        }
    }
    base
}

fn running_stub(job_id: &str, head_sha: &Value, started_at: Option<&Value>) -> Value {
    let mut m = Map::new();
    m.insert("kind".into(), json!("review_preview"));
    m.insert("protocolVersion".into(), json!(REVIEW_MCP_PROTOCOL_VERSION));
    m.insert("status".into(), json!("running"));
    m.insert("jobId".into(), json!(job_id));
    m.insert("runFingerprint".into(), json!(""));
    m.insert("headSha".into(), head_sha.clone());
    m.insert("previewWouldBlock".into(), json!(false));
    m.insert("blockers".into(), json!([]));
    m.insert("findingsPreview".into(), json!([]));
    if let Some(s) = started_at {
        m.insert("startedAt".into(), s.clone());
    }
    Value::Object(m)
}

// ---------------------------------------------------------------------------
// Хранилище прогонов
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Entry {
    job_id: String,
    run_fingerprint: String,
    head_sha: Option<Value>,
    status: String,
    started_at: Option<Value>,
    artifact_path: String,
    artifact: Value,
    preview: Option<Value>,
}

impl Entry {
    /// `entryStatus`.
    fn status(&self) -> String {
        if !self.status.is_empty() {
            return self.status.clone();
        }
        self.preview.as_ref().and_then(|p| p.get("status")).filter(|v| js_truthy(v)).map(js_to_string).unwrap_or_default()
    }

    fn has_artifact(&self) -> bool {
        match &self.artifact {
            Value::Object(m) => !m.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::String(s) => !s.is_empty(),
            _ => false,
        }
    }

    fn head_sha_v(&self) -> Value {
        self.head_sha.clone().unwrap_or(Value::Null)
    }
}

/// `hydrateSidecar`.
fn hydrate_sidecar(payload: &Value) -> Option<Entry> {
    let obj = payload.as_object()?;
    let s = |k: &str| obj.get(k).filter(|v| js_truthy(v)).map(js_to_string).unwrap_or_default();
    let preview = obj.get("preview").filter(|v| js_truthy(v)).cloned();
    let pv = |k: &str| preview.as_ref().and_then(|p| p.get(k)).filter(|v| js_truthy(v)).cloned();
    let status = {
        let st = s("status");
        if st.is_empty() { pv("status").map(|v| js_to_string(&v)).unwrap_or_default() } else { st }
    };
    Some(Entry {
        job_id: s("jobId"),
        run_fingerprint: s("runFingerprint"),
        head_sha: obj.get("headSha").cloned(),
        status,
        started_at: obj.get("startedAt").filter(|v| js_truthy(v)).cloned().or_else(|| pv("startedAt")),
        artifact_path: s("artifactPath"),
        artifact: obj.get("artifact").filter(|v| js_truthy(v)).cloned().unwrap_or_else(|| json!({})),
        preview,
    })
}

#[derive(Debug, Default)]
struct State {
    store: HashMap<String, Entry>,
    order: Vec<String>,
    running: bool,
    current_job: String,
}

impl State {
    fn forget_job(&mut self, key: &str) {
        let entry = self.store.remove(key);
        if let Some(e) = entry {
            if !e.job_id.is_empty() {
                self.store.remove(&e.job_id);
            }
            if !e.run_fingerprint.is_empty() {
                self.store.remove(&e.run_fingerprint);
            }
        }
    }

    /// `rememberRun`.
    fn remember(&mut self, entry: Entry) {
        let key = if entry.job_id.is_empty() { entry.run_fingerprint.clone() } else { entry.job_id.clone() };
        if key.is_empty() {
            return;
        }
        if !entry.job_id.is_empty() {
            if let Some(prev) = self.store.get(&entry.job_id) {
                if !prev.run_fingerprint.is_empty() && prev.run_fingerprint != entry.run_fingerprint {
                    let stale = prev.run_fingerprint.clone();
                    self.store.remove(&stale);
                }
            }
            self.store.insert(entry.job_id.clone(), entry.clone());
        }
        if !entry.run_fingerprint.is_empty() {
            self.store.insert(entry.run_fingerprint.clone(), entry);
        }
        self.order.retain(|k| *k != key);
        self.order.push(key);
        while self.order.len() > STORE_CAP {
            let oldest = self.order.remove(0);
            self.forget_job(&oldest);
        }
    }
}

// ---------------------------------------------------------------------------
// Сервер
// ---------------------------------------------------------------------------

struct Inner {
    root: PathBuf,
    env: HashMap<String, String>,
    deps: McpDeps,
    state: Mutex<State>,
    background: Mutex<Vec<JoinHandle<()>>>,
}

/// Домен ревью-MCP: один на процесс, как модульное состояние `lib/review-mcp.mjs`.
#[derive(Clone)]
pub struct ReviewMcp {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ReviewMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // env не печатаем: в нём ключ LLM и токен GitHub.
        f.debug_struct("ReviewMcp").field("root", &self.inner.root).finish_non_exhaustive()
    }
}

/// Паника вызова — ошибка вызова, а не смерть сервера.
fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    let msg = p
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    format!("internal error: panic: {msg}")
}

impl ReviewMcp {
    /// `root` — абсолютный корень MCP (пути клиента не выходят за него), `env` — окружение
    /// процесса (конфиг прогона и `REVIEW_MCP_EXTRA`).
    pub fn new(root: PathBuf, env: HashMap<String, String>, deps: McpDeps) -> Self {
        ReviewMcp {
            inner: Arc::new(Inner {
                root,
                env,
                deps,
                state: Mutex::new(State::default()),
                background: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// `resetReviewMcpStore`: память процесса забыта, sidecar-ы на диске остаются.
    pub fn reset_store(&self) {
        *self.inner.lock_state() = State::default();
    }

    /// Дождаться фоновых прогонов (`wait: false`). Сервер зовёт перед выходом по EOF,
    /// тесты — вместо `captureRun`.
    pub fn wait_background(&self) {
        let handles: Vec<JoinHandle<()>> =
            std::mem::take(&mut *self.inner.background.lock().unwrap_or_else(|e| e.into_inner()));
        for h in handles {
            let _ = h.join();
        }
    }

    /// `handleReviewMcpRequest`: ответ JSON-RPC или `None` для notification.
    pub fn handle(&self, request: &Value) -> Option<Value> {
        let id = request_id(request);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| self.dispatch(request)))
            .unwrap_or_else(|p| Err(anyhow!(panic_message(p))));
        match outcome {
            Ok(response) => response,
            Err(error) => {
                let message = error.to_string();
                let code = if message.starts_with("method not found") { -32601 } else { -32602 };
                Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
            }
        }
    }

    fn dispatch(&self, request: &Value) -> Result<Option<Value>> {
        if let Some(response) = handshake(request, &self.inner.env) {
            return Ok(response);
        }
        let id = request_id(request);
        if request.get("method").and_then(Value::as_str) != Some("tools/call") {
            bail!("method not found: {}", js_string(request.get("method")));
        }
        let params = request.get("params");
        let name_v = params.and_then(|p| p.get("name"));
        let name = name_v.and_then(Value::as_str).unwrap_or("");
        let args: Map<String, Value> = params
            .and_then(|p| p.get("arguments"))
            .filter(|v| js_truthy(v))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let inner = &self.inner;
        let payload = match name {
            REVIEW_RUN => self.call_review_run(&args)?,
            REVIEW_FINDINGS => inner.call_review_findings(&args)?,
            REVIEW_ROUTE => inner.call_review_route(&args)?,
            REVIEW_PRESCAN => inner.call_review_prescan(&args)?,
            FP_CHECK => call_fp_check(&args)?,
            FINGERPRINT_CHECK => inner.call_fingerprint_check(&args)?,
            REVIEW_ARTIFACT => inner.call_review_artifact(&args)?,
            BLAST_SCOPE | ACTIONABILITY_EXPLAIN | REVIEW_METRICS if extra_enabled(&inner.env) => match name {
                BLAST_SCOPE => inner.call_blast_scope(&args)?,
                ACTIONABILITY_EXPLAIN => inner.call_actionability_explain(&args)?,
                _ => inner.call_review_metrics(&args)?,
            },
            _ => bail!("unknown tool: {}", js_string(name_v)),
        };
        Ok(Some(rpc_result(id, payload)))
    }

    /// `callReviewRun`.
    fn call_review_run(&self, args: &Map<String, Value>) -> Result<Value> {
        let inner = &self.inner;
        if !truthy(args.get("diffPath")) {
            bail!("diffPath is required");
        }
        require_head_sha(args)?;
        inner.assert_live_diff_pin(args)?;
        let diff = inner.read_diff_text(args)?;
        let head_sha = args["headSha"].clone();
        let job_id = compute_review_job_id(
            &head_sha,
            truthy(args.get("uncommitted")),
            &args["diffPath"],
            &sha256_str(&diff.text),
        );
        let wait = args.get("wait") != Some(&Value::Bool(false));
        let (started_at, stub) = match inner.claim_review_run(args, &job_id)? {
            Claim::Answer(result) => return Ok(result),
            Claim::Start { started_at, stub } => (started_at, stub),
        };
        if wait {
            return inner.execute_review_run(args, &job_id, &started_at).map(Value::Object);
        }
        let me = Arc::clone(&self.inner);
        let args = args.clone();
        let handle = std::thread::spawn(move || {
            if let Err(error) = me.execute_review_run(&args, &job_id, &started_at) {
                (me.deps.notify)("notifications/message", json!({ "level": "error", "data": error.to_string() }));
            }
        });
        inner.background.lock().unwrap_or_else(|e| e.into_inner()).push(handle);
        Ok(stub)
    }
}

enum Claim {
    /// Ответить готовым: кэш, stub бегущего того же пина.
    Answer(Value),
    /// Слот занят этим вызовом.
    Start { started_at: String, stub: Value },
}

fn require_head_sha(args: &Map<String, Value>) -> Result<()> {
    if !truthy(args.get("headSha")) || !head_sha_ok(args.get("headSha")) {
        bail!("headSha is required");
    }
    Ok(())
}

fn rpc_result(id: Value, payload: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": crate::cli::structure_mcp::js_json_pretty(&payload) }],
            "structuredContent": payload,
        },
    })
}

struct DiffText {
    absolute: PathBuf,
    text: String,
    files: Map<String, Value>,
    changed_text: String,
}

fn line_text(rec: &Value) -> LineText {
    rec.get("text")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.parse::<u64>().ok()?, v.as_str()?.to_string()))).collect())
        .unwrap_or_default()
}

fn prior_files(files: &Map<String, Value>) -> IndexMap<String, PriorFileRec> {
    files
        .iter()
        .map(|(p, rec)| {
            let valid_lines = rec
                .get("validLines")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default();
            (p.clone(), PriorFileRec { valid_lines, text: Some(line_text(rec)) })
        })
        .collect()
}

fn base_payload(kind: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), json!(kind));
    m.insert("protocolVersion".into(), json!(REVIEW_MCP_PROTOCOL_VERSION));
    m
}

/// `callFpCheck`.
fn call_fp_check(args: &Map<String, Value>) -> Result<Value> {
    let file = if truthy(args.get("file")) { js_trim(&js_string(args.get("file"))).to_string() } else { String::new() };
    if file.is_empty() {
        bail!("file is required");
    }
    let text_of = |k: &str| match args.get(k) {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_to_string(v),
    };
    let title = text_of("title");
    let body = text_of("body");
    if utf16_len(&title) + utf16_len(&body) > TEXT_MAX {
        bail!("text too long");
    }
    let mut paths = vec![file];
    if let Some(Value::Array(extra)) = args.get("extraPaths") {
        paths.extend(extra.iter().take(EXTRA_PATHS_MAX).map(js_to_string));
    }
    let text = format!("{title} {body}");
    let pattern_id = match_fp_patterns(&text, &paths, default_fp_patterns());
    let mut out = base_payload("fp_check");
    out.insert("match".into(), json!(pattern_id.is_some()));
    out.insert("patternId".into(), pattern_id.map_or(Value::Null, |id| json!(id)));
    Ok(Value::Object(out))
}

fn unwrap_artifact_payload(payload: &Value) -> &Value {
    if let Some(a) = payload.get("artifact") {
        if a.is_object() && (truthy(payload.get("runFingerprint")) || truthy(payload.get("protocolVersion"))) {
            return a;
        }
    }
    payload
}

impl Inner {
    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log(&self) -> McpLog {
        let notify = Arc::clone(&self.deps.notify);
        Arc::new(move |msg: &str| notify("notifications/message", json!({ "level": "info", "data": msg })))
    }

    fn contained(&self, target: &str, must_exist: bool) -> Result<PathBuf> {
        resolve_contained_path(target, Some(&self.root), must_exist)
    }

    /// `assertLiveDiffPin`.
    fn assert_live_diff_pin(&self, args: &Map<String, Value>) -> Result<()> {
        if !truthy(args.get("diffPath")) {
            bail!("diffPath is required");
        }
        require_head_sha(args)?;
        if args.get("uncommitted") == Some(&Value::Bool(true)) {
            return Ok(());
        }
        let git = &self.deps.git;
        if !git.is_inside_work_tree(&self.root) {
            return Ok(());
        }
        let head = git.rev_parse_head(&self.root)?;
        if !head_sha_prefix_match(&js_string(args.get("headSha")), &head) {
            bail!("headSha does not match HEAD");
        }
        if !git.status_porcelain(&self.root)?.trim().is_empty() {
            bail!("working tree dirty; pass uncommitted: true");
        }
        Ok(())
    }

    /// `readDiffText`: путь внутри корня, текст как `readFile(…, 'utf8')`.
    fn read_diff_text(&self, args: &Map<String, Value>) -> Result<DiffText> {
        let absolute = self.contained(&js_string(args.get("diffPath")), true)?;
        let text = String::from_utf8_lossy(&std::fs::read(&absolute)?).into_owned();
        let parsed = parse_diff(&text);
        Ok(DiffText { absolute, text, files: parsed.files, changed_text: parsed.changed_text })
    }

    fn sidecar_dir(&self) -> PathBuf {
        self.root.join(SIDECAR_DIR)
    }

    /// `persistRun`.
    fn persist_run(&self, entry: &Entry) -> Result<()> {
        let id = if entry.job_id.is_empty() { &entry.run_fingerprint } else { &entry.job_id };
        if id.is_empty() {
            bail!("jobId is required to persist sidecar");
        }
        let dir = self.sidecar_dir();
        self.contained(&dir.to_string_lossy(), false)?;
        std::fs::create_dir_all(&dir)?;
        // create_dir_all мог пройти по symlink-у, созданному после проверки: сверяем ещё раз.
        self.contained(&dir.to_string_lossy(), true)?;
        let mut body = Map::new();
        body.insert("protocolVersion".into(), json!(REVIEW_MCP_PROTOCOL_VERSION));
        body.insert("jobId".into(), json!(entry.job_id));
        body.insert("runFingerprint".into(), json!(entry.run_fingerprint));
        if let Some(h) = &entry.head_sha {
            body.insert("headSha".into(), h.clone());
        }
        body.insert("status".into(), json!(entry.status()));
        body.insert("artifactPath".into(), json!(entry.artifact_path));
        body.insert("savedAt".into(), json!(now_iso()));
        let started = entry
            .started_at
            .clone()
            .filter(js_truthy)
            .or_else(|| entry.preview.as_ref().and_then(|p| p.get("startedAt")).cloned());
        if let Some(s) = started {
            body.insert("startedAt".into(), s);
        }
        if let Some(p) = entry.preview.as_ref().filter(|v| js_truthy(v)) {
            body.insert("preview".into(), p.clone());
        }
        // `entry.artifact || {}`: у мягкого отказа артефакта нет, а читатель sidecar ждёт объект.
        let artifact = if js_truthy(&entry.artifact) { entry.artifact.clone() } else { json!({}) };
        body.insert("artifact".into(), artifact);
        let mut text = json_text(&Value::Object(body));
        text.push('\n');
        std::fs::write(dir.join(format!("{id}.json")), text)?;
        self.evict_sidecars();
        Ok(())
    }

    /// `evictSidecars`: на диске не больше DISK_CAP sidecar-ов, старые по mtime уходят.
    fn evict_sidecars(&self) {
        let Ok(rd) = std::fs::read_dir(self.sidecar_dir()) else { return };
        let mut ranked: Vec<(SystemTime, PathBuf)> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .map(|e| {
                let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
                (mtime, e.path())
            })
            .collect();
        if ranked.len() <= DISK_CAP {
            return;
        }
        ranked.sort_by_key(|r| r.0);
        let excess = ranked.len() - DISK_CAP;
        for (_, path) in ranked.into_iter().take(excess) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// `readSidecarFile`.
    fn read_sidecar_file(&self, path: &Path, id: &str) -> Option<Entry> {
        let absolute = self.contained(&path.to_string_lossy(), true).ok()?;
        let text = std::fs::read(absolute).ok()?;
        let payload: Value = serde_json::from_slice(&text).ok()?;
        let entry = hydrate_sidecar(&payload)?;
        if !id.is_empty() && entry.job_id != id && entry.run_fingerprint != id {
            return None;
        }
        Some(entry)
    }

    /// `loadSidecar`: сначала `<id>.json`, затем перебор каталога (sidecar по jobId
    /// ищется и по runFingerprint).
    fn load_sidecar(&self, id: &str) -> Option<Entry> {
        let dir = self.sidecar_dir();
        if let Some(e) = self.read_sidecar_file(&dir.join(format!("{id}.json")), id) {
            return Some(e);
        }
        let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .map(|e| e.path())
            .collect();
        names.sort();
        names.iter().find_map(|p| self.read_sidecar_file(p, id))
    }

    fn lookup_entry(&self, state: &State, id: &str) -> Option<Entry> {
        state.store.get(id).cloned().or_else(|| self.load_sidecar(id))
    }

    /// `claimReviewRun` — решение и захват слота под одним мьютексом.
    fn claim_review_run(&self, args: &Map<String, Value>, job_id: &str) -> Result<Claim> {
        let head_sha = args["headSha"].clone();
        let mut state = self.lock_state();
        let existing = self.lookup_entry(&state, job_id);
        if let Some(existing) = &existing {
            let status = existing.status();
            if status == "completed" || status == "degraded" {
                if existing.has_artifact() {
                    let src = PreviewSource {
                        artifact: Some(&existing.artifact),
                        degraded: status == "degraded",
                        reason: existing.preview.as_ref().and_then(|p| p.get("reason")).cloned(),
                        would_block: existing.artifact.get("wouldBlock").cloned(),
                        written: if existing.artifact_path.is_empty() { vec![] } else { vec![existing.artifact_path.clone()] },
                    };
                    let preview = fit_preview(&src, head_sha, token_budget(args), "review_preview", job_id, None);
                    return Ok(Claim::Answer(Value::Object(preview)));
                }
                if let Some(p) = &existing.preview {
                    return Ok(Claim::Answer(p.clone()));
                }
            }
            if status == "running" {
                let stub = existing
                    .preview
                    .clone()
                    .unwrap_or_else(|| running_stub(job_id, &head_sha, existing.started_at.as_ref()));
                return Ok(Claim::Answer(stub));
            }
        }
        if state.running {
            if state.current_job == job_id {
                let live = state.store.get(job_id);
                let stub = live.and_then(|e| e.preview.clone()).unwrap_or_else(|| {
                    running_stub(job_id, &head_sha, live.and_then(|e| e.started_at.as_ref()))
                });
                return Ok(Claim::Answer(stub));
            }
            bail!("review already running");
        }
        let started_at = now_iso();
        let stub = running_stub(job_id, &head_sha, Some(&json!(started_at)));
        let entry = Entry {
            job_id: job_id.to_string(),
            run_fingerprint: String::new(),
            head_sha: Some(head_sha),
            status: "running".into(),
            started_at: Some(json!(started_at)),
            artifact_path: String::new(),
            artifact: json!({}),
            preview: Some(stub.clone()),
        };
        state.running = true;
        state.current_job = job_id.to_string();
        state.remember(entry.clone());
        if let Err(error) = self.persist_run(&entry) {
            state.running = false;
            state.current_job.clear();
            return Err(error);
        }
        Ok(Claim::Start { started_at, stub })
    }

    /// `executeReviewRun`: прогон, превью, sidecar; на любой ошибке — degraded sidecar.
    /// Слот освобождается всегда (включая панику прогона).
    fn execute_review_run(&self, args: &Map<String, Value>, job_id: &str, started_at: &str) -> Result<Map<String, Value>> {
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| self.execute_inner(args, job_id, started_at)))
            .unwrap_or_else(|p| Err(anyhow!(panic_message(p))));
        let result = match outcome {
            Ok(preview) => Ok(preview),
            Err(error) => {
                let head_sha = args["headSha"].clone();
                if let Err(e) = self.finish_degraded(job_id, head_sha, started_at, &error.to_string(), args) {
                    (self.log())(&format!("review_run: degraded sidecar не записан: {e}"));
                }
                Err(error)
            }
        };
        let mut state = self.lock_state();
        if state.current_job == job_id {
            state.running = false;
            state.current_job.clear();
        }
        result
    }

    fn execute_inner(&self, args: &Map<String, Value>, job_id: &str, started_at: &str) -> Result<Map<String, Value>> {
        let budget = token_budget(args);
        let diff_path = self.contained(&js_string(args.get("diffPath")), true)?;
        let mut prior = StickyData::default();
        if truthy(args.get("priorExportPath")) {
            prior = self.load_prior_export(&js_string(args.get("priorExportPath")))?;
        }
        let head_sha_v = args["headSha"].clone();
        let head_sha = js_to_string(&head_sha_v);
        let mut env = self.env.clone();
        env.entry("REVIEW_EXPORT_DRY_RUN".into()).or_insert_with(|| "1".into());
        let cli = CliArgs {
            dry_run: true,
            diff_file: Some(diff_path.to_string_lossy().into_owned()),
            head_sha: head_sha.clone(),
            repo: String::new(),
            pr_number: 0,
        };
        let report = self.deps.runner.run(RunRequest {
            env,
            cli,
            root: self.root.clone(),
            head_sha: head_sha.clone(),
            prior,
            log: self.log(),
        })?;
        if let Some(art_sha) = report.artifact.as_ref().and_then(|a| a.get("headSha")).filter(|v| js_truthy(v)) {
            if !prefix_match_v(Some(&head_sha_v), Some(art_sha)) {
                bail!("pinned headSha {head_sha} does not match artifact {}", js_to_string(art_sha));
            }
        }
        let src = PreviewSource {
            artifact: report.artifact.as_ref(),
            degraded: report.degraded,
            reason: report.reason.clone().map(Value::String),
            would_block: Some(json!(report.would_block)),
            written: report.written.clone(),
        };
        let preview = fit_preview(&src, head_sha_v.clone(), budget, "review_preview", job_id, Some(started_at));
        let entry = Entry {
            job_id: job_id.to_string(),
            run_fingerprint: js_to_string(&preview["runFingerprint"]),
            head_sha: Some(head_sha_v),
            status: js_to_string(&preview["status"]),
            started_at: Some(json!(started_at)),
            artifact_path: js_to_string(&preview["artifactPath"]),
            artifact: report.artifact.clone().unwrap_or(Value::Null),
            preview: Some(Value::Object(preview.clone())),
        };
        self.lock_state().remember(entry.clone());
        self.persist_run(&entry)?;
        Ok(preview)
    }

    /// `finishDegraded`.
    fn finish_degraded(&self, job_id: &str, head_sha: Value, started_at: &str, reason: &str, args: &Map<String, Value>) -> Result<()> {
        let fingerprint = sha256_str(&canonical_json(&json!({ "headSha": head_sha, "reason": reason })));
        let mut preview = Map::new();
        preview.insert("kind".into(), json!("review_preview"));
        preview.insert("protocolVersion".into(), json!(REVIEW_MCP_PROTOCOL_VERSION));
        preview.insert("status".into(), json!("degraded"));
        preview.insert("reason".into(), json!(reason));
        preview.insert("headSha".into(), head_sha.clone());
        preview.insert("jobId".into(), json!(job_id));
        preview.insert("runFingerprint".into(), json!(fingerprint));
        preview.insert("artifactPath".into(), json!(""));
        preview.insert("previewWouldBlock".into(), json!(false));
        preview.insert("blockers".into(), json!([]));
        preview.insert("findingsPreview".into(), json!([]));
        preview.insert("startedAt".into(), json!(started_at));
        preview.insert("truncated".into(), json!(false));
        if estimate_tokens(&Value::Object(preview.clone())) > token_budget(args) {
            preview.insert("truncated".into(), json!(true));
        }
        let entry = Entry {
            job_id: job_id.to_string(),
            run_fingerprint: fingerprint,
            head_sha: Some(head_sha),
            status: "degraded".into(),
            started_at: Some(json!(started_at)),
            artifact_path: String::new(),
            artifact: json!({}),
            preview: Some(Value::Object(preview)),
        };
        self.lock_state().remember(entry.clone());
        self.persist_run(&entry)
    }

    /// `loadPriorExport`.
    fn load_prior_export(&self, path: &str) -> Result<StickyData> {
        let absolute = self.contained(path, true)?;
        let payload = parse_json(&String::from_utf8_lossy(&std::fs::read(absolute)?))?;
        let arr = |v: Option<&Value>| v.and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(StickyData {
            findings: arr(payload.get("findings")),
            runs: arr(payload.get("verdict").and_then(|v| v.get("runs"))),
            sha: payload.get("headSha").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }

    fn lookup_by_run_id(&self, args: &Map<String, Value>) -> Result<(String, Entry)> {
        let job_ok = run_id_ok(args.get("jobId"));
        let fp_ok = run_id_ok(args.get("runFingerprint"));
        if job_ok && fp_ok {
            bail!("provide jobId or runFingerprint, not both");
        }
        if !job_ok && !fp_ok {
            bail!("jobId or runFingerprint is required");
        }
        let id = js_string(args.get(if job_ok { "jobId" } else { "runFingerprint" }));
        let mut state = self.lock_state();
        let entry = self.lookup_entry(&state, &id).ok_or_else(|| anyhow!("unknown runFingerprint"))?;
        if !entry.job_id.is_empty() || !entry.run_fingerprint.is_empty() {
            state.remember(entry.clone());
        }
        Ok((id, entry))
    }

    fn check_optional_head_sha(args: &Map<String, Value>) -> Result<()> {
        if truthy(args.get("headSha")) && !head_sha_ok(args.get("headSha")) {
            bail!("invalid headSha: {}", js_string(args.get("headSha")));
        }
        Ok(())
    }

    /// `callReviewFindings`.
    fn call_review_findings(&self, args: &Map<String, Value>) -> Result<Value> {
        // Порядок проверок как в JS: id, затем формат headSha, затем поиск.
        let job_ok = run_id_ok(args.get("jobId"));
        let fp_ok = run_id_ok(args.get("runFingerprint"));
        if job_ok && fp_ok {
            bail!("provide jobId or runFingerprint, not both");
        }
        if !job_ok && !fp_ok {
            bail!("jobId or runFingerprint is required");
        }
        Self::check_optional_head_sha(args)?;
        let (_, entry) = self.lookup_by_run_id(args)?;
        if truthy(args.get("headSha")) && !prefix_match_v(args.get("headSha"), entry.head_sha.as_ref()) {
            bail!(
                "pinned headSha {} does not match run {}",
                js_string(args.get("headSha")),
                js_string(entry.head_sha.as_ref())
            );
        }
        let offset = bounded_int(args.get("offset"), 0, 10000, 0);
        let limit = bounded_int(args.get("limit"), 1, 100, 20);
        let status = {
            let s = entry.status();
            if s.is_empty() { "completed".to_string() } else { s }
        };
        let mut out = base_payload("review_findings");
        out.insert("status".into(), json!(status));
        out.insert("jobId".into(), json!(entry.job_id));
        if status == "running" {
            out.insert("runFingerprint".into(), json!(""));
            if let Some(h) = &entry.head_sha {
                out.insert("headSha".into(), h.clone());
            }
            out.insert("offset".into(), json!(offset));
            out.insert("limit".into(), json!(limit));
            out.insert("total".into(), json!(0));
            out.insert("findings".into(), json!([]));
            return Ok(Value::Object(out));
        }
        let mut findings = slim_sorted(entry.artifact.get("findings"));
        if let Some(file) = truthy_val(args.get("file")) {
            findings.retain(|f| f.get("file") == Some(&file));
        }
        if let Some(sev) = truthy_val(args.get("severity")) {
            findings.retain(|f| f.get("severity") == Some(&sev));
        }
        let total = findings.len();
        let slice: Vec<Value> = findings.into_iter().skip(offset as usize).take(limit as usize).collect();
        out.insert("runFingerprint".into(), json!(entry.run_fingerprint));
        if let Some(h) = &entry.head_sha {
            out.insert("headSha".into(), h.clone());
        }
        out.insert("offset".into(), json!(offset));
        out.insert("limit".into(), json!(limit));
        out.insert("total".into(), json!(total));
        out.insert("findings".into(), Value::Array(slice));
        Ok(Value::Object(out))
    }

    /// `callReviewRoute`.
    fn call_review_route(&self, args: &Map<String, Value>) -> Result<Value> {
        self.assert_live_diff_pin(args)?;
        let diff = self.read_diff_text(args)?;
        let paths: Vec<String> = diff.files.keys().cloned().collect();
        let routed = route_diff(&paths, &diff.text, true);
        let mut out = base_payload("review_route");
        out.insert("headSha".into(), args["headSha"].clone());
        out.insert("route".into(), json!(routed.route));
        out.insert("reason".into(), json!(routed.reason));
        out.insert("files".into(), serde_json::to_value(&routed.files)?);
        Ok(Value::Object(out))
    }

    fn config_for(&self, args: &Map<String, Value>, diff_file: String) -> (Profile, Value) {
        let log = self.log();
        let profile = Profile::resolve(&self.env, &self.root, move |m| log(m))
            .unwrap_or_else(|_| Profile::default_builtin(&crate::profile::package_root()));
        let cli = CliArgs {
            dry_run: true,
            diff_file: Some(diff_file),
            head_sha: js_string(args.get("headSha")),
            repo: String::new(),
            pr_number: 0,
        };
        let cfg = load_review_config(&self.env, &cli, &profile);
        (profile, cfg)
    }

    /// `callReviewPrescan`.
    fn call_review_prescan(&self, args: &Map<String, Value>) -> Result<Value> {
        self.assert_live_diff_pin(args)?;
        let budget = token_budget(args);
        let diff = self.read_diff_text(args)?;
        let paths: Vec<String> = diff.files.keys().cloned().collect();
        let (profile, cfg) = self.config_for(args, diff.absolute.to_string_lossy().into_owned());
        let result = self.deps.stages.prescan(&cfg, &profile, &self.root, self.log(), &paths)?;
        let error = result.error.clone().filter(|e| !e.is_empty());
        let mut base = base_payload("review_prescan");
        base.insert("status".into(), json!(if error.is_some() { "degraded" } else { "completed" }));
        base.insert("headSha".into(), args["headSha"].clone());
        base.insert("scanned".into(), json!(result.scanned));
        base.insert("lintableTotal".into(), json!(result.lintable_total.unwrap_or(0)));
        base.insert("truncated".into(), json!(result.truncated));
        base.insert("skipped".into(), result.skipped.clone().map_or(json!(0), Value::String));
        if let Some(by_tool) = &result.by_tool {
            base.insert("byTool".into(), serde_json::to_value(by_tool)?);
        }
        if let Some(e) = error {
            base.insert("error".into(), json!(e));
        }
        base.insert("findings".into(), json!([]));

        // fitPrescanFindings
        let mut slim: Vec<Value> = result.findings.iter().map(slim_finding).collect();
        sort_findings(&mut slim);
        let mut out: Vec<Value> = Vec::new();
        let mut truncated = result.truncated;
        for item in &slim {
            let mut trial = base.clone();
            let mut next = out.clone();
            next.push(item.clone());
            trial.insert("findings".into(), Value::Array(next));
            trial.insert("truncated".into(), json!(true));
            if estimate_tokens(&Value::Object(trial)) > budget {
                truncated = true;
                break;
            }
            out.push(item.clone());
        }
        if out.len() < slim.len() {
            truncated = true;
        }
        base.insert("truncated".into(), json!(truncated));
        base.insert("findings".into(), Value::Array(out));
        Ok(Value::Object(base))
    }

    /// `callFingerprintCheck`.
    fn call_fingerprint_check(&self, args: &Map<String, Value>) -> Result<Value> {
        let trimmed = |k: &str| if truthy(args.get(k)) { js_trim(&js_string(args.get(k))).to_string() } else { String::new() };
        let file = trimmed("file");
        if file.is_empty() {
            bail!("file is required");
        }
        let prior_fp = trimmed("priorFp");
        if prior_fp.is_empty() {
            bail!("priorFp is required");
        }
        if !truthy(args.get("diffPath")) {
            bail!("diffPath is required");
        }
        let line = args.get("line").and_then(js_to_number).unwrap_or(f64::NAN);
        if !(line.is_finite() && line.fract() == 0.0 && line >= 1.0) {
            bail!("line is required");
        }
        if truthy(args.get("headSha")) {
            require_head_sha(args)?;
        }
        let diff = self.read_diff_text(args)?;
        let mut out = base_payload("fingerprint_check");
        let Some(rec) = diff.files.get(&file) else {
            out.insert("status".into(), json!("stale"));
            out.insert("currentFp".into(), json!(""));
            out.insert("line".into(), Value::Null);
            return Ok(Value::Object(out));
        };
        let text = line_text(rec);
        let current_fp = finding_fingerprint(Some(&text), &num_value(line));
        let files = prior_files(&diff.files);
        let stale = filter_stale_prior_findings(&[json!({ "file": file, "line": num_value(line), "fp": prior_fp })], &files);
        if let Some((_, kept_line)) = stale.kept.first() {
            let kept_line = if kept_line.is_finite() && *kept_line != 0.0 { *kept_line } else { line };
            let mut fp = finding_fingerprint(Some(&text), &num_value(kept_line));
            if fp.is_empty() {
                fp = if current_fp.is_empty() { prior_fp } else { current_fp };
            }
            out.insert("status".into(), json!("still_present"));
            out.insert("currentFp".into(), json!(fp));
            out.insert("line".into(), num_value(kept_line));
            return Ok(Value::Object(out));
        }
        if current_fp.is_empty() {
            out.insert("status".into(), json!("unknown_line"));
            out.insert("currentFp".into(), json!(""));
            out.insert("line".into(), num_value(line));
            return Ok(Value::Object(out));
        }
        out.insert("status".into(), json!("stale"));
        out.insert("currentFp".into(), json!(current_fp));
        out.insert("line".into(), Value::Null);
        Ok(Value::Object(out))
    }

    /// `callReviewArtifact`.
    fn call_review_artifact(&self, args: &Map<String, Value>) -> Result<Value> {
        let has_path = truthy(args.get("path"));
        let job_ok = run_id_ok(args.get("jobId"));
        let fp_ok = run_id_ok(args.get("runFingerprint"));
        if [has_path, job_ok, fp_ok].iter().filter(|b| **b).count() != 1 {
            bail!("provide exactly one of path, jobId, runFingerprint");
        }
        Self::check_optional_head_sha(args)?;
        let budget = token_budget(args);
        let arg_sha = truthy_val(args.get("headSha"));
        let mut out = base_payload("review_artifact");

        if job_ok || fp_ok {
            let (_, entry) = {
                let id = js_string(args.get(if job_ok { "jobId" } else { "runFingerprint" }));
                let state = self.lock_state();
                let entry = self.lookup_entry(&state, &id).ok_or_else(|| anyhow!("unknown runFingerprint"))?;
                (id, entry)
            };
            if arg_sha.is_some() && !prefix_match_v(arg_sha.as_ref(), entry.head_sha.as_ref()) {
                bail!(
                    "pinned headSha {} does not match run {}",
                    js_string(arg_sha.as_ref()),
                    js_string(entry.head_sha.as_ref())
                );
            }
            let status = {
                let s = entry.status();
                if s.is_empty() { "completed".to_string() } else { s }
            };
            let head_sha = arg_sha.unwrap_or_else(|| entry.head_sha_v());
            out.insert("status".into(), json!(status));
            out.insert("jobId".into(), json!(entry.job_id));
            if status == "running" {
                out.insert("runFingerprint".into(), json!(""));
                out.insert("headSha".into(), head_sha);
                out.insert("previewWouldBlock".into(), json!(false));
                out.insert("blockers".into(), json!([]));
                out.insert("findingsPreview".into(), json!([]));
                out.insert("truncated".into(), json!(false));
                return Ok(Value::Object(out));
            }
            let src = PreviewSource {
                artifact: Some(&entry.artifact),
                degraded: status == "degraded",
                reason: None,
                would_block: entry.artifact.get("wouldBlock").cloned(),
                written: if entry.artifact_path.is_empty() { vec![] } else { vec![entry.artifact_path.clone()] },
            };
            let preview = fit_preview(&src, head_sha.clone(), budget, "review_artifact", &entry.job_id, None);
            out.insert("headSha".into(), head_sha);
            copy_preview_fields(&preview, &mut out);
            return Ok(Value::Object(out));
        }

        let absolute = self.contained(&js_string(args.get("path")), true)?;
        if std::fs::metadata(&absolute)?.len() > ARTIFACT_MAX_BYTES {
            bail!("artifact too large");
        }
        let payload = parse_json(&String::from_utf8_lossy(&std::fs::read(&absolute)?))?;
        let artifact = unwrap_artifact_payload(&payload);
        let artifact_sha = truthy_val(artifact.get("headSha"))
            .or_else(|| truthy_val(payload.get("headSha")))
            .unwrap_or_else(|| json!(""));
        if arg_sha.is_some() && js_truthy(&artifact_sha) && !prefix_match_v(arg_sha.as_ref(), Some(&artifact_sha)) {
            bail!(
                "pinned headSha {} does not match artifact {}",
                js_string(arg_sha.as_ref()),
                js_to_string(&artifact_sha)
            );
        }
        let head_sha = arg_sha.unwrap_or(artifact_sha);
        let job_id = truthy_val(payload.get("jobId")).map(|v| js_to_string(&v)).unwrap_or_default();
        let src = PreviewSource {
            artifact: Some(artifact),
            degraded: false,
            reason: None,
            would_block: artifact.get("wouldBlock").cloned(),
            written: vec![absolute.to_string_lossy().into_owned()],
        };
        let preview = fit_preview(&src, head_sha.clone(), budget, "review_artifact", &job_id, None);
        out.insert("headSha".into(), head_sha);
        copy_preview_fields(&preview, &mut out);
        Ok(Value::Object(out))
    }

    /// `callBlastScope`.
    fn call_blast_scope(&self, args: &Map<String, Value>) -> Result<Value> {
        self.assert_live_diff_pin(args)?;
        let diff = self.read_diff_text(args)?;
        let paths: Vec<String> = diff.files.keys().cloned().collect();
        let (_profile, cfg) = self.config_for(args, js_string(args.get("diffPath")));
        let source = if diff.changed_text.is_empty() { &diff.text } else { &diff.changed_text };
        let symbols = collect_blast_symbols(source, Some(&IndexMap::new()), 25);
        let mut skip_reason: Option<String> = None;
        let affected: Vec<Value> = match self.deps.stages.blast(&cfg, &symbols) {
            Ok(blast) => {
                let m = &blast.metrics;
                if !m.skip_reason.is_empty() && m.skip_reason != "none" {
                    skip_reason = Some(if m.skip_reason == "graph_missing" {
                        "graphify_unavailable".into()
                    } else {
                        m.skip_reason.clone()
                    });
                }
                m.affected_files.iter().map(|s| json!(s)).collect()
            }
            Err(_) => {
                skip_reason = Some("graphify_unavailable".into());
                Vec::new()
            }
        };
        let changed = json!(paths);
        let files: Vec<String> =
            crate::export::suggested_scope_files(&affected, Some(&changed)).into_iter().take(BLAST_MAX_FILES).collect();
        let mut out = base_payload("blast_scope");
        out.insert("headSha".into(), args["headSha"].clone());
        out.insert("files".into(), json!(files));
        if let Some(r) = skip_reason {
            out.insert("skipReason".into(), json!(r));
        }
        Ok(Value::Object(out))
    }

    /// `callActionabilityExplain`: находка не мутируется (работаем с копией).
    fn call_actionability_explain(&self, args: &Map<String, Value>) -> Result<Value> {
        let finding = args.get("finding").filter(|v| v.is_object() || v.is_array()).ok_or_else(|| anyhow!("finding is required"))?;
        let (profile, _) = self.config_for(args, String::new());
        let ctx = ScoreCtx::new(
            Default::default(),
            Default::default(),
            Default::default(),
            profile.layout().sensitive_source(),
        )?;
        let (score, reasons) = score_actionability(finding, &ctx);
        let mut out = base_payload("actionability_explain");
        out.insert("score".into(), json!(score));
        out.insert("reasons".into(), serde_json::to_value(&reasons)?);
        Ok(Value::Object(out))
    }

    /// `callReviewMetrics`.
    fn call_review_metrics(&self, args: &Map<String, Value>) -> Result<Value> {
        if !truthy(args.get("path")) {
            bail!("path is required");
        }
        let absolute = self.contained(&js_string(args.get("path")), true)?;
        if std::fs::metadata(&absolute)?.len() > ARTIFACT_MAX_BYTES {
            bail!("artifact too large");
        }
        let text = String::from_utf8_lossy(&std::fs::read(&absolute)?).into_owned();
        let records = Value::Array(parse_jsonl(&text));
        let agg = aggregate_metrics(Some(&records))?;
        let mut out = base_payload("review_metrics");
        if let Value::Object(m) = agg {
            out.extend(m);
        }
        Ok(Value::Object(out))
    }
}

/// Поля превью, которые `review_artifact` отдаёт наружу (без `runFingerprint`/`artifactPath`).
fn copy_preview_fields(preview: &Map<String, Value>, out: &mut Map<String, Value>) {
    for key in ["previewWouldBlock", "strong", "route", "counts", "blockers", "findingsPreview", "truncated"] {
        if let Some(v) = preview.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
}

#[cfg(test)]
#[path = "review_mcp_tests.rs"]
mod tests;
