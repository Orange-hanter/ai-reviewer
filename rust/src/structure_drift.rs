//! Порт lib/structure-drift.mjs (RFC 20, T19): дрейф между документированными
//! обязательствами, подтверждёнными claims и наблюдаемыми контрактами кода,
//! жизненный цикл structure-находок между снапшотами, waivers и триаж.
//!
//! Модуль чистый: ни сети, ни файлов, ни часов — `now` для waivers передаётся
//! явно. Как и прочие structure-порты, работает на `serde_json::Value`: вход —
//! те же JSON-документы (facts/assessment/obligations export), что читает JS.
//!
//! Отклонения от JS:
//! - `Date.parse` в waivers — ISO-8601 подмножество (`YYYY-MM-DD`,
//!   `THH:MM[:SS[.fff]]`, `Z`/`±HH:MM`/`±HHMM`); дата-время без зоны JS трактует
//!   как локальное время, здесь — как UTC (producers пишут `toISOString()`, с `Z`);
//!   нераспознанная строка — NaN, waiver неактивен, как в JS;
//! - `undefined` в JSON непредставим: отсутствующее поле в шаблонных строках
//!   даёт `undefined`, как `${obj.missing}` в JS, а в ключах `subjectRefs` —
//!   `null` (в `stableId` оба превращаются в пустую строку);
//! - `\b…\b` в `suggestObligationLinks` проверяется вручную (ASCII-граница слова,
//!   как у JS-regex без флага `u`), без сборки regex из метки контракта.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_string, js_truthy};
use crate::structure_evidence::{get, iterable_values, js_default_sort, stable_id_op, unique_values, value_key};
use crate::structure_scip::locale_compare;

// ---------------------------------------------------------------------------
// Вспомогательное: JS-семантика над Value
// ---------------------------------------------------------------------------

/// `${v}` в шаблонной строке: отсутствующее поле — "undefined".
fn tpl(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(x) => js_to_string(x),
    }
}

/// `String(v ?? '')`-подобное приведение для сравнений через localeCompare.
fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => js_to_string(x),
    }
}

/// `.length` массива/строки (строка — в UTF-16 code units); прочее — 0
/// (`undefined.length` в этих местах JS не встречается: поле всегда `|| []`).
fn js_length(v: Option<&Value>) -> usize {
    match v {
        Some(Value::Array(a)) => a.len(),
        Some(Value::String(s)) => s.encode_utf16().count(),
        _ => 0,
    }
}

/// `{...v}`: объект копируется, прочее даёт пустой объект.
fn spread(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn opt_array<'a>(v: Option<&'a Value>) -> &'a [Value] {
    match v {
        Some(Value::Array(a)) => a.as_slice(),
        _ => &[],
    }
}

/// `normalized()` из JS: `String(v || '')`, срез хвостовых `()`, одной ведущей
/// точки, lowercase.
fn normalized(v: Option<&Value>) -> String {
    let s = match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    };
    // /\(\)+$/ — '(' и одна или больше ')' в конце строки.
    let mut end = s.len();
    let bytes = s.as_bytes();
    let mut closes = 0usize;
    while end > 0 && bytes[end - 1] == b')' {
        end -= 1;
        closes += 1;
    }
    let s = if closes > 0 && end > 0 && bytes[end - 1] == b'(' { &s[..end - 1] } else { s.as_str() };
    let s = s.strip_prefix('.').unwrap_or(s);
    s.to_lowercase()
}

fn is_ascii_word(c: Option<char>) -> bool {
    matches!(c, Some(c) if c.is_ascii_alphanumeric() || c == '_')
}

/// `new RegExp(`\\b${escape(label)}\\b`, 'i').test(statement)` — обе строки уже
/// в нижнем регистре; граница слова — ASCII `\w`, как у JS-regex без флага `u`.
fn mentions_symbol(statement: &str, label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    let mut from = 0usize;
    while let Some(pos) = statement[from..].find(label) {
        let start = from + pos;
        let end = start + label.len();
        let before = statement[..start].chars().next_back();
        let first = label.chars().next();
        let last = label.chars().next_back();
        let after = statement[end..].chars().next();
        if is_ascii_word(before) != is_ascii_word(first) && is_ascii_word(last) != is_ascii_word(after) {
            return true;
        }
        from = start + first.map_or(1, char::len_utf8);
    }
    false
}

/// `entity?.boundaryId?.includes(String(moduleRef))` — строка (подстрока) или
/// массив (строгое равенство элемента).
fn boundary_includes(boundary: Option<&Value>, needle: &str) -> bool {
    match boundary {
        Some(Value::String(s)) => s.contains(needle),
        Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(needle)),
        _ => false,
    }
}

fn number_json(n: i64) -> Value {
    json!(n)
}

// ---------------------------------------------------------------------------
// suggestObligationLinks
// ---------------------------------------------------------------------------

