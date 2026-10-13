//! Фидбэк ревью (R8): порт `lib/feedback-schema.mjs` + `lib/feedback.mjs` (задача T17 RFC 20).
//!
//! Две части в одном модуле, как и просит RFC:
//!
//! - **схема v1** журнала `fp-feedback.jsonl` — [`normalize_feedback_entry`],
//!   [`migrate_feedback_text`]: принимают легаси-строки без `schemaVersion`,
//!   отвергают будущие версии вместо молчаливой перезаписи;
//! - **harvest** — [`harvest_feedback`]: метки PR + реакции 👍/👎 на наших инлайнах +
//!   находки из футера sticky → JSONL-журнал, кандидаты в eval-fixtures, markdown-журнал.
//!   Сеть — только через трейт [`GithubApi`] (`traits.rs`), поэтому тест гоняет весь
//!   harvest на [`crate::testutil::FakeGithub`].
//!
//! Записи журнала остаются `serde_json::Value`, а не структурой: в лог уходит ровно
//! та форма ключей, что собирает JS (порядок ключей, отсутствующие поля, сырые
//! находки sticky под `finding`), и потребитель JSONL (`feedback-promote`) читает её
//! тем же кодом, что и раньше. Сериализация — [`crate::report::js_json_stringify`]:
//! числа печатаются как `JSON.stringify`, а не как исходная лексема
//! `arbitrary_precision`.
//!
//! Расхождения с JS (все — на входах, которых живой GitHub не присылает):
//!
//! - `{...f}` для находки sticky, которая не объект: JS разворачивает строку/массив
//!   по индексам, здесь получается `{}`;
//! - `feedback.label`, отсутствующий в конфиге, считается [`DEFAULT_FP_LABEL`] в обеих
//!   проверках harvest (`loadReviewConfig` заполняет его всегда);
//! - сообщение об ошибке разбора строки журнала — от `serde_json`, не от V8.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::constants::has_marker;
use crate::findings::{js_to_number, js_to_string, js_trim, js_truthy};
use crate::json_repair::extract_json_object;
use crate::observability::iso_from_epoch_ms;
use crate::profile::Markers;
use crate::report::{js_json_stringify, js_slice_utf16};
use crate::traits::GithubApi;
use crate::util::sha256_str;

// ---------------------------------------------------------------------------
// Схема v1 (lib/feedback-schema.mjs)
// ---------------------------------------------------------------------------

/// Версия схемы строки `fp-feedback.jsonl` (`FEEDBACK_SCHEMA_VERSION`).
pub const FEEDBACK_SCHEMA_VERSION: i64 = 1;

const SOURCES: [&str; 2] = ["label", "reaction"];
const SIGNALS: [&str; 2] = ["fp", "tp"];
const SEVERITIES: [&str; 4] = ["critical", "major", "minor", "nit"];
const LEGACY_TS: &str = "1970-01-01T00:00:00.000Z";
const LEGACY_REPO: &str = "unknown/unknown";

/// `optionalString`: непустая строка или `None`.
fn optional_string(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// JS-число → `Value` так, как его напечатает `JSON.stringify`: целое — целым,
/// не конечное — `null`, `-0` — `0`.
fn num_value(x: f64) -> Value {
    let x = x + 0.0;
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
        Value::from(x as i64)
    } else {
        serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// `typeof v === 'object' && v` — объект или массив.
fn is_js_object(v: &Value) -> bool {
    matches!(v, Value::Object(_) | Value::Array(_))
}

fn normalize_finding(raw: Option<&Value>) -> Option<Value> {
    let raw = raw.filter(|v| is_js_object(v))?;
    let get = |k: &str| raw.get(k);
    let mut out = Map::new();
    out.insert(
        "file".into(),
        Value::String(get("file").and_then(Value::as_str).unwrap_or("").to_string()),
    );
    // Math.max(0, Number(raw.line) || 0): undefined/NaN → 0.
    let line = get("line").and_then(js_to_number).filter(|x| !x.is_nan()).unwrap_or(0.0);
    out.insert("line".into(), num_value(line.max(0.0)));
    let severity = get("severity")
        .and_then(Value::as_str)
        .filter(|s| SEVERITIES.contains(s))
        .unwrap_or("minor");
    out.insert("severity".into(), Value::String(severity.into()));
    let title = optional_string(get("title")).unwrap_or("замечание");
    out.insert("title".into(), Value::String(title.into()));
    for key in ["model", "body"] {
        if let Some(s) = optional_string(get(key)) {
            out.insert(key.into(), Value::String(s.into()));
        }
    }
    Some(Value::Object(out))
}

/// `normalizeFeedbackEntry`: легаси-строку без `schemaVersion` дополняет до v1,
/// будущую версию и битую запись отвергает (`None`).
pub fn normalize_feedback_entry(raw: &Value) -> Option<Value> {
    let obj = raw.as_object()?;
    let version = match obj.get("schemaVersion") {
        None | Some(Value::Null) => Some(FEEDBACK_SCHEMA_VERSION as f64),
        Some(v) => js_to_number(v),
    };
    if version != Some(FEEDBACK_SCHEMA_VERSION as f64) {
        return None;
    }
    // Number(undefined) — NaN, а не 0: отсутствующий pr отвергается.
    let pr = obj.get("pr").and_then(js_to_number)?;
    let source = obj.get("source").and_then(Value::as_str).filter(|s| SOURCES.contains(s))?;
    let signal = obj.get("signal").and_then(Value::as_str).filter(|s| SIGNALS.contains(s))?;
    let finding = normalize_finding(obj.get("finding"))?;
    if !(pr.is_finite() && pr.fract() == 0.0 && pr > 0.0) {
        return None;
    }

    let mut out = Map::new();
    out.insert("schemaVersion".into(), Value::from(FEEDBACK_SCHEMA_VERSION));
    out.insert("ts".into(), Value::String(optional_string(obj.get("ts")).unwrap_or(LEGACY_TS).into()));
    out.insert(
        "repo".into(),
        Value::String(optional_string(obj.get("repo")).unwrap_or(LEGACY_REPO).into()),
    );
    out.insert("pr".into(), num_value(pr));
    out.insert("source".into(), Value::String(source.into()));
    out.insert("signal".into(), Value::String(signal.into()));
    out.insert("finding".into(), finding);
    for key in ["label", "reaction", "user"] {
        if let Some(s) = optional_string(obj.get(key)) {
            out.insert(key.into(), Value::String(s.into()));
        }
    }
    if let Some(n) = obj.get("commentId").filter(|v| !v.is_null()).and_then(js_to_number) {
        if n.is_finite() {
            out.insert("commentId".into(), num_value(n));
        }
    }
    Some(Value::Object(out))
}

/// `JSON.stringify(v)` с семантикой V8.
pub fn js_stringify(v: &Value) -> String {
    let mut s = String::new();
    js_json_stringify(v, &mut s);
    s
}

/// Счётчики миграции журнала.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MigrateStats {
    pub total: usize,
    pub valid: usize,
    pub migrated: usize,
    pub invalid: usize,
}

/// Результат [`migrate_feedback_text`].
#[derive(Debug, Clone, Serialize)]
pub struct MigrateResult {
    pub entries: Vec<Value>,
    pub stats: MigrateStats,
    /// Канонический JSONL v1; пустая строка, если валидных строк нет.
    pub text: String,
}

/// `migrateFeedbackText`: произвольный JSONL → канонический JSONL schema v1.
pub fn migrate_feedback_text(text: &str) -> MigrateResult {
    let mut rows = Vec::new();
    let mut stats = MigrateStats::default();
    for line in text.split('\n') {
        if js_trim(line).is_empty() {
            continue;
        }
        stats.total += 1;
        let Ok(raw) = serde_json::from_str::<Value>(line) else {
            stats.invalid += 1;
            continue;
        };
        let Some(normalized) = normalize_feedback_entry(&raw) else {
            stats.invalid += 1;
            continue;
        };
        stats.valid += 1;
        if js_stringify(&raw) != js_stringify(&normalized) {
            stats.migrated += 1;
        }
        rows.push(normalized);
    }
    let text = if rows.is_empty() {
        String::new()
    } else {
        let mut t = rows.iter().map(js_stringify).collect::<Vec<_>>().join("\n");
        t.push('\n');
        t
    };
    MigrateResult { entries: rows, stats, text }
}

// ---------------------------------------------------------------------------
// JS-коэрсии для шаблонных строк
// ---------------------------------------------------------------------------

/// `${v}` в шаблонной строке JS: отсутствующее поле — `undefined`.
fn tpl(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|el| if el.is_null() { String::new() } else { tpl(Some(el)) })
            .collect::<Vec<_>>()
            .join(","),
        Some(v) => js_to_string(v),
    }
}

/// `v || ''` в шаблоне.
fn or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if js_truthy(x) => tpl(Some(x)),
        _ => String::new(),
    }
}

/// `v || fallback` в шаблоне.
fn or_str(v: Option<&Value>, fallback: &str) -> String {
    match v {
        Some(x) if js_truthy(x) => tpl(Some(x)),
        _ => fallback.to_string(),
    }
}

