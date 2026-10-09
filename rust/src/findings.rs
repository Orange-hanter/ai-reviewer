//! Порт lib/findings.mjs (Фаза 1, шаг 3): нормализация находок, дедуп внутри
//! одной модели, кластеризация консенсуса, гейт-логика, fingerprint (sha256 кода
//! в локации), инвалидация prior findings по содержимому.
//!
//! FP-regex-матчинг (matchesKnownFalsePositive/filterFalsePositiveFindings)
//! портирован в задаче T07 (RFC 16): курируемые паттерны компилируются
//! `fancy-regex` (см. `crate::fp_patterns`), в том числе 2 паттерна с lookahead
//! `(?=…)`, из-за которых эта часть раньше жила только в JS.
//!
//! Паритет с JS сохранён, включая:
//! - канонизацию входа хеша: `trim()` + схлопывание `\s+` в пробел по JS-классу \s
//!   (включает U+FEFF, которого нет в Unicode White_Space — см. `js_is_whitespace`);
//! - JS-коэрсии Number()/truthiness для line/confidence/severity;
//! - порядок кластеров и clusterModels (порядок первого появления, как в JS Map/Set);
//! - отсутствующие поля вместо undefined (suggestion/quote/advisorClaims пропускаются);
//! - `-0` нормализуется в `0` (+0.0), как Math.max(0, n) в JS.
//!
//! Отклонения (см. parity-прогон rust/parity/findings.parity.mjs):
//! - lone surrogates во входе заменяются U+FFFD на парсинге JSON (String Rust
//!   не может их хранить) — тот же артефакт, что и в json_repair;
//! - JS Number(string) для экзотики ("0x10", "1_000") не эмулируется полностью;
//! - non-finite аргументы (NaN/Infinity) обёртка канонизирует в null ДО вызова.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::fp_patterns::{match_fp_patterns, FpPattern};

pub const BLAST_FINDING_SOURCE: &str = "blast-radius";

const SEVERITIES: [&str; 4] = ["critical", "major", "minor", "nit"];
const SPECIALIST_ROLES: [&str; 3] = ["security", "correctness", "architecture"];
const CLAIM_STATUSES: [&str; 3] = ["verified", "rejected", "downgraded"];

// ---------------------------------------------------------------------------
// JS-семантика: пробелы, коэрсии, строкификация
// ---------------------------------------------------------------------------

/// JS `\s` без unicode-флага: [\t\n\v\f\r \u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000\ufeff].
/// Отличается от Unicode White_Space (char::is_whitespace) наличием U+FEFF.
pub fn js_is_whitespace(c: char) -> bool {
    matches!(c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' '
        | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}'
        | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

/// JS `String.prototype.trim()`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(js_is_whitespace)
}

/// JS `s.replace(/\s+/g, ' ')` (без trim по краям — вызывающий делает сам).
pub fn js_collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if js_is_whitespace(c) {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// JS truthiness.
pub fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => match n.as_f64() {
            Some(x) => x != 0.0 && x.is_finite(),
            None => true, // arbitrary_precision лексема не влезает в f64 — не 0/NaN
        },
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// JS `Number(v)`; None соответствует NaN.
pub fn js_to_number(v: &Value) -> Option<f64> {
    match v {
        Value::Null => Some(0.0), // Number(null) === 0
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => js_string_to_number(s),
        // Массив: ToPrimitive идёт через join(',') — Number([]) === 0, Number([5]) === 5.
        Value::Array(a) => {
            let joined = a
                .iter()
                .map(|el| match el {
                    Value::Null => String::new(), // null/undefined → пустой элемент join
                    Value::Bool(b) => b.to_string(),
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => s.clone(),
                    other => js_to_string(other),
                })
                .collect::<Vec<_>>()
                .join(",");
            js_string_to_number(&joined)
        }
        _ => None,
    }
}

/// JS `Number(string)`; None соответствует NaN.
///
/// Строго по грамматике StringNumericLiteral (ECMA-262 7.1.4.1.1): знак допустим
/// только у десятичного литерала и `Infinity`; `0x`/`0o`/`0b` — без знака; ни
/// `inf`/`nan`/`infinity` в любом регистре, ни `_`, ни двойного знака. Разбор
/// `str::parse::<f64>` шире JS-грамматики, поэтому вход сначала проверяется.
pub fn js_string_to_number(s: &str) -> Option<f64> {
    let t = js_trim(s);
    if t.is_empty() {
        return Some(0.0); // Number('') === 0 (пустая строка ловится раньше у normalizeConfidence)
    }
    let b = t.as_bytes();
    if b.len() > 2 && b[0] == b'0' {
        let radix = match b[1] {
            b'x' | b'X' => Some(16),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        };
        if let Some(radix) = radix {
            let digits = &t[2..];
            if !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            // Без переполнения: u128 покрывает практические значения, дальше —
            // накопление в f64 (как и V8, теряет младшие биты за 2^53).
            return Some(match u128::from_str_radix(digits, radix) {
                Ok(n) => n as f64,
                Err(_) => digits.chars().fold(0.0, |acc, c| acc * radix as f64 + c.to_digit(radix).unwrap() as f64),
            });
        }
    }
    let (sign, rest) = match b[0] {
        b'-' => (-1.0, &t[1..]),
        b'+' => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    if rest == "Infinity" {
        return Some(sign * f64::INFINITY);
    }
    if !is_js_unsigned_decimal(rest) {
        return None;
    }
    // ".5" Rust парсит не во всех версиях — нормализуем к "0.5".
    let normalized = if rest.starts_with('.') { format!("0{rest}") } else { rest.to_string() };
    normalized.parse::<f64>().ok().map(|n| sign * n)
}

/// StrUnsignedDecimalLiteral без `Infinity`: `digits [. digits] [exp]` или
/// `. digits [exp]`, где exp — `[eE][+-]?digits`.
fn is_js_unsigned_decimal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let from = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - from
    };
    let mut mantissa = digits(&mut i);
    if i < b.len() && b[i] == b'.' {
        i += 1;
        mantissa += digits(&mut i);
    }
    if mantissa == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}

/// JS `Number(v) || 0` (NaN → 0; Infinity сохраняется).
pub fn js_number_or_zero(v: Option<&Value>) -> f64 {
    match v.and_then(js_to_number) {
        Some(n) => n,
        None => 0.0,
    }
}

/// JS `String(v)` для скаляров (объекты/массивы — "[object Object]"; undefined в JSON
/// не приходит, null → "null" как в JS).
pub fn js_to_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n
            .as_f64()
            .map(|x| {
                if x == x.trunc() && x.is_finite() && x.abs() < 1e21 {
                    format!("{}", x as i64)
                } else {
                    format!("{x}")
                }
            })
            .unwrap_or_else(|| n.to_string()),
        Value::String(s) => s.clone(),
        _ => "[object Object]".to_string(),
    }
}

