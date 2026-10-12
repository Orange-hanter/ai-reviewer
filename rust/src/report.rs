//! Порт lib/report.mjs (Фаза 3, шаг 5): рендер sticky-комментария — вердикт,
//! Δ-блок сравнения с прошлым прогоном, история прогонов, reasoning-секция
//! и машинный футер. Потребители: pipeline.mjs (buildSummary/buildRouterSummary/
//! buildLlmUnavailableSummary), тесты, gold-прогоны.
//!
//! Паритет с JS построчный; нюансы побайтовости (проверены parity-прогоном
//! rust/parity/report.parity.mjs):
//!   - MARKERS живут в JS-профиле: обёртка читает MARKERS.review/dataKey на
//!     каждый вызов и передаёт аргументом (как MARKERS.inline в findings);
//!   - SEV продублирован константой ниже — рассинхрон ловит op `sev` parity-прогона;
//!   - футер — это JSON.stringify(footerData) + replace `-->`/`<!--`: serde_json
//!     так не сериализует, поэтому здесь собственный js_json_stringify с
//!     семантикой V8: порядок ключей (preserve_order), \b/\f как короткие
//!     escape, не-ASCII без экранирования, числа по правилам JS Number→string
//!     (NaN/Infinity → null, целые < 1e21 без точки, экспонента с явным знаком);
//!   - JS slice/length считают UTF-16 code units: обрезки строк (slice(0, N))
//!     и арифметика бюджетов идут по UTF-16 (utf16_len/js_slice_utf16);
//!     js_slice_utf16 не рвёт суррогатную пару — при резе внутри пары
//!     отбрасывается ведущий суррогат (JS дал бы lone surrogate, что String
//!     в Rust не представить);
//!   - toFixed для строки стоимости — Rust {:.prec}: расхождение только на
//!     точных бинарных тайях (0.125), реальные суммы стоимости ими не бывают;
//!   - publicErrorLabel/extractReasoningEntries/formatCostStickyLine портируются
//!     сюда из lib/util.mjs / lib/export.mjs / lib/cost.mjs (рендеру нужны,
//!     отдельными бинарями не выносятся);
//!   - отпечаток кода (fallback fp у находок без f.fp) — crate::findings::
//!     finding_fingerprint по карте текстов файлов, которую обёртка
//!     сериализует из Map<string, DiffFileRecord>.

use std::collections::{BTreeMap, HashSet};

use anyhow::{bail, Result};
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{json, Value};

use crate::findings::{
    finding_fingerprint, js_collapse_whitespace, js_number_or_zero, js_to_number, js_to_string, js_trim,
    js_truthy, LineText,
};

pub const REASONING_ADVISOR_MAX: usize = 1800;
pub const REASONING_SECTION_MAX: usize = 10_000;
pub const STICKY_REASONING_EXCERPT: usize = 400;
pub const OUTSIDE_MAX: usize = 10;
pub const STICKY_LIMIT: usize = 65_000;

/// SEV из lib/constants.mjs — порядок элементов как в JS-объекте.
pub const SEV: &[(&str, &str)] = &[
    ("critical", "🔴"),
    ("major", "🟠"),
    ("minor", "🟡"),
    ("nit", "🔵"),
];

/// `SEV[f.severity] || '⚪'` (доступ по строковому ключу; неизвестная severity → ⚪).
pub fn sev_label(severity: &str) -> &'static str {
    SEV.iter().find(|(k, _)| *k == severity).map(|(_, v)| *v).unwrap_or("⚪")
}

/// SEV как JSON-объект (op `sev` — страж рассинхрона с lib/constants.mjs).
pub fn sev_map() -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in SEV {
        m.insert((*k).to_string(), Value::String((*v).to_string()));
    }
    Value::Object(m)
}

// ---------------------------------------------------------------------------
// JS-совместимые примитивы строк/чисел
// ---------------------------------------------------------------------------

/// Длина строки в UTF-16 code units (как JS String.length).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// `s.slice(0, maxUnits)` по UTF-16 code units; суррогатную пару на границе
/// не рвём (ведущий суррогат отбрасывается — JS дал бы lone surrogate).
pub fn js_slice_utf16(s: &str, max_units: usize) -> String {
    let mut units = 0usize;
    let mut out = String::with_capacity(s.len().min(max_units * 2));
    for c in s.chars() {
        let n = c.len_utf16();
        if units + n > max_units {
            break;
        }
        out.push(c);
        units += n;
    }
    out
}

/// JS `String(v)`: undefined (отсутствующий ключ после JSON) → "undefined".
pub fn tmpl(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(x) => js_to_string(x),
    }
}

/// JS `String(v || fallback)`: falsy (в т.ч. null/отсутствующий) → fallback.
pub fn str_coalesce(v: Option<&Value>, fallback: &str) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => fallback.to_string(),
    }
}

/// JS toLowerCase для нормализации заголовков находок (Unicode full case mapping,
/// как в V8; реальных расхождений с Rust to_lowercase на практике нет).
fn js_to_lowercase(s: &str) -> String {
    s.to_lowercase()
}

/// Нормализация заголовка для ключей находок: lowercase, без `` ` `` `*_ "«»`,
/// `\s+` → ' ', trim (по UTF-16-совместимым whitespace, как \s в JS).
fn normalize_title(f: &Value) -> String {
    let raw = str_coalesce(f.get("title"), "");
    let lowered = js_to_lowercase(&raw);
    let stripped: String = lowered.chars().filter(|c| !matches!(c, '`' | '*' | '_' | '"' | '«' | '»')).collect();
    js_trim(&js_collapse_whitespace(&stripped)).to_string()
}

/// JS Number→string для JSON.stringify и шаблонных строк: NaN/Infinity — только
/// для шаблонов; в JSON.stringify не-конечные → null (решает вызывающий).
fn js_number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if x == 0.0 {
        return "0".to_string(); // -0 → "0" в JS
    }
    let a = x.abs();
    if x.fract() == 0.0 && a < 1e21 {
        return format!("{}", x as i64);
    }
    if a >= 1e21 || a < 1e-6 {
        // Экспоненциальная форма: Rust {:e} → "1e21"/"1.5e-7"; JS пишет знак
        // экспоненты всегда ("1e+21", "1.5e-7").
        let s = format!("{x:e}");
        return match s.split_once('e') {
            Some((m, e)) => {
                let exp: i32 = e.parse().unwrap_or(0);
                format!("{}e{}{}", m, if exp < 0 { "-" } else { "+" }, exp.abs())
            }
            None => s,
        };
    }
    // Обычная форма: Rust Display — shortest round-trip без экспоненты, как JS.
    format!("{x}")
}

/// JSON.stringify(value) с семантикой V8: preserve_order ключей, `\b`/`\f`
/// короткими escape, остальные control < 0x20 — \u00xx (нижний регистр),
/// не-ASCII без экранирования, NaN/Infinity → null.
pub fn js_json_stringify(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_f64() {
            Some(x) if x.is_finite() => out.push_str(&js_number_to_string(x)),
            _ => out.push_str("null"),
        },
        Value::String(s) => js_json_escape_into(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, el) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                js_json_stringify(el, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, val)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                js_json_escape_into(k, out);
                out.push(':');
                js_json_stringify(val, out);
            }
            out.push('}');
        }
    }
}

fn js_json_escape_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// JS `Number.toFixed(prec)` (положительные значения): Rust decimal rounding;
/// расхождение с V8 только на точных бинарных тайях вроде 0.125.
pub fn js_to_fixed(x: f64, prec: usize) -> String {
    format!("{:.*}", prec, x)
}

/// `x > n` с JS-коэрсией: нечисловые значения → false (сравнение с NaN).
fn js_gt(v: Option<&Value>, n: f64) -> bool {
    match v.and_then(js_to_number) {
        Some(x) => x > n,
        None => false,
    }
}

/// `v ?? 0` для шаблонных строк таблицы истории: null/отсутствующий → "0".
fn nullish_zero(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "0".to_string(),
        Some(x) => js_to_string(x),
    }
}

fn arr_or_empty<'a>(v: Option<&'a Value>) -> Result<&'a [Value]> {
    match v {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => bail!("ожидался массив"),
    }
}

/// Нечувствительный к регистру регекс (publicErrorLabel — не горячий путь,
/// компиляция на вызов приемлема; паттерны те же, что в lib/util.mjs).
fn regex_i(pattern: &str) -> regex::Regex {
    regex::Regex::new(&format!("(?i){pattern}")).expect("валидный регекс publicErrorLabel")
}

// ---------------------------------------------------------------------------
// publicErrorLabel (lib/util.mjs)
// ---------------------------------------------------------------------------

/// Безопасная короткая метка ошибки для публичного sticky (publicErrorLabel):
/// в PR уходит только категория; сырой текст остаётся в CI-логе.
pub fn public_error_label(err: Option<&Value>) -> String {
    let m = match err {
        None | Some(Value::Null) => String::new(),
        Some(Value::Object(o)) => match o.get("message") {
            Some(v) if js_truthy(v) => js_to_string(v),
            _ => "[object Object]".to_string(),
        },
        Some(v) => js_to_string(v),
    };
    if regex_i("aborted|timed?\\s*out|ETIMEDOUT").is_match(&m) {
        return "таймаут".to_string();
    }
    if regex_i("тайм-бюджет|time[_ ]?budget").is_match(&m) {
        return "тайм-бюджет".to_string();
    }
    if regex_i("fetch failed|network|socket|ECONN|EAI_AGAIN|terminated").is_match(&m) {
        return "сеть".to_string();
    }
    if let Some(caps) = regex_i("\\b([45]\\d{2})\\b").captures(&m) {
        let http = caps.get(1).map(|c| c.as_str()).unwrap_or("");
        if http == "429" {
            return "HTTP 429 (rate limit)".to_string();
        }
        return format!("HTTP {http}");
    }
    if regex_i("json|parse|unexpected token").is_match(&m) {
        return "невалидный ответ".to_string();
    }
    "ошибка вызова".to_string()
}

// ---------------------------------------------------------------------------
// extractReasoningEntries (lib/export.mjs) — DY для JSON-артефакта и sticky
// ---------------------------------------------------------------------------

