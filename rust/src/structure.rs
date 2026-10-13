//! Порт `lib/structure.mjs` (RFC 20, T21): glue `analyze_structure` — собирает
//! facts → assessment → drift → triage из провайдеров structure-контура.
//!
//! Граница (RFC 01, п. 1; RFC 20 «Чего не делаем»): модуль не импортирует
//! `pipeline`/`engine`/`github`/`llm` и ничего не блокирует — тест
//! `boundary_*` ниже читает исходники как текст и держит это правило.
//!
//! Внешние эффекты — через [`StructureIo`] (аналог `deps` в JS): чтение файлов,
//! `git` и часы. [`RealStructureIo`] — боевой вариант, тесты подставляют свой.
//!
//! Расхождения с JS (осознанные):
//! - исходники тест-файлов читаются через `read_repo_file`: боевой вариант
//!   проверяет realpath внутри `repoRoot` (`util::resolve_contained_path`), так что
//!   symlink из PR наружу репозитория не читается — файл пропускается (fail-open,
//!   как любая ошибка чтения в JS). JS читает по ссылке;
//! - `options.scipBuffer` (Buffer) не представим в JSON-опциях и не поддержан:
//!   in-memory индекс аттестуется ре-сериализацией, как в JS без `scipBuffer`;
//! - текст ошибки разбора графа после префикса `invalid structure graph JSON (…):`
//!   — сообщение serde_json, а не V8.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::findings::{js_is_whitespace, js_to_string, js_truthy};
use crate::observability::iso_from_epoch_ms;
use crate::report::js_json_stringify;
use crate::structure_assessments::{assess_dependency_risk, build_structure_assessment};
use crate::structure_drift::{
    apply_structure_waivers, compute_structure_drift, suggest_obligation_links, triage_structure_findings,
    DriftInput,
};
use crate::structure_evidence::{create_evidence_receipt, js_default_sort, sha256_value, unique_values, value_key};
use crate::structure_graphify::{build_structure_views, clamp_limit, extract_graphify_facts, GraphifyOptions};
use crate::structure_identity::reconcile_identity;
use crate::structure_lcov::{lcov_observations, parse_lcov};
use crate::structure_mutation::{critical_contract_refs, parse_mutation_report};
use crate::structure_protocol::{
    facts_fingerprint_op, validate_obligations_export, validate_structure_assessment, validate_structure_facts,
    OBLIGATIONS_SCHEMA, STRUCTURE_FACTS_SCHEMA, STRUCTURE_PROTOCOL_VERSION,
};
use crate::structure_scip::{
    decode_scip_index, extract_scip_facts, merge_scip_with_graphify, normalize_repo_path, sha256_hex, ExtractOptions,
};
use crate::structure_tests::{
    extract_test_cases_from_source, generate_property_drafts, merge_test_cases, parse_property_report,
};
use crate::util::{js_resolve, resolve_contained_path};

/// `STRUCTURE_TOOL_VERSION` из `lib/structure.mjs`.
pub const STRUCTURE_TOOL_VERSION: &str = "1.0.0";
/// `STRUCTURE_FACTS_VERSION` = `STRUCTURE_PROTOCOL_VERSION`.
pub const STRUCTURE_FACTS_VERSION: i64 = STRUCTURE_PROTOCOL_VERSION;

/// Таймаут и потолок вывода `git` (`gitValue` в JS: `timeout: 5000`, `maxBuffer: 1 МБ`).
const GIT_TIMEOUT: Duration = Duration::from_millis(5000);
const GIT_MAX_BUFFER: usize = 1024 * 1024;
/// Сколько тест-файлов сканируется на кейсы (`testPaths.slice(0, 1000)`).
const MAX_TEST_SOURCES: usize = 1000;

// ---------------------------------------------------------------------------
// Внешние эффекты
// ---------------------------------------------------------------------------

/// Внешние эффекты `analyze_structure` (`deps.readFile`/`deps.execFileP` и часы в JS).
pub trait StructureIo {
    /// Байты файла по пути, как его дал оператор (`--graph`, `--lcov`, …).
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;

    /// Исходник тест-файла из графа: `rel` уже прошёл `normalize_repo_path`.
    /// По умолчанию — `read_file(resolve(root, rel))`, как в JS.
    fn read_repo_file(&self, root: &Path, rel: &str) -> std::io::Result<Vec<u8>> {
        self.read_file(&js_resolve(root, rel))
    }

    /// `git -C <cwd> <args…>`: stdout при успехе, `None` — любой сбой.
    fn git(&self, cwd: &Path, args: &[&str]) -> Option<String>;

    /// Текущее время, мс эпохи (`new Date()`).
    fn now_ms(&self) -> i64;

    /// Рабочий каталог (`process.cwd()`).
    fn cwd(&self) -> PathBuf;
}

/// Боевые эффекты: ФС, `git` с таймаутом, системные часы.
pub struct RealStructureIo;

impl StructureIo for RealStructureIo {
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_repo_file(&self, root: &Path, rel: &str) -> std::io::Result<Vec<u8>> {
        // Путь пришёл из графа — вход от автора PR: symlink наружу корня не читаем.
        let path = resolve_contained_path(rel, Some(root), true)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e.to_string()))?;
        std::fs::read(path)
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> Option<String> {
        run_git(cwd, args)
    }

    fn now_ms(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }

    fn cwd(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    }
}