/// `v || fallback` как значение.
fn or_value(v: Option<&Value>, fallback: Value) -> Value {
    match v {
        Some(x) if js_truthy(x) => x.clone(),
        _ => fallback,
    }
}

/// `entry.finding?.<key>`.
fn finding_field<'a>(entry: &'a Value, key: &str) -> Option<&'a Value> {
    entry.get("finding").and_then(|f| f.get(key))
}

// ---------------------------------------------------------------------------
// lib/feedback.mjs
// ---------------------------------------------------------------------------

/// Метка PR «ревью ошиблось» по умолчанию.
pub const DEFAULT_FP_LABEL: &str = "ai-review-fp";
/// Куда движок кладёт кандидатов в fixtures, если проект не переопределил путь.
pub const DEFAULT_CANDIDATES_DIR: &str = ".ai-review/feedback/candidates";
/// Доверенные авторы наших комментариев по умолчанию.
pub const DEFAULT_TRUSTED_AUTHORS: &str = "github-actions[bot]";
const UNKNOWN_LINE: i64 = 0;

/// Класс `\s` JS (без unicode-флага) для regex-движка: Unicode White_Space crate
/// `regex` отличается от него на U+FEFF.
const JS_WS: &str = r"[\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";

/// `FINDING_RE` из JS. Старый формат включал модель в заголовок; новый оставляет
/// только severity+title. Жадный `(.+)` до последнего `):` сохраняет совместимость
/// с моделями в скобках: leftmost-first семантика crate `regex` даёт тот же захват,
/// что backtracking V8. JS `.` не берёт `\n\r  ` — класс ниже это повторяет.
fn finding_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let src = format!(
            r"\*\*(critical|major|minor|nit)(?:{ws}*\(([^\n\r\x{{2028}}\x{{2029}}]+)\))?:{ws}*([^*]+)\*\*",
            ws = JS_WS
        );
        Regex::new(&src).expect("FINDING_RE компилируется")
    })
}

/// `findingKey`: `file|line|severity|title`.
pub fn finding_key(f: &Value) -> String {
    format!(
        "{}|{}|{}|{}",
        or_empty(f.get("file")),
        or_str(f.get("line"), &UNKNOWN_LINE.to_string()),
        or_empty(f.get("severity")),
        or_empty(f.get("title")),
    )
}

/// `feedbackDedupeKey`: у реакций в ключ входит пользователь — два человека,
/// поставившие 👎 одному инлайну, это два сигнала, а не дубль.
pub fn feedback_dedupe_key(entry: &Value) -> String {
    let source = entry.get("source");
    let user_part = if source.and_then(Value::as_str) == Some("reaction") {
        or_empty(entry.get("user"))
    } else {
        String::new()
    };
    let empty = Value::Object(Map::new());
    let finding = entry.get("finding").filter(|f| js_truthy(f)).unwrap_or(&empty);
    format!(
        "{}#{}:{}:{}:{}:{}:{}",
        tpl(entry.get("repo")),
        tpl(entry.get("pr")),
        tpl(source),
        tpl(entry.get("signal")),
        finding_key(finding),
        or_empty(entry.get("commentId")),
        user_part,
    )
}

/// `parseTrustedAuthors`: CSV → список без пустых.
pub fn parse_trusted_authors(raw: &str) -> Vec<String> {
    raw.split(',').map(|s| js_trim(s).to_string()).filter(|s| !s.is_empty()).collect()
}

/// Доверенный автор inline/sticky — только exact allowlist (без учёта регистра) +
/// GitHub `user.type == "Bot"`. Пустой allowlist — доверяем всем.
pub fn is_trusted_review_author(login: Option<&str>, trusted: &[String], user: Option<&Value>) -> bool {
    if trusted.is_empty() {
        return true;
    }
    let Some(login) = login.filter(|l| !l.is_empty()) else {
        return false;
    };
    let l = login.to_lowercase();
    if !trusted.iter().any(|a| a.to_lowercase() == l) {
        return false;
    }
    match user.and_then(|u| u.get("type")) {
        Some(t) if js_truthy(t) && t.as_str() != Some("Bot") => false,
        _ => true,
    }
}

/// `c.user?.login` как строка.
fn user_login(v: &Value) -> Option<&str> {
    v.get("user").and_then(|u| u.get("login")).and_then(Value::as_str)
}

/// `hasMarker(body, marker)`: только строка, маркер в начале.
fn body_has_marker(v: &Value, marker: &str) -> bool {
    v.get("body").and_then(Value::as_str).is_some_and(|b| has_marker(b, marker))
}

/// Все находки из сгруппированного инлайна (`parseReviewCommentFindings`).
pub fn parse_review_comment_findings(comment: &Value, markers: &Markers) -> Vec<Value> {
    if !body_has_marker(comment, &markers.inline) {
        return Vec::new();
    }
    let body = comment.get("body").and_then(Value::as_str).unwrap_or("");
    let file = or_value(comment.get("path"), Value::String(String::new()));
    // Number(comment.line || comment.original_line) || 0
    let line_src = match comment.get("line") {
        Some(v) if js_truthy(v) => Some(v),
        _ => comment.get("original_line"),
    };
    let line = line_src.and_then(js_to_number).filter(|x| *x != 0.0 && !x.is_nan()).unwrap_or(0.0);
    let line = num_value(line);
    let mut out = Vec::new();
    for m in finding_re().captures_iter(body) {
        let mut f = Map::new();
        f.insert("file".into(), file.clone());
        f.insert("line".into(), line.clone());
        f.insert("severity".into(), Value::String(m[1].to_string()));
        f.insert("title".into(), Value::String(js_trim(&m[3]).to_string()));
        let model = m.get(2).map(|x| js_trim(x.as_str())).unwrap_or("");
        f.insert("model".into(), Value::String(model.to_string()));
        out.push(Value::Object(f));
    }
    out
}

/// Первая находка инлайна (обратная совместимость, `parseReviewCommentFinding`).
pub fn parse_review_comment_finding(comment: &Value, markers: &Markers) -> Option<Value> {
    parse_review_comment_findings(comment, markers).into_iter().next()
}

/// Находки из машинного футера sticky (`<ns>-data:`), `parseStickyFindings`.
///
/// Как в JS, берётся ПЕРВЫЙ футер (`MARKERS.dataRe.exec`), а не последний, как в
/// carry-over (`constants::extract_sticky_footer_raw`): harvest читает только наш
/// sticky от доверенного автора, и порт сохраняет эталонное поведение.
pub fn parse_sticky_findings(body: &str, markers: &Markers) -> Vec<Value> {
    if body.is_empty() {
        return Vec::new();
    }
    let Some(raw) = markers.capture_data(body) else {
        return Vec::new();
    };
    let findings_of = |data: &Value| match data.get("findings") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    // JSON.parse(...) и `data.findings`: у null обращение к полю бросает, и JS
    // уходит в ремонт; у остальных значений поля просто нет.
    match serde_json::from_str::<Value>(js_trim(&raw)) {
        Ok(data) if !data.is_null() => findings_of(&data),
        // Футер обрезан лимитом GitHub — ремонт JSON (как в readPriorFindings).
        _ => match extract_json_object(&raw) {
            Ok(outcome) => findings_of(&outcome.value),
            Err(_) => Vec::new(),
        },
    }
}

fn reaction_signal(content: Option<&str>) -> Option<&'static str> {
    match content {
        Some("+1") => Some("tp"),
        Some("-1") => Some("fp"),
        _ => None,
    }
}

/// Вход [`collect_feedback_entries`] — объект `input` из JS.
pub struct CollectInput<'a> {
    pub repo: &'a str,
    pub pr: &'a Value,
    /// Метки PR: строки или объекты `{name}`.
    pub labels: &'a [Value],
    pub inline_comments: &'a [Value],
    pub sticky_findings: &'a [Value],
    /// Реакции по `String(comment.id)`.
    pub reactions_by_comment_id: &'a HashMap<String, Vec<Value>>,
    pub fp_label: &'a str,
    pub trusted_authors: &'a [String],
    pub markers: &'a Markers,
    /// Метка времени записей (`new Date().toISOString()` в JS).
    pub ts: &'a str,
}