/// Порядок ключей в записях — как в JS-объектах (footer сериализуется в sticky).
pub fn extract_reasoning_entries(
    reviews: Option<&Value>,
    review_mode: &str,
    excerpt_len: usize,
    error_max: usize,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for r in arr_or_empty(reviews)? {
        let refs = arr_or_empty(r.get("references"))?;
        if !refs.is_empty() {
            for refv in refs {
                let role = str_coalesce(refv.get("role"), "advisor");
                // model: ref.model — как есть; отсутствующий ключ → undefined,
                // в JSON выпадает (не строка "undefined" — это семантика tmpl
                // для шаблонных строк, здесь объект идёт в JSON.stringify).
                let model = refv.get("model").cloned();
                let chunk_count = refv.get("chunkCount").cloned().unwrap_or(json!(1));
                if js_truthy(refv.get("error").unwrap_or(&Value::Null)) {
                    let mut e = serde_json::Map::new();
                    e.insert("role".into(), Value::String(role));
                    if let Some(m) = model {
                        e.insert("model".into(), m);
                    }
                    e.insert("chunkCount".into(), chunk_count);
                    e.insert("error".into(), Value::String(js_slice_utf16(&js_to_string(refv.get("error").unwrap()), error_max)));
                    out.push(Value::Object(e));
                } else if js_truthy(refv.get("text").unwrap_or(&Value::Null)) {
                    let text = js_to_string(refv.get("text").unwrap());
                    let mut e = serde_json::Map::new();
                    e.insert("role".into(), Value::String(role));
                    if let Some(m) = model {
                        e.insert("model".into(), m);
                    }
                    e.insert("chunkCount".into(), chunk_count);
                    e.insert("excerpt".into(), Value::String(js_slice_utf16(&text, excerpt_len)));
                    e.insert("truncated".into(), Value::Bool(utf16_len(&text) > excerpt_len));
                    out.push(Value::Object(e));
                }
            }
            continue;
        }
        if review_mode != "moa" && !js_truthy(r.get("error").unwrap_or(&Value::Null)) && js_truthy(r.get("summary").unwrap_or(&Value::Null)) {
            let text = js_to_string(r.get("summary").unwrap());
            let mut e = serde_json::Map::new();
            e.insert("role".into(), Value::String("model".into()));
            if let Some(m) = r.get("model").cloned() {
                e.insert("model".into(), m);
            }
            e.insert("excerpt".into(), Value::String(js_slice_utf16(&text, excerpt_len)));
            e.insert("truncated".into(), Value::Bool(utf16_len(&text) > excerpt_len));
            out.push(Value::Object(e));
        }
    }
    Ok(out)
}

/// buildStickyReasoningFooter: excerpt-записи с excerpt или error.
pub fn build_sticky_reasoning_footer(
    reviews: Option<&Value>,
    review_mode: &str,
    excerpt_len: usize,
) -> Result<Vec<Value>> {
    Ok(extract_reasoning_entries(reviews, review_mode, excerpt_len, 200)?
        .into_iter()
        .filter(|e| {
            js_truthy(e.get("excerpt").unwrap_or(&Value::Null)) || js_truthy(e.get("error").unwrap_or(&Value::Null))
        })
        .collect())
}

// ---------------------------------------------------------------------------
// buildReasoningSection — collapsible MoA advisor prose / consensus / jury
// ---------------------------------------------------------------------------

fn reasoning_title(has_refs: bool, has_claims: bool, has_consensus: bool) -> &'static str {
    if has_refs && has_claims {
        return "MoA reasoning (advisors → aggregator)";
    }
    if has_refs {
        return "MoA advisor notes";
    }
    if has_claims {
        return "Jury (advisor → aggregator)";
    }
    if has_consensus {
        return "Model reasoning (consensus)";
    }
    "Review reasoning"
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningSection {
    pub text: String,
    pub truncated: bool,
}