/// `execFile('git', ['-C', cwd, ...args], {timeout: 5000, maxBuffer: 1 МБ})`:
/// таймаут или переполнение убивают процесс, любой сбой — `None`.
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    // Читатель в отдельном потоке: иначе процесс с выводом больше буфера трубы
    // заблокируется на записи, а мы — на ожидании.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(GIT_MAX_BUFFER as u64 + 1).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let buf = reader.join().ok()?;
    if !status?.success() || buf.len() > GIT_MAX_BUFFER {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// ---------------------------------------------------------------------------
// Мелкие помощники JS-семантики
// ---------------------------------------------------------------------------

fn opt<'a>(options: &'a Value, key: &str) -> Option<&'a Value> {
    options.get(key).filter(|v| !v.is_null())
}

/// `options[key]` как truthy-значение (`options.x || …`).
fn truthy<'a>(options: &'a Value, key: &str) -> Option<&'a Value> {
    options.get(key).filter(|v| js_truthy(v))
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(js_is_whitespace)
}

fn text_of(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `optionalText(path)`: пустой путь или ошибка чтения — `None`.
fn optional_text(io: &dyn StructureIo, path: Option<&Value>) -> Option<String> {
    let path = path.filter(|v| js_truthy(v))?;
    io.read_file(Path::new(&js_to_string(path))).ok().map(text_of)
}

/// `optionalJson(path)`: нечитаемый файл — `None`, битый JSON — ошибка (как в JS).
fn optional_json(io: &dyn StructureIo, path: Option<&Value>, label: &str) -> Result<Option<Value>> {
    match optional_text(io, path) {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| anyhow!("{label}: {e}")),
    }
}

/// `options.x ?? await optionalText(options.xPath)`.
fn text_or_path(io: &dyn StructureIo, options: &Value, text_key: &str, path_key: &str) -> Option<String> {
    match opt(options, text_key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(js_to_string(other)),
        None => optional_text(io, options.get(path_key)),
    }
}

fn js_stringify(v: &Value) -> String {
    let mut out = String::new();
    js_json_stringify(v, &mut out);
    out
}

fn arr(v: Option<&Value>) -> Vec<Value> {
    v.and_then(Value::as_array).cloned().unwrap_or_default()
}

/// `[...new Set(strings)].sort()`.
fn unique_sorted(items: Vec<String>) -> Vec<String> {
    let values: Vec<Value> = items.into_iter().map(Value::String).collect();
    let mut unique = unique_values(&values);
    js_default_sort(&mut unique);
    unique.into_iter().filter_map(|v| v.as_str().map(String::from)).collect()
}

/// `\s` JS: `js_is_whitespace` + U+FEFF (у `regex` `\s` — Unicode White_Space, он шире).
fn graph_built_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)Built from commit:[\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]*`?([0-9a-f]{7,64})`?",
        )
        .expect("регекс отчёта графа")
    })
}

/// `graphStatus(report, headSha)`: свежесть графа по `Built from commit:` в отчёте.
fn graph_status(report: Option<&str>, head_sha: &str) -> &'static str {
    let Some(report) = report.filter(|r| !r.is_empty()) else { return "unknown" };
    let Some(built) = graph_built_re().captures(report).and_then(|c| c.get(1)) else { return "unknown" };
    let built = built.as_str();
    if head_sha.starts_with(built) || built.starts_with(head_sha) { "fresh" } else { "stale" }
}

/// `/(?:^|\/)(?:test|tests|__tests__|e2e)(?:\/|$)|\.(?:test|spec)\.[^.]+$/i`.
fn test_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|/)(?:test|tests|__tests__|e2e)(?:/|$)|\.(?:test|spec)\.[^.\n\r\x{2028}\x{2029}]+$")
            .expect("регекс тест-пути")
    })
}

// ---------------------------------------------------------------------------
// Шаги сборки
// ---------------------------------------------------------------------------

/// `contractsWithLabels`: контракт + `label` его сущности (или `''`).
fn contracts_with_labels(facts: &Map<String, Value>) -> Vec<Value> {
    let mut labels: HashMap<String, Value> = HashMap::new();
    for entity in arr(facts.get("entities")) {
        if let Some(id) = entity.get("observedId") {
            let label = entity.get("label").filter(|v| js_truthy(v)).cloned().unwrap_or(json!(""));
            labels.insert(value_key(id), label);
        }
    }
    arr(facts.get("contracts"))
        .into_iter()
        .map(|mut contract| {
            let label = contract
                .get("entityRef")
                .and_then(|r| labels.get(&value_key(r)))
                .cloned()
                .unwrap_or(json!(""));
            if let Value::Object(m) = &mut contract {
                m.insert("label".into(), label);
            }
            contract
        })
        .collect()
}

/// `enrichTestsFromSources`: кейсы из исходников тест-файлов поверх графовых.
fn enrich_tests_from_sources(facts: &Map<String, Value>, repo_root: &Path, io: &dyn StructureIo) -> Vec<Value> {
    let entities = arr(facts.get("entities"));
    let test_cases = arr(facts.get("testCases"));
    let mut paths: Vec<String> = test_cases
        .iter()
        .filter_map(|tc| tc.get("path").and_then(Value::as_str).map(String::from))
        .collect();
    paths.extend(
        entities
            .iter()
            .filter_map(|e| e.get("path").and_then(Value::as_str))
            .filter(|p| test_path_re().is_match(p))
            .map(String::from),
    );
    let paths = unique_sorted(paths);
    let contracts = contracts_with_labels(facts);
    let mut scanned = Vec::new();
    for path in paths.iter().take(MAX_TEST_SOURCES) {
        let Ok(rel) = normalize_repo_path(path) else { continue };
        let Ok(bytes) = io.read_repo_file(repo_root, &rel) else { continue };
        scanned.extend(extract_test_cases_from_source(path, &text_of(bytes), &contracts, &entities));
    }
    merge_test_cases(vec![test_cases, scanned])
}

