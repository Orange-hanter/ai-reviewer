//! Порт lib/structure-protocol.mjs: ручная валидация structure-пейлоадов против
//! контракта schemas/*.json (schemaRef/version/kind, лимиты размеров, пути,
//! ссылочная целостность фактов, sha256-отпечатки) + фингерпринты и
//! temporalAlignment.
//!
//! Схемы с диска НЕ читаются — ни JS-оригинал, ни этот порт их не открывают:
//! проверки зашиты в коде как зеркало контракта (это и есть «ручная валидация»),
//! schemas/*.json остаются документацией-контрактом для внешних JSON-Schema
//! тулов. Дисковый I/O в JS-обёртке не появляется, поведенческой разницы нет.
//!
//! Паритет с JS, включая:
//! - семантику лимитов: отсутствующий/нечисловой ключ — «нет проверки»
//!   (`len > undefined`/`NaN` → false), null — 0 (`len > null` → len > 0),
//!   bool — 0/1, числовая строка — Number(строка); в ТЕКСТАХ ошибок лимит
//!   подставляется как в JS-шаблоне (raw-значение: null → "null");
//! - длину строк — UTF-16 code units (`value.length`), не байты и не codepoints;
//! - порядок Object.values — JS own-property enumeration: целочисленные ключи
//!   (< 2^32-1, каноническая запись) по возрастанию, затем прочие в порядке
//!   вставки (от него зависит, какая из одновременных ошибок inspectValue
//!   бросится первой);
//! - Set-сравнение observedId/references — SameValueZero: числа по f64
//!   (-0 === 0), строки по значению, объекты по идентичности;
//! - `||`-семантику (falsy → fallback) в fingerprint/alignment и путях;
//! - ошибки «глубокого» доступа: boundary=null бросает V8-TypeError
//!   "Cannot read properties of null (reading 'paths')", truthy не-итерируемый
//!   paths — "not iterable" (iterable_values из structure_evidence).
//!
//! Отклонения (как у прочих модулей): тексты SyntaxError JSON.parse не
//! воспроизводятся (V8: "Unexpected token 'o', \"nope\" is not valid JSON";
//! здесь — anyhow-контекст serde) — вердикт (бросок) совпадает; безумно глубокая
//! вложенность (>~10k уровней) на стороне JS бросит RangeError V8, здесь —
//! stack overflow процесса: обе стороны падают, тексты не сравниваются.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_number, js_to_string, js_truthy};
use crate::structure_evidence::{
    get, iterable_values, js_json_string, js_number_to_string, sha256_str,
};
use crate::structure_scip::normalize_repo_path;

pub const STRUCTURE_PROTOCOL_VERSION: i64 = 1;
pub const STRUCTURE_FACTS_SCHEMA: &str = "code-structure/structure-facts.v1";
pub const STRUCTURE_ASSESSMENT_SCHEMA: &str = "code-structure/structure-assessment.v1";
pub const OBLIGATIONS_SCHEMA: &str = "code-structure/obligations-export.v1";

/// DEFAULT_STRUCTURE_LIMITS из JS (дефолтный аргумент limits).
pub fn default_structure_limits() -> Value {
    json!({
        "maxBytes": 20 * 1024 * 1024,
        "maxEntities": 5000,
        "maxEdges": 15000,
        "maxContracts": 5000,
        "maxBoundaries": 5000,
        "maxTests": 10000,
        "maxObligations": 10000,
        "maxPathLength": 1000,
        "maxStringLength": 10000,
        "maxDepth": 30,
    })
}

/// Один лимит: raw-значение для текста ошибки (JS-шаблон `${limits.maxX}`)
/// и число для сравнения (JS Number(raw)).
#[derive(Clone)]
pub struct Limit {
    pub raw: Value,
    pub num: f64,
}

/// Лимиты как Option: None — «проверки нет» (в JS ключ был undefined/NaN).
#[derive(Clone, Default)]
pub struct Limits {
    pub max_bytes: Option<Limit>,
    pub max_entities: Option<Limit>,
    pub max_edges: Option<Limit>,
    pub max_contracts: Option<Limit>,
    pub max_boundaries: Option<Limit>,
    pub max_tests: Option<Limit>,
    pub max_obligations: Option<Limit>,
    pub max_path_length: Option<Limit>,
    pub max_string_length: Option<Limit>,
    pub max_depth: Option<Limit>,
}