/// `collectFeedbackEntries`: метка FP × находки sticky + реакции × находки инлайна.
pub fn collect_feedback_entries(input: &CollectInput) -> Vec<Value> {
    let mut entries = Vec::new();
    let base = |source: &str, signal: &str| {
        let mut e = Map::new();
        e.insert("schemaVersion".into(), Value::from(FEEDBACK_SCHEMA_VERSION));
        e.insert("ts".into(), Value::String(input.ts.into()));
        e.insert("repo".into(), Value::String(input.repo.into()));
        e.insert("pr".into(), input.pr.clone());
        e.insert("source".into(), Value::String(source.into()));
        e.insert("signal".into(), Value::String(signal.into()));
        e
    };
    let has_fp_label = input.labels.iter().any(|l| {
        let name = if l.is_string() { Some(l) } else { l.get("name") };
        name.and_then(Value::as_str) == Some(input.fp_label)
    });

    if has_fp_label {
        for f in input.sticky_findings {
            let mut e = base("label", "fp");
            e.insert("label".into(), Value::String(input.fp_label.into()));
            let spread = match f {
                Value::Object(m) => Value::Object(m.clone()),
                _ => Value::Object(Map::new()),
            };
            e.insert("finding".into(), spread);
            entries.push(Value::Object(e));
        }
    }

    for c in input.inline_comments {
        if !body_has_marker(c, &input.markers.inline) {
            continue;
        }
        if !is_trusted_review_author(user_login(c), input.trusted_authors, c.get("user")) {
            continue;
        }
        let findings = parse_review_comment_findings(c, input.markers);
        if findings.is_empty() {
            continue;
        }
        let key = tpl(c.get("id"));
        let Some(reactions) = input.reactions_by_comment_id.get(&key) else {
            continue;
        };
        for r in reactions {
            let content = r.get("content").and_then(Value::as_str);
            let Some(signal) = reaction_signal(content) else {
                continue;
            };
            for finding in &findings {
                let mut e = base("reaction", signal);
                e.insert("reaction".into(), Value::String(content.unwrap_or("").into()));
                if let Some(id) = c.get("id") {
                    e.insert("commentId".into(), id.clone());
                }
                e.insert("finding".into(), finding.clone());
                // user: r.user?.login — undefined в JSON не попадает.
                if let Some(login) = r.get("user").and_then(|u| u.get("login")) {
                    e.insert("user".into(), login.clone());
                }
                entries.push(Value::Object(e));
            }
        }
    }
    entries
}

/// `dedupeFeedbackEntries`: существующие + новые без повторов по [`feedback_dedupe_key`].
pub fn dedupe_feedback_entries(existing: &[Value], incoming: &[Value]) -> Vec<Value> {
    let mut seen: std::collections::HashSet<String> = existing.iter().map(feedback_dedupe_key).collect();
    let mut out = existing.to_vec();
    for e in incoming {
        if seen.insert(feedback_dedupe_key(e)) {
            out.push(e.clone());
        }
    }
    out
}

/// `readFeedbackLog`: нормализованные строки журнала; битые пропускаются с записью
/// в лог, отсутствующий файл — пустой журнал.
pub fn read_feedback_log(log_path: &Path, log: &(dyn Fn(&str) + Sync)) -> Vec<Value> {
    let Ok(raw) = fs::read_to_string(log_path) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for (i, line) in js_trim(&raw).split('\n').filter(|l| !l.is_empty()).enumerate() {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => match normalize_feedback_entry(&v) {
                Some(n) => rows.push(n),
                None => log(&format!(
                    "feedback log skip invalid line {}: unsupported schema or fields",
                    i + 1
                )),
            },
            Err(e) => log(&format!("feedback log skip corrupt line {}: {e}", i + 1)),
        }
    }
    rows
}

/// Результат дозаписи журнала (JSONL или markdown).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AppendResult {
    pub appended: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Очередь дозаписей журнала: в JS — цепочка промисов `feedbackLogQueue`, здесь —
/// процессный мьютекс; строки двух параллельных harvest не перемешиваются.
static FEEDBACK_LOG_LOCK: Mutex<()> = Mutex::new(());

/// `appendFeedbackLog`: дописать нормализованные записи в JSONL. Невалидные
/// отбрасываются с записью в лог; ошибка записи — в `error`, не паника.
pub fn append_feedback_log(log_path: &Path, entries: &[Value], log: &(dyn Fn(&str) + Sync)) -> AppendResult {
    if entries.is_empty() {
        return AppendResult::default();
    }
    let normalized: Vec<Value> = entries.iter().filter_map(normalize_feedback_entry).collect();
    let skipped = entries.len() - normalized.len();
    if skipped > 0 {
        let noun = if skipped == 1 { "entry" } else { "entries" };
        log(&format!("feedback log skipped {skipped} invalid {noun}"));
    }
    if normalized.is_empty() {
        return AppendResult::default();
    }
    let _guard = FEEDBACK_LOG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let write = || -> std::io::Result<()> {
        if let Some(dir) = log_path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let lines: String = normalized.iter().map(|e| format!("{}\n", js_stringify(e))).collect();
        let mut f = OpenOptions::new().create(true).append(true).open(log_path)?;
        f.write_all(lines.as_bytes())
    };
    match write() {
        Ok(()) => AppendResult { appended: normalized.len(), error: None },
        Err(e) => {
            log(&format!("feedback log write failed: {e}"));
            AppendResult { appended: 0, error: Some(e.to_string()) }
        }
    }
}

/// `fixtureSlug`: ASCII-слаг заголовка + строка; для неASCII-заголовка — sha256.
fn fixture_slug(entry: &Value) -> String {
    let title = or_str(finding_field(entry, "title"), "finding");
    let line = or_str(finding_field(entry, "line"), &UNKNOWN_LINE.to_string());
    let lower = title.to_lowercase();
    let mut ascii = String::new();
    let mut in_run = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            ascii.push(c);
            in_run = false;
        } else if !in_run {
            ascii.push('-');
            in_run = true;
        }
    }
    // /(^-|-$)/g: прогоны уже схлопнуты, так что снимается максимум по одному с краёв.
    let ascii = ascii.strip_prefix('-').unwrap_or(&ascii);
    let ascii = ascii.strip_suffix('-').unwrap_or(ascii);
    let ascii: String = ascii.chars().take(24).collect();
    if ascii.len() >= 3 {
        return format!("{ascii}-L{line}");
    }
    let hash = sha256_str(&title);
    format!("h{}-L{line}", &hash[..8])
}

/// `buildFixtureCandidate`: черновик eval-fixture из FP-записи; `None` без файла.
pub fn build_fixture_candidate(entry: &Value) -> Option<Value> {
    let slug = fixture_slug(entry);
    // id становится именем файла (write_fixture_candidates) и каталога fixture
    // (promote, sanitizeFixtureId): поля находки не должны пронести `/`, `..`.
    // `replace(/[^A-Za-z0-9_-]/g, '-')` без флага u — по UTF-16 единицам.
    let id: String = js_slice_utf16(&format!("fp-pr-{}-{slug}", tpl(entry.get("pr"))), 64)
        .chars()
        .flat_map(|c| {
            let keep = c.is_ascii_alphanumeric() || c == '_' || c == '-';
            let (ch, n) = if keep { (c, 1) } else { ('-', c.len_utf16()) };
            std::iter::repeat_n(ch, n)
        })
        .collect();
    let file = finding_field(entry, "file").filter(|f| js_truthy(f))?.clone();

    let mut from = Map::new();
    for key in ["ts", "pr", "repo", "source", "signal", "reaction", "label", "commentId"] {
        if let Some(v) = entry.get(key) {
            from.insert(key.into(), v.clone());
        }
    }
    let title = or_str(finding_field(entry, "title"), "finding");
    let description = format!(
        "FP harvest PR #{}: {title} ({}/{})",
        tpl(entry.get("pr")),
        tpl(entry.get("source")),
        tpl(entry.get("signal")),
    );
    let mut finding = Map::new();
    finding.insert("file".into(), file);
    finding.insert("line".into(), or_value(finding_field(entry, "line"), Value::from(1)));
    finding.insert("severity".into(), or_value(finding_field(entry, "severity"), Value::from("major")));
    finding.insert("confidence".into(), serde_json::json!(0.95));
    finding.insert("title".into(), or_value(finding_field(entry, "title"), Value::from("замечание")));
    finding.insert(
        "body".into(),
        Value::from("auto-import from GitHub feedback (R8) — дополните diff.patch"),
    );
    finding.insert("model".into(), or_value(finding_field(entry, "model"), Value::from("harvest")));

    let mut out = Map::new();
    out.insert("suggestedId".into(), Value::String(id.clone()));
    out.insert("fromFeedback".into(), Value::Object(from));
    out.insert(
        "meta".into(),
        serde_json::json!({
            "id": id,
            "description": description,
            "expected": { "shouldBlock": false, "blockerCount": 0 },
        }),
    );
    out.insert("findings".into(), Value::Array(vec![Value::Object(finding)]));
    out.insert(
        "diffNote".into(),
        Value::String(format!(
            "Скопируйте unified diff из PR #{} в fixtures/{id}/diff.patch",
            tpl(entry.get("pr"))
        )),
    );
    Some(Value::Object(out))
}

/// `JSON.stringify(v, null, 2)`.
pub fn js_stringify_pretty(v: &Value) -> String {
    fn go(v: &Value, depth: usize, out: &mut String) {
        let pad = |d: usize, out: &mut String| out.push_str(&"  ".repeat(d));
        match v {
            Value::Array(a) if !a.is_empty() => {
                out.push_str("[\n");
                for (i, el) in a.iter().enumerate() {
                    pad(depth + 1, out);
                    go(el, depth + 1, out);
                    out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
                }
                pad(depth, out);
                out.push(']');
            }
            Value::Object(m) if !m.is_empty() => {
                out.push_str("{\n");
                for (i, (k, val)) in m.iter().enumerate() {
                    pad(depth + 1, out);
                    js_json_stringify(&Value::String(k.clone()), out);
                    out.push_str(": ");
                    go(val, depth + 1, out);
                    out.push_str(if i + 1 < m.len() { ",\n" } else { "\n" });
                }
                pad(depth, out);
                out.push('}');
            }
            other => js_json_stringify(other, out),
        }
    }
    let mut s = String::new();
    go(v, 0, &mut s);
    s
}