/// `applyBoundaries`: сущностям — boundaryId по умолчанию, рёбрам — crossBoundary/crossCommunity.
fn apply_boundaries(facts: &mut Map<String, Value>) {
    let fallback = arr(facts.get("boundaries"))
        .iter()
        .find(|b| b.get("kind").and_then(Value::as_str) == Some("repository"))
        .and_then(|b| b.get("id"))
        .filter(|v| js_truthy(v))
        .cloned()
        .unwrap_or(json!("boundary:repository"));
    let entities: Vec<Value> = arr(facts.get("entities"))
        .into_iter()
        .map(|mut entity| {
            if let Value::Object(m) = &mut entity {
                let current = m.get("boundaryId").filter(|v| js_truthy(v)).cloned();
                m.insert("boundaryId".into(), current.unwrap_or_else(|| fallback.clone()));
            }
            entity
        })
        .collect();
    let mut by_id: HashMap<String, &Value> = HashMap::new();
    for entity in &entities {
        if let Some(id) = entity.get("observedId") {
            by_id.insert(value_key(id), entity);
        }
    }
    let lookup = |edge: &Value, key: &str| edge.get(key).and_then(|id| by_id.get(&value_key(id)).copied());
    let present = |v: Option<&Value>| v.is_some_and(|v| !v.is_null());
    let dependencies: Vec<Value> = arr(facts.get("dependencies"))
        .into_iter()
        .map(|mut edge| {
            let from = lookup(&edge, "from");
            let to = lookup(&edge, "to");
            let cross_boundary = match (from, to) {
                (Some(f), Some(t)) => f.get("boundaryId") != t.get("boundaryId"),
                _ => false,
            };
            let cross_community = match edge.get("crossCommunity").filter(|v| js_truthy(v)) {
                Some(v) => v.clone(),
                None => {
                    let fc = from.and_then(|f| f.get("community"));
                    let tc = to.and_then(|t| t.get("community"));
                    json!(present(fc) && present(tc) && value_key(fc.unwrap()) != value_key(tc.unwrap()))
                }
            };
            if let Value::Object(m) = &mut edge {
                m.insert("crossBoundary".into(), json!(cross_boundary));
                m.insert("crossCommunity".into(), cross_community);
            }
            edge
        })
        .collect();
    facts.insert("entities".into(), Value::Array(entities));
    facts.insert("dependencies".into(), Value::Array(dependencies));
}

/// Общие поля квитанций одного прогона.
struct ReceiptCtx<'a> {
    config_hash: &'a str,
    scope_hash: &'a str,
    trust_tier: &'a Value,
    generated_at: &'a str,
}

/// `artifactReceipt`: EvidenceReceipt на артефакт провайдера.
#[allow(clippy::too_many_arguments)]
fn artifact_receipt(
    ctx: &ReceiptCtx,
    provider: &str,
    tool_version: Value,
    capability: &str,
    head_sha: &str,
    uri: &str,
    sha256: String,
    observation: Value,
    evidence_ceiling: &str,
    limitations: &[&str],
) -> Result<Value> {
    let input = json!({
        "provider": provider,
        "toolVersion": tool_version,
        "capability": capability,
        "headSha": head_sha,
        "configHash": ctx.config_hash,
        "scopeHash": ctx.scope_hash,
        "artifact": { "uri": uri, "sha256": sha256 },
        "observation": observation,
        "trustTier": ctx.trust_tier,
        "evidenceCeiling": evidence_ceiling,
        "limitations": limitations,
        "observedAt": ctx.generated_at,
    });
    create_evidence_receipt(Some(&input), ctx.generated_at)
}

/// `options.maxX || default` → лимит для SCIP (целое ≥ 0, иначе дефолт).
fn scip_limit(v: &Value, default: usize) -> usize {
    match v.as_f64() {
        Some(x) if x.is_finite() && x >= 0.0 => x.floor() as usize,
        _ => default,
    }
}

// ---------------------------------------------------------------------------
// analyzeStructure
// ---------------------------------------------------------------------------