/// JS `${v}` в шаблоне: undefined → "undefined", null → "null", строка как
/// есть, числа — через ECMA Number::toString, объекты — аппроксимация serde.
fn js_template(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n
            .as_f64()
            .map(js_number_to_string)
            .unwrap_or_else(|| n.to_string()),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// JS Number(value) для значения лимита: null → 0, bool → 0/1, числовая
/// строка → число, нечисловая строка/объект → None (NaN: сравнение всегда false).
fn limit_number(v: &Value) -> Option<f64> {
    js_to_number(v)
}

fn limit_field(m: &Map<String, Value>, key: &str) -> Option<Limit> {
    let raw = m.get(key)?;
    limit_number(raw).map(|num| Limit {
        raw: raw.clone(),
        num,
    })
}

/// Парсинг аргумента limits. `first_field` — поле, которое JS прочитал бы
/// первым по порядку выполнения: при limits=null JS бросил бы TypeError
/// "Cannot read properties of null (reading '<first_field>')".
pub fn limits_from(v: Option<&Value>, first_field: &str) -> Result<Limits> {
    match v {
        None => limits_from(Some(&default_structure_limits()), first_field),
        Some(Value::Null) => {
            bail!("Cannot read properties of null (reading '{first_field}')")
        }
        Some(Value::Object(m)) => Ok(Limits {
            max_bytes: limit_field(m, "maxBytes"),
            max_entities: limit_field(m, "maxEntities"),
            max_edges: limit_field(m, "maxEdges"),
            max_contracts: limit_field(m, "maxContracts"),
            max_boundaries: limit_field(m, "maxBoundaries"),
            max_tests: limit_field(m, "maxTests"),
            max_obligations: limit_field(m, "maxObligations"),
            max_path_length: limit_field(m, "maxPathLength"),
            max_string_length: limit_field(m, "maxStringLength"),
            max_depth: limit_field(m, "maxDepth"),
        }),
        // truthy/falsy не-объект: limits.maxX — undefined → проверок нет
        Some(_) => Ok(Limits::default()),
    }
}

/// Текст лимета для сообщения как в JS-шаблоне (`${limits.maxX}`).
fn limit_text(l: &Limit) -> String {
    js_template(Some(&l.raw))
}

/// JS `value.length` для строк — UTF-16 code units.
fn js_string_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Ключ — канонический JS array index: только цифры, < 2^32-1, без ведущих
/// нулей ("01" — не индекс).
fn js_array_index(k: &str) -> Option<u32> {
    if k.is_empty() || k.len() > 10 || !k.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = k.parse().ok()?;
    if n >= u32::MAX as u64 || n.to_string() != k {
        return None;
    }
    Some(n as u32)
}

/// Порядок Object.keys / Object.values: целочисленные ключи по возрастанию,
/// затем строковые в порядке вставки (serde preserve_order даёт тот же
/// document-order для остальных ключей).
fn js_own_keys(m: &Map<String, Value>) -> Vec<&String> {
    let mut indices: Vec<(u32, &String)> = Vec::new();
    let mut rest: Vec<&String> = Vec::new();
    for k in m.keys() {
        match js_array_index(k) {
            Some(i) => indices.push((i, k)),
            None => rest.push(k),
        }
    }
    indices.sort_by_key(|(i, _)| *i);
    indices.into_iter().map(|(_, k)| k).chain(rest).collect()
}

/// JS Set-key (SameValueZero): числа по f64 (-0 === 0), объекты по идентичности.
fn svz_key(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("bool:{b}"),
        Value::Number(n) => match n.as_f64() {
            Some(x) => {
                let x = if x == 0.0 { 0.0f64 } else { x };
                format!("num:{:016x}", x.to_bits())
            }
            None => format!("num:{n}"),
        },
        Value::String(s) => format!("str:{s}"),
        other => format!("obj:{:p}", other as *const Value),
    }
}

/// String(value || '') — falsy → пустая строка, иначе JS String(value).
fn js_string_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------------------
// Базовые проверки (assertKind / inspectValue / assertPaths)
// ---------------------------------------------------------------------------

/// Порт assertKind: объект, version === 1, kind, schemaRef (строгое
/// равенство строк). Возвращает мапу payload'а для последующих проверок.
fn assert_kind<'a>(
    payload: &'a Value,
    kind: &str,
    schema_ref: &str,
) -> Result<&'a Map<String, Value>> {
    let m = match payload {
        Value::Object(m) => m,
        _ => bail!("{kind} payload must be an object"),
    };
    match m.get("version") {
        Some(Value::Number(n)) if n.as_f64() == Some(1.0) => {}
        other => bail!("unsupported {kind} version: {}", js_template(other)),
    }
    match m.get("kind") {
        Some(Value::String(s)) if s == kind => {}
        _ => bail!("expected kind={kind}"),
    }
    match m.get("schemaRef") {
        Some(Value::String(s)) if s == schema_ref => {}
        _ => bail!("expected schemaRef={schema_ref}"),
    }
    Ok(m)
}