/// Результат [`write_fixture_candidates`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WriteCandidatesResult {
    pub written: usize,
    pub skipped: usize,
}

/// `writeFixtureCandidates`: по файлу на FP-запись; существующий файл не
/// перезаписывается (`flag: 'wx'`) и считается пропущенным.
pub fn write_fixture_candidates(
    dir: &Path,
    entries: &[Value],
    log: &(dyn Fn(&str) + Sync),
) -> Result<WriteCandidatesResult> {
    let fp: Vec<&Value> = entries
        .iter()
        .filter(|e| e.get("signal").and_then(Value::as_str) == Some("fp"))
        .collect();
    let mut r = WriteCandidatesResult::default();
    if fp.is_empty() {
        return Ok(r);
    }
    fs::create_dir_all(dir)?;
    for entry in fp {
        let Some(candidate) = build_fixture_candidate(entry) else {
            r.skipped += 1;
            continue;
        };
        let id = candidate["suggestedId"].as_str().unwrap_or_default().to_string();
        let path = dir.join(format!("{id}.json"));
        let res = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut f| f.write_all(format!("{}\n", js_stringify_pretty(&candidate)).as_bytes()));
        match res {
            Ok(()) => r.written += 1,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => r.skipped += 1,
            Err(e) => log(&format!("candidate write skip {id}: {e}")),
        }
    }
    Ok(r)
}

/// `formatFeedbackMarkdown`: блок FP-журнала по одной записи.
pub fn format_feedback_markdown(entry: &Value, candidates_dir: &str) -> String {
    let candidate = build_fixture_candidate(entry);
    let file = finding_field(entry, "file");
    let loc = match file {
        Some(f) if js_truthy(f) => {
            let line = match finding_field(entry, "line") {
                Some(l) if js_truthy(l) => format!(":{}", tpl(Some(l))),
                _ => String::new(),
            };
            format!("`{}{line}`", tpl(Some(f)))
        }
        _ => "(no location)".to_string(),
    };
    let src = if entry.get("source").and_then(Value::as_str) == Some("reaction") {
        let emoji = if entry.get("reaction").and_then(Value::as_str) == Some("-1") { "👎" } else { "👍" };
        format!("reaction {emoji} на inline #{}", tpl(entry.get("commentId")))
    } else {
        format!("label `{}`", tpl(entry.get("label")))
    };
    let signal = tpl(entry.get("signal"));
    let cand_line = match &candidate {
        Some(c) => format!(
            "- Signal: **{signal}** · candidate: `{candidates_dir}/{}.json`\n",
            c["suggestedId"].as_str().unwrap_or_default()
        ),
        None => format!("- Signal: **{signal}** (no fixture — missing file path)\n"),
    };
    let ts = js_slice_utf16(&tpl(entry.get("ts")), 10);
    let user = match entry.get("user") {
        Some(u) if js_truthy(u) => format!(" (@{})", tpl(Some(u))),
        _ => String::new(),
    };
    format!(
        "### {ts} · PR #{} (R8 auto)\n- Источник: {src}{user}\n- Finding: [{}] {loc} «{}»\n{cand_line}",
        tpl(entry.get("pr")),
        tpl(finding_field(entry, "severity")),
        or_empty(finding_field(entry, "title")),
    )
}

/// `appendFeedbackMarkdown`: FP-записи — в конец markdown-журнала (создаётся с
/// заголовком, если файла нет).
pub fn append_feedback_markdown(
    md_path: &Path,
    entries: &[Value],
    log: &(dyn Fn(&str) + Sync),
    candidates_dir: &str,
) -> AppendResult {
    let fp: Vec<&Value> = entries
        .iter()
        .filter(|e| e.get("signal").and_then(Value::as_str) == Some("fp"))
        .collect();
    if fp.is_empty() {
        return AppendResult::default();
    }
    let run = || -> std::io::Result<()> {
        let md = match fs::read_to_string(md_path) {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::NotFound => "# AI-review FP journal (R8 auto-init)\n\n".to_string(),
            Err(e) => return Err(e),
        };
        let block: Vec<String> = fp.iter().map(|e| format_feedback_markdown(e, candidates_dir)).collect();
        let md = format!(
            "{}\n{}\n",
            md.trim_end_matches(crate::findings::js_is_whitespace),
            block.join("\n")
        );
        fs::write(md_path, md)
    };
    match run() {
        Ok(()) => AppendResult { appended: fp.len(), error: None },
        Err(e) => {
            log(&format!("feedback markdown append failed: {e}"));
            AppendResult { appended: 0, error: Some(e.to_string()) }
        }
    }
}

/// Итог harvest — объект, который возвращает `harvestFeedback`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarvestResult {
    pub pr: Value,
    pub repo: String,
    pub new_entries: Vec<Value>,
    pub persisted_entries: usize,
    pub total_in_log: usize,
    pub candidates_written: usize,
    pub warnings: Vec<String>,
    pub write_errors: Vec<String>,
    pub dry_run: bool,
    pub ok: bool,
}

/// `formatHarvestReport`: текстовый отчёт CLI.
pub fn format_harvest_report(result: &HarvestResult) -> String {
    let mut lines = vec![
        "=== pr-review feedback harvest (R8) ===".to_string(),
        format!("PR #{} · {}", tpl(Some(&result.pr)), result.repo),
        format!(
            "Новых записей: {} (persisted: {}, всего в логе: {})",
            result.new_entries.len(),
            result.persisted_entries,
            result.total_in_log
        ),
        format!("Fixture candidates: {}", result.candidates_written),
    ];
    for e in &result.new_entries {
        lines.push(format!(
            "- [{}] {} · {}:{} «{}»",
            tpl(e.get("signal")),
            tpl(e.get("source")),
            tpl(finding_field(e, "file")),
            tpl(finding_field(e, "line")),
            tpl(finding_field(e, "title")),
        ));
    }
    if result.new_entries.is_empty() {
        lines.push("- (нет нового feedback)".into());
    }
    if !result.warnings.is_empty() {
        lines.push(String::new());
        lines.push("Warnings:".into());
        lines.extend(result.warnings.iter().map(|w| format!("- {w}")));
    }
    if !result.write_errors.is_empty() {
        lines.push(String::new());
        lines.push("Write errors:".into());
        lines.extend(result.write_errors.iter().map(|e| format!("- {e}")));
    }
    lines.join("\n")
}

/// `fetchReactionsPool`: реакции на инлайны пулом из `concurrency` потоков.
/// Отказ по одному комменту — запись в лог, а не провал harvest.
fn fetch_reactions_pool(
    comments: &[Value],
    github: &dyn GithubApi,
    concurrency: Option<f64>,
    log: &(dyn Fn(&str) + Sync),
) -> HashMap<String, Vec<Value>> {
    let out = Mutex::new(HashMap::new());
    // Math.max(1, concurrency || 6); NaN (нечисловая строка) → ноль воркеров, как
    // `Array.from({length: NaN})` в JS.
    let pool = match concurrency {
        Some(c) if c.is_nan() => f64::NAN,
        Some(c) => c.max(1.0),
        None => 6.0,
    };
    let workers = if pool.is_nan() { 0 } else { pool.min(comments.len() as f64) as usize };
    let idx = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = idx.fetch_add(1, Ordering::SeqCst);
                let Some(c) = comments.get(i) else { break };
                let id_str = tpl(c.get("id"));
                let res = match c.get("id").and_then(Value::as_u64) {
                    Some(id) => github.list_pull_comment_reactions(id),
                    None => Err(anyhow::anyhow!("id комментария не целое число")),
                };
                match res {
                    Ok(r) => {
                        out.lock().unwrap_or_else(|p| p.into_inner()).insert(id_str, r);
                    }
                    Err(e) => log(&format!("reactions for comment {id_str} skip: {e}")),
                }
            });
        }
    });
    out.into_inner().unwrap_or_else(|p| p.into_inner())
}

fn now_iso() -> String {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    iso_from_epoch_ms(ms)
}