/// Порт `analyzeStructure(options, deps)`. `options` — JSON-объект с теми же
/// полями, что у JS (`graphText`/`graphPath`, `lcovText`/`lcovPath`, `scipIndex`/`scipPath`,
/// `obligationsExport`/`obligationsPath`, `mutationReport`/`mutationReportPath`, …).
/// Возвращает `{facts, assessment, propertyDrafts, drift, triage, diagnostics}`.
pub fn analyze_structure(options: &Value, io: &dyn StructureIo) -> Result<Value> {
    let empty = json!({});
    let options = if options.is_object() { options } else { &empty };

    let cwd = io.cwd();
    let repo_root = match truthy(options, "repoRoot") {
        Some(v) => js_resolve(&cwd, &js_to_string(v)),
        None => js_resolve(&cwd, "."),
    };
    let generated_at = match truthy(options, "generatedAt") {
        Some(v) => js_to_string(v),
        None => iso_from_epoch_ms(io.now_ms()),
    };
    let git_value = |args: &[&str], fallback: &str| -> String {
        io.git(&repo_root, args)
            .map(|out| js_trim(&out).to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    };
    let head_sha = match truthy(options, "headSha") {
        Some(v) => js_to_string(v),
        None => git_value(&["rev-parse", "HEAD"], "0000000"),
    };
    let branch_ref = match truthy(options, "branchRef") {
        Some(v) => js_to_string(v),
        None => git_value(&["symbolic-ref", "--short", "HEAD"], "unknown"),
    };
    let scope = match truthy(options, "scope") {
        Some(v) => normalize_repo_path(&js_to_string(v))?,
        None => String::new(),
    };
    let layout = truthy(options, "layout").cloned().unwrap_or(json!({}));
    let max_entities = truthy(options, "maxEntities").cloned().unwrap_or(json!(5000));
    let max_edges = truthy(options, "maxEdges").cloned().unwrap_or(json!(15000));
    let config_input = json!({
        "scope": scope,
        "includeAmbiguous": options.get("includeAmbiguous") == Some(&Value::Bool(true)),
        "layout": layout,
        "maxEntities": max_entities,
        "maxEdges": max_edges,
    });
    let config_hash = sha256_value(&config_input);
    let scope_hash = sha256_value(&json!(if scope.is_empty() { "." } else { scope.as_str() }));
    let trust_tier = truthy(options, "trustTier").cloned().unwrap_or(json!("trusted_local"));
    let ctx = ReceiptCtx {
        config_hash: &config_hash,
        scope_hash: &scope_hash,
        trust_tier: &trust_tier,
        generated_at: &generated_at,
    };
    let mut warnings: Vec<String> = Vec::new();
    let mut receipts: Vec<Value> = Vec::new();

    // --- граф Graphify --------------------------------------------------------
    let graph_path = truthy(options, "graphPath").map(js_to_string);
    let graph_text = text_or_path(io, options, "graphText", "graphPath");
    let mut graph = json!({ "nodes": [], "links": [] });
    if let Some(text) = graph_text.as_deref().filter(|t| !t.is_empty()) {
        graph = serde_json::from_str(text).map_err(|e| {
            anyhow!(
                "invalid structure graph JSON ({}): {e}",
                graph_path.as_deref().unwrap_or("memory:graph.json")
            )
        })?;
    }
    let graph_report = text_or_path(io, options, "graphReportText", "graphReportPath");
    let analysis = match truthy(options, "graphAnalysis") {
        Some(v) => Some(v.clone()),
        None => {
            let path = match truthy(options, "graphAnalysisPath") {
                Some(p) => Some(p.clone()),
                // join(dirname(graphPath), '.graphify_analysis.json')
                None => graph_path.as_deref().map(|p| {
                    let dir = Path::new(p).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
                    match dir.as_str() {
                        "" | "." => json!(".graphify_analysis.json"),
                        d => json!(format!("{}/.graphify_analysis.json", d.trim_end_matches('/'))),
                    }
                }),
            };
            optional_json(io, path.as_ref(), "invalid graph analysis JSON")?
        }
    };
    let has_graph = graph_text.as_deref().is_some_and(|t| !t.is_empty());
    let current_graph_status = if has_graph { graph_status(graph_report.as_deref(), &head_sha) } else { "missing" };
    let graph_options = GraphifyOptions {
        analysis: analysis.filter(|v| !v.is_null()),
        layout: Some(layout.clone()),
        scope: scope.clone(),
        include_ambiguous: options.get("includeAmbiguous").is_some_and(js_truthy),
        max_entities: clamp_limit(Some(&max_entities), 5000),
        max_edges: clamp_limit(Some(&max_edges), 15000),
        ..GraphifyOptions::default()
    };
    if graph.is_null() {
        bail!("Cannot read properties of null (reading 'nodes')");
    }
    let mut extracted = extract_graphify_facts(Some(&graph), &graph_options);
    warnings.extend(arr(extracted.get("warnings")).iter().map(js_to_string));
    if let Some(text) = graph_text.as_deref().filter(|t| !t.is_empty()) {
        let links = match graph.get("links").filter(|v| js_truthy(v)) {
            Some(l) => l,
            None => graph.get("edges").unwrap_or(&Value::Null),
        };
        let count = |v: Option<&Value>| v.and_then(Value::as_array).map_or(0, Vec::len);
        receipts.push(artifact_receipt(
            &ctx,
            "graphify",
            truthy(options, "graphifyVersion").cloned().unwrap_or(json!("unknown")),
            "topology",
            &head_sha,
            graph_path.as_deref().unwrap_or("memory:graph.json"),
            sha256_hex(text.as_bytes()),
            json!({ "type": "graph_inventory", "value": { "nodes": count(graph.get("nodes")), "edges": count(Some(links)) } }),
            "static_inventory",
            &["symbol kinds and source ranges may be heuristic"],
        )?);
    } else {
        warnings.push("graph unavailable".into());
    }

    // --- SCIP -------------------------------------------------------------------
    let scip_path = truthy(options, "scipPath").map(js_to_string);
    let scip_index_opt = truthy(options, "scipIndex");
    let scip_requested = scip_path.is_some() || scip_index_opt.is_some();
    let scip_version = truthy(options, "scipVersion").cloned().unwrap_or(json!("0.4.0"));
    if scip_requested {
        // Любой сбой SCIP — только категория в warnings: текст ошибки может нести пути.
        let scip_step = |extracted: &mut Map<String, Value>| -> Result<Value> {
            let index = match scip_index_opt {
                Some(index) => index.clone(),
                None => Value::Object(decode_scip_index(&io.read_file(Path::new(scip_path.as_deref().unwrap()))?)?),
            };
            let scip = extract_scip_facts(
                Some(&index),
                &ExtractOptions {
                    scope: Some(scope.clone()),
                    max_entities: scip_limit(&max_entities, 5000),
                    max_edges: scip_limit(&max_edges, 15000),
                },
            )?;
            let scip = Value::Object(scip);
            *extracted = merge_scip_with_graphify(Some(&scip), Some(&Value::Object(extracted.clone())))?;
            let contents = match &scip_path {
                Some(path) => io.read_file(Path::new(path))?,
                None => js_stringify(&index).into_bytes(),
            };
            artifact_receipt(
                &ctx,
                "scip-typescript",
                scip_version.clone(),
                "semantic_symbols",
                &head_sha,
                scip_path.as_deref().unwrap_or("memory:index.scip"),
                sha256_hex(&contents),
                json!({ "type": "symbol_inventory", "value": {
                    "entities": arr(scip.get("entities")).len(),
                    "references": arr(scip.get("dependencies")).len(),
                } }),
                "static_inventory",
                &["SCIP relationships are references, not runtime calls"],
            )
        };
        match scip_step(&mut extracted) {
            Ok(receipt) => receipts.push(receipt),
            Err(_) => warnings.push("SCIP unavailable: index could not be decoded".into()),
        }
    }

    apply_boundaries(&mut extracted);
    let test_cases = enrich_tests_from_sources(&extracted, &repo_root, io);
    extracted.insert("testCases".into(), Value::Array(test_cases));
    let previous = arr(opt(options, "previousFacts").and_then(|p| p.get("facts")).and_then(|f| f.get("entities")));
    let identity = reconcile_identity(&arr(extracted.get("entities")), &previous);
    extracted.insert("entities".into(), identity.get("entities").cloned().unwrap_or(json!([])));
    let field = |k: &str| extracted.get(k).cloned().unwrap_or(Value::Null);
    let facts = json!({
        "boundaries": field("boundaries"),
        "entities": field("entities"),
        "contracts": field("contracts"),
        "dependencies": field("dependencies"),
        "testCases": field("testCases"),
    });
    let provider_version = format!(
        "graphify:{}|{}",
        js_to_string(truthy(options, "graphifyVersion").unwrap_or(&json!("unknown"))),
        if scip_requested { format!("scip:{}", js_to_string(&scip_version)) } else { "scip:none".into() }
    );
    let fingerprint = facts_fingerprint_op(&json!({
        "headSha": head_sha,
        "providerVersion": provider_version,
        "configHash": config_hash,
        "scope": scope,
    }));
    let mut provenance = Map::new();
    provenance.insert("headSha".into(), json!(head_sha));
    provenance.insert("branchRef".into(), json!(branch_ref));
    provenance.insert("prNumber".into(), truthy(options, "prNumber").cloned().unwrap_or(Value::Null));
    provenance.insert("isDefaultBranch".into(), json!(options.get("isDefaultBranch") == Some(&Value::Bool(true))));
    provenance.insert("toolVersion".into(), json!(STRUCTURE_TOOL_VERSION));
    provenance.insert("configHash".into(), json!(config_hash));
    provenance.insert("scope".into(), json!(scope));
    provenance.insert("graphStatus".into(), json!(current_graph_status));
    // undefined в JS выпадает из JSON — ключа нет
    if let Some(capabilities) = extracted.get("capabilities") {
        provenance.insert("providerCapabilities".into(), capabilities.clone());
    }
    provenance.insert("warnings".into(), json!(unique_sorted(warnings.clone())));
    provenance.insert("truncated".into(), json!(extracted.get("truncated") == Some(&Value::Bool(true))));
    let facts_payload = json!({
        "schemaRef": STRUCTURE_FACTS_SCHEMA,
        "version": STRUCTURE_FACTS_VERSION,
        "kind": "structure_facts",
        "generatedAt": generated_at,
        "fingerprint": fingerprint,
        "provenance": provenance,
        "facts": facts,
        "identityEvents": identity.get("events").cloned().unwrap_or(json!([])),
        "views": build_structure_views(Some(&facts)),
        "receipts": receipts,
    });
    validate_structure_facts(&facts_payload, None)?;

    // --- обязательства ----------------------------------------------------------
    let obligations_export = match truthy(options, "obligationsExport") {
        Some(v) => v.clone(),
        None => optional_json(io, options.get("obligationsPath"), "invalid obligations JSON")?
            .filter(js_truthy)
            .unwrap_or_else(|| {
                json!({
                    "schemaRef": OBLIGATIONS_SCHEMA,
                    "version": 1,
                    "kind": "obligations_export",
                    "project": truthy(options, "project").cloned().unwrap_or(json!("unknown")),
                    "revision": "none",
                    "headSha": null,
                    "exportedAt": generated_at,
                    "obligations": [],
                })
            }),
    };
    validate_obligations_export(&obligations_export, None)?;
    let obligations = obligations_export.get("obligations").cloned().unwrap_or(json!([]));

    // --- LCOV -------------------------------------------------------------------
    let test_head_sha = truthy(options, "testHeadSha").map(js_to_string).unwrap_or_else(|| head_sha.clone());
    let lcov_text = text_or_path(io, options, "lcovText", "lcovPath").filter(|t| !t.is_empty());
    let lcov = match &lcov_text {
        Some(text) => Some(Value::Object(parse_lcov(text, &repo_root.to_string_lossy())?)),
        None => None,
    };
    let mut entity_by_path: Vec<(String, Value)> = Vec::new();
    for entity in arr(facts.get("entities")) {
        let path = entity.get("path").map(js_to_string).unwrap_or_else(|| "undefined".into());
        match entity_by_path.iter_mut().find(|(p, _)| *p == path) {
            Some((_, Value::Array(list))) => list.push(entity),
            _ => entity_by_path.push((path, json!([entity]))),
        }
    }
    let coverage_observations = match &lcov {
        Some(lcov) => lcov_observations(Some(lcov), &entity_by_path),
        None => Vec::new(),
    };
    let mut assessment_receipts: Vec<Value> = Vec::new();
    if let (Some(text), Some(lcov)) = (&lcov_text, &lcov) {
        assessment_receipts.push(artifact_receipt(
            &ctx,
            "lcov",
            truthy(options, "lcovVersion").cloned().unwrap_or(json!("1")),
            "aggregate_coverage",
            &test_head_sha,
            &truthy(options, "lcovPath").map(js_to_string).unwrap_or_else(|| "memory:lcov.info".into()),
            sha256_hex(text.as_bytes()),
            json!({ "type": "coverage_inventory", "value": lcov.get("totals").cloned().unwrap_or(Value::Null) }),
            "aggregate_lcov",
            &["aggregate LCOV cannot prove which test executed a function"],
        )?);
    }

    // --- property / mutation ------------------------------------------------------
    let property_drafts = generate_property_drafts(&arr(Some(&obligations)));
    let property_report = match truthy(options, "propertyReport") {
        Some(v) => Some(v.clone()),
        None => optional_json(io, options.get("propertyReportPath"), "invalid property report JSON")?,
    };
    let property_results = match property_report.filter(js_truthy) {
        Some(report) => parse_property_report(Some(&report), &property_drafts),
        None => Vec::new(),
    };
    let coverage_json = Value::Array(coverage_observations.clone());
    let preliminary_risk = assess_dependency_risk(Some(&facts), Some(&coverage_json), opt(options, "thresholds"))?;
    let in_memory_mutation = truthy(options, "mutationReport").cloned();
    let mutation_report_text = match in_memory_mutation {
        Some(_) => None,
        None => optional_text(io, options.get("mutationReportPath")),
    };
    let mut mutation_report = in_memory_mutation;
    if mutation_report.is_none() {
        if let Some(text) = mutation_report_text.as_deref().filter(|t| !t.is_empty()) {
            match serde_json::from_str::<Value>(text) {
                Ok(v) => mutation_report = Some(v).filter(js_truthy),
                Err(_) => warnings.push(format!(
                    "mutation report ignored: invalid JSON ({})",
                    options.get("mutationReportPath").map(js_to_string).unwrap_or_else(|| "undefined".into())
                )),
            }
        }
    }
    let mutation = match &mutation_report {
        Some(report) => {
            let refs = critical_contract_refs(Some(&facts), Some(&preliminary_risk), Some(&obligations))?;
            parse_mutation_report(Some(report), Some(&facts), &arr(Some(&refs)))?
        }
        None => json!({
            "observations": [],
            "warnings": [],
            "summary": { "total": 0, "killed": 0, "survived": 0, "noCoverage": 0, "timeout": 0 },
        }),
    };
    if let Some(report) = &mutation_report {
        let tool_version = report
            .get("schemaVersion")
            .filter(|v| js_truthy(v))
            .or_else(|| truthy(options, "mutationVersion"))
            .map(js_to_string)
            .unwrap_or_else(|| "unknown".into());
        // файл — сырые байты с диска, in-memory отчёт аттестует ре-сериализацию
        let contents = mutation_report_text.clone().unwrap_or_else(|| js_stringify(report));
        assessment_receipts.push(artifact_receipt(
            &ctx,
            "mutation-testing-elements",
            json!(tool_version),
            "mutation_testing",
            &test_head_sha,
            &truthy(options, "mutationReportPath").map(js_to_string).unwrap_or_else(|| "memory:mutation.json".into()),
            sha256_hex(contents.as_bytes()),
            json!({ "type": "mutation_inventory", "value": mutation.get("summary").cloned().unwrap_or(Value::Null) }),
            "mutation_bound",
            &["equivalent mutants may survive without indicating a test defect"],
        )?);
    }

    // --- assessment ---------------------------------------------------------------
    let mut test_run = Map::new();
    if let Some(id) = options.get("testRunId") {
        test_run.insert("id".into(), id.clone());
    }
    test_run.insert("headSha".into(), json!(test_head_sha));
    test_run.insert(
        "producedAt".into(),
        truthy(options, "testProducedAt").cloned().unwrap_or_else(|| json!(generated_at)),
    );
    let thresholds = truthy(options, "thresholds").cloned().unwrap_or(json!({}));
    let assessment_payload = build_structure_assessment(
        Some(&facts_payload),
        Some(&obligations_export),
        lcov.as_ref(),
        Some(&coverage_json),
        Some(&Value::Array(property_results)),
        Some(&mutation),
        Some(&Value::Array(assessment_receipts)),
        Some(&Value::Object(test_run)),
        Some(&thresholds),
        Some(&json!(generated_at)),
    )?;
    validate_structure_assessment(&assessment_payload, None)?;

    // --- drift / triage ---------------------------------------------------------------
    let link_suggestions = Value::Array(suggest_obligation_links(Some(&obligations), Some(&facts), 5));
    let confirmed_claims = truthy(options, "confirmedClaims").cloned().unwrap_or(json!([]));
    let mut drift = compute_structure_drift(&DriftInput {
        facts: &facts_payload,
        assessment: &assessment_payload,
        obligations_export: Some(&obligations_export),
        confirmed_claims: Some(&confirmed_claims),
        link_suggestions: Some(&link_suggestions),
    })?;
    let waived = apply_structure_waivers(
        &arr(drift.get("findings")),
        &arr(truthy(options, "waivers")),
        io.now_ms(),
    );
    let triage = triage_structure_findings(&waived, 20)?;
    if let Value::Object(m) = &mut drift {
        m.insert("findings".into(), Value::Array(waived));
    }

    let mut all_warnings = warnings;
    all_warnings.extend(arr(lcov.as_ref().and_then(|l| l.get("warnings"))).iter().map(js_to_string));
    all_warnings.extend(arr(mutation.get("warnings")).iter().map(js_to_string));
    let scip_used = scip_requested
        && arr(facts_payload.get("receipts"))
            .iter()
            .any(|r| r.get("provider").and_then(Value::as_str) == Some("scip-typescript"));
    Ok(json!({
        "facts": facts_payload,
        "assessment": assessment_payload,
        "propertyDrafts": property_drafts,
        "drift": drift,
        "triage": triage,
        "diagnostics": {
            "warnings": unique_sorted(all_warnings),
            "scipUsed": scip_used,
            "evidenceCeiling": if lcov.is_some() { "aggregate_lcov" } else { "static_inventory" },
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Файлы в памяти, git по таблице, фиксированные часы.
    struct MemIo {
        files: HashMap<String, Vec<u8>>,
        git: HashMap<String, String>,
        now_ms: i64,
        git_calls: RefCell<Vec<String>>,
    }

    impl MemIo {
        fn new(files: &Value, git: &Value) -> Self {
            let to_map = |v: &Value| -> HashMap<String, String> {
                v.as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect())
                    .unwrap_or_default()
            };
            MemIo {
                files: to_map(files).into_iter().map(|(k, v)| (k, v.into_bytes())).collect(),
                git: to_map(git),
                // 2026-09-02: waiver с expiresAt 2099 активен, с 2000 — истёк
                now_ms: 1_788_307_200_000,
                git_calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl StructureIo for MemIo {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.files
                .get(path.to_string_lossy().as_ref())
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "ENOENT"))
        }
        fn git(&self, _cwd: &Path, args: &[&str]) -> Option<String> {
            let key = args.join(" ");
            self.git_calls.borrow_mut().push(key.clone());
            self.git.get(&key).cloned()
        }
        fn now_ms(&self) -> i64 {
            self.now_ms
        }
        fn cwd(&self) -> PathBuf {
            PathBuf::from("/work")
        }
    }

    fn golden() -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("parity/golden-structure/t21.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("t21.json")).expect("t21.json — JSON")
    }

    /// Эталон JS: rust/parity/golden-structure/t21.json (генератор рядом).
    #[test]
    fn golden_cases_match_js() {
        let golden = golden();
        let cases = golden["cases"].as_array().expect("cases");
        assert!(cases.len() >= 8);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let io = MemIo::new(&case["files"], &case["git"]);
            let result = analyze_structure(&case["options"], &io);
            match case["error"].as_str() {
                Some(prefix) => {
                    let err = format!("{:#}", result.expect_err(name));
                    assert!(err.starts_with(prefix), "{name}: {err}");
                }
                None => {
                    let got = result.unwrap_or_else(|e| panic!("{name}: {e:#}"));
                    let want = &case["output"];
                    for key in ["facts", "assessment", "propertyDrafts", "drift", "triage", "diagnostics"] {
                        assert_eq!(
                            js_stringify(&got[key]),
                            js_stringify(&want[key]),
                            "{name}: поле {key} расходится с JS"
                        );
                    }
                    assert_eq!(js_stringify(&got), js_stringify(want), "{name}");
                }
            }
        }
    }

    #[test]
    fn git_fallbacks_and_default_generated_at() {
        let io = MemIo::new(&json!({}), &json!({}));
        let out = analyze_structure(&json!({}), &io).unwrap();
        let prov = &out["facts"]["provenance"];
        assert_eq!(out["facts"]["generatedAt"], json!(iso_from_epoch_ms(io.now_ms)));
        assert_eq!(out["facts"]["facts"]["entities"], json!([]));
        assert_eq!(prov["headSha"], json!("0000000"));
        assert_eq!(prov["branchRef"], json!("unknown"));
        assert_eq!(prov["graphStatus"], json!("missing"));
        assert_eq!(*io.git_calls.borrow(), vec!["rev-parse HEAD", "symbolic-ref --short HEAD"]);
    }

    #[test]
    fn git_output_is_trimmed_and_empty_falls_back() {
        let io = MemIo::new(&json!({}), &json!({ "rev-parse HEAD": "  \n", "symbolic-ref --short HEAD": "\tmain\u{FEFF}\n" }));
        let out = analyze_structure(&json!({ "generatedAt": "2026-09-02T00:00:00.000Z" }), &io).unwrap();
        assert_eq!(out["facts"]["provenance"]["headSha"], json!("0000000"));
        assert_eq!(out["facts"]["provenance"]["branchRef"], json!("main"));
    }

    #[test]
    fn graph_status_matches_js_regex() {
        assert_eq!(graph_status(None, "abc"), "unknown");
        assert_eq!(graph_status(Some("no marker"), "abcdef1234"), "unknown");
        assert_eq!(graph_status(Some("Built from commit: `abcdef1`"), "abcdef1234"), "fresh");
        assert_eq!(graph_status(Some("built FROM commit:\u{FEFF}ABCDEF1234"), "abcdef1234"), "stale");
        assert_eq!(graph_status(Some("Built from commit: abcdef1234567"), "abcdef1234"), "fresh");
        assert_eq!(graph_status(Some("Built from commit: 1234567"), "abcdef1234"), "stale");
        // короче 7 — не sha
        assert_eq!(graph_status(Some("Built from commit: abc"), "abc"), "unknown");
    }

    #[test]
    fn scope_escape_is_rejected() {
        let io = MemIo::new(&json!({}), &json!({}));
        for scope in ["../outside", "/etc", "a/../../b"] {
            assert!(analyze_structure(&json!({ "scope": scope, "headSha": "abc", "branchRef": "m" }), &io).is_err(), "{scope}");
        }
    }

    #[test]
    fn scip_failure_is_a_category_warning_without_paths() {
        let io = MemIo::new(&json!({ "/repo/index.scip": "\u{FF}\u{FF}garbage" }), &json!({}));
        let out = analyze_structure(
            &json!({ "scipPath": "/secret/missing.scip", "repoRoot": "/repo", "headSha": "abcdef1", "branchRef": "m", "generatedAt": "2026-09-02T00:00:00.000Z" }),
            &io,
        )
        .unwrap();
        let warnings = out["diagnostics"]["warnings"].as_array().unwrap();
        assert!(warnings.contains(&json!("SCIP unavailable: index could not be decoded")));
        assert!(!js_stringify(&out).contains("/secret/"), "путь SCIP не должен утечь");
        assert_eq!(out["diagnostics"]["scipUsed"], json!(false));
    }

    /// Пути тест-файлов из графа — вход автора PR: `..`/абсолютные отсекает
    /// normalize_repo_path, symlink наружу — боевой read_repo_file.
    #[test]
    fn test_sources_do_not_escape_repo_root() {
        let base = tempfile::tempdir().unwrap();
        let repo = base.path().join("repo");
        std::fs::create_dir_all(repo.join("test")).unwrap();
        let outside = base.path().join("secret.test.ts");
        std::fs::write(&outside, "test('leaked secret name', () => { assert.ok(1); });").unwrap();
        std::fs::write(repo.join("test/ok.test.ts"), "test('inside case', () => { assert.ok(1); });").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, repo.join("test/link.test.ts")).unwrap();
        let graph = json!({
            "nodes": [
                { "id": "a", "label": "a()", "file_type": "code", "source_file": "test/link.test.ts", "source_location": "L1", "_callable": true },
                { "id": "b", "label": "b()", "file_type": "code", "source_file": "test/ok.test.ts", "source_location": "L1", "_callable": true },
                { "id": "c", "label": "c()", "file_type": "code", "source_file": "../secret.test.ts", "source_location": "L1", "_callable": true },
            ],
            "links": [],
        });
        let out = analyze_structure(
            &json!({
                "graphText": js_stringify(&graph),
                "repoRoot": repo.to_string_lossy(),
                "headSha": "abcdef1",
                "branchRef": "main",
                "generatedAt": "2026-09-02T00:00:00.000Z",
            }),
            &RealStructureIo,
        )
        .unwrap();
        let text = js_stringify(&out["facts"]["facts"]["testCases"]);
        assert!(text.contains("inside case"), "{text}");
        assert!(!text.contains("leaked secret name"), "{text}");
    }

    #[test]
    fn real_git_outside_repo_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_git(dir.path(), &["rev-parse", "HEAD"]), None);
    }

    /// Граница RFC 01: structure-контур не тянет гейт, гейт не знает structure.
    #[test]
    fn boundary_structure_does_not_import_gate() {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        // Склейка, а не литерал: иначе этот файл нашёл бы запрет в самом тесте.
        let forbidden: Vec<String> =
            ["pipeline", "engine", "github", "llm"].iter().map(|m| format!("crate::{m}")).collect();
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.starts_with("structure") || !name.ends_with(".rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for f in &forbidden {
                assert!(!text.contains(f.as_str()), "{name} импортирует {f}");
            }
        }
        let pipeline = std::fs::read_to_string(src.join("pipeline.rs")).unwrap_or_default();
        assert!(!pipeline.contains(&format!("crate::{}", "structure")), "pipeline.rs знает про structure");
    }
}