pub fn build_reasoning_section(
    reviews: Option<&Value>,
    all_findings: Option<&Value>,
    enabled: bool,
    review_mode: &str,
) -> Result<ReasoningSection> {
    if !enabled {
        return Ok(ReasoningSection { text: String::new(), truncated: false });
    }
    let reviews_arr = arr_or_empty(reviews)?;
    let findings_arr = arr_or_empty(all_findings)?;

    let mut refs: Vec<&Value> = Vec::new();
    for r in reviews_arr {
        for refv in arr_or_empty(r.get("references"))? {
            if js_truthy(refv.get("text").unwrap_or(&Value::Null)) || js_truthy(refv.get("error").unwrap_or(&Value::Null)) {
                refs.push(refv);
            }
        }
    }
    let claims_findings: Vec<&Value> = findings_arr
        .iter()
        .filter(|f| !arr_or_empty(f.get("advisorClaims")).unwrap_or(&[]).is_empty())
        .collect();
    let consensus_rows: Vec<&Value> = if review_mode != "moa" {
        reviews_arr
            .iter()
            .filter(|r| !js_truthy(r.get("error").unwrap_or(&Value::Null)) && js_truthy(r.get("summary").unwrap_or(&Value::Null)))
            .collect()
    } else {
        Vec::new()
    };

    if refs.is_empty() && claims_findings.is_empty() && consensus_rows.is_empty() {
        return Ok(ReasoningSection { text: String::new(), truncated: false });
    }

    let mut body = String::new();
    let mut budget: usize = REASONING_SECTION_MAX;
    let mut truncated = false;

    for refv in &refs {
        let label = match refv.get("role") {
            Some(role) if js_truthy(role) => format!("{} · {}", js_to_string(role), tmpl(refv.get("model"))),
            _ => tmpl(refv.get("model")),
        };
        let chunk = if js_truthy(refv.get("error").unwrap_or(&Value::Null)) {
            format!("⚠️ {}", public_error_label(refv.get("error")))
        } else {
            let raw = js_to_string(refv.get("text").unwrap());
            let mut chunk = js_slice_utf16(&raw, REASONING_ADVISOR_MAX);
            if utf16_len(&raw) > REASONING_ADVISOR_MAX {
                chunk.push_str("\n\n…(обрезано)");
                truncated = true;
            }
            chunk
        };
        let chunk_suffix = if js_gt(refv.get("chunkCount"), 1.0) {
            format!(" ({} chunks)", js_to_string(refv.get("chunkCount").unwrap()))
        } else {
            String::new()
        };
        let block = format!("#### {label}{chunk_suffix}\n\n{chunk}\n\n");
        if utf16_len(&block) > budget {
            truncated = true;
            break;
        }
        budget -= utf16_len(&block);
        body.push_str(&block);
    }

    for r in &consensus_rows {
        let raw = js_to_string(r.get("summary").unwrap());
        let mut chunk = js_slice_utf16(&raw, REASONING_ADVISOR_MAX);
        if utf16_len(&raw) > REASONING_ADVISOR_MAX {
            chunk.push_str("\n\n…(обрезано)");
            truncated = true;
        }
        let block = format!("#### {}\n\n{chunk}\n\n", tmpl(r.get("model")));
        if utf16_len(&block) > budget {
            truncated = true;
            break;
        }
        budget -= utf16_len(&block);
        body.push_str(&block);
    }

    if !claims_findings.is_empty() && budget > 200 {
        body.push_str("#### Jury (advisor → aggregator)\n\n");
        for f in claims_findings.iter().take(12) {
            let loc = match f.get("file") {
                Some(v) if js_truthy(v) => {
                    let line_part = if js_truthy(f.get("line").unwrap_or(&Value::Null)) {
                        format!(":{}", tmpl(f.get("line")))
                    } else {
                        String::new()
                    };
                    format!("`{}{}`", js_to_string(v), line_part)
                }
                _ => "—".to_string(),
            };
            let lines = arr_or_empty(f.get("advisorClaims"))?
                .iter()
                .map(|c| {
                    let reason = match c.get("reason") {
                        Some(rv) if js_truthy(rv) => format!(": {}", js_slice_utf16(&js_to_string(rv), 160)),
                        _ => String::new(),
                    };
                    format!("- **{}** {}{}", tmpl(c.get("role")), tmpl(c.get("status")), reason)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let block = format!("- {} «{}»\n{}\n", loc, js_slice_utf16(&str_coalesce(f.get("title"), ""), 80), lines);
            if utf16_len(&block) > budget {
                truncated = true;
                break;
            }
            budget -= utf16_len(&block);
            body.push_str(&block);
        }
    }

    if js_trim(&body).is_empty() {
        return Ok(ReasoningSection { text: String::new(), truncated: false });
    }

    if truncated {
        body.push_str("\n> _Reasoning обрезан по лимиту секции (~10k симв.). Полный JSON artifact — без prose body._\n");
    }

    let title = reasoning_title(!refs.is_empty(), !claims_findings.is_empty(), !consensus_rows.is_empty() && refs.is_empty());
    let text = format!("\n<details><summary>🔗 {title}</summary>\n\n{}\n\n</details>\n", js_trim(&body));
    Ok(ReasoningSection { text, truncated })
}

// ---------------------------------------------------------------------------
// Ключи находок / Δ / стабильность / воспроизводимость вердикта
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingKeys {
    pub fp: Option<String>,
    pub loc: String,
    pub finding: String,
    pub point: String,
}

/// Ключи находки между прогонами (findingKeys в lib/report.mjs).
pub fn finding_keys(f: &Value) -> FindingKeys {
    let title = normalize_title(f);
    let file = str_coalesce(f.get("file"), "");
    let line = js_number_or_zero(f.get("line"));
    let loc = format!("loc:{file}:{}:{title}", js_number_to_string(line));
    let fp = match f.get("fp") {
        Some(v) if js_truthy(v) => Some(format!("fp:{}", js_to_string(v))),
        _ => None,
    };
    let (finding, point) = match &fp {
        Some(fp) => (format!("{fp}|{title}"), fp.clone()),
        None => (loc.clone(), loc.clone()),
    };
    FindingKeys { fp, loc, finding, point }
}

/// findingIdentity — сильнейший ключ находки.
pub fn finding_identity(f: &Value) -> String {
    finding_keys(f).finding
}

fn same_finding(a: &Value, b: &Value) -> bool {
    let ka = finding_keys(a);
    let kb = finding_keys(b);
    if ka.fp.is_some() && kb.fp.is_some() {
        return ka.finding == kb.finding;
    }
    ka.loc == kb.loc
}

fn same_point(a: &Value, b: &Value) -> bool {
    let ka = finding_keys(a);
    let kb = finding_keys(b);
    if ka.fp.is_some() && kb.fp.is_some() {
        return ka.fp == kb.fp;
    }
    ka.loc == kb.loc
}

pub struct DiffOutcome {
    pub added: Vec<Value>,
    pub gone: Vec<Value>,
    pub kept: Vec<Value>,
}

/// Что изменилось с прошлого прогона (diffFindings) — по спискам, с заголовком.
pub fn diff_findings(current: Option<&Value>, prior: Option<&Value>) -> Result<DiffOutcome> {
    let cur = arr_or_empty(current)?;
    let prev = arr_or_empty(prior)?;
    let added = cur.iter().filter(|f| !prev.iter().any(|p| same_finding(f, p))).cloned().collect();
    let gone = prev.iter().filter(|p| !cur.iter().any(|f| same_finding(f, p))).cloned().collect();
    let kept = cur.iter().filter(|f| prev.iter().any(|p| same_finding(f, p))).cloned().collect();
    Ok(DiffOutcome { added, gone, kept })
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PointDelta {
    pub kept: usize,
    pub added: usize,
    pub gone: usize,
    pub current: usize,
    pub prior: usize,
}

/// Пересечение/разности прогонов по ТОЧКАМ КОДА (findingPointDelta): каждое
/// место учитывается один раз, совпадение «съедает» одну точку прошлого набора.
pub fn finding_point_delta(current: Option<&Value>, prior: Option<&Value>) -> Result<PointDelta> {
    let uniq = |list: &[Value]| -> Vec<Value> {
        let mut seen: IndexMap<String, ()> = IndexMap::new();
        let mut out = Vec::new();
        for f in list {
            let key = finding_keys(f).point;
            if !seen.contains_key(&key) {
                seen.insert(key, ());
                out.push(f.clone());
            }
        }
        out
    };
    let cur = uniq(arr_or_empty(current)?);
    let prev = uniq(arr_or_empty(prior)?);
    let mut used: HashSet<usize> = HashSet::new();
    let mut kept = 0usize;
    for f in &cur {
        if let Some(hit) = prev.iter().enumerate().position(|(i, p)| !used.contains(&i) && same_point(f, p)) {
            used.insert(hit);
            kept += 1;
        }
    }
    Ok(PointDelta { kept, added: cur.len() - kept, gone: prev.len() - kept, current: cur.len(), prior: prev.len() })
}

/// Jaccard по точкам кода; null (None) — когда сравнивать не с чем.
pub fn findings_stability(current: Option<&Value>, prior: Option<&Value>, prior_known: bool) -> Result<Option<f64>> {
    let d = finding_point_delta(current, prior)?;
    let union = d.kept + d.added + d.gone;
    if !prior_known || union == 0 {
        return Ok(None);
    }
    let v = (d.kept as f64) / (union as f64);
    Ok(Some((v * 100.0).round() / 100.0))
}

/// Доля → проценты; `—` там, где метрика не определена (formatStability).
pub fn format_stability(v: Option<f64>) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{}%", (x * 100.0).round() as i64),
        _ => "—".to_string(),
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VerdictRepro {
    pub total: usize,
    pub blocked: usize,
    pub flaky: bool,
}

/// Воспроизводимость вердикта на одном коммите (verdictReproducibility):
/// только записи того же sha с булевым blocked; меньше двух записей → null.
pub fn verdict_reproducibility(runs: Option<&Value>, sha: Option<&Value>) -> Result<Option<VerdictRepro>> {
    let short = js_slice_utf16(&str_coalesce(sha, ""), 7);
    if short.is_empty() {
        return Ok(None);
    }
    let same_sha: Vec<&Value> = arr_or_empty(runs)?
        .iter()
        .filter(|r| matches!(r.get("sha"), Some(Value::String(s)) if *s == short) && matches!(r.get("blocked"), Some(Value::Bool(_))))
        .collect();
    if same_sha.len() < 2 {
        return Ok(None);
    }
    let blocked = same_sha.iter().filter(|r| matches!(r.get("blocked"), Some(Value::Bool(true)))).count();
    Ok(Some(VerdictRepro { total: same_sha.len(), blocked, flaky: blocked > 0 && blocked < same_sha.len() }))
}

fn verdict_mark(blocked: Option<&Value>) -> &'static str {
    match blocked {
        Some(Value::Bool(true)) => "⛔",
        Some(Value::Bool(false)) => "✅",
        _ => "—",
    }
}

/// escapeMdInline: заголовок/модель находки — инлайн-текст, разметка экранируется.
fn escape_md_inline(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for c in raw.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// escapeMdBlock: тело находки — гасим только `<` (не закрыть `<details>`, не открыть комментарий).
fn escape_md_block(raw: &str) -> String {
    raw.replace('<', "&lt;")
}

/// codePath: путь в backticks без backtick внутри.
fn code_path(raw: &str) -> String {
    raw.chars().filter(|c| *c != '`').collect()
}

/// Строка Δ-блока по одной находке (deltaLine): severity, путь в backticks,
/// заголовок с экранированием markdown, обрезанный до 110 UTF-16 юнитов.
pub fn delta_line(f: &Value) -> String {
    let safe_title = js_slice_utf16(&escape_md_inline(&str_coalesce(f.get("title"), "замечание")), 110);
    let severity = f.get("severity").and_then(Value::as_str).unwrap_or("");
    let loc = match f.get("file") {
        Some(v) if js_truthy(v) => {
            let no_ticks: String = js_to_string(v).chars().filter(|c| *c != '`').collect();
            let line_part = if js_truthy(f.get("line").unwrap_or(&Value::Null)) {
                format!(":{}", tmpl(f.get("line")))
            } else {
                String::new()
            };
            format!(" `{no_ticks}{line_part}`")
        }
        _ => String::new(),
    };
    format!("- {}{loc} {safe_title}", sev_label(severity))
}

/// Δ-блок + история прогонов (buildDeltaSection). Маркеры не нужны.
pub fn build_delta_section(
    current: Option<&Value>,
    prior: Option<&Value>,
    prior_runs: Option<&Value>,
    prior_sha: Option<&Value>,
    current_run: Option<&Value>,
) -> Result<String> {
    let prior_arr = arr_or_empty(prior)?;
    let runs_arr = arr_or_empty(prior_runs)?;
    if prior_arr.is_empty() && runs_arr.is_empty() {
        return Ok(String::new());
    }
    let delta = diff_findings(current, prior)?;
    let stability = findings_stability(current, prior, true)?;
    let stability_note = match stability {
        None => String::new(),
        Some(v) => format!(", стабильность {}", format_stability(Some(v))),
    };
    let sha_note = if js_truthy(prior_sha.unwrap_or(&Value::Null)) {
        format!(" (прошлый прогон `{}`)", tmpl(prior_sha))
    } else {
        String::new()
    };
    let mut md = format!(
        "\n> 🔄 **С прошлого прогона{sha_note}:** +{} новых, −{} закрыто, {} без изменений{stability_note}.\n",
        delta.added.len(),
        delta.gone.len(),
        delta.kept.len(),
    );
    if !delta.added.is_empty() || !delta.gone.is_empty() {
        let mut parts = Vec::new();
        if !delta.added.is_empty() {
            let lines = delta.added.iter().take(10).map(delta_line).collect::<Vec<_>>().join("\n");
            parts.push(format!("**Новые ({})**\n{lines}", delta.added.len()));
        }
        if !delta.gone.is_empty() {
            let lines = delta.gone.iter().take(10).map(delta_line).collect::<Vec<_>>().join("\n");
            parts.push(format!("**Закрыты или сняты ({})**\n{lines}", delta.gone.len()));
        }
        md.push_str(&format!("\n<details><summary>🔄 Что изменилось</summary>\n\n{}\n\n</details>\n", parts.join("\n\n")));
    }
    if !runs_arr.is_empty() {
        let rows: Vec<String> = runs_arr
            .iter()
            .rev()
            .take(5)
            .rev()
            .map(|r| {
                let c = r.get("counts").filter(|v| v.is_object());
                let cell = |key: &str| match c {
                    None => "0".to_string(),
                    Some(obj) => nullish_zero(obj.get(key)),
                };
                let sha = match r.get("sha") {
                    Some(v) if js_truthy(v) => format!("`{}`", tmpl(r.get("sha"))),
                    _ => "`—`".to_string(),
                };
                format!(
                    "| {sha} | {} | {} | {} | {} | {} | +{} / −{} | {} |",
                    verdict_mark(r.get("blocked")),
                    cell("critical"),
                    cell("major"),
                    cell("minor"),
                    cell("nit"),
                    nullish_zero(r.get("added")),
                    nullish_zero(r.get("gone")),
                    format_stability(r.get("stability").and_then(Value::as_f64)),
                )
            })
            .collect();
        let mut all_runs: Vec<Value> = runs_arr.to_vec();
        let repro = match current_run {
            Some(cr) => {
                all_runs.push(cr.clone());
                verdict_reproducibility(Some(&Value::Array(all_runs)), cr.get("sha"))?
            }
            None => None,
        };
        let repro_line = match &repro {
            Some(r) if r.flaky => format!(
                "\n\n⛔ **Вердикт на `{}` невоспроизводим: {} из {} прогонов на этом коммите блокировали merge.** Дифф между ними не менялся — значит, недетерминирован сам гейт, а не код.",
                tmpl(current_run.and_then(|cr| cr.get("sha"))),
                r.blocked,
                r.total,
            ),
            Some(r) => format!(
                "\n\n{} Вердикт на `{}` воспроизводим: {} на этом коммите.",
                if r.blocked > 0 { "⛔" } else { "✅" },
                tmpl(current_run.and_then(|cr| cr.get("sha"))),
                if r.blocked > 0 {
                    format!("блокировали все {} прогонов", r.total)
                } else {
                    format!("ни один из {} прогонов", r.total)
                },
            ),
            None => String::new(),
        };
        let summary_line = match &repro {
            Some(r) if r.flaky => format!(
                "📈 История прогонов · ⛔ вердикт нестабилен ({} из {} на этом коммите)",
                r.blocked, r.total
            ),
            _ => "📈 История прогонов".to_string(),
        };
        md.push_str(&format!(
            "\n<details><summary>{summary_line}</summary>\n\n| sha | гейт | 🔴 | 🟠 | 🟡 | 🔵 | Δ | стаб. |\n|---|---|---|---|---|---|---|---|\n{}{}\n\n</details>\n",
            rows.join("\n"),
            repro_line,
        ));
    }
    Ok(md)
}

// ---------------------------------------------------------------------------
// Walkthrough / LLM-unavailable / router summary
// ---------------------------------------------------------------------------

/// Walkthrough в начало sticky (buildWalkthroughSection): самое длинное
/// осмысленное summary (первое из максимальных выигрывает, как reduce в JS).
pub fn build_walkthrough_section(reviews: Option<&Value>) -> Result<String> {
    let ok: Vec<&Value> = arr_or_empty(reviews)?
        .iter()
        .filter(|r| !js_truthy(r.get("error").unwrap_or(&Value::Null)) && !js_trim(&str_coalesce(r.get("summary"), "")).is_empty())
        .collect();
    if ok.is_empty() {
        return Ok(String::new());
    }
    let mut best = ok[0];
    let mut best_len = utf16_len(js_trim(&str_coalesce(best.get("summary"), "")));
    for r in &ok[1..] {
        let len = utf16_len(js_trim(&str_coalesce(r.get("summary"), "")));
        if len > best_len {
            best = r;
            best_len = len;
        }
    }
    let summary = js_slice_utf16(&js_collapse_whitespace(js_trim(&str_coalesce(best.get("summary"), ""))), 600);
    if summary.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("### Walkthrough\n\n{summary}\n\n"))
}

/// Обратные кавычки вокруг сырого значения (code() в buildLlmUnavailableSummary):
/// delimiter из `` ` `` длиной max_run+1 (минимум 1), чтобы сырые backticks не ломали.
fn md_code(raw: &str) -> String {
    let max_run = {
        let mut max = 0usize;
        let mut cur = 0usize;
        for c in raw.chars() {
            if c == '`' {
                cur += 1;
                max = max.max(cur);
            } else {
                cur = 0;
            }
        }
        max
    };
    // JS: Math.max(1, ...runs) + 1 — delimiter минимум 2 (даже без backticks в raw).
    let delimiter = "`".repeat(max_run.max(1) + 1);
    format!("{delimiter}{raw}{delimiter}")
}

/// Экранирование inline-текста (text() в buildLlmUnavailableSummary).
fn md_text(value: Option<&Value>, fallback: &str) -> String {
    let raw = str_coalesce(value, fallback);
    let mut out = String::with_capacity(raw.len() + 8);
    for c in raw.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '#' | '+' | '.' | '!' | '|' | '~' | '<' | '>') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Sticky для полного LLM-аутеджа: fail-open/fail-closed единым formatter'ом.
pub fn build_llm_unavailable_summary(
    llm_required: bool,
    why_public: Option<&Value>,
    models: Option<&Value>,
    prescan_blockers: Option<&Value>,
    prescan_advisory: Option<&Value>,
    review_marker: &str,
) -> Result<String> {
    let why = str_coalesce(why_public, "—");
    let format_findings = |findings: &[Value]| -> String {
        findings
            .iter()
            .map(|f| {
                let line = match f.get("line").and_then(js_to_number) {
                    Some(n) if n.is_finite() && n > 0.0 => format!(":{}", js_number_to_string(n)),
                    _ => String::new(),
                };
                let raw_loc = format!("{}{line}", str_coalesce(f.get("file"), "—"));
                format!(
                    "- {} **{}** {}",
                    md_code(&raw_loc),
                    md_text(f.get("severity"), "minor"),
                    md_text(f.get("title"), "замечание"),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let blockers = arr_or_empty(prescan_blockers)?;
    let advisory = arr_or_empty(prescan_advisory)?;
    let blocker_section = if !blockers.is_empty() {
        format!(
            "\n\n### ⛔ Prescan-находки ({}) — требуют исправления независимо от LLM\n{}",
            blockers.len(),
            format_findings(blockers),
        )
    } else {
        String::new()
    };
    let advisory_section = if !advisory.is_empty() {
        format!(
            "\n\n### 🔧 Prescan-находки (advisory, {})\n{}",
            advisory.len(),
            format_findings(advisory),
        )
    } else {
        String::new()
    };
    let prescan_section = format!("{blocker_section}{advisory_section}");
    let model_list = arr_or_empty(models)?.iter().map(|m| js_to_string(m)).collect::<Vec<_>>().join(", ");
    let md = if llm_required {
        format!(
            "{review_marker}\n## 🤖 Авто код-ревью\n\n⛔ ИИ-ревью недоступно ({why}).{prescan_section}\n\n<sub>Модели: {model_list}. Без LLM-ревью гейт не может подтвердить PR — merge заблокирован (fail-closed, `REVIEW_LLM_REQUIRED=1`).{} Перезапустите job позже; осознанное ослабление — `REVIEW_LLM_REQUIRED=0`. Детали — в логе CI job.</sub>",
            if !blockers.is_empty() { " Prescan-находки также блокируют merge." } else { "" },
        )
    } else {
        format!(
            "{review_marker}\n## 🤖 Авто код-ревью\n\n{} ИИ-ревью временно недоступно ({why}).{prescan_section}\n\n<sub>Модели: {model_list}.{} Детали — в логе CI job.</sub>",
            if !blockers.is_empty() { "⛔" } else { "⚠️" },
            if !blockers.is_empty() { " Prescan-находки блокируют merge." } else { " Это не блокирует merge." },
        )
    };
    if utf16_len(&md) > STICKY_LIMIT {
        Ok(format!(
            "{}\n\n> …(отчёт обрезан под лимит GitHub)\n",
            js_slice_utf16(&md, STICKY_LIMIT - 100)
        ))
    } else {
        Ok(md)
    }
}

/// Sticky для skip/light (R4) — без LLM, merge не блокируется (buildRouterSummary).
pub fn build_router_summary(
    route: &Value,
    head_sha: Option<&Value>,
    changed_paths: Option<&Value>,
    router_enabled: bool,
    review_marker: &str,
) -> Result<String> {
    let route_name = str_coalesce(route.get("route"), "");
    let reason = str_coalesce(route.get("reason"), "—");
    let labels: BTreeMap<&str, &str> = [("skip", "⏭ пропуск LLM"), ("light", "📄 docs/config-only")].into_iter().collect();
    let title = labels.get(route_name.as_str()).copied().unwrap_or(&route_name);
    let mut md = format!("{review_marker}\n## 🤖 Авто код-ревью — {title}\n\n");
    md.push_str(&format!(
        "> ✅ **Merge не блокируется:** роутер классифицировал PR как `{route_name}` ({reason}). ИИ-ревью кода не запускалось — экономия токенов на lockfile/docs/rename-only.\n\n"
    ));
    md.push_str(&format!("### Изменённые файлы ({})\n", arr_or_empty(changed_paths)?.len()));
    for f in arr_or_empty(route.get("files"))? {
        md.push_str(&format!("- `{}` _({})_\n", tmpl(f.get("path")), tmpl(f.get("class"))));
    }
    let sha = {
        let s = tmpl(head_sha);
        let s7 = js_slice_utf16(&s, 7);
        if s7.is_empty() {
            "—".to_string()
        } else {
            s7
        }
    };
    md.push_str(&format!(
        "\n<sub>Router R4 · commit {sha} · REVIEW_ROUTER={}</sub>",
        if router_enabled { "1" } else { "0" },
    ));
    Ok(md)
}

// ---------------------------------------------------------------------------
// formatCostStickyLine (lib/cost.mjs)
// ---------------------------------------------------------------------------

/// `💰 ~$0.12 · 45k tokens` для sticky; '' без usage (formatCostStickyLine).
/// toFixed — Rust decimal rounding: расхождение с V8 только на точных бинарных
/// тайях (0.125), реальными суммами стоимости не достигается.
pub fn format_cost_sticky_line(snapshot: Option<&Value>) -> String {
    let Some(snap) = snapshot else { return String::new() };
    if !js_truthy(snap.get("total_tokens").unwrap_or(&Value::Null)) {
        return String::new();
    }
    // total_tokens на практике — число из costTracker.snapshot(); нечисловые
    // truthy-значения JS коэрсит по месту — здесь они дают 0 (не встречаются).
    let tokens = snap.get("total_tokens").and_then(js_to_number).unwrap_or(0.0);
    let tok_str = if tokens >= 1_000_000.0 {
        format!("{}M", js_to_fixed(tokens / 1_000_000.0, 1))
    } else if tokens >= 1000.0 {
        format!("{}k", js_number_to_string((tokens / 1000.0).round()))
    } else {
        js_number_to_string(tokens)
    };
    let cost_known = matches!(snap.get("cost_known"), Some(Value::Bool(true)));
    let cost_usd = snap.get("cost_usd").and_then(js_to_number).unwrap_or(0.0);
    if cost_known && cost_usd > 0.0 {
        let usd = if cost_usd < 0.01 {
            js_to_fixed(cost_usd, 3)
        } else {
            js_to_fixed(cost_usd, 2)
        };
        return format!("💰 ~${usd} · {tok_str} tokens");
    }
    if tokens > 0.0 {
        return format!("💰 {tok_str} tokens");
    }
    String::new()
}

// ---------------------------------------------------------------------------
// buildSummary — полный sticky
// ---------------------------------------------------------------------------

/// Результат buildSummary: md + счётчики + Δ-метрика + вердикт.
#[derive(Debug, Clone, Serialize)]
pub struct SummaryOutcome {
    pub md: String,
    pub counts: Value,
    pub total: usize,
    pub delta: Value,
    pub verdict: Value,
}

/// Конфиг рендера (slim-версия cfg из loadReviewConfig — собирает обёртка).
#[derive(Debug, Clone)]
pub struct SummaryCfg {
    pub verifier_model: String,
    pub gate_block_on: Vec<String>,
    pub gate_block_single: Vec<String>,
    pub gate_consensus_min: Value,
    pub gate_confidence_min: Value,
    pub review_mode: String,
    pub models_list: Vec<String>,
    pub moa_aggregator: String,
    pub moa_references: Vec<String>,
    pub verifier_enabled: bool,
    pub specialists_enabled: bool,
    pub specialists_assignments: Vec<Value>,
    pub reasoning_in_sticky: bool,
}

impl SummaryCfg {
    pub fn from_value(cfg: Option<&Value>) -> Result<SummaryCfg> {
        let cfg = cfg.unwrap_or(&Value::Null);
        let gate = cfg.get("gate").unwrap_or(&Value::Null);
        let str_vec = |v: Option<&Value>| -> Vec<String> {
            match v {
                Some(Value::Array(a)) => a.iter().map(|x| js_to_string(x)).collect(),
                _ => Vec::new(),
            }
        };
        let models = cfg.get("models").unwrap_or(&Value::Null);
        let moa = cfg.get("moa").unwrap_or(&Value::Null);
        let verifier = cfg.get("verifier").unwrap_or(&Value::Null);
        let specialists = cfg.get("specialists").unwrap_or(&Value::Null);
        let export = cfg.get("export").unwrap_or(&Value::Null);
        Ok(SummaryCfg {
            verifier_model: tmpl(verifier.get("model")),
            gate_block_on: str_vec(gate.get("blockOn")),
            gate_block_single: str_vec(gate.get("blockSingle")),
            gate_consensus_min: gate.get("consensusMin").cloned().unwrap_or(Value::Null),
            gate_confidence_min: gate.get("confidenceMin").cloned().unwrap_or(Value::Null),
            review_mode: str_coalesce(cfg.get("reviewMode"), "consensus"),
            models_list: str_vec(models.get("list")),
            moa_aggregator: tmpl(moa.get("aggregator")),
            moa_references: str_vec(moa.get("references")),
            verifier_enabled: matches!(verifier.get("enabled"), Some(Value::Bool(true))),
            specialists_enabled: matches!(specialists.get("enabled"), Some(Value::Bool(true))),
            specialists_assignments: match specialists.get("assignments") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            },
            reasoning_in_sticky: match export.get("reasoningInSticky") {
                Some(Value::Bool(b)) => *b,
                Some(Value::Null) | None => true, // undefined → !== false
                Some(_) => true,
            },
        })
    }
}

/// Счётчики по severity с фиксированным порядком ключей (как JS-литерал).
fn counts_object() -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    for (k, _) in SEV {
        m.insert((*k).to_string(), json!(0));
    }
    m
}

/// Баннер «verifier недоступен» (RFC 18) — паритетно verifierUnavailableBannerJs.
fn verifier_unavailable_banner(vu: &Value) -> String {
    let reason = tmpl(vu.get("reason"));
    let unverified = vu.get("unverified").and_then(Value::as_f64).unwrap_or(0.0);
    let n = format!(
        "{} замечани{} уровня блока",
        tmpl(vu.get("unverified")),
        if unverified == 1.0 { "е" } else { "й" },
    );
    if vu.get("blocked").and_then(Value::as_bool).unwrap_or(false) {
        format!(
            "> ⛔ **Merge заблокирован: verifier недоступен** ({reason}) — {n} не проверены вторым судьёй и показаны как minor. Перезапустите ревью. Пропускать merge при сбое verifier — `REVIEW_LLM_REQUIRED=0`.\n\n"
        )
    } else {
        format!(
            "> ⚠️ **Verifier недоступен** ({reason}) — {n} не проверены и понижены до minor; merge не блокируется (`REVIEW_LLM_REQUIRED=0`).\n\n"
        )
    }
}

/// Полный sticky (buildSummary). cfg — slim-конфиг (SummaryCfg::from_value),
/// input — сериализованный input pipeline'а (см. шапку модуля), markers —
/// MARKERS.review/dataKey активного профиля (аргументом, как MARKERS.inline
/// в findings: профиль живёт на JS-стороне).
pub fn build_summary(
    cfg: Option<&Value>,
    input: Option<&Value>,
    file_texts: Option<&Value>,
    blocking: usize,
    review_marker: &str,
    data_key: &str,
) -> Result<SummaryOutcome> {
    let cfg = SummaryCfg::from_value(cfg)?;
    let input = input.unwrap_or(&Value::Null);
    let reviews = arr_or_empty(input.get("reviews"))?;
    let all_findings = arr_or_empty(input.get("allFindings"))?;
    let outside = arr_or_empty(input.get("outside"))?;
    let blast_blocks = arr_or_empty(input.get("blastBlocks"))?;
    let prior_findings = arr_or_empty(input.get("priorFindings"))?;
    let prior_runs = arr_or_empty(input.get("priorRuns"))?;
    let blast_metrics = input.get("blastMetrics").filter(|v| v.is_object());
    let note = str_coalesce(input.get("note"), "");
    let head_sha = input.get("headSha");
    // Verifier недоступен (RFC 18): {reason, unverified, blocked} или null.
    let verifier_unavailable = input.get("verifierUnavailable").filter(|v| v.is_object());
    let degraded = verifier_unavailable
        .and_then(|v| v.get("blocked"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Тексты файлов диффа (для запасного fp) — карта из обёртки.
    let texts: BTreeMap<String, LineText> = match file_texts {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(path, t)| {
                let text: LineText = match t {
                    Value::Object(tm) => tm
                        .iter()
                        .filter_map(|(k, v)| Some((k.parse().ok()?, v.as_str()?.to_string())))
                        .collect(),
                    _ => BTreeMap::new(),
                };
                (path.clone(), text)
            })
            .collect(),
        Some(_) => bail!("fileTexts должен быть объектом"),
    };

    let mut counts = counts_object();
    for f in all_findings {
        if let Some(sev) = f.get("severity").and_then(Value::as_str) {
            if let Some(slot) = counts.get_mut(sev) {
                *slot = json!(slot.as_f64().unwrap_or(0.0) + 1.0);
            }
        }
    }
    let total = all_findings.len();
    let verdict_ru = if blocking > 0 {
        "🔴 нужны правки"
    } else if degraded {
        "⛔ ревью не завершено"
    } else if total > 0 {
        "💬 есть замечания"
    } else {
        "✅ можно мёржить"
    };
    let consensus_tier: Vec<&String> = cfg.gate_block_on.iter().filter(|s| !cfg.gate_block_single.contains(s)).collect();
    let rule_note = if cfg.review_mode == "moa" {
        format!(
            "{} — от MoA-aggregator; confidence ≥{}",
            cfg.gate_block_on.join("/"),
            tmpl(Some(&cfg.gate_confidence_min)),
        )
    } else {
        format!(
            "{} — от любой модели{}; confidence ≥{} для блока",
            cfg.gate_block_single.join("/"),
            if !consensus_tier.is_empty() {
                format!("; {} — по консенсусу ≥{} модел.", consensus_tier.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("/"), tmpl(Some(&cfg.gate_consensus_min)))
            } else {
                String::new()
            },
            tmpl(Some(&cfg.gate_confidence_min)),
        )
    };

    let mut md = format!("{review_marker}\n## 🤖 Авто код-ревью — {verdict_ru}\n\n");
    if blocking > 0 {
        md.push_str(&format!(
            "> ⛔ **Merge заблокирован:** {} замечани{}. Правило блокировки: {rule_note}. Исправьте (или ослабьте `REVIEW_BLOCK_ON`/`REVIEW_BLOCK_SINGLE`/`REVIEW_CONSENSUS`).\n\n",
            blocking,
            if blocking == 1 { "е" } else { "й" },
        ));
    } else {
        let has_block_tier = all_findings.iter().any(|f| {
            f.get("severity").and_then(Value::as_str).map(|s| cfg.gate_block_on.iter().any(|b| b == s)).unwrap_or(false)
        });
        if has_block_tier && cfg.review_mode != "moa" {
            md.push_str(&format!(
                "> ✅ Блокеров нет: {} есть лишь у одной модели — показаны как замечания (для блока нужен консенсус ≥{}); {} не найдено.\n\n",
                if !consensus_tier.is_empty() { consensus_tier.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("/") } else { cfg.gate_block_on.join("/") },
                tmpl(Some(&cfg.gate_consensus_min)),
                cfg.gate_block_single.join("/"),
            ));
        } else if has_block_tier && cfg.review_mode == "moa" {
            md.push_str(&format!(
                "> ✅ Merge не блокируется: aggregator не подтвердил {} как блокеры.\n\n",
                cfg.gate_block_on.join("/"),
            ));
        }
    }
    if let Some(vu) = verifier_unavailable {
        md.push_str(&verifier_unavailable_banner(vu));
    }

    md.push_str(&build_walkthrough_section(Some(&Value::Array(reviews.to_vec())))?);

    md.push_str("### Оценки моделей\n");
    for r in reviews {
        let fb_note = if js_truthy(r.get("fallbackFor").unwrap_or(&Value::Null)) {
            format!(" _(резерв вместо {})_", tmpl(r.get("fallbackFor")))
        } else {
            String::new()
        };
        if js_truthy(r.get("error").unwrap_or(&Value::Null)) {
            md.push_str(&format!("- **{}**{} — ⚠️ недоступна ({})\n", tmpl(r.get("model")), fb_note, public_error_label(r.get("error"))));
            continue;
        }
        let mut c = counts_object();
        for f in arr_or_empty(r.get("findings"))? {
            if let Some(sev) = f.get("severity").and_then(Value::as_str) {
                if let Some(slot) = c.get_mut(sev) {
                    *slot = json!(slot.as_f64().unwrap_or(0.0) + 1.0);
                }
            }
        }
        let findings_len = arr_or_empty(r.get("findings"))?.len();
        md.push_str(&format!(
            "- **{}**{} — {} ({}{} {}{} {}{} {}{}): {}\n",
            tmpl(r.get("model")),
            fb_note,
            findings_len,
            sev_label("critical"), nullish_zero(c.get("critical")),
            sev_label("major"), nullish_zero(c.get("major")),
            sev_label("minor"), nullish_zero(c.get("minor")),
            sev_label("nit"), nullish_zero(c.get("nit")),
            str_coalesce(r.get("summary"), ""),
        ));
        for refv in arr_or_empty(r.get("references"))? {
            let label = match refv.get("role") {
                Some(v) if js_truthy(v) => format!("{}/{}", js_to_string(v), tmpl(refv.get("model"))),
                _ => tmpl(refv.get("model")),
            };
            if js_truthy(refv.get("error").unwrap_or(&Value::Null)) {
                md.push_str(&format!("  - specialist **{label}** — ⚠️ {}\n", public_error_label(refv.get("error"))));
            } else {
                md.push_str(&format!("  - specialist **{label}** — ok ({} симв.)\n", utf16_len(&str_coalesce(refv.get("text"), ""))));
            }
        }
    }

    let file_count = input.get("fileCount").and_then(js_to_number).unwrap_or(0.0);
    md.push_str(&format!(
        "\n**Итого:** {total} замечаний — {}{} {}{} {}{} {}{}. Файлов в диффе: {}.\n",
        sev_label("critical"), nullish_zero(counts.get("critical")),
        sev_label("major"), nullish_zero(counts.get("major")),
        sev_label("minor"), nullish_zero(counts.get("minor")),
        sev_label("nit"), nullish_zero(counts.get("nit")),
        js_number_to_string(file_count),
    ));
    let cost_line = format_cost_sticky_line(input.get("costSnapshot"));
    if !cost_line.is_empty() {
        md.push_str(&format!("\n> {cost_line}\n"));
    }

    // Отпечатки считаются ОДИН раз: они нужны и Δ-блоку, и футеру.
    let mut slim: Vec<Value> = Vec::with_capacity(all_findings.len());
    for f in all_findings {
        let fp = match f.get("fp") {
            Some(v) if js_truthy(v) => Some(js_to_string(v)),
            _ => {
                let text = f.get("file").and_then(Value::as_str).and_then(|p| texts.get(p));
                let fp = finding_fingerprint(text, f.get("line").unwrap_or(&Value::Null));
                if fp.is_empty() { None } else { Some(fp) }
            }
        };
        let mut o = serde_json::Map::new();
        o.insert("file".into(), f.get("file").cloned().unwrap_or(Value::Null));
        o.insert("line".into(), f.get("line").cloned().unwrap_or(Value::Null));
        o.insert("severity".into(), f.get("severity").cloned().unwrap_or(Value::Null));
        o.insert("title".into(), f.get("title").cloned().unwrap_or(Value::Null));
        o.insert("model".into(), f.get("model").cloned().unwrap_or(Value::Null));
        if js_gt(f.get("dupCount"), 1.0) {
            o.insert("dupCount".into(), f.get("dupCount").cloned().unwrap_or(Value::Null));
        }
        if let Some(fp) = fp {
            o.insert("fp".into(), Value::String(fp));
        }
        slim.push(Value::Object(o));
    }
    let slim_arr = Value::Array(slim);
    let delta = diff_findings(Some(&slim_arr), Some(&Value::Array(prior_findings.to_vec())))?;
    let points = finding_point_delta(Some(&slim_arr), Some(&Value::Array(prior_findings.to_vec())))?;
    let prior_known = !prior_findings.is_empty() || !prior_runs.is_empty();
    let stability = findings_stability(Some(&slim_arr), Some(&Value::Array(prior_findings.to_vec())), prior_known)?;

    let sha7 = js_slice_utf16(&tmpl(head_sha), 7);
    let mut current_run = serde_json::Map::new();
    current_run.insert("sha".into(), Value::String(sha7.clone()));
    current_run.insert("counts".into(), Value::Object(counts.clone()));
    current_run.insert("added".into(), json!(delta.added.len()));
    current_run.insert("gone".into(), json!(delta.gone.len()));
    if let Some(v) = stability {
        current_run.insert("stability".into(), json!(v));
    }
    current_run.insert("blocked".into(), Value::Bool(blocking > 0 || degraded));
    let current_run = Value::Object(current_run);

    md.push_str(&build_delta_section(
        Some(&slim_arr),
        Some(&Value::Array(prior_findings.to_vec())),
        Some(&Value::Array(prior_runs.to_vec())),
        input.get("priorSha"),
        Some(&current_run),
    )?);

    let verifier_stats = input.get("verifierStats").filter(|v| v.is_object());
    if let Some(vs) = verifier_stats {
        if cfg.verifier_enabled
            && (js_truthy(vs.get("rejected").unwrap_or(&Value::Null))
                || js_truthy(vs.get("downgraded").unwrap_or(&Value::Null))
                || js_truthy(vs.get("verified").unwrap_or(&Value::Null))
                || js_truthy(vs.get("unverified").unwrap_or(&Value::Null)))
        {
            let unv = if js_truthy(vs.get("unverified").unwrap_or(&Value::Null)) {
                format!(", без подтверждения → minor {}", tmpl(vs.get("unverified")))
            } else {
                String::new()
            };
            md.push_str(&format!(
                "\n> 🔎 Verifier (Pass B, {}): подтверждено {}, отклонено FP {}, понижено {}{unv}.\n",
                cfg.verifier_model.clone(),
                nullish_zero(vs.get("verified")),
                nullish_zero(vs.get("rejected")),
                nullish_zero(vs.get("downgraded")),
            ));
        }
    }
    let refutation_stats = input.get("refutationStats").filter(|v| v.is_object());
    if let Some(rs) = refutation_stats {
        if cfg.specialists_enabled && (js_truthy(rs.get("dropped").unwrap_or(&Value::Null)) || js_truthy(rs.get("downgraded").unwrap_or(&Value::Null))) {
            let nc = if js_truthy(rs.get("downgradedNoClaims").unwrap_or(&Value::Null)) {
                format!(", noClaims→minor {}", tmpl(rs.get("downgradedNoClaims")))
            } else {
                String::new()
            };
            md.push_str(&format!(
                "\n> ⚖️ Refutation (R5 jury): dropped {}, downgraded {}{nc}, kept {}.\n",
                nullish_zero(rs.get("dropped")),
                nullish_zero(rs.get("downgraded")),
                nullish_zero(rs.get("kept")),
            ));
        }
    }
    let prescan = input.get("prescanResult").filter(|v| v.is_object());
    if let Some(ps) = prescan {
        let pf = arr_or_empty(ps.get("findings"))?;
        if !pf.is_empty() {
            let tools = match ps.get("byTool").filter(|v| v.is_object()) {
                Some(bt) => format!("eslint {} + ast-grep {}", nullish_zero(bt.get("eslint")), nullish_zero(bt.get("astgrep"))),
                None => format!("{}", pf.len()),
            };
            md.push_str(&format!(
                "\n> 🔧 Prescan (R6 eslint + ast-grep): {tools} детерминированных finding(s) на {} файлах (LLM dedup).\n",
                tmpl(ps.get("scanned")),
            ));
        }
    }
    if js_truthy(input.get("note").unwrap_or(&Value::Null)) {
        md.push_str(&format!("\n> ⚠️ {note}\n"));
    }

    if !outside.is_empty() {
        let rank = |sev: Option<&str>| -> i32 {
            match sev {
                Some("critical") => 0,
                Some("major") => 1,
                Some("minor") => 2,
                Some("nit") => 3,
                _ => 9,
            }
        };
        // Стабильная сортировка по rank (как Array.prototype.sort в JS).
        let mut sorted: Vec<&Value> = outside.iter().collect();
        sorted.sort_by_key(|f| rank(f.get("severity").and_then(Value::as_str)));
        let (shown, tail) = sorted.split_at(OUTSIDE_MAX.min(sorted.len()));
        md.push_str("\n### Замечания вне строк диффа\n");
        for f in shown {
            let loc = match f.get("file") {
                Some(v) if js_truthy(v) => {
                    let line_part = if js_truthy(f.get("line").unwrap_or(&Value::Null)) {
                        format!(":{}", tmpl(f.get("line")))
                    } else {
                        String::new()
                    };
                    format!(" (`{}`{line_part})", code_path(&js_to_string(v)))
                }
                _ => String::new(),
            };
            let dup = if js_gt(f.get("dupCount"), 1.0) {
                format!(" ×{}", tmpl(f.get("dupCount")))
            } else {
                String::new()
            };
            md.push_str(&format!(
                "- {} **{}**¹{dup}{loc} — {}\n",
                sev_label(f.get("severity").and_then(Value::as_str).unwrap_or("")),
                escape_md_inline(&str_coalesce(f.get("title"), "замечание")),
                escape_md_block(&str_coalesce(f.get("body"), "")),
            ));
        }
        if !tail.is_empty() {
            let rows = tail
                .iter()
                .map(|f| {
                    let loc = match f.get("file") {
                        Some(v) if js_truthy(v) => {
                            let line_part = if js_truthy(f.get("line").unwrap_or(&Value::Null)) {
                                format!(":{}", tmpl(f.get("line")))
                            } else {
                                String::new()
                            };
                            format!(" (`{}`{line_part})", code_path(&js_to_string(v)))
                        }
                        _ => String::new(),
                    };
                    let dup = if js_gt(f.get("dupCount"), 1.0) {
                        format!(" ×{}", tmpl(f.get("dupCount")))
                    } else {
                        String::new()
                    };
                    format!("- {} **{}**{dup}{loc}", sev_label(f.get("severity").and_then(Value::as_str).unwrap_or("")), escape_md_inline(&str_coalesce(f.get("title"), "замечание")))
                })
                .collect::<Vec<_>>()
                .join("\n");
            md.push_str(&format!(
                "\n<details><summary>…и ещё {} advisory</summary>\n\n{}\n\n<sub>Полные тела — в artifact `pr-review-export` (JSON/SARIF).</sub>\n\n</details>\n",
                tail.len(),
                rows,
            ));
        }
    }

    let fp_removed = arr_or_empty(input.get("fpRemoved"))?;
    if !fp_removed.is_empty() {
        let rows = fp_removed
            .iter()
            .take(10)
            .map(|f| {
                let loc = match f.get("file") {
                    Some(v) if js_truthy(v) => {
                        let line_part = if js_truthy(f.get("line").unwrap_or(&Value::Null)) {
                            format!(":{}", tmpl(f.get("line")))
                        } else {
                            String::new()
                        };
                        // fpRemoved: `:line` ВНУТРИ backticks (в отличие от outside-секции).
                        format!(" (`{}{}`)", code_path(&js_to_string(v)), line_part)
                    }
                    _ => String::new(),
                };
                format!(
                    "- {} **{}** _({})_{loc}",
                    sev_label(f.get("severity").and_then(Value::as_str).unwrap_or("")),
                    escape_md_inline(&str_coalesce(f.get("title"), "замечание")),
                    escape_md_inline(&tmpl(f.get("model"))),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        md.push_str(&format!(
            "\n<details><summary>🗑 Отфильтровано как известные FP ({})</summary>\n\n{rows}\n\n",
            fp_removed.len(),
        ));
        md.push_str("> _Скрыто FP-паттернами из feedback (`ops/pr-review/feedback/fp-patterns.jsonl`) или сверкой с деревом репозитория («файла нет» о файле вне PR; путь — в логе прогона). Если среди них есть верное замечание — это ошибка фильтра, поправьте его._\n\n</details>\n");
    }

    let blast_summary = {
        let m = blast_metrics;
        let resolved = match m {
            Some(m) => format!("{}/{}", match m.get("symbols_resolved") { None | Some(Value::Null) => "—".to_string(), Some(v) => js_to_string(v) }, nullish_zero(m.get("symbols_total"))),
            None => if !blast_blocks.is_empty() { "—/—".to_string() } else { "0/—".to_string() },
        };
        let fresh = match m {
            Some(m) => match m.get("graph_status") {
                Some(v) if js_truthy(v) => js_to_string(v),
                _ => match m.get("graph_fresh") {
                    Some(Value::Bool(true)) => "fresh".to_string(),
                    Some(Value::Bool(false)) => "stale".to_string(),
                    _ => "unknown".to_string(),
                },
            },
            None => "unknown".to_string(),
        };
        let skip = match m {
            Some(m) => match m.get("skip_reason") {
                Some(v) if js_truthy(v) && js_to_string(v) != "none" => format!(" · skip: {}", js_to_string(v)),
                _ => String::new(),
            },
            None => String::new(),
        };
        format!("📡 Blast-radius (Graphify) · {} блоков · symbols {resolved} · graph {fresh}{skip}", blast_blocks.len())
    };
    if !blast_blocks.is_empty() {
        md.push_str(&format!(
            "\n<details><summary>{blast_summary}</summary>\n\n{}\n\n<sub>AST code graph: caller @ path [relation][EXTRACTED|INFERRED]; cross-community — сильнее слой-эвристик. Callers вне диффа — сигнал регрессии для LLM/verifier.</sub>\n\n</details>\n",
            blast_blocks.iter().map(|b| js_to_string(b)).collect::<Vec<_>>().join("\n"),
        ));
    } else {
        md.push_str(&format!(
            "\n<details><summary>{blast_summary}</summary>\n\n_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._\n\n</details>\n",
        ));
    }

    let reasoning_enabled = cfg.reasoning_in_sticky;
    let reasoning = build_reasoning_section(
        Some(&Value::Array(reviews.to_vec())),
        Some(&Value::Array(all_findings.to_vec())),
        reasoning_enabled,
        &cfg.review_mode,
    )?;
    md.push_str(&reasoning.text);

    let gate_note = if !cfg.gate_block_on.is_empty() {
        format!("блокирует merge: {rule_note}")
    } else {
        "ревью советующее".to_string()
    };
    let model_foot = if cfg.review_mode == "moa" {
        if cfg.specialists_enabled {
            format!(
                "MoA specialists: {} · aggregator: {}",
                cfg.specialists_assignments
                    .iter()
                    .map(|a| format!("{}={}", tmpl(a.get("role")), tmpl(a.get("model"))))
                    .collect::<Vec<_>>()
                    .join(", "),
                cfg.moa_aggregator,
            )
        } else {
            format!("MoA advisors: {} · aggregator: {}", cfg.moa_references.join(", "), cfg.moa_aggregator)
        }
    } else {
        format!("Модели: {}", cfg.models_list.join(", "))
    };
    let commit = {
        let s = js_slice_utf16(&tmpl(head_sha), 7);
        if s.is_empty() {
            "—".to_string()
        } else {
            s
        }
    };
    md.push_str(&format!("\n<sub>¹ {model_foot} · commit {commit} · {gate_note}; инфра-сбои merge не блокируют.</sub>"));

    let reasoning_footer = if reasoning_enabled {
        build_sticky_reasoning_footer(Some(&Value::Array(reviews.to_vec())), &cfg.review_mode, STICKY_REASONING_EXCERPT)?
    } else {
        Vec::new()
    };
    let mut runs: Vec<Value> = prior_runs.to_vec();
    runs.push(current_run.clone());
    let runs: Vec<Value> = runs.into_iter().rev().take(5).rev().collect();

    let mut footer_data = serde_json::Map::new();
    footer_data.insert("sha".into(), Value::String(sha7.clone()));
    footer_data.insert("findings".into(), slim_arr.clone());
    footer_data.insert("runs".into(), Value::Array(runs));
    if !reasoning_footer.is_empty() {
        footer_data.insert("reasoning".into(), Value::Array(reasoning_footer));
    }
    let mut footer_json = String::new();
    js_json_stringify(&Value::Object(footer_data), &mut footer_json);
    // `-->`/`<!--` внутри JSON ломают ленивый разбор футера на следующем прогоне:
    // >/< — валидные JSON-escape, в сыром тексте последовательностей больше нет.
    let footer_json = footer_json.replace("-->", "--\\u003e").replace("<!--", "\\u003c!--");
    let footer = format!("\n<!-- {data_key}{footer_json} -->");

    if utf16_len(&md) + utf16_len(&footer) > STICKY_LIMIT {
        md = format!(
            "{}\n\n> …(отчёт обрезан под лимит GitHub)\n",
            js_slice_utf16(&md, (STICKY_LIMIT as isize - utf16_len(&footer) as isize - 120).max(0) as usize),
        );
        if !reasoning.text.is_empty() {
            md.push_str("\n> _Секция reasoning могла быть частично обрезана — см. artifact `pr-review-export`._\n");
        }
    } else if reasoning.truncated {
        md.push_str("\n> _Reasoning частично обрезан в sticky — полные excerpts в JSON artifact._\n");
    }

    let all_runs = {
        let mut v = prior_runs.to_vec();
        v.push(current_run.clone());
        v
    };
    let reproducibility = verdict_reproducibility(Some(&Value::Array(all_runs)), Some(&json!(sha7)))?;

    let mut delta_obj = serde_json::Map::new();
    delta_obj.insert("kept".into(), json!(points.kept));
    delta_obj.insert("added".into(), json!(points.added));
    delta_obj.insert("gone".into(), json!(points.gone));
    delta_obj.insert("stability".into(), match stability {
        None => Value::Null,
        Some(v) => json!(v),
    });
    let mut findings_delta = serde_json::Map::new();
    findings_delta.insert("added".into(), json!(delta.added.len()));
    findings_delta.insert("gone".into(), json!(delta.gone.len()));
    findings_delta.insert("kept".into(), json!(delta.kept.len()));
    delta_obj.insert("findings".into(), Value::Object(findings_delta));

    let mut verdict = serde_json::Map::new();
    verdict.insert("sha".into(), json!(sha7));
    verdict.insert("blocked".into(), Value::Bool(blocking > 0 || degraded));
    verdict.insert("reproducibility".into(), match reproducibility {
        None => Value::Null,
        Some(r) => json!({"total": r.total, "blocked": r.blocked, "flaky": r.flaky}),
    });

    Ok(SummaryOutcome {
        md: format!("{md}{footer}"),
        counts: Value::Object(counts),
        total,
        delta: Value::Object(delta_obj),
        verdict: Value::Object(verdict),
    })
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn f(fp: Option<&str>, file: &str, line: u64, title: &str) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("file".into(), json!(file));
        m.insert("line".into(), json!(line));
        m.insert("severity".into(), json!("major"));
        m.insert("title".into(), json!(title));
        if let Some(fp) = fp {
            m.insert("fp".into(), json!(fp));
        }
        Value::Object(m)
    }

    #[test]
    fn sev_map_matches_constants() {
        let m = sev_map();
        assert_eq!(m["critical"], "🔴");
        assert_eq!(m["major"], "🟠");
        assert_eq!(m["minor"], "🟡");
        assert_eq!(m["nit"], "🔵");
        assert_eq!(sev_label("weird"), "⚪");
    }

    #[test]
    fn finding_keys_fp_vs_loc() {
        let k = finding_keys(&f(Some("abc123"), "a.ts", 1, "«Титул»_с  пробелами"));
        assert_eq!(k.fp.as_deref(), Some("fp:abc123"));
        assert_eq!(k.finding, "fp:abc123|титулс пробелами");
        assert_eq!(k.point, "fp:abc123");
        let legacy = finding_keys(&f(None, "a.ts", 1, "Титул"));
        assert_eq!(legacy.finding, "loc:a.ts:1:титул");
        assert_eq!(legacy.point, "loc:a.ts:1:титул");
        // Без fp, но с координатой: line коэрсится, заголовок нормализуется.
        let nofp = finding_keys(&f(None, "b.ts", 0, "  A  B  "));
        assert_eq!(nofp.loc, "loc:b.ts:0:a b");
    }

    #[test]
    fn diff_findings_identity_by_fp_and_title() {
        let cur = vec![f(Some("x"), "a.ts", 1, "одна"), f(Some("y"), "a.ts", 2, "другая")];
        let prior = vec![f(Some("x"), "a.ts", 40, "одна")];
        let d = diff_findings(Some(&Value::Array(cur)), Some(&Value::Array(prior))).unwrap();
        assert_eq!((d.added.len(), d.kept.len(), d.gone.len()), (1, 1, 0));
        // Тот же fp, другой заголовок → закрыта + новая.
        let cur = vec![f(Some("x"), "a.ts", 1, "новая формулировка")];
        let prior = vec![f(Some("x"), "a.ts", 1, "старая формулировка")];
        let d = diff_findings(Some(&Value::Array(cur)), Some(&Value::Array(prior))).unwrap();
        assert_eq!((d.added.len(), d.kept.len(), d.gone.len()), (1, 0, 1));
    }

    #[test]
    fn point_delta_dedupes_points() {
        let cur = vec![f(Some("x"), "a.ts", 2, "SQL"), f(Some("x"), "a.ts", 2, "обработка ошибок")];
        let prior = vec![f(Some("x"), "a.ts", 2, "SQL")];
        let d = finding_point_delta(Some(&Value::Array(cur)), Some(&Value::Array(prior))).unwrap();
        assert_eq!(d, PointDelta { kept: 1, added: 0, gone: 0, current: 1, prior: 1 });
        assert_eq!(findings_stability(Some(&Value::Array(vec![f(Some("x"), "a.ts", 2, "SQL"), f(Some("x"), "a.ts", 2, "др")])), Some(&Value::Array(vec![f(Some("x"), "a.ts", 2, "SQL")])), true).unwrap(), Some(1.0));
        assert_eq!(findings_stability(Some(&Value::Array(vec![])), Some(&Value::Array(vec![])), true).unwrap(), None);
        assert_eq!(findings_stability(Some(&Value::Array(vec![])), Some(&Value::Array(vec![f(Some("a"), "a", 1, "t")])), false).unwrap(), None);
        // Jaccard 1/(1+13) ≈ 0.07.
        let survivor = vec![f(Some("c0"), "a", 1, "t")];
        let mut big = survivor.clone();
        for i in 0..13 {
            big.push(f(Some(&format!("g{i}")), "a", 1, "t"));
        }
        assert_eq!(findings_stability(Some(&Value::Array(survivor)), Some(&Value::Array(big)), true).unwrap(), Some(0.07));
    }

    #[test]
    fn verdict_reproducibility_semantics() {
        let runs = json!([
            {"sha": "abc1234", "blocked": true},
            {"sha": "abc1234", "blocked": false},
            {"sha": "abc1234"},                       // вердикт неизвестен — не считается
            {"sha": "deadbee", "blocked": true},
        ]);
        let r = verdict_reproducibility(Some(&runs), Some(&json!("abc1234more"))).unwrap().unwrap();
        assert_eq!(r, VerdictRepro { total: 2, blocked: 1, flaky: true });
        assert_eq!(verdict_reproducibility(Some(&runs), Some(&json!("deadbee"))).unwrap(), None);
        assert_eq!(verdict_reproducibility(Some(&runs), Some(&json!(""))).unwrap(), None);
    }

    #[test]
    fn js_json_stringify_matches_v8_escapes() {
        let mut s = String::new();
        js_json_stringify(
            &json!({"a": "x<!--y-->z", "b": "\u{8}\u{c}\t\n\r\"\\", "c": "кириллика 🔥", "n": 2, "f": 0.5, "z": null}),
            &mut s,
        );
        assert_eq!(
            s,
            "{\"a\":\"x<!--y-->z\",\"b\":\"\\u0001\\b\\f\\t\\n\\r\\\"\\\\\",\"c\":\"кириллика 🔥\",\"n\":2,\"f\":0.5,\"z\":null}"
        );
        // NaN/Infinity → null (как JSON.stringify).
        let mut s = String::new();
        js_json_stringify(&json!([1e21, 1e-7, 0.000001, 123456789.123]), &mut s);
        assert_eq!(s, "[1e+21,1e-7,0.000001,123456789.123]");
    }

    #[test]
    fn utf16_slice_boundary_keeps_valid_string() {
        // 199 emoji (398 юнитов) + 'а' — граница 400 юнитов ровно после 'а'.
        let s = format!("{}a", "🔥".repeat(199));
        assert_eq!(utf16_len(&s), 399);
        assert_eq!(js_slice_utf16(&s, 400), s);
        // 199 emoji занимают ровно 398 юнитов — влезают целиком, 'а' (юнит 398) — нет.
        assert_eq!(js_slice_utf16(&s, 398), "🔥".repeat(199));
        assert_eq!(js_slice_utf16(&s, 399), s);
        // Рез на 397: последний emoji (2 юнита) целиком не влезает — 198 штук.
        assert_eq!(js_slice_utf16(&s, 397), "🔥".repeat(198));
    }

    #[test]
    fn delta_line_escapes_markdown() {
        let line = delta_line(&json!({"file": "a`b.ts", "line": 3, "severity": "critical", "title": "x`*_[<|]\\y"}));
        assert_eq!(line, "- 🔴 `ab.ts:3` x\\`\\*\\_\\[\\<\\|\\]\\\\y");
        assert_eq!(delta_line(&json!({"severity": "weird", "title": ""})), "- ⚪ замечание");
    }

    #[test]
    fn cost_line_formats_tokens_and_usd() {
        assert_eq!(format_cost_sticky_line(None), "");
        assert_eq!(format_cost_sticky_line(Some(&json!({"total_tokens": 0}))), "");
        assert_eq!(format_cost_sticky_line(Some(&json!({"total_tokens": 500}))), "💰 500 tokens");
        assert_eq!(format_cost_sticky_line(Some(&json!({"total_tokens": 45321, "cost_known": true, "cost_usd": 0.1234}))), "💰 ~$0.12 · 45k tokens");
        assert_eq!(format_cost_sticky_line(Some(&json!({"total_tokens": 2500000, "cost_known": true, "cost_usd": 12.3456}))), "💰 ~$12.35 · 2.5M tokens");
        assert_eq!(format_cost_sticky_line(Some(&json!({"total_tokens": 45321, "cost_known": false}))), "💰 45k tokens");
    }

    #[test]
    fn summary_footer_round_trips_and_escapes_markers() {
        let cfg = json!({
            "gate": {"blockOn": ["critical", "major"], "blockSingle": ["critical"], "consensusMin": 2, "confidenceMin": 0.7},
            "reviewMode": "consensus",
            "models": {"list": ["m1"]},
            "moa": {"aggregator": "agg", "references": []},
            "verifier": {"enabled": false},
            "specialists": {"enabled": false, "assignments": []},
        });
        let input = json!({
            "reviews": [],
            "allFindings": [{"file": "a.ts", "line": 1, "severity": "major", "title": "цитирует --> маркер", "model": "m1", "fp": "abc123"}],
            "outside": [], "blastBlocks": [],
            "fileCount": 1, "headSha": "abcdef1234567", "note": "",
            "priorFindings": [], "priorRuns": [],
        });
        let out = build_summary(Some(&cfg), Some(&input), None, 0, "<!-- ai-review -->", "ai-review-data:").unwrap();
        assert!(out.md.starts_with("<!-- ai-review -->\n## 🤖 Авто код-ревью"));
        // Сырого `-->` в теле футера нет; JSON после парсинга совпадает.
        let open = out.md.rfind("<!-- ai-review-data:").unwrap();
        let body = &out.md[open + "<!-- ai-review-data:".len()..out.md.len() - 3];
        assert!(!body.contains("-->"));
        let parsed: Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["sha"], "abcdef1");
        assert_eq!(parsed["findings"][0]["fp"], "abc123");
        assert_eq!(parsed["runs"][0]["blocked"], false);
        // Маркер-цитата в заголовке восстановлена в JSON без потерь.
        assert_eq!(parsed["findings"][0]["title"], "цитирует --> маркер");
        assert_eq!(out.total, 1);
        assert_eq!(out.verdict["sha"], "abcdef1");
        assert_eq!(out.verdict["blocked"], false);
    }

    #[test]
    fn llm_unavailable_summary_limit_and_escapes() {
        let md = build_llm_unavailable_summary(
            true,
            Some(&json!("тайм-аут")),
            Some(&json!(["m1"])),
            Some(&json!([{"file": "a.ts", "line": 2, "severity": "major", "title": "x`y"}])),
            None,
            "<!-- ai-review -->",
        )
        .unwrap();
        assert!(md.contains("⛔ ИИ-ревью недоступно (тайм-аут)."));
        // code(): delimiter из 2+ backticks, title-экранирование.
        assert!(md.contains("``a.ts:2`` **major** x\\`y"));
        assert!(md.contains("REVIEW_LLM_REQUIRED=1"));
        assert!(utf16_len(&md) <= STICKY_LIMIT);
    }

    #[test]
    fn public_error_label_categories() {
        assert_eq!(public_error_label(Some(&json!({"message": "aborted"}))), "таймаут");
        assert_eq!(public_error_label(Some(&json!({"message": "ETIMEDOUT"}))), "таймаут");
        assert_eq!(public_error_label(Some(&json!({"message": "hit time budget"}))), "тайм-бюджет");
        assert_eq!(public_error_label(Some(&json!({"message": "fetch failed ECONNREFUSED"}))), "сеть");
        assert_eq!(public_error_label(Some(&json!({"message": "HTTP 429 too many"}))), "HTTP 429 (rate limit)");
        assert_eq!(public_error_label(Some(&json!({"message": "server 500 boom"}))), "HTTP 500");
        assert_eq!(public_error_label(Some(&json!({"message": "unexpected token <"}))), "невалидный ответ");
        assert_eq!(public_error_label(Some(&json!({"message": "что-то ещё"}))), "ошибка вызова");
        assert_eq!(public_error_label(None), "ошибка вызова");
    }

    #[test]
    fn router_summary_renders_routes() {
        let md = build_router_summary(
            &json!({"route": "skip", "reason": "lockfile", "files": [{"path": "p.json", "class": "generated"}]}),
            Some(&json!("abcdef123")),
            Some(&json!([1])),
            true,
            "<!-- ai-review -->",
        )
        .unwrap();
        assert!(md.contains("⏭ пропуск LLM"));
        assert!(md.contains("- `p.json` _(generated)_"));
        assert!(md.contains("commit abcdef1 · REVIEW_ROUTER=1"));
    }
}