/// `harvestFeedback`: метки + инлайн 👎/👍 + находки sticky → журнал и кандидаты.
///
/// `cfg` — конфиг из `loadReviewConfig` (`config.rs`), читаются `github.repo`,
/// `github.prNumber` и секция `feedback`. Относительные пути секции разрешаются от
/// `cwd` (в JS — `process.cwd()`). Сбой GitHub-чтения меток/инлайнов/sticky —
/// `Err` (в JS `Promise.all` отклоняется); сбой записи — в `write_errors`.
pub fn harvest_feedback(
    cfg: &Value,
    markers: &Markers,
    github: &dyn GithubApi,
    cwd: &Path,
    log: &(dyn Fn(&str) + Sync),
) -> Result<HarvestResult> {
    let null = Value::Null;
    let feedback = cfg.get("feedback").unwrap_or(&null);
    let gh = cfg.get("github").unwrap_or(&null);
    let repo = gh.get("repo").filter(|v| js_truthy(v));
    let pr = gh.get("prNumber").filter(|v| js_truthy(v));
    let (Some(repo), Some(pr)) = (repo, pr) else {
        bail!("harvest требует REPO и PR_NUMBER");
    };
    let repo = tpl(Some(repo));
    let fb = |k: &str| feedback.get(k);
    let fb_str = |k: &str| or_empty(fb(k));
    let fb_on = |k: &str| fb(k).is_some_and(js_truthy);

    let trusted = match fb("trustedAuthors") {
        None => parse_trusted_authors(DEFAULT_TRUSTED_AUTHORS),
        Some(v) => parse_trusted_authors(&tpl(Some(v))),
    };
    let fp_label = fb("label").and_then(Value::as_str).unwrap_or(DEFAULT_FP_LABEL).to_string();

    let labels = github.list_issue_labels()?;
    let inline_comments = github.list_inline_comments()?;
    let sticky = github.read_sticky_review()?;

    let mut warnings = Vec::new();
    let mut sticky_findings = Vec::new();
    if let Some(sticky) = sticky.as_ref() {
        if let Some(body) = sticky.get("body").filter(|b| js_truthy(b)) {
            if is_trusted_review_author(user_login(sticky), &trusted, sticky.get("user")) {
                sticky_findings = parse_sticky_findings(body.as_str().unwrap_or(""), markers);
            } else {
                let who = or_str(sticky.get("user").and_then(|u| u.get("login")), "?");
                warnings.push(format!("sticky comment by untrusted author {who} — skipped for label harvest"));
            }
        }
    }

    let has_fp_label = labels.iter().any(|l| l.get("name").and_then(Value::as_str) == Some(fp_label.as_str()));
    if has_fp_label && sticky_findings.is_empty() {
        warnings.push(format!("label {fp_label} present but no trusted sticky findings — label harvest skipped"));
    }

    let trusted_inline: Vec<Value> = inline_comments
        .into_iter()
        .filter(|c| {
            body_has_marker(c, &markers.inline) && is_trusted_review_author(user_login(c), &trusted, c.get("user"))
        })
        .collect();
    let concurrency = match fb("reactionConcurrency") {
        Some(v) if js_truthy(v) => Some(js_to_number(v).unwrap_or(f64::NAN)),
        _ => None,
    };
    let reactions = fetch_reactions_pool(&trusted_inline, github, concurrency, log);

    let ts = now_iso();
    let incoming = collect_feedback_entries(&CollectInput {
        repo: &repo,
        pr,
        labels: &labels,
        inline_comments: &trusted_inline,
        sticky_findings: &sticky_findings,
        reactions_by_comment_id: &reactions,
        fp_label: &fp_label,
        trusted_authors: &trusted,
        markers,
        ts: &ts,
    });

    let log_path = crate::util::js_resolve(cwd, &fb_str("logFile"));
    let existing = read_feedback_log(&log_path, log);
    let merged = dedupe_feedback_entries(&existing, &incoming);
    let new_entries: Vec<Value> = merged[existing.len()..].to_vec();

    let write = fb_on("write");
    let mut candidates_written = 0;
    let mut persisted = 0;
    let mut write_errors = Vec::new();

    if write && !new_entries.is_empty() {
        let r = append_feedback_log(&log_path, &new_entries, log);
        if let Some(err) = r.error {
            write_errors.push(format!("JSONL: {err}"));
        } else if r.appended != new_entries.len() {
            write_errors.push(format!("JSONL: appended {}/{}", r.appended, new_entries.len()));
        } else {
            persisted = r.appended;
        }

        if fb_on("writeCandidates") && write_errors.is_empty() {
            let r = write_fixture_candidates(&crate::util::js_resolve(cwd, &fb_str("candidatesDir")), &new_entries, log)?;
            candidates_written = r.written;
        }
        if fb_on("appendMarkdown") && write_errors.is_empty() {
            let r = append_feedback_markdown(
                &crate::util::js_resolve(cwd, &fb_str("mdPath")),
                &new_entries,
                log,
                &tpl(fb("candidatesDir")),
            );
            if let Some(err) = r.error {
                write_errors.push(format!("markdown: {err}"));
            }
        }
    }

    Ok(HarvestResult {
        pr: pr.clone(),
        repo,
        new_entries,
        persisted_entries: if write { persisted } else { 0 },
        total_in_log: merged.len(),
        candidates_written,
        warnings,
        dry_run: !write,
        ok: write_errors.is_empty(),
        write_errors,
    })
}

#[cfg(test)]
mod tests {
    //! Порт R8-тестов из `test/pr-review.eval.test.mjs` + эталонные строки, снятые
    //! с JS (`lib/feedback.mjs`, `lib/feedback-schema.mjs`) на тех же входах.
    use super::*;
    use crate::testutil::{CallRecord, FakeGithub};
    use serde_json::json;
    use std::sync::Arc;

    fn markers() -> Markers {
        Markers::default()
    }

    fn orakul() -> Markers {
        Markers::new("orakul-ai-review")
    }

    fn nolog() -> impl Fn(&str) + Sync {
        |_: &str| {}
    }

    fn inline(body: &str) -> String {
        format!("{}\n{body}", markers().inline)
    }

    fn trusted() -> Vec<String> {
        vec!["github-actions[bot]".into()]
    }

    fn collect(
        labels: &[Value],
        comments: &[Value],
        sticky: &[Value],
        reactions: &HashMap<String, Vec<Value>>,
        trusted_authors: &[String],
    ) -> Vec<Value> {
        let m = markers();
        collect_feedback_entries(&CollectInput {
            repo: "o/r",
            pr: &json!(1),
            labels,
            inline_comments: comments,
            sticky_findings: sticky,
            reactions_by_comment_id: reactions,
            fp_label: DEFAULT_FP_LABEL,
            trusted_authors,
            markers: &m,
            ts: "2026-07-05T00:00:00.000Z",
        })
    }

    #[test]
    fn parse_review_comment_finding_production_emoji() {
        let c = json!({ "path": "server/a.ts", "line": 10,
            "body": inline("🟠 **major (glm-5.2): SQL injection risk**\n\nbody") });
        let f = parse_review_comment_finding(&c, &markers()).unwrap();
        assert_eq!(f["severity"], "major");
        assert_eq!(f["title"], "SQL injection risk");
        assert_eq!(f["model"], "glm-5.2");
    }