/// Порт `suggestObligationLinks(obligations, facts, { limit })`: для обязательств
/// без `contractRefs` — кандидаты-контракты со скорингом по детерминированным
/// сигналам (упоминание символа, пересечение путей, граница модуля, сигнатура).
pub fn suggest_obligation_links(obligations: Option<&Value>, facts: Option<&Value>, limit: usize) -> Vec<Value> {
    let mut entity_by_id: HashMap<String, &Value> = HashMap::new();
    for entity in opt_array(facts.and_then(|f| get(f, "entities"))) {
        entity_by_id.insert(value_key(get(entity, "observedId").unwrap_or(&Value::Null)), entity);
    }
    let contracts = opt_array(facts.and_then(|f| get(f, "contracts")));
    let mut out: Vec<Value> = Vec::new();
    for obligation in opt_array(obligations) {
        if js_length(get(obligation, "contractRefs")) > 0 {
            continue;
        }
        let statement = normalized(get(obligation, "statement"));
        let doc_path = normalized(get(obligation, "docRef"));
        let module_ref = get(obligation, "moduleRef").filter(|v| js_truthy(v)).map(js_to_string);
        let mut candidates: Vec<(i64, String, Value)> = Vec::new();
        for contract in contracts {
            let entity = entity_by_id
                .get(&value_key(get(contract, "entityRef").unwrap_or(&Value::Null)))
                .copied();
            let label = normalized(entity.and_then(|e| get(e, "label")));
            let mut score = 0i64;
            let mut reasons: Vec<Value> = Vec::new();
            if !label.is_empty() && mentions_symbol(&statement, &label) {
                score += 5;
                reasons.push(json!("symbol-mentioned"));
            }
            let entity_path = entity.and_then(|e| get(e, "path")).filter(|v| js_truthy(v)).map(js_to_string);
            if let Some(path) = entity_path.filter(|_| !doc_path.is_empty()) {
                let lower = path.to_lowercase();
                if doc_path
                    .split('/')
                    .any(|part| part.encode_utf16().count() > 2 && lower.contains(part))
                {
                    score += 2;
                    reasons.push(json!("document-path-overlap"));
                }
            }
            if let Some(m) = &module_ref {
                if boundary_includes(entity.and_then(|e| get(e, "boundaryId")), m) {
                    score += 3;
                    reasons.push(json!("module-boundary"));
                }
            }
            if get(contract, "kind").and_then(Value::as_str) == Some("signature") {
                score += 1;
                reasons.push(json!("semantic-signature"));
            }
            if score <= 0 {
                continue;
            }
            let confidence = if score >= 6 {
                "extracted"
            } else if score >= 3 {
                "inferred"
            } else {
                "heuristic"
            };
            let mut c = Map::new();
            if let Some(id) = get(contract, "id") {
                c.insert("contractRef".into(), id.clone());
            }
            if let Some(er) = get(contract, "entityRef") {
                c.insert("entityRef".into(), er.clone());
            }
            c.insert("score".into(), number_json(score));
            c.insert("confidence".into(), json!(confidence));
            c.insert("reasons".into(), Value::Array(reasons));
            candidates.push((score, str_or_empty(get(contract, "id")), Value::Object(c)));
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| locale_compare(&a.1, &b.1)));
        candidates.truncate(limit);
        let resolution = if candidates.len() == 1 && candidates[0].0 >= 6 {
            "unique"
        } else if !candidates.is_empty() {
            "ambiguous"
        } else {
            "unresolved"
        };
        let mut item = Map::new();
        if let Some(id) = get(obligation, "id") {
            item.insert("obligationRef".into(), id.clone());
        }
        item.insert(
            "candidates".into(),
            Value::Array(candidates.into_iter().map(|(_, _, v)| v).collect()),
        );
        item.insert("resolution".into(), json!(resolution));
        out.push(Value::Object(item));
    }
    out.sort_by(|a, b| locale_compare(&str_or_empty(get(a, "obligationRef")), &str_or_empty(get(b, "obligationRef"))));
    out
}

// ---------------------------------------------------------------------------
// computeStructureDrift
// ---------------------------------------------------------------------------

struct FindingSpec<'a> {
    priority: &'a str,
    remediation_target: &'a str,
    summary: String,
    evidence: Vec<Value>,
    missing_evidence: Vec<Value>,
    snapshot: Option<&'a Value>,
}

/// `[...new Set(xs)].sort()`.
fn unique_sorted(xs: &[Value]) -> Vec<Value> {
    let mut v = unique_values(xs);
    js_default_sort(&mut v);
    v
}

/// Порт `finding()`: subjectRefs канонизируются до хэширования — иначе порядок
/// ломает реконсиляцию (отпечаток — ключ сходимости между снапшотами).
fn make_finding(rule_id: &str, subject_refs: &[Value], spec: FindingSpec) -> Value {
    let refs = unique_sorted(subject_refs);
    let mut parts = vec![json!(rule_id)];
    parts.extend(refs.iter().cloned());
    let fingerprint = stable_id_op("structure-finding", &parts);
    let snapshot = spec.snapshot.cloned();
    let mut m = Map::new();
    m.insert("id".into(), json!(fingerprint));
    m.insert("fingerprint".into(), json!(fingerprint));
    m.insert("ruleId".into(), json!(rule_id));
    m.insert("priority".into(), json!(spec.priority));
    m.insert("status".into(), json!("open"));
    m.insert("subjectRefs".into(), Value::Array(refs));
    m.insert("remediationTarget".into(), json!(spec.remediation_target));
    m.insert("summary".into(), json!(spec.summary));
    m.insert("evidence".into(), Value::Array(unique_sorted(&spec.evidence)));
    m.insert("missingEvidence".into(), Value::Array(unique_sorted(&spec.missing_evidence)));
    // undefined-снапшот в JSON.stringify пропадает — повторяем пропуском ключа.
    if let Some(s) = &snapshot {
        m.insert("firstSeenSnapshot".into(), s.clone());
        m.insert("lastSeenSnapshot".into(), s.clone());
    }
    m.insert("resolvedBySnapshot".into(), Value::Null);
    Value::Object(m)
}