/// Порт inspectValue: глубина (root = 0, `depth > maxDepth`), длина строк
/// (UTF-16 code units), рекурсия по массивам и Object.values.
fn inspect_value(v: &Value, limits: &Limits, depth: usize) -> Result<()> {
    if let Some(l) = &limits.max_depth {
        if depth as f64 > l.num {
            bail!("structure payload nesting exceeds {}", limit_text(l));
        }
    }
    if let Value::String(s) = v {
        if let Some(l) = &limits.max_string_length {
            if js_string_len(s) as f64 > l.num {
                bail!(
                    "structure payload string exceeds {} characters",
                    limit_text(l)
                );
            }
        }
    }
    match v {
        Value::Array(items) => {
            for item in items {
                inspect_value(item, limits, depth + 1)?;
            }
        }
        Value::Object(m) => {
            for k in js_own_keys(m) {
                inspect_value(&m[k], limits, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Порт assertPaths: record?.[pathField] != null → normalizeRepoPath(String(v || '')).
fn assert_paths(records: &[Value], path_field: &str) -> Result<()> {
    for record in records {
        let v = match record {
            Value::Object(m) => m.get(path_field),
            _ => None, // record?.[pathField] для скаляра — undefined
        };
        if let Some(x) = v {
            if !x.is_null() {
                normalize_repo_path(&js_string_or_empty(Some(x)))?;
            }
        }
    }
    Ok(())
}

/// Порт assertReferentialIntegrity: ребро/контракт не может ссылаться на
/// сущность вне набора; пустая строка и null-рефы не проверяются; текст
/// ошибки включает JSON.stringify(String(ref)).
fn assert_referential_integrity(facts: &Map<String, Value>) -> Result<()> {
    let empty: Vec<Value> = Vec::new();
    let entities = match facts.get("entities") {
        Some(Value::Array(a)) => a,
        _ => &empty,
    };
    let mut known: HashSet<String> = HashSet::new();
    for entity in entities {
        if let Value::Object(m) = entity {
            if let Some(id) = m.get("observedId") {
                known.insert(svz_key(id));
            }
        }
    }
    let check = |refv: Option<&Value>, what: &str| -> Result<()> {
        let r = match refv {
            Some(r) => r,
            None => return Ok(()),
        };
        if r.is_null() {
            return Ok(());
        }
        if matches!(r, Value::String(s) if s.is_empty()) {
            return Ok(());
        }
        if !known.contains(&svz_key(r)) {
            bail!(
                "{what} references unknown entity: {}",
                js_json_string(&js_to_string(r))
            );
        }
        Ok(())
    };
    fn field<'a>(item: &'a Value, key: &str) -> Option<&'a Value> {
        item.as_object().and_then(|m| m.get(key))
    }
    let deps = match facts.get("dependencies") {
        Some(Value::Array(a)) => a,
        _ => &empty,
    };
    for dep in deps {
        check(field(dep, "from"), "dependency.from")?;
        check(field(dep, "to"), "dependency.to")?;
    }
    let contracts = match facts.get("contracts") {
        Some(Value::Array(a)) => a,
        _ => &empty,
    };
    for contract in contracts {
        check(field(contract, "entityRef"), "contract.entityRef")?;
        check(field(contract, "targetRef"), "contract.targetRef")?;
    }
    Ok(())
}

/// Проверка длин путей границ: normalizeRepoPath(path).length > maxPathLength.
/// boundary=null — V8-TypeError; truthy не-итерируемый paths — TypeError
/// "not iterable" (через iterable_values); строка paths итерируется посимвольно.
fn assert_boundary_paths(boundaries: &[Value], limits: &Limits) -> Result<()> {
    for boundary in boundaries {
        let paths_val = match boundary {
            Value::Null => {
                bail!("Cannot read properties of null (reading 'paths')")
            }
            other => other.as_object().and_then(|m| m.get("paths")),
        };
        for path in iterable_values(paths_val, "paths")? {
            let normalized = normalize_repo_path(&js_string_or_empty(Some(&path)))?;
            if let Some(l) = &limits.max_path_length {
                if js_string_len(&normalized) as f64 > l.num {
                    bail!("path length limit exceeded");
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Публичные валидаторы
// ---------------------------------------------------------------------------

/// Порт validateStructureFacts: вернёт payload при успехе (как JS), иначе
/// доменная ошибка с посимвольно тем же текстом.
pub fn validate_structure_facts(payload: &Value, limits: Option<&Value>) -> Result<Value> {
    let limits = limits_from(limits, "maxDepth")?;
    let m = assert_kind(payload, "structure_facts", STRUCTURE_FACTS_SCHEMA)?;
    inspect_value(payload, &limits, 0)?;
    let empty_map = Map::new();
    // payload.facts || {}: falsy → {}; truthy не-объект — доступ к полям даёт
    // undefined → тот же итог «arrays are required»
    let facts = match m.get("facts") {
        Some(Value::Object(f)) => f,
        _ => &empty_map,
    };
    let arr = |k: &str| match facts.get(k) {
        Some(Value::Array(a)) => Some(a.as_slice()),
        _ => None,
    };
    if arr("boundaries").is_none()
        || arr("entities").is_none()
        || arr("contracts").is_none()
        || arr("dependencies").is_none()
        || arr("testCases").is_none()
    {
        bail!("structure facts arrays are required");
    }
    let over = |a: Option<&[Value]>, l: &Option<Limit>| -> bool {
        match (a, l) {
            (Some(a), Some(l)) => a.len() as f64 > l.num,
            _ => false,
        }
    };
    if over(arr("entities"), &limits.max_entities) {
        bail!("entity limit exceeded");
    }
    if over(arr("dependencies"), &limits.max_edges) {
        bail!("dependency limit exceeded");
    }
    if over(arr("contracts"), &limits.max_contracts) {
        bail!("contract limit exceeded");
    }
    if over(arr("boundaries"), &limits.max_boundaries) {
        bail!("boundary limit exceeded");
    }
    if over(arr("testCases"), &limits.max_tests) {
        bail!("test case limit exceeded");
    }
    assert_paths(arr("entities").unwrap_or(&[]), "path")?;
    assert_paths(arr("testCases").unwrap_or(&[]), "path")?;
    assert_referential_integrity(facts)?;
    assert_boundary_paths(arr("boundaries").unwrap_or(&[]), &limits)?;
    if !is_sha256_hex(&js_string_or_empty(m.get("fingerprint"))) {
        bail!("facts fingerprint must be sha256");
    }
    Ok(payload.clone())
}

/// Порт validateObligationsExport.
pub fn validate_obligations_export(payload: &Value, limits: Option<&Value>) -> Result<Value> {
    let limits = limits_from(limits, "maxDepth")?;
    let m = assert_kind(payload, "obligations_export", OBLIGATIONS_SCHEMA)?;
    inspect_value(payload, &limits, 0)?;
    let obligations = match m.get("obligations") {
        Some(Value::Array(a)) => a,
        _ => bail!("obligations array is required"),
    };
    if let Some(l) = &limits.max_obligations {
        if obligations.len() as f64 > l.num {
            bail!("obligation limit exceeded");
        }
    }
    for obligation in obligations {
        let field = |k: &str| obligation.as_object().and_then(|m| m.get(k));
        let ok = ["id", "contentHash", "statement"]
            .iter()
            .all(|k| field(k).map(js_truthy).unwrap_or(false));
        if !ok {
            bail!("obligation id, contentHash and statement are required");
        }
    }
    Ok(payload.clone())
}

/// Порт validateStructureAssessment.
pub fn validate_structure_assessment(payload: &Value, limits: Option<&Value>) -> Result<Value> {
    let limits = limits_from(limits, "maxDepth")?;
    let m = assert_kind(payload, "structure_assessment", STRUCTURE_ASSESSMENT_SCHEMA)?;
    inspect_value(payload, &limits, 0)?;
    for field in ["fingerprint", "factsFingerprint", "coverageHash", "thresholdsHash"] {
        if !is_sha256_hex(&js_string_or_empty(m.get(field))) {
            bail!("{field} must be sha256");
        }
    }
    match m.get("temporalAlignment") {
        Some(Value::String(s)) if ["aligned", "mismatch", "unknown"].contains(&s.as_str()) => {}
        _ => bail!("invalid temporalAlignment"),
    }
    let scenarios_ok = match m.get("assessments") {
        Some(Value::Object(a)) => matches!(a.get("contractScenarios"), Some(Value::Array(_))),
        _ => false, // assessments?.contractScenarios для не-объекта — undefined
    };
    let is_arr = |k: &str| matches!(m.get(k), Some(Value::Array(_)));
    if !is_arr("coverageObservations") || !scenarios_ok || !is_arr("hints") || !is_arr("receipts") {
        bail!("assessment arrays are required");
    }
    Ok(payload.clone())
}

/// Порт assertPayloadSize: Buffer.byteLength(String(text || '')) и проверка
/// maxBytes. Возвращает число байт.
pub fn assert_payload_size(text: &str, limits: Option<&Value>) -> Result<f64> {
    let limits = limits_from(limits, "maxBytes")?;
    let bytes = text.len() as f64; // Buffer.byteLength — UTF-8 байты
    if let Some(l) = &limits.max_bytes {
        if bytes > l.num {
            bail!("structure payload exceeds {} bytes", limit_text(l));
        }
    }
    Ok(bytes)
}

/// JS String(text || '') для аргумента text: null/falsy → '', иначе String(v).
pub fn payload_text(v: Option<&Value>) -> String {
    js_string_or_empty(v)
}

/// Порт parseStructurePayload: assertPayloadSize → JSON.parse → диспетчер
/// по kind. Тексты SyntaxError JSON.parse не воспроизводятся (отклонение,
/// см. шапку модуля) — вердикт совпадает.
pub fn parse_structure_payload(text: &str, limits: Option<&Value>) -> Result<Value> {
    assert_payload_size(text, limits)?;
    let payload: Value =
        serde_json::from_str(text).context("вход не является валидным JSON")?;
    let kind = payload.as_object().and_then(|m| m.get("kind"));
    match kind {
        Some(Value::String(s)) if s == "structure_facts" => {
            validate_structure_facts(&payload, limits)
        }
        Some(Value::String(s)) if s == "structure_assessment" => {
            validate_structure_assessment(&payload, limits)
        }
        Some(Value::String(s)) if s == "obligations_export" => {
            validate_obligations_export(&payload, limits)
        }
        other => bail!("unsupported structure payload kind: {}", js_template(other)),
    }
}

// ---------------------------------------------------------------------------
// Фингерпринты и temporalAlignment
// ---------------------------------------------------------------------------

/// Элемент Array.prototype.join: null/undefined → '', иначе String(v).
fn join_part(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => js_to_string(x),
    }
}

/// v || fallback по truthiness.
fn or_fallback(v: Option<&Value>, fallback: &str) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => fallback.to_string(),
    }
}

/// Порт factsFingerprint: sha256(['structure_facts', headSha, providerVersion,
/// configHash, scope || ''].join('\0')).
pub fn facts_fingerprint_op(input: &Value) -> String {
    let get = |k: &str| get(input, k);
    let parts = [
        "structure_facts".to_string(),
        join_part(get("headSha")),
        join_part(get("providerVersion")),
        join_part(get("configHash")),
        or_fallback(get("scope"), ""),
    ];
    sha256_str(&parts.join("\0"))
}

/// Порт assessmentFingerprint: sha256(['structure_assessment', facts,
/// obligationsRevision || 'none', coverageHash, thresholdsHash].join('\0')).
pub fn assessment_fingerprint_op(input: &Value) -> String {
    let get = |k: &str| get(input, k);
    let parts = [
        "structure_assessment".to_string(),
        join_part(get("facts")),
        or_fallback(get("obligationsRevision"), "none"),
        join_part(get("coverageHash")),
        join_part(get("thresholdsHash")),
    ];
    sha256_str(&parts.join("\0"))
}

/// Порт temporalAlignment: String(sha || '')-сравнения; нехватка facts/test →
/// 'unknown', расхождение sha → 'mismatch', иначе 'aligned'.
pub fn temporal_alignment_op(input: &Value) -> &'static str {
    let get = |k: &str| get(input, k);
    let facts = or_fallback(get("factsHeadSha"), "");
    let test = or_fallback(get("testHeadSha"), "");
    let docs = or_fallback(get("obligationsHeadSha"), "");
    if facts.is_empty() || test.is_empty() {
        return "unknown";
    }
    if facts != test {
        return "mismatch";
    }
    if !docs.is_empty() && docs != facts {
        return "mismatch";
    }
    "aligned"
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn h(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn minimal_facts() -> Value {
        json!({
            "schemaRef": "code-structure/structure-facts.v1",
            "version": 1,
            "kind": "structure_facts",
            "generatedAt": "2026-09-02T00:00:00.000Z",
            "fingerprint": h('a'),
            "provenance": {},
            "facts": {
                "boundaries": [], "entities": [], "contracts": [],
                "dependencies": [], "testCases": [],
            },
            "identityEvents": [], "views": {}, "receipts": [],
        })
    }

    fn minimal_obligations() -> Value {
        json!({
            "schemaRef": "code-structure/obligations-export.v1",
            "version": 1,
            "kind": "obligations_export",
            "project": "p",
            "revision": "r",
            "exportedAt": "2026-09-02T00:00:00.000Z",
            "obligations": [],
        })
    }

    fn minimal_assessment() -> Value {
        json!({
            "schemaRef": "code-structure/structure-assessment.v1",
            "version": 1,
            "kind": "structure_assessment",
            "generatedAt": "2026-09-02T00:00:00.000Z",
            "fingerprint": h('a'),
            "factsFingerprint": h('b'),
            "obligationsRevision": "none",
            "coverageHash": h('c'),
            "thresholdsHash": h('d'),
            "testRun": {"id": "t", "headSha": "abcdef1", "producedAt": "2026-09-02T00:00:00.000Z"},
            "temporalAlignment": "aligned",
            "coverageObservations": [],
            "assessments": {"contractScenarios": []},
            "hints": [],
            "receipts": [],
        })
    }

    fn entity(id: &str) -> Value {
        json!({
            "observedId": id, "lineageId": id, "label": id, "kind": "function",
            "path": "src/a.ts", "confidence": "exact", "evidence": [], "externalIds": {},
        })
    }

    #[test]
    fn minimal_payloads_pass() {
        assert!(validate_structure_facts(&minimal_facts(), None).is_ok());
        assert!(validate_obligations_export(&minimal_obligations(), None).is_ok());
        assert!(validate_structure_assessment(&minimal_assessment(), None).is_ok());
        // validators возвращают сам payload
        let facts = minimal_facts();
        assert_eq!(validate_structure_facts(&facts, None).unwrap(), facts);
    }

    #[test]
    fn assert_kind_branches() {
        let err = validate_structure_facts(&Value::Null, None).unwrap_err();
        assert_eq!(format!("{err:#}"), "structure_facts payload must be an object");
        let err = validate_structure_facts(&json!([]), None).unwrap_err();
        assert_eq!(format!("{err:#}"), "structure_facts payload must be an object");
        let mut bad = minimal_facts();
        bad["version"] = json!(2);
        assert_eq!(
            format!("{:#}", validate_structure_facts(&bad, None).unwrap_err()),
            "unsupported structure_facts version: 2"
        );
        let mut bad = minimal_facts();
        bad.as_object_mut().unwrap().remove("version");
        assert_eq!(
            format!("{:#}", validate_structure_facts(&bad, None).unwrap_err()),
            "unsupported structure_facts version: undefined"
        );
        let mut bad = minimal_facts();
        bad["kind"] = json!("mystery");
        assert_eq!(
            format!("{:#}", validate_structure_facts(&bad, None).unwrap_err()),
            "expected kind=structure_facts"
        );
        let mut bad = minimal_facts();
        bad["schemaRef"] = json!("nope");
        assert_eq!(
            format!("{:#}", validate_structure_facts(&bad, None).unwrap_err()),
            "expected schemaRef=code-structure/structure-facts.v1"
        );
    }

    #[test]
    fn facts_arrays_and_limits() {
        let mut bad = minimal_facts();
        bad["facts"] = json!({"entities": [], "contracts": [], "dependencies": [], "testCases": []});
        assert_eq!(
            format!("{:#}", validate_structure_facts(&bad, None).unwrap_err()),
            "structure facts arrays are required"
        );
        // порядок лимитов: entities раньше dependencies
        let mut bad = minimal_facts();
        bad["facts"] = json!({
            "boundaries": [], "entities": [entity("e1"), entity("e2")],
            "contracts": [], "dependencies": [], "testCases": [],
        });
        let two = json!({"maxEntities": 1});
        assert_eq!(
            format!("{:#}", validate_structure_facts(&bad, Some(&two)).unwrap_err()),
            "entity limit exceeded"
        );
        // maxEntities выше — проходит (остальные лимиты отсутствуют → нет проверок)
        let ok = json!({"maxEntities": 100});
        assert!(validate_structure_facts(&bad, Some(&ok)).is_ok());
        // null-лимит — JS `len > null` → len > 0
        let null_limit = json!({"maxEntities": serde_json::Value::Null});
        assert_eq!(
            format!("{:#}", validate_structure_facts(&bad, Some(&null_limit)).unwrap_err()),
            "entity limit exceeded"
        );
        // строковый лимит — Number("1") === 1
        let str_limit = json!({"maxEntities": "1"});
        assert_eq!(
            format!("{:#}", validate_structure_facts(&bad, Some(&str_limit)).unwrap_err()),
            "entity limit exceeded"
        );
        // нечисловой лимит — NaN-сравнение → нет проверки
        let nan_limit = json!({"maxEntities": "nope"});
        assert!(validate_structure_facts(&bad, Some(&nan_limit)).is_ok());
    }

    #[test]
    fn facts_referential_integrity() {
        let mut facts = minimal_facts();
        facts["facts"] = json!({
            "boundaries": [], "entities": [entity("known")], "contracts": [],
            "dependencies": [{
                "id": "e1", "from": "known", "to": "gone", "relation": "calls",
                "confidence": "exact", "evidence": [],
            }],
            "testCases": [],
        });
        assert_eq!(
            format!("{:#}", validate_structure_facts(&facts, None).unwrap_err()),
            "dependency.to references unknown entity: \"gone\""
        );
        facts["facts"]["dependencies"][0]["to"] = json!("known");
        facts["facts"]["contracts"] = json!([{
            "id": "c1", "entityRef": "known", "kind": "export",
            "targetRef": "gone", "confidence": "exact", "evidence": [],
        }]);
        assert_eq!(
            format!("{:#}", validate_structure_facts(&facts, None).unwrap_err()),
            "contract.targetRef references unknown entity: \"gone\""
        );
        // пустая строка и null-рефы не проверяются
        facts["facts"]["contracts"][0]["targetRef"] = json!("");
        assert!(validate_structure_facts(&facts, None).is_ok());
    }

    #[test]
    fn facts_paths_and_fingerprint() {
        let mut facts = minimal_facts();
        facts["facts"]["entities"] = json!([entity("e1")]);
        facts["facts"]["entities"][0]["path"] = json!("../evil");
        assert!(format!("{:#}", validate_structure_facts(&facts, None).unwrap_err())
            .starts_with("unsafe repository path:"));
        facts["facts"]["entities"][0]["path"] = json!("src/a.ts");
        facts["fingerprint"] = json!("ff");
        assert_eq!(
            format!("{:#}", validate_structure_facts(&facts, None).unwrap_err()),
            "facts fingerprint must be sha256"
        );
        facts["fingerprint"] = json!(h('a').to_uppercase());
        assert_eq!(
            format!("{:#}", validate_structure_facts(&facts, None).unwrap_err()),
            "facts fingerprint must be sha256"
        );
    }

    #[test]
    fn boundary_path_length_and_null_boundary() {
        let mut facts = minimal_facts();
        facts["facts"] = json!({
            "boundaries": [{"id": "b1", "kind": "module", "name": "m", "paths": ["x".repeat(1001)]}],
            "entities": [], "contracts": [], "dependencies": [], "testCases": [],
        });
        assert_eq!(
            format!("{:#}", validate_structure_facts(&facts, None).unwrap_err()),
            "path length limit exceeded"
        );
        facts["facts"]["boundaries"][0]["paths"] = json!(["x".repeat(1000)]);
        assert!(validate_structure_facts(&facts, None).is_ok());
        facts["facts"]["boundaries"] = json!([serde_json::Value::Null]);
        assert_eq!(
            format!("{:#}", validate_structure_facts(&facts, None).unwrap_err()),
            "Cannot read properties of null (reading 'paths')"
        );
    }

    #[test]
    fn inspect_value_string_len_is_utf16() {
        // 5001 astral-символов = 10002 UTF-16 code units > 10000; байтов — 20004
        let mut facts = minimal_facts();
        facts["facts"]["entities"] = json!([entity("e1")]);
        facts["facts"]["entities"][0]["label"] = json!("😀".repeat(5001));
        assert_eq!(
            format!("{:#}", validate_structure_facts(&facts, None).unwrap_err()),
            "structure payload string exceeds 10000 characters"
        );
        // BMP-строка: 5000 символов = 5000 code units — ок
        facts["facts"]["entities"][0]["label"] = json!("a".repeat(5000));
        assert!(validate_structure_facts(&facts, None).is_ok());
    }

    #[test]
    fn inspect_value_depth() {
        let mut facts = minimal_facts();
        facts["facts"]["entities"] = json!([entity("e1")]);
        // вложенность: payload=0, facts=1, entities=[2], {3}, externalIds={4}...
        // цепочка глубины 31 от корня бросает при depth > 30
        let mut deep = json!({});
        let mut cur = &mut deep;
        for _ in 0..27 {
            let next = json!({});
            cur["x"] = next;
            cur = &mut cur["x"];
        }
        facts["facts"]["entities"][0]["externalIds"] = deep;
        assert_eq!(
            format!("{:#}", validate_structure_facts(&facts, None).unwrap_err()),
            "structure payload nesting exceeds 30"
        );
    }

    #[test]
    fn inspect_value_integer_like_keys_first() {
        // JS own-property enumeration: целочисленные ключи идут раньше
        // строковых независимо от порядка в документе — первая же ошибка
        // бросается на "2", а не на "b".
        let mut facts = minimal_facts();
        facts["facts"]["entities"] = json!([entity("e1")]);
        facts["facts"]["entities"][0]["externalIds"] = json!({
            "b": "x".repeat(20000),
            "2": "y".repeat(20000),
            "a": "z".repeat(20000),
        });
        let err = validate_structure_facts(&facts, None).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "structure payload string exceeds 10000 characters"
        );
    }

    #[test]
    fn obligations_branches() {
        let mut bad = minimal_obligations();
        bad["obligations"] = json!({});
        assert_eq!(
            format!("{:#}", validate_obligations_export(&bad, None).unwrap_err()),
            "obligations array is required"
        );
        let mut bad = minimal_obligations();
        bad["obligations"] = json!([{"id": "o1", "contentHash": h('a'), "statement": "s"}]);
        assert!(validate_obligations_export(&bad, None).is_ok());
        for broken in [
            json!({"id": "", "contentHash": h('a'), "statement": "s"}),
            json!({"id": "o1", "contentHash": h('a')}),
            json!({"id": "o1", "contentHash": 0, "statement": "s"}),
            json!({"id": "o1", "contentHash": h('a'), "statement": null}),
        ] {
            bad["obligations"] = json!([broken]);
            assert_eq!(
                format!("{:#}", validate_obligations_export(&bad, None).unwrap_err()),
                "obligation id, contentHash and statement are required"
            );
        }
        let many: Vec<Value> = (0..10001)
            .map(|i| json!({"id": format!("o{i}"), "contentHash": h('a'), "statement": "s"}))
            .collect();
        bad["obligations"] = json!(many);
        assert_eq!(
            format!("{:#}", validate_obligations_export(&bad, None).unwrap_err()),
            "obligation limit exceeded"
        );
    }

    #[test]
    fn assessment_branches() {
        let mut bad = minimal_assessment();
        bad["fingerprint"] = json!("ff");
        assert_eq!(
            format!("{:#}", validate_structure_assessment(&bad, None).unwrap_err()),
            "fingerprint must be sha256"
        );
        let mut bad2 = minimal_assessment();
        bad2["thresholdsHash"] = json!(h('d').to_uppercase());
        assert_eq!(
            format!("{:#}", validate_structure_assessment(&bad2, None).unwrap_err()),
            "thresholdsHash must be sha256"
        );
        let mut bad = minimal_assessment();
        bad["temporalAlignment"] = json!("bogus");
        assert_eq!(
            format!("{:#}", validate_structure_assessment(&bad, None).unwrap_err()),
            "invalid temporalAlignment"
        );
        for (k, v) in [
            ("coverageObservations", json!({})),
            ("assessments", json!(serde_json::Value::Null)),
            ("hints", json!("no")),
            ("receipts", json!(42)),
        ] {
            let mut bad = minimal_assessment();
            bad[k] = v;
            assert_eq!(
                format!("{:#}", validate_structure_assessment(&bad, None).unwrap_err()),
                "assessment arrays are required",
                "field {k}"
            );
        }
    }

    #[test]
    fn payload_size_semantics() {
        assert_eq!(assert_payload_size("", None).unwrap(), 0.0);
        // UTF-8 байты, не code units
        assert_eq!(assert_payload_size("é😀", None).unwrap(), 6.0);
        let tiny = json!({"maxBytes": 5});
        let err = assert_payload_size("123456", Some(&tiny)).unwrap_err();
        assert_eq!(format!("{err:#}"), "structure payload exceeds 5 bytes");
        // null-лимит: bytes > 0; в тексте — raw null
        let null_limit = json!({"maxBytes": serde_json::Value::Null});
        let err = assert_payload_size("x", Some(&null_limit)).unwrap_err();
        assert_eq!(format!("{err:#}"), "structure payload exceeds null bytes");
        // limits=null — V8 TypeError
        let err = assert_payload_size("x", Some(&Value::Null)).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "Cannot read properties of null (reading 'maxBytes')"
        );
    }

    #[test]
    fn parse_structure_payload_dispatch() {
        let facts = minimal_facts();
        let text = serde_json::to_string(&facts).unwrap();
        assert_eq!(parse_structure_payload(&text, None).unwrap(), facts);
        let err = parse_structure_payload("null", None).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "unsupported structure payload kind: undefined"
        );
        let err = parse_structure_payload("42", None).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "unsupported structure payload kind: undefined"
        );
        let err = parse_structure_payload("{\"kind\": \"mystery\"}", None).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "unsupported structure payload kind: mystery"
        );
        // size-гейт раньше парсинга
        let tiny = json!({"maxBytes": 5});
        let err = parse_structure_payload(&text, Some(&tiny)).unwrap_err();
        assert_eq!(format!("{err:#}"), "structure payload exceeds 5 bytes");
    }

    #[test]
    fn fingerprints_match_js_vectors() {
        // сверено с node: sha256(['structure_facts', ...].join('\0'))
        let fp = facts_fingerprint_op(&json!({
            "headSha": "abcdef1234567890",
            "providerVersion": "1.0.0",
            "configHash": h('c'),
            "scope": "src",
        }));
        assert_eq!(fp.len(), 64);
        // scope falsy → ''
        let no_scope = facts_fingerprint_op(&json!({
            "headSha": "abcdef1234567890",
            "providerVersion": "1.0.0",
            "configHash": h('c'),
        }));
        let empty_scope = facts_fingerprint_op(&json!({
            "headSha": "abcdef1234567890",
            "providerVersion": "1.0.0",
            "configHash": h('c'),
            "scope": 0,
        }));
        assert_eq!(no_scope, empty_scope);
        let a = assessment_fingerprint_op(&json!({
            "facts": "f", "coverageHash": "c", "thresholdsHash": "t",
        }));
        let b = assessment_fingerprint_op(&json!({
            "facts": "f", "obligationsRevision": "", "coverageHash": "c", "thresholdsHash": "t",
        }));
        let c = assessment_fingerprint_op(&json!({
            "facts": "f", "obligationsRevision": 0, "coverageHash": "c", "thresholdsHash": "t",
        }));
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_ne!(
            a,
            assessment_fingerprint_op(&json!({
                "facts": "f", "obligationsRevision": "r1", "coverageHash": "c", "thresholdsHash": "t",
            }))
        );
    }

    #[test]
    fn temporal_alignment_vectors() {
        let v = |a: Value, b: Value, c: Value| {
            temporal_alignment_op(&json!({
                "factsHeadSha": a, "testHeadSha": b, "obligationsHeadSha": c,
            }))
        };
        assert_eq!(v(json!("s1"), json!("s1"), json!("")), "aligned");
        assert_eq!(v(json!("s1"), json!("s1"), json!("s1")), "aligned");
        assert_eq!(v(json!("s1"), json!("s2"), json!("")), "mismatch");
        assert_eq!(v(json!("s1"), json!("s1"), json!("s2")), "mismatch");
        assert_eq!(v(json!(""), json!("s1"), json!("")), "unknown");
        assert_eq!(v(json!("s1"), json!(""), json!("")), "unknown");
        assert_eq!(v(Value::Null, json!("s1"), json!("")), "unknown");
        assert_eq!(v(json!(0), json!("s1"), json!("")), "unknown");
    }
}