    #[test]
    fn parse_review_comment_findings_grouped_and_compact() {
        let c = json!({ "path": "server/s.ts", "line": 5,
            "body": inline("🔴 **critical (m1): leak**\n\na\n\n---\n\n🟠 **major (m2): race**") });
        let fs = parse_review_comment_findings(&c, &markers());
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0]["title"], "leak");
        assert_eq!(fs[1]["title"], "race");

        let c = json!({ "path": "server/s.ts", "line": 5, "body": inline("**major: race**\n\nbody") });
        assert_eq!(
            parse_review_comment_findings(&c, &markers()),
            vec![json!({ "file": "server/s.ts", "line": 5, "severity": "major", "title": "race", "model": "" })]
        );
    }

    #[test]
    fn parse_review_comment_findings_matches_js_on_nested_parens() {
        // Эталон JS: модель в скобках + original_line вместо line.
        let c = json!({ "path": "a.ts", "original_line": 8,
            "body": inline("**major (gpt (mini)): x (y)**\n**nit: z**") });
        let got = Value::Array(parse_review_comment_findings(&c, &markers()));
        assert_eq!(
            js_stringify(&got),
            r#"[{"file":"a.ts","line":8,"severity":"major","title":"x (y)","model":"gpt (mini)"},{"file":"a.ts","line":8,"severity":"nit","title":"z","model":""}]"#
        );
    }

    #[test]
    fn parse_review_comment_findings_requires_marker_at_start() {
        let c = json!({ "path": "a.ts", "line": 1, "body": format!("цитата {}\n**major: x**", markers().inline) });
        assert!(parse_review_comment_findings(&c, &markers()).is_empty());
        let c = json!({ "path": "a.ts", "line": 1, "body": 5 });
        assert!(parse_review_comment_findings(&c, &markers()).is_empty());
    }

    #[test]
    fn is_trusted_review_author_cases() {
        let t = trusted();
        assert!(is_trusted_review_author(Some("github-actions[bot]"), &t, Some(&json!({ "type": "Bot" }))));
        assert!(is_trusted_review_author(Some("GitHub-Actions[bot]"), &t, None));
        assert!(!is_trusted_review_author(Some("evil"), &t, Some(&json!({ "type": "User" }))));
        assert!(!is_trusted_review_author(Some("github-actions[bot]"), &t, Some(&json!({ "type": "User" }))));
        assert!(!is_trusted_review_author(Some("fake[bot]"), &t, Some(&json!({ "type": "Bot" }))));
        assert!(!is_trusted_review_author(None, &t, None));
        assert!(is_trusted_review_author(Some("any"), &[], None));
    }

    #[test]
    fn parse_trusted_authors_csv() {
        assert_eq!(parse_trusted_authors(" a , ,b,"), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_sticky_findings_namespace_footer() {
        let data = json!({ "findings": [{ "file": "app/x.ts", "line": 3, "severity": "major", "title": "bug" }] });
        let body = format!("<!-- orakul-ai-review -->\n<!-- orakul-ai-review-data:{} -->", js_stringify(&data));
        let fs = parse_sticky_findings(&body, &orakul());
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0]["file"], "app/x.ts");
        // Чужой namespace футера не видит.
        assert!(parse_sticky_findings(&body, &markers()).is_empty());
    }

    #[test]
    fn parse_sticky_findings_repairs_truncated_footer() {
        // Эталоны JS: обрыв посреди ключа не чинится, обрыв после значения — чинится.
        let m = markers();
        let run = |b: &str| js_stringify(&Value::Array(parse_sticky_findings(b, &m)));
        assert_eq!(run("<!-- ai-review-data:{\"findings\":[{\"file\":\"a.ts\",\"line\":1},{\"file\":\"b.ts\",\"li -->"), "[]");
        assert_eq!(
            run("<!-- ai-review-data:{\"findings\":[{\"file\":\"a.ts\",\"line\":1},{\"file\":\"b.ts\" -->"),
            r#"[{"file":"a.ts","line":1},{"file":"b.ts"}]"#
        );
        assert_eq!(
            run("<!-- ai-review-data:{\"findings\":[{\"file\":\"a.ts\",\"line\":1}],\"history\":[1,2 -->"),
            r#"[{"file":"a.ts","line":1}]"#
        );
        assert!(parse_sticky_findings("<!-- ai-review-data:null -->", &markers()).is_empty());
        assert!(parse_sticky_findings("", &markers()).is_empty());
    }

    #[test]
    fn collect_reaction_expands_grouped_findings() {
        let c = json!({ "id": 1, "path": "x.ts", "line": 1,
            "body": inline("**critical (m1): a**\n\n---\n\n**major (m2): b**"),
            "user": { "login": "github-actions[bot]", "type": "Bot" } });
        let r = HashMap::from([("1".to_string(), vec![json!({ "content": "-1", "user": { "login": "dev" } })])]);
        let entries = collect(&[], &[c], &[], &r, &trusted());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["finding"]["title"], "a");
        assert_eq!(entries[1]["finding"]["title"], "b");
        assert_eq!(entries[0]["signal"], "fp");
        assert_eq!(entries[0]["source"], "reaction");
        assert_eq!(entries[0]["user"], "dev");
        assert_eq!(entries[0]["commentId"], 1);
        // Порядок ключей — как у JS-объекта.
        let keys: Vec<&str> = entries[0].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["schemaVersion", "ts", "repo", "pr", "source", "signal", "reaction", "commentId", "finding", "user"]
        );
    }

    #[test]
    fn collect_untrusted_inline_skipped_and_neutral_reaction_ignored() {
        let c = json!({ "id": 2, "path": "x.ts", "line": 1, "body": inline("**major (m): spoof**"),
            "user": { "login": "evil-user", "type": "User" } });
        let r = HashMap::from([("2".to_string(), vec![json!({ "content": "-1", "user": { "login": "dev" } })])]);
        assert!(collect(&[], &[c], &[], &r, &trusted()).is_empty());

        let c = json!({ "id": 3, "path": "x.ts", "line": 1, "body": inline("**major (m): ok**"),
            "user": { "login": "github-actions[bot]", "type": "Bot" } });
        let r = HashMap::from([("3".to_string(), vec![json!({ "content": "heart" }), json!({ "content": "+1" })])]);
        let entries = collect(&[], &[c], &[], &r, &trusted());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["signal"], "tp");
        assert!(entries[0].get("user").is_none());
    }

    #[test]
    fn collect_label_with_sticky() {
        let sticky = [json!({ "file": "a.ts", "line": 1, "severity": "major", "title": "x" })];
        let entries = collect(&[json!({ "name": DEFAULT_FP_LABEL })], &[], &sticky, &HashMap::new(), &[]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["signal"], "fp");
        assert_eq!(entries[0]["source"], "label");
        assert_eq!(entries[0]["label"], DEFAULT_FP_LABEL);
        // Строковая метка тоже считается.
        assert_eq!(collect(&[json!(DEFAULT_FP_LABEL)], &[], &sticky, &HashMap::new(), &[]).len(), 1);
        assert!(collect(&[json!({ "name": "other" })], &[], &sticky, &HashMap::new(), &[]).is_empty());
    }

    #[test]
    fn dedupe_entries() {
        let base = vec![json!({ "repo": "o/r", "pr": 1, "source": "label", "signal": "fp",
            "finding": { "file": "a", "line": 1, "title": "t" } })];
        let mut dup = base[0].clone();
        dup["ts"] = json!("2026-01-02");
        assert_eq!(dedupe_feedback_entries(&base, &[dup]).len(), 1);
        assert!(!feedback_dedupe_key(&base[0]).is_empty());

        let alice = json!({ "repo": "o/r", "pr": 1, "source": "reaction", "signal": "fp", "commentId": 9,
            "user": "alice", "finding": { "file": "a.ts", "line": 1, "severity": "major", "title": "t" } });
        let mut bob = alice.clone();
        bob["user"] = json!("bob");
        assert_eq!(dedupe_feedback_entries(&[alice], &[bob]).len(), 2);
    }

    #[test]
    fn dedupe_key_matches_js() {
        let e = json!({ "repo": "o/r", "pr": 1, "source": "label", "signal": "fp",
            "finding": { "file": "a", "line": 0, "title": "t" } });
        assert_eq!(feedback_dedupe_key(&e), "o/r#1:label:fp:a|0||t::");
    }

    #[test]
    fn build_fixture_candidate_basic_and_null_without_file() {
        let c = build_fixture_candidate(&json!({ "pr": 7, "repo": "o/r", "ts": "2026-07-05T00:00:00Z",
            "source": "reaction", "signal": "fp",
            "finding": { "file": "server/x.ts", "line": 2, "severity": "major", "title": "False alarm" } }))
        .unwrap();
        assert!(c["suggestedId"].as_str().unwrap().starts_with("fp-pr-7-"));
        assert_eq!(c["meta"]["expected"]["shouldBlock"], false);
        assert_eq!(c["findings"][0]["file"], "server/x.ts");
        assert!(build_fixture_candidate(&json!({ "pr": 1, "finding": { "line": 1, "title": "x" } })).is_none());
    }

    fn js_entry() -> Value {
        json!({ "ts": "2026-07-05T12:00:00Z", "pr": 7, "repo": "o/r", "source": "reaction", "signal": "fp",
            "reaction": "-1", "commentId": 11, "user": "dev",
            "finding": { "file": "server/x.ts", "line": 2, "severity": "major", "title": "Ложная тревога" } })
    }

    #[test]
    fn build_fixture_candidate_pretty_matches_js() {
        // Эталон: JSON.stringify(buildFixtureCandidate(e), null, 2) в Node.
        let expected = "{\n  \"suggestedId\": \"fp-pr-7-hfb7c11bd-L2\",\n  \"fromFeedback\": {\n    \"ts\": \"2026-07-05T12:00:00Z\",\n    \"pr\": 7,\n    \"repo\": \"o/r\",\n    \"source\": \"reaction\",\n    \"signal\": \"fp\",\n    \"reaction\": \"-1\",\n    \"commentId\": 11\n  },\n  \"meta\": {\n    \"id\": \"fp-pr-7-hfb7c11bd-L2\",\n    \"description\": \"FP harvest PR #7: Ложная тревога (reaction/fp)\",\n    \"expected\": {\n      \"shouldBlock\": false,\n      \"blockerCount\": 0\n    }\n  },\n  \"findings\": [\n    {\n      \"file\": \"server/x.ts\",\n      \"line\": 2,\n      \"severity\": \"major\",\n      \"confidence\": 0.95,\n      \"title\": \"Ложная тревога\",\n      \"body\": \"auto-import from GitHub feedback (R8) — дополните diff.patch\",\n      \"model\": \"harvest\"\n    }\n  ],\n  \"diffNote\": \"Скопируйте unified diff из PR #7 в fixtures/fp-pr-7-hfb7c11bd-L2/diff.patch\"\n}";
        assert_eq!(js_stringify_pretty(&build_fixture_candidate(&js_entry()).unwrap()), expected);

        let mut e = js_entry();
        e["finding"]["title"] = json!("False alarm: SQL (inj)!");
        assert_eq!(build_fixture_candidate(&e).unwrap()["suggestedId"], "fp-pr-7-false-alarm-sql-inj-L2");
    }

    #[test]
    fn format_feedback_markdown_matches_js() {
        assert_eq!(
            format_feedback_markdown(&js_entry(), DEFAULT_CANDIDATES_DIR),
            "### 2026-07-05 · PR #7 (R8 auto)\n- Источник: reaction 👎 на inline #11 (@dev)\n- Finding: [major] `server/x.ts:2` «Ложная тревога»\n- Signal: **fp** · candidate: `.ai-review/feedback/candidates/fp-pr-7-hfb7c11bd-L2.json`\n"
        );
    }

    #[test]
    fn normalize_legacy_row_keeps_identity() {
        let legacy = json!({ "ts": "2026-07-05T00:00:00.000Z", "repo": "o/r", "pr": 7, "source": "reaction",
            "signal": "fp", "commentId": 42, "user": "reviewer",
            "finding": { "file": "server/a.ts", "line": 9, "severity": "major", "title": "false alarm", "model": "m1" } });
        let n = normalize_feedback_entry(&legacy).unwrap();
        assert_eq!(n["schemaVersion"], FEEDBACK_SCHEMA_VERSION);
        assert_eq!(n["pr"], 7);
        assert_eq!(n["commentId"], 42);
        assert_eq!(n["finding"]["title"], "false alarm");
    }

    #[test]
    fn normalize_edge_cases_match_js() {
        assert!(normalize_feedback_entry(&json!({ "pr": 1.5, "source": "label", "signal": "fp", "finding": {} })).is_none());
        assert!(normalize_feedback_entry(&json!({ "schemaVersion": 2, "pr": 1, "source": "label", "signal": "fp", "finding": {} })).is_none());
        assert!(normalize_feedback_entry(&json!([1])).is_none());
        // Массив в finding — typeof 'object' в JS, поля по умолчанию.
        let n = normalize_feedback_entry(&json!({ "pr": 3, "source": "label", "signal": "fp", "finding": [] })).unwrap();
        assert_eq!(
            js_stringify(&n),
            r#"{"schemaVersion":1,"ts":"1970-01-01T00:00:00.000Z","repo":"unknown/unknown","pr":3,"source":"label","signal":"fp","finding":{"file":"","line":0,"severity":"minor","title":"замечание"}}"#
        );
    }

    /// Граница доверия журнала: Rust не принимает строки, которые JS отвергает, и
    /// наоборот. Эталон снят с `normalizeFeedbackEntry` (RFC 20 SEC): расхождение
    /// было в `Number(string)` — `--5`, `-0x5`, `infinity` Rust понимал как числа,
    /// а `0b101`/`0o17` — нет.
    #[test]
    fn normalize_number_coercion_matches_js() {
        let f = json!({ "file": "a.ts", "line": 3, "title": "t" });
        let row = |extra: Value| {
            let mut base = json!({ "pr": 5, "source": "label", "signal": "fp", "finding": f.clone() });
            for (k, v) in extra.as_object().unwrap() {
                base[k] = v.clone();
            }
            normalize_feedback_entry(&base).map(|v| js_stringify(&v))
        };
        let ok = |line: u32, tail: &str| {
            Some(format!(
                r#"{{"schemaVersion":1,"ts":"1970-01-01T00:00:00.000Z","repo":"unknown/unknown","pr":5,"source":"label","signal":"fp","finding":{{"file":"a.ts","line":{line},"severity":"minor","title":"t"}}{tail}}}"#
            ))
        };
        assert_eq!(row(json!({ "schemaVersion": "--1" })), None);
        assert_eq!(row(json!({ "pr": "--5" })), None);
        assert_eq!(row(json!({ "pr": "-0x5" })), None);
        assert_eq!(row(json!({ "pr": "0b101" })), ok(3, ""));
        assert_eq!(row(json!({ "finding": { "file": "a.ts", "line": "infinity", "title": "t" } })), ok(0, ""));
        assert_eq!(row(json!({ "commentId": "0o17" })), ok(3, r#","commentId":15"#));
    }

    /// suggestedId — имя файла кандидата: поля находки не проносят `/` и `..`
    /// (RFC 20 SEC; эталон — `test/feedback-candidate-path.test.mjs`).
    #[test]
    fn fixture_candidate_id_is_safe_file_name() {
        let entry = |line: Value, title: &str| {
            json!({ "pr": 7, "source": "label", "signal": "fp", "label": "ai-review-fp",
                "finding": { "file": "server/a.ts", "line": line, "severity": "major", "title": title } })
        };
        let id = |e: &Value| build_fixture_candidate(e).unwrap()["suggestedId"].as_str().unwrap().to_string();
        assert_eq!(id(&entry(json!(12), "unsafe eval")), "fp-pr-7-unsafe-eval-L12");
        let evil = entry(json!("/../../../escape"), "unsafe eval");
        assert_eq!(id(&evil), "fp-pr-7-unsafe-eval-L----------escape");
        // Суррогатная пара — две UTF-16 единицы, как в JS `replace` без флага u.
        assert_eq!(id(&entry(json!("😀"), "unsafe eval")), "fp-pr-7-unsafe-eval-L--");

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a/b/candidates");
        // Каталог, через который `..` в id вывел бы запись наружу.
        std::fs::create_dir_all(dir.join("fp-pr-7-unsafe-eval-L")).unwrap();
        let r = write_fixture_candidates(&dir, &[evil], &|_| {}).unwrap();
        assert_eq!(r.written, 1);
        assert!(dir.join("fp-pr-7-unsafe-eval-L----------escape.json").is_file());
        assert!(!root.path().join("a/escape.json").exists());
    }

    #[test]
    fn migrate_matches_js_and_is_idempotent() {
        let text = "{\"pr\":\"7\",\"source\":\"label\",\"signal\":\"tp\",\"commentId\":\"3\",\"finding\":{\"line\":\"-4\",\"severity\":\"x\",\"title\":\"\",\"model\":\"m\"},\"extra\":1}\n  \n{\"schemaVersion\":2,\"pr\":1}\n[1]\n";
        let r = migrate_feedback_text(text);
        assert_eq!(r.stats, MigrateStats { total: 3, valid: 1, migrated: 1, invalid: 2 });
        assert_eq!(
            r.text,
            "{\"schemaVersion\":1,\"ts\":\"1970-01-01T00:00:00.000Z\",\"repo\":\"unknown/unknown\",\"pr\":7,\"source\":\"label\",\"signal\":\"tp\",\"finding\":{\"file\":\"\",\"line\":0,\"severity\":\"minor\",\"title\":\"замечание\",\"model\":\"m\"},\"commentId\":3}\n"
        );

        let legacy = js_stringify(&json!({ "ts": "2026-07-05T00:00:00.000Z", "repo": "o/r", "pr": 7,
            "source": "label", "signal": "fp",
            "finding": { "file": "server/a.ts", "line": 9, "severity": "major", "title": "false alarm" } }));
        let first = migrate_feedback_text(&format!("{legacy}\nnot-json\n"));
        assert_eq!(first.stats.total, 2);
        assert_eq!(first.stats.migrated, 1);
        assert_eq!(first.stats.invalid, 1);
        assert!(first.text.contains("\"schemaVersion\":1"));
        let second = migrate_feedback_text(&first.text);
        assert_eq!(second.stats.migrated, 0);
        assert_eq!(second.stats.valid, 1);
        assert_eq!(second.text, first.text);
        // Числовая лексема 7.0 печатается как в JSON.stringify — не миграция.
        let again = migrate_feedback_text(&second.text.replace("\"pr\":7", "\"pr\":7.0"));
        assert_eq!(again.stats.migrated, 0);
    }

    #[test]
    fn append_and_read_log_round_trip_and_skip_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("nested/fp.jsonl");
        let entry = json!({ "ts": "2026-07-05", "repo": "o/r", "pr": 1, "source": "reaction", "signal": "fp",
            "finding": { "file": "a.ts", "line": 1, "severity": "major", "title": "t" } });
        let r = append_feedback_log(&log_path, &[entry, json!({ "pr": 0 })], &nolog());
        assert_eq!(r, AppendResult { appended: 1, error: None });
        let rows = read_feedback_log(&log_path, &nolog());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["pr"], 1);

        let corrupt = dir.path().join("c.jsonl");
        let good = js_stringify(&json!({ "pr": 1, "source": "label", "signal": "fp",
            "finding": { "file": "a.ts", "line": 1, "title": "t" } }));
        fs::write(&corrupt, format!("{{bad}}\n{good}\n")).unwrap();
        let logged = Mutex::new(Vec::new());
        let rows = read_feedback_log(&corrupt, &|m: &str| logged.lock().unwrap().push(m.to_string()));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["pr"], 1);
        assert!(logged.lock().unwrap()[0].starts_with("feedback log skip corrupt line 1:"));
        assert!(read_feedback_log(&dir.path().join("missing.jsonl"), &nolog()).is_empty());
    }

    #[test]
    fn write_fixture_candidates_skips_existing() {
        let dir = tempfile::tempdir().unwrap();
        let entry = json!({ "pr": 3, "repo": "o/r", "ts": "2026-07-05", "source": "label", "signal": "fp",
            "label": DEFAULT_FP_LABEL, "finding": { "file": "b.ts", "line": 4, "severity": "major", "title": "dup test" } });
        let tp = json!({ "pr": 3, "source": "reaction", "signal": "tp", "finding": { "file": "b.ts" } });
        let r1 = write_fixture_candidates(dir.path(), &[entry.clone(), tp], &nolog()).unwrap();
        let r2 = write_fixture_candidates(dir.path(), &[entry], &nolog()).unwrap();
        assert_eq!(r1, WriteCandidatesResult { written: 1, skipped: 0 });
        assert_eq!(r2, WriteCandidatesResult { written: 0, skipped: 1 });
        let text = fs::read_to_string(dir.path().join("fp-pr-3-dup-test-L4.json")).unwrap();
        assert!(text.ends_with("}\n"));
    }

    #[test]
    fn append_feedback_markdown_appends_at_end_and_inits() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = dir.path().join("fp.md");
        fs::write(&md_path, "## Журнал\n\n### old entry\n").unwrap();
        let r = append_feedback_markdown(&md_path, &[js_entry()], &nolog(), DEFAULT_CANDIDATES_DIR);
        assert_eq!(r.appended, 1);
        let md = fs::read_to_string(&md_path).unwrap();
        assert!(md.find("### old entry").unwrap() < md.find("R8 auto").unwrap());
        assert!(md.starts_with("## Журнал\n\n### old entry\n### 2026-07-05"));

        let fresh = dir.path().join("new.md");
        append_feedback_markdown(&fresh, &[js_entry()], &nolog(), DEFAULT_CANDIDATES_DIR);
        assert!(fs::read_to_string(&fresh).unwrap().starts_with("# AI-review FP journal (R8 auto-init)\n### "));
    }

    fn harvest_cfg(write: bool, log_file: &str) -> Value {
        json!({
            "github": { "repo": "o/r", "prNumber": 5 },
            "feedback": {
                "label": DEFAULT_FP_LABEL,
                "logFile": log_file,
                "candidatesDir": "cand",
                "mdPath": "fp.md",
                "appendMarkdown": true,
                "writeCandidates": true,
                "write": write,
                "trustedAuthors": "github-actions[bot]",
                "reactionConcurrency": 2,
            }
        })
    }

    fn bot_inline(id: u64, title: &str) -> Value {
        json!({ "id": id, "path": "a.ts", "line": 3, "body": inline(&format!("**major (m): {title}**")),
            "user": { "login": "github-actions[bot]", "type": "Bot" } })
    }

    #[test]
    fn harvest_dry_run_with_fake_github() {
        let dir = tempfile::tempdir().unwrap();
        let mut gh = FakeGithub::new();
        gh.inline_comments = vec![bot_inline(1, "bug")];
        gh.reactions = vec![json!({ "content": "-1", "user": { "login": "dev" } })];
        let r = harvest_feedback(&harvest_cfg(false, "fb/test-fp.jsonl"), &markers(), &gh, dir.path(), &nolog()).unwrap();
        assert_eq!(r.new_entries.len(), 1);
        assert!(r.dry_run);
        assert!(r.ok);
        assert_eq!(r.persisted_entries, 0);
        assert!(!dir.path().join("fb/test-fp.jsonl").exists());
        let calls = gh.core.calls.records();
        assert!(calls.contains(&CallRecord::ListPullCommentReactions { comment_id: 1 }));
        assert!(format_harvest_report(&r).contains("- [fp] reaction · a.ts:3 «bug»"));
    }

    #[test]
    fn harvest_write_persists_dedupes_and_skips_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let mut gh = FakeGithub::new();
        let foreign = json!({ "id": 9, "path": "a.ts", "line": 3, "body": inline("**major (m): spoof**"),
            "user": { "login": "evil", "type": "User" } });
        gh.inline_comments = vec![bot_inline(1, "bug"), bot_inline(2, "race"), foreign];
        gh.reactions = vec![json!({ "content": "-1", "user": { "login": "dev" } })];
        gh.issue_labels = vec![json!({ "name": DEFAULT_FP_LABEL })];
        let data = json!({ "findings": [{ "file": "s.ts", "line": 7, "severity": "major", "title": "sticky bug" }] });
        gh.sticky_review = Some(json!({
            "body": format!("{}\n<!-- ai-review-data:{} -->", markers().review, js_stringify(&data)),
            "user": { "login": "github-actions[bot]", "type": "Bot" },
        }));
        let cfg = harvest_cfg(true, "fb/fp.jsonl");
        let r = harvest_feedback(&cfg, &markers(), &gh, dir.path(), &nolog()).unwrap();
        assert!(r.ok, "{:?}", r.write_errors);
        assert_eq!(r.new_entries.len(), 3);
        assert_eq!(r.persisted_entries, 3);
        assert_eq!(r.candidates_written, 3);
        assert!(r.warnings.is_empty());
        // Реакции за чужим инлайном не запрашиваются.
        assert!(!gh.core.calls.records().contains(&CallRecord::ListPullCommentReactions { comment_id: 9 }));
        assert_eq!(read_feedback_log(&dir.path().join("fb/fp.jsonl"), &nolog()).len(), 3);
        assert!(fs::read_to_string(dir.path().join("fp.md")).unwrap().contains("label `ai-review-fp`"));

        // Повторный прогон: всё уже в журнале.
        let r2 = harvest_feedback(&cfg, &markers(), &gh, dir.path(), &nolog()).unwrap();
        assert!(r2.new_entries.is_empty());
        assert_eq!(r2.total_in_log, 3);
        assert!(format_harvest_report(&r2).contains("- (нет нового feedback)"));
    }

    #[test]
    fn harvest_log_path_with_dotdot_resolves_lexically() {
        // lib/feedback.mjs: resolve(process.cwd(), feedback.logFile) снимает `..` без ФС —
        // журнал `nodir/../fp.jsonl` тот же `fp.jsonl`, старые записи видны dedupe.
        let dir = tempfile::tempdir().unwrap();
        let mut gh = FakeGithub::new();
        gh.inline_comments = vec![bot_inline(1, "bug")];
        gh.reactions = vec![json!({ "content": "-1", "user": { "login": "dev" } })];
        let r = harvest_feedback(&harvest_cfg(true, "fp.jsonl"), &markers(), &gh, dir.path(), &nolog()).unwrap();
        assert_eq!(r.persisted_entries, 1);
        let r2 = harvest_feedback(&harvest_cfg(true, "nodir/../fp.jsonl"), &markers(), &gh, dir.path(), &nolog()).unwrap();
        assert!(r2.new_entries.is_empty(), "запись уже в журнале");
        assert_eq!(r2.total_in_log, 1);
        assert!(!dir.path().join("nodir").exists(), "лишний каталог не создаётся");
    }

    #[test]
    fn harvest_untrusted_sticky_warns_and_reaction_failure_is_soft() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(crate::testutil::CallLog::new());
        let mut gh = FakeGithub::with_log(log);
        gh.core = std::mem::take(&mut gh.core).failing(|rec| match rec {
            CallRecord::ListPullCommentReactions { .. } => Some("boom".into()),
            _ => None,
        });
        gh.inline_comments = vec![bot_inline(4, "bug")];
        gh.issue_labels = vec![json!({ "name": DEFAULT_FP_LABEL })];
        gh.sticky_review = Some(json!({ "body": "<!-- ai-review -->", "user": { "login": "evil", "type": "User" } }));
        let logged = Mutex::new(Vec::new());
        let r = harvest_feedback(&harvest_cfg(false, "fp.jsonl"), &markers(), &gh, dir.path(), &|m: &str| {
            logged.lock().unwrap().push(m.to_string())
        })
        .unwrap();
        assert_eq!(
            r.warnings,
            vec![
                "sticky comment by untrusted author evil — skipped for label harvest".to_string(),
                "label ai-review-fp present but no trusted sticky findings — label harvest skipped".to_string(),
            ]
        );
        assert!(r.new_entries.is_empty());
        assert_eq!(logged.lock().unwrap().as_slice(), ["reactions for comment 4 skip: boom"]);
    }

    #[test]
    fn harvest_requires_repo_and_pr() {
        let gh = FakeGithub::new();
        let cfg = json!({ "github": { "repo": "o/r" }, "feedback": {} });
        let err = harvest_feedback(&cfg, &markers(), &gh, Path::new("."), &nolog()).unwrap_err();
        assert_eq!(err.to_string(), "harvest требует REPO и PR_NUMBER");
        assert!(gh.core.calls.records().is_empty());
    }

    #[test]
    fn harvest_github_read_error_propagates() {
        let mut gh = FakeGithub::new();
        gh.core = std::mem::take(&mut gh.core).failing(|rec| match rec {
            CallRecord::ListIssueLabels => Some("403".into()),
            _ => None,
        });
        let dir = tempfile::tempdir().unwrap();
        assert!(harvest_feedback(&harvest_cfg(false, "fp.jsonl"), &markers(), &gh, dir.path(), &nolog()).is_err());
    }
}