/// Вход `computeStructureDrift({...})`.
pub struct DriftInput<'a> {
    pub facts: &'a Value,
    pub assessment: &'a Value,
    pub obligations_export: Option<&'a Value>,
    pub confirmed_claims: Option<&'a Value>,
    /// `null`/отсутствие — подсказки считаются здесь же.
    pub link_suggestions: Option<&'a Value>,
}

/// Порт `computeStructureDrift`. Все находки выходят со статусом `open`:
/// жизненный цикл ведёт только `reconcile_structure_findings`.
pub fn compute_structure_drift(input: &DriftInput) -> Result<Value> {
    let facts = input.facts;
    let assessment = input.assessment;
    let inner = get(facts, "facts");
    let contract_ids: HashSet<String> = opt_array(inner.and_then(|f| get(f, "contracts")))
        .iter()
        .map(|c| value_key(get(c, "id").unwrap_or(&Value::Null)))
        .collect();
    let entity_ids: HashSet<String> = opt_array(inner.and_then(|f| get(f, "entities")))
        .iter()
        .map(|e| value_key(get(e, "observedId").unwrap_or(&Value::Null)))
        .collect();
    let obligations = input.obligations_export.and_then(|o| get(o, "obligations"));
    let suggestions: Vec<Value> = match input.link_suggestions {
        Some(v) if js_truthy(v) => match v {
            Value::Array(a) => a.clone(),
            _ => bail!("suggestions.map is not a function"),
        },
        _ => suggest_obligation_links(obligations, inner, 5),
    };
    let mut suggestion_by_obligation: HashMap<String, &Value> = HashMap::new();
    for item in &suggestions {
        suggestion_by_obligation.insert(value_key(get(item, "obligationRef").unwrap_or(&Value::Null)), item);
    }
    let snapshot = get(assessment, "fingerprint");
    let mut findings: Vec<Value> = Vec::new();

    let Some(provenance) = get(facts, "provenance").filter(|p| !p.is_null()) else {
        bail!("Cannot read properties of undefined (reading 'graphStatus')");
    };
    let graph_status = get(provenance, "graphStatus");
    if graph_status.and_then(Value::as_str) != Some("fresh") {
        findings.push(make_finding(
            "structure.graph_stale",
            &[json!("structure:graph")],
            FindingSpec {
                priority: "info",
                remediation_target: "code",
                summary: "Rebuild the structure graph at the pinned commit.".into(),
                evidence: vec![json!(format!("graph-status:{}", tpl(graph_status)))],
                missing_evidence: vec![json!("fresh_graph")],
                snapshot,
            },
        ));
    }

    for obligation in iterable_values(obligations, "obligations")? {
        if get(&obligation, "status").and_then(Value::as_str) != Some("confirmed") {
            continue;
        }
        let refs_value = get(&obligation, "contractRefs");
        let refs: Vec<Value> = match refs_value {
            Some(Value::Array(a)) => a.clone(),
            Some(v) if js_truthy(v) => bail!("(obligation.contractRefs || []).filter is not a function"),
            _ => Vec::new(),
        };
        let linked = refs.iter().filter(|r| contract_ids.contains(&value_key(r))).count();
        let id = get(&obligation, "id");
        if js_length(refs_value) == 0 {
            let evidence: Vec<Value> = suggestion_by_obligation
                .get(&value_key(id.unwrap_or(&Value::Null)))
                .and_then(|s| get(s, "candidates"))
                .and_then(Value::as_array)
                .map(|cands| {
                    cands
                        .iter()
                        .map(|c| {
                            json!(format!(
                                "candidate:{}:{}",
                                tpl(get(c, "contractRef")),
                                tpl(get(c, "score"))
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            findings.push(make_finding(
                "docs.obligation_unlinked",
                &[id.cloned().unwrap_or(Value::Null)],
                FindingSpec {
                    priority: "medium",
                    remediation_target: "claim",
                    summary: format!("Confirm a code contract link for {}.", tpl(id)),
                    evidence,
                    missing_evidence: vec![json!("confirmed_contract_link")],
                    snapshot,
                },
            ));
        } else if linked == 0 {
            let mut subjects = vec![id.cloned().unwrap_or(Value::Null)];
            subjects.extend(refs.iter().cloned());
            findings.push(make_finding(
                "docs.contract_link_broken",
                &subjects,
                FindingSpec {
                    priority: "high",
                    remediation_target: "docs",
                    summary: format!(
                        "Documented contract link for {} does not exist in the pinned snapshot.",
                        tpl(id)
                    ),
                    evidence: vec![json!(format!(
                        "obligation:{}:{}",
                        tpl(id),
                        tpl(get(&obligation, "contentHash"))
                    ))],
                    missing_evidence: vec![json!("existing_contract")],
                    snapshot,
                },
            ));
        }
    }

    for claim in iterable_values(input.confirmed_claims, "confirmedClaims")? {
        if get(&claim, "status").and_then(Value::as_str) != Some("confirmed") {
            continue;
        }
        let subject = get(&claim, "subjectRef").cloned().unwrap_or(Value::Null);
        let kind = get(&claim, "kind");
        let exists = if kind.and_then(Value::as_str) == Some("entity_exists") {
            entity_ids.contains(&value_key(&subject))
        } else {
            contract_ids.contains(&value_key(&subject))
        };
        if exists {
            continue;
        }
        let content_hash = match get(&claim, "contentHash") {
            Some(v) if js_truthy(v) => js_to_string(v),
            _ => String::new(),
        };
        let claim_id = get(&claim, "id");
        findings.push(make_finding(
            "contract.confirmed_claim_drift",
            &[claim_id.cloned().unwrap_or(Value::Null), subject],
            FindingSpec {
                priority: "high",
                remediation_target: "docs",
                summary: format!("Confirmed {} claim no longer resolves to observed code.", tpl(kind)),
                evidence: vec![json!(format!("claim:{}:{}", tpl(claim_id), content_hash))],
                missing_evidence: vec![json!("observed_subject")],
                snapshot,
            },
        ));
    }

    let scenarios = get(assessment, "assessments").and_then(|a| get(a, "contractScenarios"));
    for scenario in iterable_values(scenarios, "contractScenarios")? {
        let status = get(&scenario, "status").and_then(Value::as_str);
        if !matches!(status, Some("missing") | Some("partial")) {
            continue;
        }
        let mut subjects = vec![get(&scenario, "obligationRef").cloned().unwrap_or(Value::Null)];
        subjects.extend(iterable_values(get(&scenario, "contractRefs"), "contractRefs")?);
        findings.push(make_finding(
            "scenario.must_obligation_gap",
            &subjects,
            FindingSpec {
                priority: if status == Some("missing") { "high" } else { "medium" },
                remediation_target: "test",
                summary: format!(
                    "Scenario {} is {}: {}",
                    tpl(get(&scenario, "kind")),
                    tpl(get(&scenario, "status")),
                    tpl(get(&scenario, "statusReason"))
                ),
                evidence: iterable_values(get(&scenario, "evidence"), "evidence")?,
                missing_evidence: iterable_values(get(&scenario, "missingEvidence"), "missingEvidence")?,
                snapshot,
            },
        ));
    }

    findings.sort_by(|a, b| locale_compare(&str_or_empty(get(a, "fingerprint")), &str_or_empty(get(b, "fingerprint"))));
    let revision = match input.obligations_export.and_then(|o| get(o, "revision")) {
        Some(v) if js_truthy(v) => v.clone(),
        _ => json!("none"),
    };
    let mut out = Map::new();
    out.insert("version".into(), json!(1));
    out.insert("kind".into(), json!("structure_drift"));
    if let Some(v) = get(facts, "fingerprint") {
        out.insert("factsFingerprint".into(), v.clone());
    }
    if let Some(v) = snapshot {
        out.insert("assessmentFingerprint".into(), v.clone());
    }
    out.insert("obligationsRevision".into(), revision);
    if let Some(v) = get(assessment, "temporalAlignment") {
        out.insert("temporalAlignment".into(), v.clone());
    }
    out.insert("findings".into(), Value::Array(findings));
    out.insert("linkSuggestions".into(), Value::Array(suggestions));
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// reconcileStructureFindings
// ---------------------------------------------------------------------------

fn is_terminal(status: Option<&Value>) -> bool {
    matches!(status.and_then(Value::as_str), Some("resolved") | Some("superseded"))
}

/// Порт `retireFinding`: находка пропала из текущего снапшота. Один пропуск —
/// `pending_verify` (шумный снапшот: битый граф, срезанный lcov), второй подряд —
/// `resolved`. Терминальные статусы идемпотентны. `docs.obligation_unlinked`
/// снимается сразу (`superseded`): правило перестало применяться, а не
/// «перестало наблюдаться».
fn retire_finding(prior: &Value, snapshot: &str) -> Value {
    let mut m = spread(prior);
    if is_terminal(get(prior, "status")) {
        return Value::Object(m);
    }
    if get(prior, "ruleId").and_then(Value::as_str) == Some("docs.obligation_unlinked") {
        m.insert("status".into(), json!("superseded"));
        m.insert("resolvedBySnapshot".into(), json!(snapshot));
    } else if get(prior, "status").and_then(Value::as_str) == Some("pending_verify") {
        m.insert("status".into(), json!("resolved"));
        m.insert("resolvedBySnapshot".into(), json!(snapshot));
    } else {
        m.insert("status".into(), json!("pending_verify"));
        m.insert("resolvedBySnapshot".into(), Value::Null);
    }
    Value::Object(m)
}

/// Порт `reconcileStructureFindings(previous, current, snapshotFingerprint)`.
/// Жизненный цикл: open → (пропала) pending_verify → (пропала снова) resolved;
/// повторное наблюдение переоткрывает, `firstSeenSnapshot` берётся из прошлого
/// состояния. `lastSeenSnapshot` у пропавшей находки не двигается.
pub fn reconcile_structure_findings(previous: &[Value], current: &[Value], snapshot: &str) -> Vec<Value> {
    let fp_key = |v: &Value| value_key(get(v, "fingerprint").unwrap_or(&Value::Null));
    let mut previous_by_fp: HashMap<String, &Value> = HashMap::new();
    for item in previous {
        previous_by_fp.insert(fp_key(item), item);
    }
    let current_fps: HashSet<String> = current.iter().map(fp_key).collect();
    let mut out: Vec<Value> = Vec::new();
    for item in current {
        let Some(prior) = previous_by_fp.get(&fp_key(item)) else {
            out.push(item.clone());
            continue;
        };
        let mut m = spread(prior);
        for (k, v) in spread(item) {
            m.insert(k, v);
        }
        m.insert("status".into(), json!("open"));
        let first = match get(prior, "firstSeenSnapshot") {
            Some(v) if js_truthy(v) => Some(v.clone()),
            _ => get(item, "firstSeenSnapshot").cloned(),
        };
        match first {
            Some(v) => {
                m.insert("firstSeenSnapshot".into(), v);
            }
            // `prior.first || item.first` дал undefined — ключ в JSON пропадает.
            None => {
                m.shift_remove("firstSeenSnapshot");
            }
        }
        m.insert("lastSeenSnapshot".into(), json!(snapshot));
        m.insert("resolvedBySnapshot".into(), Value::Null);
        out.push(Value::Object(m));
    }
    for prior in previous {
        if current_fps.contains(&fp_key(prior)) {
            continue;
        }
        out.push(retire_finding(prior, snapshot));
    }
    out.sort_by(|a, b| locale_compare(&str_or_empty(get(a, "fingerprint")), &str_or_empty(get(b, "fingerprint"))));
    out
}

// ---------------------------------------------------------------------------
// applyStructureWaivers
// ---------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `Date.parse` для ISO-8601 подмножества (см. шапку модуля) → миллисекунды
/// эпохи; `None` соответствует NaN.
pub fn js_date_parse_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let digits = |i: &mut usize, n: usize| -> Option<i64> {
        if *i + n > b.len() || !b[*i..*i + n].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let v = std::str::from_utf8(&b[*i..*i + n]).ok()?.parse().ok()?;
        *i += n;
        Some(v)
    };
    let expect = |i: &mut usize, c: u8| -> Option<()> {
        if b.get(*i) == Some(&c) {
            *i += 1;
            Some(())
        } else {
            None
        }
    };
    let year = digits(&mut i, 4)?;
    expect(&mut i, b'-')?;
    let month = digits(&mut i, 2)?;
    expect(&mut i, b'-')?;
    let day = digits(&mut i, 2)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut hh, mut mm, mut ss, mut ms) = (0i64, 0i64, 0i64, 0i64);
    let mut offset_min = 0i64;
    if i < b.len() {
        if !matches!(b[i], b'T' | b't' | b' ') {
            return None;
        }
        i += 1;
        hh = digits(&mut i, 2)?;
        expect(&mut i, b':')?;
        mm = digits(&mut i, 2)?;
        if b.get(i) == Some(&b':') {
            i += 1;
            ss = digits(&mut i, 2)?;
            if b.get(i) == Some(&b'.') {
                i += 1;
                let start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                let frac = &s[start..i.min(start + 3)];
                ms = format!("{frac:0<3}").parse().ok()?;
            }
        }
        if hh > 24 || mm > 59 || ss > 59 || (hh == 24 && (mm, ss, ms) != (0, 0, 0)) {
            return None;
        }
        match b.get(i) {
            None => {}
            Some(b'Z') | Some(b'z') => i += 1,
            Some(&sign @ (b'+' | b'-')) => {
                i += 1;
                let oh = digits(&mut i, 2)?;
                if b.get(i) == Some(&b':') {
                    i += 1;
                }
                let om = digits(&mut i, 2)?;
                let total = oh * 60 + om;
                offset_min = if sign == b'+' { total } else { -total };
            }
            _ => return None,
        }
        if i != b.len() {
            return None;
        }
    }
    let days = days_from_civil(year, month, day);
    Some(((days * 24 + hh) * 60 + mm - offset_min) * 60_000 + ss * 1000 + ms)
}

/// Порт `applyStructureWaivers(findings, waivers, now)`: активный waiver
/// (owner + reason + expiresAt в будущем) помечает находку `suppressed`.
/// `now_ms` — миллисекунды эпохи (`now.getTime()`).
pub fn apply_structure_waivers(findings: &[Value], waivers: &[Value], now_ms: i64) -> Vec<Value> {
    let mut active: HashMap<String, &Value> = HashMap::new();
    for waiver in waivers {
        let ok = ["owner", "reason", "expiresAt"]
            .iter()
            .all(|k| get(waiver, k).is_some_and(js_truthy));
        if !ok {
            continue;
        }
        let expires = get(waiver, "expiresAt").map(js_to_string).unwrap_or_default();
        if !js_date_parse_ms(&expires).is_some_and(|t| t > now_ms) {
            continue;
        }
        active.insert(value_key(get(waiver, "findingFingerprint").unwrap_or(&Value::Null)), waiver);
    }
    findings
        .iter()
        .map(|item| {
            let mut m = spread(item);
            match active.get(&value_key(get(item, "fingerprint").unwrap_or(&Value::Null))) {
                Some(w) => {
                    m.insert("suppressed".into(), json!(true));
                    m.insert("waiver".into(), (*w).clone());
                }
                None => {
                    m.insert("suppressed".into(), json!(false));
                }
            }
            Value::Object(m)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// triageStructureFindings
// ---------------------------------------------------------------------------

fn priority_rank(v: Option<&Value>) -> i32 {
    match v.and_then(Value::as_str) {
        Some("high") => 3,
        Some("medium") => 2,
        Some("info") => 1,
        _ => 0,
    }
}

/// `Array.prototype.join(', ')`: null/undefined — пустая строка.
fn js_join(items: &[Value], sep: &str) -> String {
    items
        .iter()
        .map(|v| if v.is_null() { String::new() } else { js_to_string(v) })
        .collect::<Vec<_>>()
        .join(sep)
}

/// Порт `triageStructureFindings(findings, { limit })`: открытые и не
/// подавленные находки по приоритету, затем ruleId и fingerprint, с
/// объяснением и следующим действием.
pub fn triage_structure_findings(findings: &[Value], limit: usize) -> Result<Vec<Value>> {
    let mut open: Vec<&Value> = findings
        .iter()
        .filter(|item| !get(item, "suppressed").is_some_and(js_truthy) && !is_terminal(get(item, "status")))
        .collect();
    open.sort_by(|a, b| {
        priority_rank(get(b, "priority"))
            .cmp(&priority_rank(get(a, "priority")))
            .then_with(|| locale_compare(&str_or_empty(get(a, "ruleId")), &str_or_empty(get(b, "ruleId"))))
            .then_with(|| {
                locale_compare(&str_or_empty(get(a, "fingerprint")), &str_or_empty(get(b, "fingerprint")))
            })
    });
    open.truncate(limit);
    let mut out = Vec::with_capacity(open.len());
    for item in open {
        let missing = match get(item, "missingEvidence") {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::String(s)) => vec![json!(s)],
            _ => bail!("Cannot read properties of undefined (reading 'length')"),
        };
        let has_missing = match get(item, "missingEvidence") {
            Some(Value::String(s)) => !s.is_empty(),
            _ => !missing.is_empty(),
        };
        let mut m = spread(item);
        let second = if has_missing {
            format!("Missing: {}", js_join(&missing, ", "))
        } else {
            "Evidence threshold satisfied.".to_string()
        };
        m.insert(
            "explain".into(),
            json!([get(item, "summary").cloned().unwrap_or(Value::Null), second]),
        );
        m.insert(
            "nextAction".into(),
            get(item, "remediationTarget").cloned().unwrap_or(Value::Null),
        );
        out.push(Value::Object(m));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structure_evidence::sha256_str;

    fn facts_doc(graph_status: &str) -> Value {
        json!({
            "fingerprint": "facts-fp",
            "provenance": { "graphStatus": graph_status },
            "facts": {
                "entities": [
                    { "observedId": "ent:validate", "label": "validate()", "path": "src/auth/validate.ts", "boundaryId": "boundary:auth" },
                    { "observedId": "ent:jwt", "label": "JwtValidator", "path": "src/auth/jwt.ts" }
                ],
                "contracts": [
                    { "id": "contract:validate", "entityRef": "ent:validate", "kind": "signature" },
                    { "id": "contract:jwt", "entityRef": "ent:jwt", "kind": "entrypoint" }
                ]
            }
        })
    }

    fn assessment_doc() -> Value {
        json!({ "fingerprint": "assess-fp", "temporalAlignment": "aligned", "assessments": { "contractScenarios": [] } })
    }

    fn obligation(contract_refs: Value) -> Value {
        json!({
            "id": "obl:validate",
            "source": "cod-doc-section",
            "contentHash": sha256_str("validate"),
            "priority": "must",
            "kind": "happy_path",
            "statement": "validate MUST accept valid tokens",
            "status": "confirmed",
            "docRef": "docs/auth/tokens.md",
            "moduleRef": "auth",
            "contractRefs": contract_refs,
        })
    }

    #[test]
    fn normalized_matches_js() {
        assert_eq!(normalized(Some(&json!("validate()"))), "validate");
        assert_eq!(normalized(Some(&json!("run())"))), "run");
        assert_eq!(normalized(Some(&json!(".Foo"))), "foo");
        assert_eq!(normalized(Some(&json!("a)"))), "a)");
        assert_eq!(normalized(None), "");
        assert_eq!(normalized(Some(&json!(0))), "");
    }

    #[test]
    fn mentions_symbol_uses_word_boundaries() {
        assert!(mentions_symbol("validate must accept", "validate"));
        assert!(!mentions_symbol("revalidate must", "validate"));
        assert!(mentions_symbol("call validate.", "validate"));
        assert!(!mentions_symbol("validated", "validate"));
        // метка с не-словесным краем: \b перед '$' требует словесный символ слева
        assert!(!mentions_symbol("use $foo", "$foo"));
        assert!(mentions_symbol("a$foo", "$foo"));
    }

    #[test]
    fn suggestions_score_and_resolve() {
        let facts = facts_doc("fresh");
        let obls = json!([obligation(json!([]))]);
        let s = suggest_obligation_links(Some(&obls), get(&facts, "facts"), 5);
        assert_eq!(s.len(), 1);
        let cands = s[0]["candidates"].as_array().unwrap();
        assert_eq!(cands[0]["contractRef"], "contract:validate");
        // 5 symbol + 2 path (auth) + 3 boundary + 1 signature
        assert_eq!(cands[0]["score"], 11);
        assert_eq!(cands[0]["confidence"], "extracted");
        assert_eq!(
            cands[0]["reasons"],
            json!(["symbol-mentioned", "document-path-overlap", "module-boundary", "semantic-signature"])
        );
        // jwt: только пересечение пути
        assert_eq!(cands[1]["contractRef"], "contract:jwt");
        assert_eq!(cands[1]["score"], 2);
        assert_eq!(cands[1]["confidence"], "heuristic");
        assert_eq!(s[0]["resolution"], "ambiguous");
        // обязательство со ссылками пропускается
        let linked = json!([obligation(json!(["contract:validate"]))]);
        assert!(suggest_obligation_links(Some(&linked), get(&facts, "facts"), 5).is_empty());
        // limit=1 и единственный сильный кандидат → unique
        let one = suggest_obligation_links(Some(&obls), get(&facts, "facts"), 1);
        assert_eq!(one[0]["resolution"], "unique");
    }

    #[test]
    fn drift_waiver_triage_flow() {
        let facts = facts_doc("fresh");
        let assessment = assessment_doc();
        let obls = json!({ "revision": "1", "obligations": [obligation(json!([]))] });
        let drift = compute_structure_drift(&DriftInput {
            facts: &facts,
            assessment: &assessment,
            obligations_export: Some(&obls),
            confirmed_claims: None,
            link_suggestions: None,
        })
        .unwrap();
        assert_eq!(drift["kind"], "structure_drift");
        assert_eq!(drift["obligationsRevision"], "1");
        assert_eq!(drift["temporalAlignment"], "aligned");
        let findings = drift["findings"].as_array().unwrap().clone();
        assert_eq!(findings.len(), 1);
        let unlinked = &findings[0];
        assert_eq!(unlinked["ruleId"], "docs.obligation_unlinked");
        assert_eq!(unlinked["remediationTarget"], "claim");
        assert_eq!(
            unlinked["evidence"],
            json!(["candidate:contract:jwt:2", "candidate:contract:validate:11"])
        );
        assert_eq!(unlinked["firstSeenSnapshot"], "assess-fp");
        let fp = unlinked["fingerprint"].as_str().unwrap().to_string();
        // значение снято с JS computeStructureDrift на тех же входах
        assert_eq!(fp, "structure-finding:d6a5d8ea14126e90465d7eee");
        assert_eq!(
            fp,
            stable_id_op("structure-finding", &[json!("docs.obligation_unlinked"), json!("obl:validate")])
        );

        let now = js_date_parse_ms("2026-09-02T00:00:00.000Z").unwrap();
        let waivers = [json!({
            "findingFingerprint": fp,
            "owner": "team",
            "reason": "legacy",
            "expiresAt": "2026-10-01T00:00:00.000Z",
        })];
        let waived = apply_structure_waivers(&findings, &waivers, now);
        assert_eq!(waived[0]["suppressed"], true);
        assert_eq!(waived[0]["waiver"]["owner"], "team");
        assert!(triage_structure_findings(&waived, 20).unwrap().is_empty());
        // просроченный waiver не действует
        let later = js_date_parse_ms("2026-10-02T00:00:00Z").unwrap();
        let expired = apply_structure_waivers(&findings, &waivers, later);
        assert_eq!(expired[0]["suppressed"], false);
        let triaged = triage_structure_findings(&expired, 20).unwrap();
        assert_eq!(triaged.len(), 1);
        assert_eq!(
            triaged[0]["explain"],
            json!([unlinked["summary"].clone(), "Missing: confirmed_contract_link"])
        );
        assert_eq!(triaged[0]["nextAction"], "claim");
    }

    #[test]
    fn graph_stale_broken_link_claim_and_scenario_findings() {
        let facts = facts_doc("stale");
        let assessment = json!({
            "fingerprint": "assess-fp",
            "assessments": { "contractScenarios": [
                { "obligationRef": "obl:a", "contractRefs": ["contract:validate"], "kind": "happy_path",
                  "status": "missing", "statusReason": "no test", "evidence": ["b", "a", "a"], "missingEvidence": ["test"] },
                { "obligationRef": "obl:b", "contractRefs": [], "kind": "edge", "status": "covered",
                  "statusReason": "ok", "evidence": [], "missingEvidence": [] }
            ]}
        });
        let obls = json!({ "obligations": [obligation(json!(["contract:gone"]))] });
        let claims = json!([
            { "id": "claim:1", "status": "confirmed", "kind": "entity_exists", "subjectRef": "ent:gone" },
            { "id": "claim:2", "status": "confirmed", "kind": "entity_exists", "subjectRef": "ent:jwt" },
            { "id": "claim:3", "status": "draft", "kind": "contract_exists", "subjectRef": "contract:gone" }
        ]);
        let drift = compute_structure_drift(&DriftInput {
            facts: &facts,
            assessment: &assessment,
            obligations_export: Some(&obls),
            confirmed_claims: Some(&claims),
            link_suggestions: Some(&json!([])),
        })
        .unwrap();
        assert_eq!(drift["obligationsRevision"], "none");
        assert!(drift.get("temporalAlignment").is_none());
        let findings = drift["findings"].as_array().unwrap();
        let by_rule = |r: &str| findings.iter().find(|f| f["ruleId"] == r).cloned().unwrap();
        assert_eq!(findings.len(), 4);
        let stale = by_rule("structure.graph_stale");
        assert_eq!(stale["priority"], "info");
        assert_eq!(stale["evidence"], json!(["graph-status:stale"]));
        let broken = by_rule("docs.contract_link_broken");
        assert_eq!(broken["subjectRefs"], json!(["contract:gone", "obl:validate"]));
        assert_eq!(
            broken["evidence"],
            json!([format!("obligation:obl:validate:{}", sha256_str("validate"))])
        );
        let claim = by_rule("contract.confirmed_claim_drift");
        assert_eq!(claim["evidence"], json!(["claim:claim:1:"]));
        assert_eq!(claim["summary"], "Confirmed entity_exists claim no longer resolves to observed code.");
        let gap = by_rule("scenario.must_obligation_gap");
        assert_eq!(gap["priority"], "high");
        assert_eq!(gap["summary"], "Scenario happy_path is missing: no test");
        assert_eq!(gap["evidence"], json!(["a", "b"]));
        // находки отсортированы по fingerprint
        let fps: Vec<&str> = findings.iter().map(|f| f["fingerprint"].as_str().unwrap()).collect();
        let mut sorted = fps.clone();
        sorted.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(fps, sorted);
    }

    #[test]
    fn fingerprints_independent_of_subject_refs_order() {
        let facts = facts_doc("fresh");
        let assessment = assessment_doc();
        let with = |refs: Value| {
            let obls = json!({ "revision": "1", "obligations": [obligation(refs)] });
            let d = compute_structure_drift(&DriftInput {
                facts: &facts,
                assessment: &assessment,
                obligations_export: Some(&obls),
                confirmed_claims: None,
                link_suggestions: Some(&json!([])),
            })
            .unwrap();
            d["findings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["ruleId"] == "docs.contract_link_broken")
                .cloned()
                .unwrap()
        };
        let a = with(json!(["contract:a", "contract:b"]));
        let b = with(json!(["contract:b", "contract:a"]));
        assert_eq!(a["fingerprint"], b["fingerprint"]);
        assert_eq!(a["subjectRefs"], b["subjectRefs"]);
    }

    #[test]
    fn missing_provenance_is_type_error() {
        let facts = json!({ "facts": {} });
        let err = compute_structure_drift(&DriftInput {
            facts: &facts,
            assessment: &assessment_doc(),
            obligations_export: None,
            confirmed_claims: None,
            link_suggestions: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("graphStatus"));
    }

    fn prior_finding(overrides: Value) -> Value {
        let mut base = json!({
            "id": "f1",
            "fingerprint": "f1",
            "ruleId": "structure.contract_untested",
            "priority": "medium",
            "status": "open",
            "subjectRefs": ["contract:validate"],
            "summary": "contract has no test",
            "evidence": [],
            "missingEvidence": [],
            "firstSeenSnapshot": "snap-1",
            "lastSeenSnapshot": "snap-1",
            "resolvedBySnapshot": null,
        });
        for (k, v) in overrides.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn reconcile_closes_vanished_finding_in_two_snapshots() {
        let pending = reconcile_structure_findings(&[prior_finding(json!({}))], &[], "snap-2").remove(0);
        assert_eq!(pending["status"], "pending_verify");
        assert_eq!(pending["resolvedBySnapshot"], Value::Null);
        assert_eq!(pending["lastSeenSnapshot"], "snap-1");
        let resolved = reconcile_structure_findings(&[pending], &[], "snap-3").remove(0);
        assert_eq!(resolved["status"], "resolved");
        assert_eq!(resolved["resolvedBySnapshot"], "snap-3");
        assert_eq!(resolved["lastSeenSnapshot"], "snap-1");
        let again = reconcile_structure_findings(&[resolved.clone()], &[], "snap-4").remove(0);
        assert_eq!(again, resolved);
        assert!(triage_structure_findings(&[again], 20).unwrap().is_empty());
    }

    #[test]
    fn reconcile_reopens_finding_observed_again() {
        let pending = reconcile_structure_findings(&[prior_finding(json!({}))], &[], "snap-2").remove(0);
        let resolved = reconcile_structure_findings(&[pending], &[], "snap-3").remove(0);
        let current = prior_finding(json!({ "firstSeenSnapshot": "snap-4", "lastSeenSnapshot": "snap-4" }));
        let reopened = reconcile_structure_findings(&[resolved], &[current], "snap-4").remove(0);
        assert_eq!(reopened["status"], "open");
        assert_eq!(reopened["resolvedBySnapshot"], Value::Null);
        assert_eq!(reopened["lastSeenSnapshot"], "snap-4");
        assert_eq!(reopened["firstSeenSnapshot"], "snap-1");
        assert_eq!(triage_structure_findings(&[reopened], 20).unwrap().len(), 1);
    }

    #[test]
    fn reconcile_supersedes_obligation_link_in_one_snapshot() {
        let prior = prior_finding(json!({ "ruleId": "docs.obligation_unlinked" }));
        let superseded = reconcile_structure_findings(&[prior], &[], "snap-2").remove(0);
        assert_eq!(superseded["status"], "superseded");
        assert_eq!(superseded["resolvedBySnapshot"], "snap-2");
        assert_eq!(reconcile_structure_findings(&[superseded.clone()], &[], "snap-3")[0], superseded);
    }

    #[test]
    fn triage_orders_by_priority_rule_and_fingerprint() {
        let items = [
            prior_finding(json!({ "fingerprint": "c", "priority": "info", "ruleId": "a" })),
            prior_finding(json!({ "fingerprint": "b", "priority": "high", "ruleId": "z" })),
            prior_finding(json!({ "fingerprint": "a", "priority": "high", "ruleId": "z" })),
            prior_finding(json!({ "fingerprint": "d", "priority": "medium", "ruleId": "m", "missingEvidence": ["x", "y"] })),
            prior_finding(json!({ "fingerprint": "e", "status": "superseded" })),
        ];
        let t = triage_structure_findings(&items, 20).unwrap();
        let fps: Vec<&str> = t.iter().map(|f| f["fingerprint"].as_str().unwrap()).collect();
        assert_eq!(fps, ["a", "b", "d", "c"]);
        assert_eq!(t[2]["explain"][1], "Missing: x, y");
        assert_eq!(t[0]["explain"][1], "Evidence threshold satisfied.");
        assert_eq!(triage_structure_findings(&items, 2).unwrap().len(), 2);
    }

    #[test]
    fn date_parse_subset() {
        assert_eq!(js_date_parse_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(js_date_parse_ms("1970-01-02"), Some(86_400_000));
        assert_eq!(js_date_parse_ms("2026-09-02T00:00:00.000Z"), Some(1_788_307_200_000));
        assert_eq!(js_date_parse_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(js_date_parse_ms("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(js_date_parse_ms("not a date"), None);
        assert_eq!(js_date_parse_ms("2026-13-01"), None);
    }
}