/// `x || ''` — falsy значения заменяются пустой строкой.
fn js_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// normalizeConfidence / normalizeFinding
// ---------------------------------------------------------------------------

/// Отсутствующий/битый confidence → 0.5; clamp 0..1 (с нормализацией -0 в 0, как Math.max).
pub fn normalize_confidence(raw: &Value) -> f64 {
    if raw.is_null() {
        return 0.5; // обёртка канонизирует undefined/NaN/Infinity в null
    }
    if let Some(s) = raw.as_str() {
        if s.is_empty() {
            return 0.5;
        }
    }
    match js_to_number(raw) {
        Some(n) if n.is_finite() => {
            let c = n.clamp(0.0, 1.0);
            c + 0.0 // -0 → +0
        }
        _ => 0.5,
    }
}

/// Порт normalizeAdvisorClaims (lib/specialists.mjs): фильтр ролей/статусов,
/// reason по умолчанию '', severity только у downgraded minor/nit.
/// None соответствует undefined (поле пропускается).
/// pub: переиспользуется бинарём specialists (op normalizeAdvisorClaims).
pub fn normalize_advisor_claims(raw: Option<&Value>) -> Option<Vec<Value>> {
    let arr = raw?.as_array()?;
    let claims: Vec<Value> = arr
        .iter()
        .filter(|c| {
            c.is_object()
                && c.get("role").and_then(Value::as_str).map(|r| SPECIALIST_ROLES.contains(&r)).unwrap_or(false)
                && c.get("status").and_then(Value::as_str).map(|s| CLAIM_STATUSES.contains(&s)).unwrap_or(false)
        })
        .map(|c| {
            let mut claim = Map::new();
            claim.insert("role".into(), c.get("role").unwrap().clone());
            claim.insert("status".into(), c.get("status").unwrap().clone());
            claim.insert(
                "reason".into(),
                Value::String(c.get("reason").and_then(Value::as_str).unwrap_or("").to_string()),
            );
            let sev = c.get("severity").and_then(Value::as_str).unwrap_or("");
            if c.get("status").and_then(Value::as_str) == Some("downgraded")
                && (sev == "minor" || sev == "nit")
            {
                claim.insert("severity".into(), Value::String(sev.to_string()));
            }
            Value::Object(claim)
        })
        .collect();
    if claims.is_empty() {
        None
    } else {
        Some(claims)
    }
}

/// Нормализация одной находки из JSON модели; Value::Null соответствует null в JS.
pub fn normalize_finding(f: &Value, model: &str, require_quote: bool) -> Value {
    if !f.is_object() {
        return Value::Null;
    }
    let quote = match f.get("quote") {
        Some(Value::String(s)) => js_trim(s),
        _ => "",
    };
    let severity = f
        .get("severity")
        .and_then(Value::as_str)
        .filter(|s| SEVERITIES.contains(s))
        .unwrap_or("minor");
    if require_quote && quote.is_empty() && (severity == "critical" || severity == "major") {
        return Value::Null;
    }
    let mut out = Map::new();
    out.insert(
        "file".into(),
        Value::String(f.get("file").and_then(Value::as_str).unwrap_or("").to_string()),
    );
    out.insert("line".into(), json!(js_number_or_zero(f.get("line"))));
    out.insert("severity".into(), Value::String(severity.to_string()));
    out.insert(
        "title".into(),
        Value::String(f.get("title").and_then(Value::as_str).unwrap_or("замечание").to_string()),
    );
    out.insert(
        "body".into(),
        Value::String(f.get("body").and_then(Value::as_str).unwrap_or("").to_string()),
    );
    if let Some(s) = f.get("suggestion").and_then(Value::as_str) {
        out.insert("suggestion".into(), Value::String(s.to_string()));
    }
    if !quote.is_empty() {
        out.insert("quote".into(), Value::String(quote.to_string()));
    }
    out.insert("confidence".into(), json!(normalize_confidence(f.get("confidence").unwrap_or(&Value::Null))));
    if let Some(claims) = normalize_advisor_claims(f.get("advisorClaims")) {
        out.insert("advisorClaims".into(), Value::Array(claims));
    }
    out.insert("model".into(), Value::String(model.to_string()));
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// sameFindingKey / dedupeSameModelFindings
// ---------------------------------------------------------------------------

/// Ключ дубля: модель|файл|строка|severity|нормализованный заголовок.
/// Путь НЕ приводится к нижнему регистру (case-sensitive ФС) — как в JS.
pub fn same_finding_key(f: &Value) -> String {
    let title_raw = js_or_empty(f.get("title")).to_lowercase();
    let mut title = String::with_capacity(title_raw.len());
    for c in title_raw.chars() {
        if !matches!(c, '`' | '*' | '_' | '"' | '«' | '»') {
            title.push(c);
        }
    }
    let title = js_collapse_whitespace(&title);
    let mut title = title.as_str();
    loop {
        let trimmed = title.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?') || js_is_whitespace(c));
        if trimmed.len() == title.len() {
            break;
        }
        title = trimmed;
    }
    let title = js_trim(title);
    let model = js_or_empty(f.get("model"));
    let file = js_trim(&js_or_empty(f.get("file"))).to_string();
    let line = js_number_or_zero(f.get("line"));
    let severity = js_or_empty(f.get("severity"));
    format!("{model}|{file}|{line}|{severity}|{title}")
}

/// Схлопывает повторы ОДНОЙ модели (копии, не мутация входа). Возвращает
/// (kept, dropped) — dropped для лога вызывающего.
pub fn dedupe_same_model_findings(findings: &[Value]) -> (Vec<Value>, usize) {
    let mut seen: IndexMap<String, usize> = IndexMap::new();
    let mut kept: Vec<Value> = Vec::new();
    for f in findings {
        let key = same_finding_key(f);
        if let Some(&prev_idx) = seen.get(&key) {
            let prev = &mut kept[prev_idx];
            let next = match prev.get("dupCount") {
                Some(v) if js_truthy(v) => js_to_number(v).unwrap_or(1.0) + 1.0,
                _ => 2.0, // (prev.dupCount || 1) + 1
            };
            prev.as_object_mut().unwrap().insert("dupCount".into(), json!(next));
        } else {
            seen.insert(key, kept.len());
            kept.push(f.clone());
        }
    }
    let dropped = findings.len() - kept.len();
    (kept, dropped)
}

// ---------------------------------------------------------------------------
// clusterFindings / applyPrescanBlockPolicy / resolveBlockers (gate-ядро)
// ---------------------------------------------------------------------------

/// Гейт из JS: consensusMin None соответствует undefined (сравнения `>=` ложны);
/// confidenceMin None — дефолт 0.8. null в JS коэрсится в 0 — обёртка не передаёт null.
#[derive(Debug, Clone)]
pub struct Gate {
    pub block_on: Vec<String>,
    pub block_single: Vec<String>,
    pub consensus_min: Option<f64>,
    pub confidence_min: f64,
    pub strict: bool,
}

impl Gate {
    pub fn from_value(v: &Value) -> Gate {
        let str_list = |k: &str| {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default()
        };
        let num = |k: &str| v.get(k).and_then(Value::as_f64);
        Gate {
            block_on: str_list("blockOn"),
            block_single: str_list("blockSingle"),
            consensus_min: num("consensusMin"),
            confidence_min: num("confidenceMin").unwrap_or(0.8),
            strict: v.get("strict").and_then(Value::as_bool).unwrap_or(false),
        }
    }

    fn in_block_on(&self, severity: Option<&Value>) -> bool {
        severity
            .and_then(Value::as_str)
            .map(|s| self.block_on.iter().any(|b| b == s))
            .unwrap_or(false)
    }

    fn in_block_single(&self, severity: Option<&Value>) -> bool {
        severity
            .and_then(Value::as_str)
            .map(|s| self.block_single.iter().any(|b| b == s))
            .unwrap_or(false)
    }
}

/// Blast soft-gate policy: чистый blast-кластер блокирует только после verifier.
/// null соответствует JS null (fail-open / не blast-кластер).
pub fn soft_consensus_for_blast_cluster(block_tier: &[Value]) -> Option<bool> {
    if block_tier.is_empty() {
        return None;
    }
    if !block_tier
        .iter()
        .all(|f| f.get("source").and_then(Value::as_str) == Some(BLAST_FINDING_SOURCE))
    {
        return None;
    }
    Some(
        block_tier
            .iter()
            .any(|f| f.get("verifierStatus").and_then(Value::as_str) == Some("verified")),
    )
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingMutation {
    pub index: usize,
    pub confidence: f64,
    pub consensus: bool,
    pub cluster_models: Vec<Value>,
    pub confidence_block: bool,
    pub consensus_block: bool,
}

#[derive(Debug)]
pub struct ClusterOutcome {
    pub mutations: Vec<FindingMutation>,
    pub clusters: Vec<Vec<usize>>,
}

fn cluster_key(f: &Value) -> String {
    let file = js_to_string(f.get("file").unwrap_or(&Value::Null));
    let line = js_number_or_zero(f.get("line"));
    let bucket = if line > 0.0 {
        format!("{}", ((line - 1.0) / 3.0).floor())
    } else {
        "out".to_string()
    };
    format!("{file}|{bucket}")
}

/// Кластеризация по file|line-bucket (3-строчные бакеты) + гейт-логика.
/// Мутации не применяются к входу — возвращаются отдельно (обёртка накладывает
/// на исходные объекты, сохраняя семантику разделяемых ссылок в пайплайне).
pub fn cluster_findings(findings: &[Value], gate: &Gate) -> ClusterOutcome {
    let mut clusters: IndexMap<String, Vec<usize>> = IndexMap::new();
    for (i, f) in findings.iter().enumerate() {
        clusters.entry(cluster_key(f)).or_default().push(i);
    }

    let mut mutations = Vec::with_capacity(findings.len());
    let mut cluster_lists = Vec::with_capacity(clusters.len());
    for group in clusters.values() {
        // Модели кластера: порядок первого появления (JS Set).
        let mut models: Vec<Value> = Vec::new();
        let mut model_seen: Vec<String> = Vec::new();
        let mut single_models: Vec<String> = Vec::new();
        let mut consensus_models: Vec<String> = Vec::new();
        for &i in group {
            let f = &findings[i];
            let model_key = serde_json::to_string(f.get("model").unwrap_or(&Value::Null)).unwrap();
            if !model_seen.iter().any(|m| m == &model_key) {
                model_seen.push(model_key.clone());
                models.push(f.get("model").unwrap_or(&Value::Null).clone());
            }
            let sev = f.get("severity");
            if gate.in_block_single(sev) && !single_models.iter().any(|m| m == &model_key) {
                single_models.push(model_key.clone());
            }
            if gate.in_block_on(sev)
                && !gate.in_block_single(sev)
                && !consensus_models.iter().any(|m| m == &model_key)
            {
                consensus_models.push(model_key);
            }
        }
        let tier_block = !single_models.is_empty()
            || match gate.consensus_min {
                Some(min) => consensus_models.len() as f64 >= min,
                None => false, // size >= undefined — всегда ложь в JS
            };
        let block_candidates: Vec<usize> = group
            .iter()
            .copied()
            .filter(|&i| gate.in_block_on(findings[i].get("severity")))
            .collect();
        let max_conf = block_candidates
            .iter()
            .map(|&i| normalize_confidence(findings[i].get("confidence").unwrap_or(&Value::Null)))
            .fold(f64::NEG_INFINITY, f64::max);
        let confidence_block = block_candidates.is_empty() || max_conf >= gate.confidence_min;
        let mut consensus_block = tier_block && confidence_block;
        if gate.strict && consensus_block {
            consensus_block = group.iter().any(|&i| {
                let f = &findings[i];
                gate.in_block_on(f.get("severity"))
                    && (js_truthy(f.get("prescan").unwrap_or(&Value::Null))
                        || f.get("verifierStatus").and_then(Value::as_str) == Some("verified"))
            });
        } else if consensus_block {
            let block_tier: Vec<Value> = block_candidates
                .iter()
                .map(|&i| findings[i].clone())
                .collect();
            if let Some(soft) = soft_consensus_for_blast_cluster(&block_tier) {
                consensus_block = soft;
            }
        }
        let consensus = models.len() >= 2;
        for &i in group {
            mutations.push(FindingMutation {
                index: i,
                confidence: normalize_confidence(findings[i].get("confidence").unwrap_or(&Value::Null)),
                consensus,
                cluster_models: models.clone(),
                confidence_block,
                consensus_block,
            });
        }
        cluster_lists.push(group.clone());
    }
    ClusterOutcome {
        mutations,
        clusters: cluster_lists,
    }
}

/// Prescan block-политика: не-blockRules и выключенный blockEnabled → advisory.
/// Возвращает мутации только для prescan-находок (в JS мутируются только они).
pub fn apply_prescan_block_policy(
    clusters: &[Vec<usize>],
    findings: &[Value],
    prescan_cfg: Option<&Value>,
) -> Vec<(usize, bool)> {
    let Some(cfg) = prescan_cfg else { return Vec::new() };
    if cfg.get("enabled").and_then(Value::as_bool) == Some(false) {
        return Vec::new();
    }
    let block_enabled_false = cfg.get("blockEnabled").and_then(Value::as_bool) == Some(false);
    let mut out = Vec::new();
    for group in clusters {
        for &i in group {
            let f = &findings[i];
            if !js_truthy(f.get("prescan").unwrap_or(&Value::Null)) {
                continue;
            }
            let prescan_block = js_truthy(f.get("prescanBlock").unwrap_or(&Value::Null));
            if block_enabled_false || !prescan_block {
                out.push((i, false));
            }
        }
    }
    out
}

#[derive(Debug)]
pub struct ResolveOutcome {
    pub mutations: Vec<FindingMutation>,
    pub prescan_mutations: Vec<(usize, bool)>,
    pub clusters: Vec<Vec<usize>>,
    pub blockers: Vec<usize>,
    pub would_block: bool,
}

/// Ядро resolveBlockers ПОСЛЕ FP-фильтра (он остаётся в JS): cluster → prescan → блокеры.
pub fn resolve_blockers(findings: &[Value], gate: &Gate, prescan_cfg: Option<&Value>) -> ResolveOutcome {
    let outcome = cluster_findings(findings, gate);
    let prescan_mutations = apply_prescan_block_policy(&outcome.clusters, findings, prescan_cfg);

    let consensus_block_of = |idx: usize| -> bool {
        let base = outcome
            .mutations
            .iter()
            .find(|m| m.index == idx)
            .map(|m| m.consensus_block)
            .unwrap_or(false);
        let forced = prescan_mutations
            .iter()
            .find(|(i, _)| *i == idx)
            .map(|&(_, b)| b);
        forced.unwrap_or(base)
    };

    let mut blockers = Vec::new();
    for group in &outcome.clusters {
        if group.iter().any(|&i| consensus_block_of(i)) {
            let first = group
                .iter()
                .copied()
                .find(|&i| consensus_block_of(i))
                .unwrap_or(group[0]);
            blockers.push(first);
        }
    }
    let would_block = !blockers.is_empty();
    ResolveOutcome {
        mutations: outcome.mutations,
        prescan_mutations,
        clusters: outcome.clusters,
        blockers,
        would_block,
    }
}

// ---------------------------------------------------------------------------
// FP-фильтр (matchesKnownFalsePositive / filterFalsePositiveFindings)
// ---------------------------------------------------------------------------

/// `${f.title} ${f.body}` — шаблонная строка JS: отсутствующее поле даёт
/// `"undefined"`, `null` → `"null"`. Оба варианта реально доходят до регексов
/// (модель нередко отдаёт находку без `body`), поэтому коэрсию воспроизводим,
/// а не подменяем пустой строкой.
fn finding_fp_text(f: &Value) -> String {
    let part = |name: &str| match f.get(name) {
        None => "undefined".to_string(),
        Some(v) => js_to_string(v),
    };
    format!("{} {}", part("title"), part("body"))
}

/// `matchesKnownFalsePositive`: текст совпал с курируемым FP-паттерном.
pub fn matches_known_false_positive(
    text: &str,
    changed_paths: &[String],
    patterns: &[FpPattern],
) -> bool {
    match_fp_patterns(text, changed_paths, patterns).is_some()
}

/// Результат `filterFalsePositiveFindings`: индексы оставленных находок и
/// лог-сообщения в том же порядке и формулировке, что в JS (их читает sticky —
/// снятые FP показываются отдельным блоком, а не удаляются молча).
#[derive(Debug, Default)]
pub struct FpFilterOutcome {
    pub kept: Vec<usize>,
    pub logs: Vec<String>,
}

/// `filterFalsePositiveFindings`: снимает находки, чей `title + body` попал в
/// известный FP-паттерн. Возвращает индексы, а не сами находки: обёртка
/// накладывает результат на ИСХОДНЫЕ объекты (семантика разделяемых ссылок).
pub fn filter_false_positive_findings(
    findings: &[Value],
    changed_paths: &[String],
    patterns: &[FpPattern],
) -> FpFilterOutcome {
    let mut out = FpFilterOutcome::default();
    for (i, f) in findings.iter().enumerate() {
        let text = finding_fp_text(f);
        if matches_known_false_positive(&text, changed_paths, patterns) {
            out.logs
                .push(format!("filtered false finding: {}", js_or_empty_raw(f.get("title"))));
        } else {
            out.kept.push(i);
        }
    }
    out
}

/// `${f.title}` в лог-строке — та же коэрсия, что и в тексте для матчинга.
fn js_or_empty_raw(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(x) => js_to_string(x),
    }
}

// ---------------------------------------------------------------------------
// findingFingerprint / attachFingerprints
// ---------------------------------------------------------------------------

/// Номер строки RIGHT-стороны → её текст (часть DiffFileRecord из diff.rs).
pub type LineText = BTreeMap<u64, String>;

/// Отпечаток строки, на которую показывает находка: sha256 (12 hex) от
/// `trim()` + схлопывание `\s+` собственной строки. '' если строки нет/она пустая.
pub fn finding_fingerprint(text: Option<&LineText>, line: &Value) -> String {
    let n = js_number_or_zero(Some(line));
    let raw = if n > 0.0 && n.fract() == 0.0 && n.is_finite() {
        text.and_then(|t| t.get(&(n as u64))).map(String::as_str)
    } else {
        None
    };
    let body = js_collapse_whitespace(js_trim(raw.unwrap_or("")));
    if body.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(body.as_bytes());
    let mut hex = String::with_capacity(12);
    for b in digest.iter().take(6) {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// fp → номера строк файла (по возрастанию — как порядок вставки в JS-парсере).
fn file_fingerprints(text: Option<&LineText>) -> IndexMap<String, Vec<u64>> {
    let mut map: IndexMap<String, Vec<u64>> = IndexMap::new();
    let Some(text) = text else { return map };
    for (line, _) in text {
        let fp = finding_fingerprint(Some(text), &json!(line));
        if fp.is_empty() {
            continue;
        }
        map.entry(fp).or_default().push(*line);
    }
    map
}

/// Ближайшая к прежней позиции строка; при равном расстоянии — меньший номер
/// (строгое `<` в JS reduce: первый минимум побеждает, строки по возрастанию).
fn nearest_line(lines: &[u64], line: f64) -> u64 {
    let mut best = lines[0];
    for &cur in &lines[1..] {
        if ((cur as f64) - line).abs() < ((best as f64) - line).abs() {
            best = cur;
        }
    }
    best
}

/// Инвалидация prior findings по содержимому (fp) с legacy-откатом ±3 строки.
/// kept.line — итоговая строка: равна исходной (коэрсированной) либо обновлённой.
pub struct StaleOutcome {
    pub kept: Vec<(usize, f64)>,
    pub code_changed: usize,
    pub out_of_diff: usize,
    pub stale_legacy: usize,
    pub legacy: usize,
}

pub struct PriorFileRec {
    pub valid_lines: Vec<f64>,
    pub text: Option<LineText>,
}

pub fn filter_stale_prior_findings(
    priors: &[Value],
    files: &IndexMap<String, PriorFileRec>,
) -> StaleOutcome {
    let mut kept: Vec<(usize, f64)> = Vec::new();
    let mut fp_cache: IndexMap<String, IndexMap<String, Vec<u64>>> = IndexMap::new();
    let mut code_changed = 0usize;
    let mut out_of_diff = 0usize;
    let mut stale_legacy = 0usize;
    let mut legacy = 0usize;

    for (i, f) in priors.iter().enumerate() {
        let file = f.get("file");
        let rec = match file {
            Some(v) if js_truthy(v) => v.as_str().and_then(|p| files.get(p)),
            _ => None,
        };
        let line = js_number_or_zero(f.get("line"));
        let Some(rec) = rec else {
            out_of_diff += 1;
            continue;
        };
        let fp_val = f.get("fp").unwrap_or(&Value::Null);
        if js_truthy(fp_val) {
            // truthy fp: строка → сверка отпечатков; иной скаляр в JS даёт miss в Map — codeChanged.
            let fp = fp_val.as_str().unwrap_or("");
            let key = js_to_string(file.unwrap());
            if !fp_cache.contains_key(&key) {
                fp_cache.insert(key.clone(), file_fingerprints(rec.text.as_ref()));
            }
            let hits = fp_cache.get(&key).and_then(|m| m.get(fp));
            match hits {
                Some(hits) if !hits.is_empty() => {
                    let at = nearest_line(hits, line);
                    kept.push((i, at as f64));
                }
                _ => code_changed += 1,
            }
            continue;
        }
        legacy += 1;
        let fresh = line > 0.0 && rec.valid_lines.iter().any(|vl| (vl - line).abs() <= 3.0);
        if fresh {
            kept.push((i, line));
        } else {
            stale_legacy += 1;
        }
    }
    StaleOutcome {
        kept,
        code_changed,
        out_of_diff,
        stale_legacy,
        legacy,
    }
}

// ---------------------------------------------------------------------------
// Тесты — ключевые случаи из test/pr-review.test.mjs / pr-review.eval.test.mjs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(v: serde_json::Value) -> Value {
        v
    }

    /// `Number(string)` по грамматике StringNumericLiteral; эталон — V8.
    #[test]
    fn js_string_to_number_matches_v8() {
        let nan = [
            "infinity", "INFINITY", "inf", "nan", "NaN", "--5", "+-5", "-+5", "-0x10", "0x", "0x+5", "e5", "1e",
            ".", "1_000", "0b2", "+0b1",
        ];
        for s in nan {
            assert_eq!(js_string_to_number(s), None, "{s:?}");
        }
        let num: [(&str, f64); 16] = [
            ("0x10", 16.0), ("0b101", 5.0), ("0o17", 15.0), ("0B11", 3.0), ("5.", 5.0), ("+.5", 0.5),
            ("-.5", -0.5), ("1e3", 1000.0), ("1E+2", 100.0), (" 12 ", 12.0), ("\u{a0} 7 ", 7.0),
            ("Infinity", f64::INFINITY), ("-Infinity", f64::NEG_INFINITY), ("+Infinity", f64::INFINITY),
            ("00012", 12.0), ("0x1fffffffffffffffff", 590_295_810_358_705_700_000.0),
        ];
        for (s, want) in num {
            assert_eq!(js_string_to_number(s), Some(want), "{s:?}");
        }
        assert!(js_string_to_number("-0").unwrap().is_sign_negative());
    }

    #[test]
    fn normalize_confidence_edges() {
        assert_eq!(normalize_confidence(&Value::Null), 0.5);
        assert_eq!(normalize_confidence(&json!("")), 0.5);
        assert_eq!(normalize_confidence(&json!("abc")), 0.5);
        assert_eq!(normalize_confidence(&json!(0.9)), 0.9);
        assert_eq!(normalize_confidence(&json!(1.5)), 1.0);
        assert_eq!(normalize_confidence(&json!(-0.1)), 0.0);
        assert_eq!(normalize_confidence(&json!("0.9")), 0.9);
    }

    #[test]
    fn normalize_finding_defaults_and_require_quote() {
        let f = normalize_finding(&json!({"file": "a.ts", "line": "5", "severity": "weird", "title": 7}), "m1", false);
        assert_eq!(f["file"], "a.ts");
        assert_eq!(f["line"], 5.0);
        assert_eq!(f["severity"], "minor");
        assert_eq!(f["title"], "замечание");
        assert_eq!(f["confidence"], 0.5);
        assert_eq!(f["model"], "m1");
        assert!(f.get("suggestion").is_none());
        assert!(f.get("quote").is_none());
        assert!(normalize_finding(&Value::Null, "m", false).is_null());
        assert!(normalize_finding(&json!({"severity": "critical"}), "m", true).is_null());
        assert!(normalize_finding(&json!({"severity": "major", "quote": "  x "}), "m", true)["quote"] == "x");
    }

    #[test]
    fn normalize_advisor_claims_filters() {
        let claims = normalize_advisor_claims(Some(&json!([
            {"role": "security", "status": "verified", "reason": "ok"},
            {"role": "bogus", "status": "verified"},
            {"role": "correctness", "status": "downgraded", "severity": "nit"},
            {"role": "architecture", "status": "pending"},
        ])));
        let claims = claims.unwrap();
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0]["reason"], "ok");
        assert_eq!(claims[1]["severity"], "nit");
        assert!(normalize_advisor_claims(Some(&json!([]))).is_none());
        assert!(normalize_advisor_claims(Some(&json!("nope"))).is_none());
    }

    #[test]
    fn same_finding_key_normalizes_title() {
        let a = same_finding_key(&finding(json!({"model": "m", "file": " a.ts ", "line": 1, "severity": "major", "title": "  `Bug` —  «Утечка»...  "})));
        let b = same_finding_key(&finding(json!({"model": "m", "file": "a.ts", "line": 1, "severity": "major", "title": "bug — утечка"})));
        assert_eq!(a, b);
        assert!(same_finding_key(&finding(json!({"model": "m", "file": "A.ts", "line": 1, "severity": "major", "title": "x"}))).contains("A.ts"));
    }

    #[test]
    fn dedupe_accumulates_dup_count() {
        let findings = vec![
            json!({"model": "m", "file": "a.ts", "line": 1, "severity": "major", "title": "Bug"}),
            json!({"model": "m", "file": "a.ts", "line": 1, "severity": "major", "title": "`bug`."}),
            json!({"model": "m", "file": "a.ts", "line": 1, "severity": "major", "title": "bug", "dupCount": 4}),
            json!({"model": "m2", "file": "a.ts", "line": 1, "severity": "major", "title": "bug"}),
        ];
        let (kept, dropped) = dedupe_same_model_findings(&findings);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped, 2);
        // prev в JS — копия ПЕРВОЙ находки ключа: входящий dupCount=4 игнорируется,
        // счётчик растёт только от числа дублей (1-й → 2-й → 3-й = 3).
        assert_eq!(kept[0]["dupCount"], 3.0);
        assert!(kept[1].get("dupCount").is_none(), "другая модель — отдельный ключ");
    }

    fn gate() -> Gate {
        Gate::from_value(&json!({
            "blockOn": ["critical", "major"],
            "blockSingle": ["critical"],
            "consensusMin": 1.0,
            "confidenceMin": 0.8,
        }))
    }

    #[test]
    fn cluster_low_confidence_critical_does_not_block() {
        let findings = vec![json!({"file": "a.ts", "line": 1, "severity": "critical", "confidence": 0.4, "model": "m1"})];
        let out = cluster_findings(&findings, &gate());
        assert_eq!(out.mutations.len(), 1);
        assert!(!out.mutations[0].consensus_block);
        assert!(!out.mutations[0].confidence_block);
    }

    #[test]
    fn cluster_blast_soft_gate_requires_verifier() {
        let mk = |status: &str, source: &str| json!({
            "file": "a.ts", "line": 1, "severity": "major", "confidence": 0.9,
            "model": "blast", "source": source, "verifierStatus": status,
        });
        let findings = vec![mk("unverified", BLAST_FINDING_SOURCE)];
        let out = cluster_findings(&findings, &gate());
        assert!(!out.mutations[0].consensus_block, "без verified — не блок");

        let findings = vec![mk("verified", BLAST_FINDING_SOURCE)];
        let out = cluster_findings(&findings, &gate());
        assert!(out.mutations[0].consensus_block);

        // смешанный кластер (не чистый blast) — soft-гейт не применяется
        let findings = vec![mk("unverified", BLAST_FINDING_SOURCE), json!({
            "file": "a.ts", "line": 1, "severity": "major", "confidence": 0.9, "model": "llm",
        })];
        let out = cluster_findings(&findings, &gate());
        assert!(out.mutations.iter().all(|m| m.consensus_block), "LLM в кластере — обычный гейт");
    }

    #[test]
    fn cluster_buckets_are_three_lines() {
        let findings = vec![
            json!({"file": "a.ts", "line": 0, "severity": "major", "model": "m1", "confidence": 0.9}),
            json!({"file": "a.ts", "line": 1, "severity": "major", "model": "m2", "confidence": 0.9}),
            json!({"file": "a.ts", "line": 3, "severity": "major", "model": "m3", "confidence": 0.9}),
            json!({"file": "a.ts", "line": 4, "severity": "major", "model": "m4", "confidence": 0.9}),
        ];
        let out = cluster_findings(&findings, &gate());
        assert_eq!(out.clusters.len(), 3, "line 0 — 'out', 1–3 бакет, 4 — следующий");
        assert_eq!(out.clusters[1].len(), 2);
        assert!(out.mutations.iter().any(|m| m.consensus), "m2+m3 в бакете — консенсус");
    }

    #[test]
    fn prescan_policy_softens_non_block_rules() {
        let findings = vec![
            json!({"file": "a.ts", "line": 1, "severity": "critical", "model": "p", "prescan": true, "prescanBlock": false}),
            json!({"file": "a.ts", "line": 1, "severity": "critical", "model": "q", "prescan": true, "prescanBlock": true}),
        ];
        let clusters = vec![vec![0], vec![1]];
        let cfg = json!({"enabled": true, "blockEnabled": true});
        let muts = apply_prescan_block_policy(&clusters, &findings, Some(&cfg));
        assert_eq!(muts, vec![(0, false)]);
        // blockEnabled: false — гасит и block-правила
        let cfg = json!({"enabled": true, "blockEnabled": false});
        let muts = apply_prescan_block_policy(&clusters, &findings, Some(&cfg));
        assert_eq!(muts, vec![(0, false), (1, false)]);
        // enabled: false / отсутствует — без мутаций
        assert!(apply_prescan_block_policy(&clusters, &findings, Some(&json!({"enabled": false}))).is_empty());
        assert!(apply_prescan_block_policy(&clusters, &findings, None).is_empty());
    }

    #[test]
    fn resolve_blockers_picks_first_consensus_block_member() {
        let findings = vec![
            json!({"file": "a.ts", "line": 1, "severity": "minor", "model": "m1", "confidence": 0.5}),
            json!({"file": "a.ts", "line": 1, "severity": "major", "model": "m2", "confidence": 0.95}),
        ];
        let out = resolve_blockers(&findings, &gate(), None);
        assert!(out.would_block);
        // consensusBlock в JS ставится ВСЕМ членам кластера → блокер — первый элемент,
        // даже если сама находка вне block-tier (minor с конф. 0.5).
        assert_eq!(out.blockers, vec![0]);
    }

    fn sample_text() -> LineText {
        let mut m = LineText::new();
        m.insert(1, "const a = 1;".to_string());
        m.insert(2, "const bug = parseInt(raw);".to_string());
        m.insert(3, "   ".to_string());
        m
    }

    #[test]
    fn finding_fingerprint_sha256_of_collapsed_line() {
        let text = sample_text();
        let fp = finding_fingerprint(Some(&text), &json!(2));
        assert_eq!(fp.len(), 12);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        // контрольный хеш против node: sha256("const bug = parseInt(raw);").slice(0,12)
        let expected = {
            let digest = Sha256::digest(b"const bug = parseInt(raw);");
            let mut hex = String::new();
            for b in digest.iter().take(6) {
                hex.push_str(&format!("{b:02x}"));
            }
            hex
        };
        assert_eq!(fp, expected);
        assert_eq!(finding_fingerprint(Some(&text), &json!(3)), "", "пустая/whitespace-only строка — ''");
        assert_eq!(finding_fingerprint(Some(&text), &json!(0)), "", "line 0 — ''");
        assert_eq!(finding_fingerprint(None, &json!(2)), "", "файл вне диффа — ''");
        // whitespace в файле схлопывается: те же отступы → тот же отпечаток
        let mut padded = LineText::new();
        padded.insert(2, "   const   bug =  parseInt(raw);   ".to_string());
        assert_eq!(
            finding_fingerprint(Some(&padded), &json!(2)),
            fp,
            "переформатирование отступов не меняет отпечаток"
        );
    }

    #[test]
    fn stale_prior_fingerprint_and_legacy_paths() {
        let mut files: IndexMap<String, PriorFileRec> = IndexMap::new();
        files.insert("foo.ts".to_string(), PriorFileRec {
            valid_lines: vec![1.0, 2.0, 3.0],
            text: Some(sample_text()),
        });
        // fp находится → kept с обновлённой строкой
        let fp2 = finding_fingerprint(Some(&sample_text()), &json!(2));
        let priors = vec![
            json!({"file": "foo.ts", "line": 2, "fp": fp2}),
            json!({"file": "foo.ts", "line": 9, "fp": "gone12345678"}),
            json!({"file": "gone.ts", "line": 1}),
            json!({"file": "foo.ts", "line": 2, "title": "legacy ±3"}),
            json!({"file": "foo.ts", "line": 40, "title": "legacy stale"}),
            json!({"file": "foo.ts", "line": 0, "title": "legacy no line"}),
        ];
        let out = filter_stale_prior_findings(&priors, &files);
        assert_eq!(out.kept.len(), 2, "fp-hit и legacy ±3");
        assert_eq!(out.kept[0], (0, 2.0));
        assert_eq!(out.kept[1], (3, 2.0));
        assert_eq!(out.code_changed, 1);
        assert_eq!(out.out_of_diff, 1);
        assert_eq!(out.stale_legacy, 2);
        assert_eq!(out.legacy, 3);
    }

    #[test]
    fn stale_prior_nearest_line_tie_takes_smaller() {
        let mut text = LineText::new();
        text.insert(5, "}".to_string());
        text.insert(7, "}".to_string());
        let fp = finding_fingerprint(Some(&text), &json!(5));
        let mut files: IndexMap<String, PriorFileRec> = IndexMap::new();
        files.insert("a.ts".to_string(), PriorFileRec { valid_lines: vec![], text: Some(text) });
        let priors = vec![json!({"file": "a.ts", "line": 6, "fp": fp})];
        let out = filter_stale_prior_findings(&priors, &files);
        assert_eq!(out.kept[0].1, 5.0, "равное расстояние → меньший номер");
    }

    // -----------------------------------------------------------------------
    // FP-фильтр — кейсы перенесены из test/pr-review.test.mjs и
    // test/pr-review.eval.test.mjs (id теста указан в комментарии)
    // -----------------------------------------------------------------------

    fn fp() -> &'static [crate::fp_patterns::FpPattern] {
        crate::fp_patterns::default_fp_patterns()
    }

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn is_fp(text: &str, list: &[&str]) -> bool {
        matches_known_false_positive(text, &paths(list), fp())
    }

    /// «filterFalsePositiveFindings: гасит ложные e2e и file-absent, оставляет реальные»
    #[test]
    fn fp_filter_drops_e2e_and_file_absent_keeps_real() {
        let changed = paths(&["app/e2e/foo.spec.ts", "server/foo.ts"]);
        let findings = vec![
            json!({"file": "server/foo.ts", "line": 5, "severity": "minor", "title": "нет e2e тестов", "body": ""}),
            json!({"file": "server/foo.ts", "line": 6, "severity": "minor", "title": "проблема", "body": "файл foo.ts отсутствует в диффе"}),
            json!({"file": "server/foo.ts", "line": 7, "severity": "major", "title": "реальный баг", "body": "null pointer при пустом вводе"}),
        ];
        let out = filter_false_positive_findings(&findings, &changed, fp());
        assert_eq!(out.kept, vec![2], "остаётся только реальная находка");
        assert_eq!(
            out.logs,
            vec![
                "filtered false finding: нет e2e тестов".to_string(),
                "filtered false finding: проблема".to_string(),
            ]
        );
    }

    /// «fp-pattern metrics-absent-observability: срабатывает только на observability-путях»
    #[test]
    fn fp_requires_path_scopes_pattern() {
        let mk = || {
            vec![json!({
                "file": "ops/observability/hermes/prometheus.yml", "line": 1, "severity": "major",
                "title": "Alert использует отсутствующие метрики Docker", "body": "", "model": "m"
            })]
        };
        let scoped = filter_false_positive_findings(
            &mk(),
            &paths(&["ops/observability/hermes/prometheus.yml"]),
            fp(),
        );
        assert!(scoped.kept.is_empty(), "на observability-путях — отфильтровано");
        let unscoped = filter_false_positive_findings(&mk(), &paths(&["app/server/api.ts"]), fp());
        assert_eq!(unscoped.kept, vec![0], "вне observability — requiresPath не сработал");
    }

    /// «matchesKnownFalsePositive: e2e-in-diff pattern» + «fp-patterns.jsonl: e2e-in-diff matches»
    #[test]
    fn fp_e2e_in_diff_pattern() {
        assert!(is_fp("нет e2e теста на удаление", &["app/e2e/foo.spec.ts"]));
        assert!(!is_fp("file not in pr diff", &["server/auth.ts"]));
    }

    /// «fp-patterns: softblast-typedef + graphify-path-norm speculation»
    #[test]
    fn fp_round2_softblast_and_graphify() {
        let typedef = "typedef Finding не описывает blast-radius маркеры, а softBlast дублирует проверку через два поля";
        assert!(is_fp(typedef, &["lib/findings.mjs"]));
        assert!(is_fp(
            "Дедуп по d.file без нормализации пути — разные представления путей от graphify",
            &["lib/graphify.mjs"]
        ));
        assert!(is_fp(
            "Потенциальная command-injection при передаче REVIEW_GRAPHIFY_DEPTH в graphify CLI",
            &["docs/configuration.md"]
        ));
        assert!(is_fp(
            "Удалён costTracker из deps createReviewEngine — требуется верификация",
            &["pipeline.mjs"]
        ));
        assert!(!is_fp(typedef, &["app/server/x.ts"]), "без requiresPath-пути не срабатывает");
    }

    /// «fp-patterns: round3 dash-flag + softblast boundary + redact dup»
    #[test]
    fn fp_round3() {
        assert!(is_fp(
            "символ начинается с `-` и looks like a CLI flag для graphify affected",
            &["lib/graphify.mjs"]
        ));
        assert!(is_fp(
            "Нарушение границ модулей: hardcoded blast-radius soft-tier в findings",
            &["lib/findings.mjs"]
        ));
        assert!(is_fp(
            "дублирование redactSecretsForLog между util.mjs и observability",
            &["lib/util.mjs"]
        ));
    }

    /// «fp-patterns: round4 cross-chunk + findings cluster + test nits»
    #[test]
    fn fp_round4() {
        for (text, path) in [
            ("Три FP-паттерна подряд глушат архитектурные претензии к findings.mjs", "feedback/fp-patterns.jsonl"),
            ("Зависимость: filterFindingsFixedInDiff должен экспортироваться с эквивалентной семантикой", "pipeline.mjs"),
            ("Зависимость: emitBlastRadiusFindings должен поддерживать dropAmbiguous", "pipeline.mjs"),
            ("Прямой импорт graphify-интернов в обход barrel index.mjs", "test/pr-review.test.mjs"),
            ("escapeMdTicks безвозвратно удаляет backticks из caller/symbol/file", "lib/blast-findings.mjs"),
            ("Смена контракта aggregateMetrics.byModel: Map → Object", "lib/metrics.mjs"),
        ] {
            assert!(is_fp(text, &[path]), "паттерн не сработал: {text}");
        }
    }

    /// «fp-patterns: structure platform, 29 находок PR #6 (dry-run 2026-09-02)»
    #[test]
    fn fp_structure_platform_29_findings() {
        let cases: [(&str, &str, &str); 29] = [
            ("plan-todo-completeness", "Несогласованность статусов todo и заявлений о завершённости", ".cursor/plans/structure-platform-integration.plan.md"),
            ("mcp-oversized-eof-tail", "Незавершённое поглощение oversized-кадра при обрыве потока", "bin/pr-review-structure-mcp.mjs"),
            ("structure-cli-base-source-required", "Проверка «есть хотя бы один источник» не учитывает --obligations/--mutation-report/--property-report", "bin/pr-review-structure.mjs"),
            ("trust-tier-default-documented", "Дефолт REVIEW_STRUCTURE_TRUST_TIER=trusted_local потенциально небезопасен для CI недоверенных PR", "docs/configuration.md"),
            ("index-reexport-primitives-ok", "Дублирование пути импорта канонических примитивов через structure-evidence.mjs", "index.mjs"),
            ("trust-tier-validated-in-receipt", "trustTier из env не валидируется по whitelist", "lib/config.mjs"),
            ("structure-namespace-parallel-ok", "Параллельный namespace structure.* дублирует часть graphify.*", "lib/config.mjs"),
            ("hotspot-confidence-conservative", "Возможно инвертированная семантика confidence для hotspot", "lib/structure-assessments.mjs"),
            ("property-evidence-vs-bound", "evidence добавляется по property.status без учёта confidence, используемого в propertyBound", "lib/structure-assessments.mjs"),
            ("context-truncated-conservative", "Ложное срабатывание флага truncated при точном совпадении лимита", "lib/structure-context.mjs"),
            ("drift-unique-threshold-strict-by-design", "Слишком строгий порог для resolution=unique", "lib/structure-drift.mjs"),
            ("drift-path-overlap-heuristic", "Ложные совпадения document-path-overlap по подстроке", "lib/structure-drift.mjs"),
            ("export-env-read-intentional", "Скрытая конфигурация через process.env внутри lib-модуля", "lib/structure-export.mjs"),
            ("crossboundary-deepest-prefix-ok", "crossBoundary считается по самому глубокому совпавшему префиксу, включая шумные module-границы", "lib/structure-graphify.mjs"),
            ("lcov-match-edge-case-heuristic", "Неоднозначный edge-case в matchLcovFunction: одинаковая строка, разные имена, обе не совпали", "lib/structure-lcov.mjs"),
            ("lcov-kind-list-coupling-ok", "Неявная связь между lcov-матчингом и списком kind'ов из других экстракторов", "lib/structure-lcov.mjs"),
            ("mcp-pin-args-both-allowed", "callStructureContext допускает одновременную передачу headSha и snapshotFingerprint без проверки согласованности", "lib/structure-mcp.mjs"),
            ("scip-truncation-drops-edges-by-design", "Усечение entities по maxEntities молча теряет связи relationships", "lib/structure-scip.mjs"),
            ("scip-definition-choice-deterministic", "Недетерминированный выбор definition при нескольких документах", "lib/structure-scip.mjs"),
            ("test-body-boundary-heuristic", "Эвристическая граница тела теста может обрезаться внутри строк/комментариев", "lib/structure-tests.mjs"),
            ("short-contract-labels-heuristic", "Короткие labels контрактов могут давать ложные совпадения в targetContractRefs", "lib/structure-tests.mjs"),
            ("structure-primitives-single-source", "Возможное дублирование канонических примитивов normalizeRepoPath/sha256", "lib/structure.mjs"),
            ("structure-symlink-guard-landed", "Чтение тестовых файлов без защиты от symlink-escape", "lib/structure.mjs"),
            ("graph-sha-prefix-compare-ok", "Ложное совпадение статуса графа при коротких SHA-префиксах", "lib/structure.mjs"),
            ("schema-path-pattern-duplication-ok", "Дублирование определения path-паттерна между схемами", "schemas/structure-facts.v1.schema.json"),
            ("schema-no-cross-field-check-ok", "Отсутствие кросс-полевой проверки согласованности числовых метрик dataQuality", "schemas/structure-assessment.v1.schema.json"),
            ("clamp-test-asserts-behaviour", "Тест клэмпинга dependencyDepth не проверяет фактическое значение maxDepth в ответе", "test/structure.test.mjs"),
            ("mcp-test-watchdog-ok", "Watchdog-таймаут 10s на весь набор с искусственными паузами split-режима может давать флаки на медленном CI", "test/structure.test.mjs"),
            ("mutation-line-zero-unreachable", "Number(...) || null теряет валидное значение 0", "lib/structure-mutation.mjs"),
        ];
        for (id, title, path) in cases {
            assert!(is_fp(title, &[path]), "паттерн не сработал: {id}");
        }

        // контроль на переглушение: настоящая находка в том же файле гаситься не должна
        for (text, path) in [
            ("Гонка при записи snapshot: два процесса перезаписывают файл", "lib/structure.mjs"),
            ("SQL-инъекция в построении запроса", "lib/structure-scip.mjs"),
            ("Утечка токена в лог при ошибке конфигурации", "lib/config.mjs"),
            ("Пропущен await у записи отчёта — файл может быть пустым", "lib/structure-export.mjs"),
        ] {
            assert!(!is_fp(text, &[path]), "переглушение: {text}");
        }
    }

    /// `${f.title} ${f.body}` — JS-коэрсия отсутствующих полей в "undefined".
    #[test]
    fn fp_text_uses_js_template_coercion() {
        assert_eq!(finding_fp_text(&json!({"title": "t", "body": "b"})), "t b");
        assert_eq!(finding_fp_text(&json!({"title": "t"})), "t undefined");
        assert_eq!(finding_fp_text(&json!({"title": null, "body": 5})), "null 5");
        assert_eq!(finding_fp_text(&json!({})), "undefined undefined");
    }

    /// Пустой набор паттернов ничего не фильтрует (opts.patterns: [] в JS).
    #[test]
    fn fp_empty_pattern_set_keeps_all() {
        let findings = vec![json!({"title": "нет e2e тестов", "body": ""})];
        let out = filter_false_positive_findings(&findings, &paths(&["app/e2e/a.spec.ts"]), &[]);
        assert_eq!(out.kept, vec![0]);
        assert!(out.logs.is_empty());
    }
}
