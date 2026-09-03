//! Порт lib/structure-mutation.mjs: разбор mutation-testing-elements отчётов в
//! наблюдения evidence и выбор критичных контрактов для скоупа мутаций.
//!
//! Паритет с JS сохранён, включая:
//! - `Number(location.start?.line || mutant.line || 0) || null`: `||`-цепочка по
//!   truthiness, `Number('') === 0`, `Number('abc') → NaN → null`, строка '0' —
//!   truthy, но даёт line null; Infinity сохраняется (JSON.stringify → null);
//! - entityRef/observedId как ключи JS Map: строки/числа/буллы — по значению,
//!   объекты — по идентичности (в JSON-мире равные объекты не совпадают);
//! - guards `entity.line/endLine && location.line && … > line`: оба truthy и
//!   сравнение без NaN, иначе guard пропускается;
//! - без location.line — привязка только при единственном кандидате файла;
//!   иначе stable-сортировка по `|entity.line || line - line|` (NaN-дистанция —
//!   как компаратор 0 у V8);
//! - статусы: точная строка из MUTANT_STATUS, иначе `String(status ||
//!   'unknown').toLowerCase()`; killedBy — Set-династия по raw-значениям, потом
//!   map(String) и побайтовая сортировка; финальная сортировка — localeCompare;
//! - `report.files` принимает и массив (Object.entries по индексам, как в JS);
//! - `flatMap(contractRefs || [])` в criticalContractRefs: не-массив оборачивается
//!   в один элемент; `entityRef === observedId` — строгое равенство (undefined
//!   === undefined); итог — filter(Boolean) + дефолтный Array.sort (по String).

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_number, js_to_string, js_truthy};
use crate::structure_scip::{locale_compare, normalize_repo_path, stable_id};

/// Значение-заглушка для мест, где JS читал бы undefined из отсутствующего ключа.
const NULL: Value = Value::Null;

fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(js_truthy).unwrap_or(false)
}

const MUTANT_STATUS: &[(&str, &str)] = &[
    ("Killed", "killed"),
    ("Survived", "survived"),
    ("NoCoverage", "no_coverage"),
    ("Timeout", "timeout"),
    ("CompileError", "compile_error"),
    ("RuntimeError", "runtime_error"),
    ("Ignored", "ignored"),
];

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object().and_then(|o| o.get(key))
}

/// JS Map/Set-ключ для значения из JSON: скаляры — по значению (числа через
/// f64: 1e2 и 100 совпадают, как SameValueZero), объекты — по идентичности:
/// в JS два равных, но разных объекта не совпадают, поэтому тут они получают
/// уникальные ключи (порядковый след не восстанавливаем — объекты как ключи
/// на практике не используются).
fn value_key(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("bool:{b}"),
        Value::Number(n) => n
            .as_f64()
            .map(|x| format!("num:{x}"))
            .unwrap_or_else(|| format!("num:{n}")),
        Value::String(s) => format!("str:{s}"),
        other => format!("obj:{}", serde_json::to_string(other).unwrap_or_default()),
    }
}

/// JS `===` для значений из JSON: скаляры по значению (0 === -0, NaN !== NaN),
/// объекты/массивы — только по идентичности (разные объекты никогда не равны).
fn strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true, // undefined === undefined
        (Some(x), Some(y)) => match (x, y) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(p), Value::Bool(q)) => p == q,
            (Value::Number(p), Value::Number(q)) => match (p.as_f64(), q.as_f64()) {
                (Some(m), Some(n)) => m == n, // f64 ==: -0 == 0, NaN != NaN
                _ => p == q,
            },
            (Value::String(p), Value::String(q)) => p == q,
            _ => false,
        },
        _ => false,
    }
}

/// num_json из structure_lcov: целые — через i64 (5.0 → 5), остальное — f64;
/// не-конечные → null (JSON.stringify(NaN/Infinity) === 'null').
fn num_json(n: f64) -> Value {
    if n.is_nan() || n.is_infinite() {
        Value::Null
    } else if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// JS `n || null` для уже прогнанного через Number(): 0/-0/NaN → null,
/// Infinity сохраняется (но при сериализации в JSON станет null).
fn num_or_null(n: Option<f64>) -> Option<f64> {
    match n {
        Some(x) if x != 0.0 && !x.is_nan() => Some(x),
        _ => None,
    }
}

/// JS String(n) для числа: NaN/±Infinity по-JS, целые — без '.0'.
fn f64_js_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n == f64::INFINITY {
        "Infinity".into()
    } else if n == f64::NEG_INFINITY {
        "-Infinity".into()
    } else if n == n.trunc() && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// `part ?? ''` из stableId: null/undefined → '', иначе String(part).
fn stable_part(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => js_to_string(x),
    }
}

/// `mutant.location || {}` + цепочка `a?.line || mutant.line || 0` с
/// truthiness-проверками; property access на не-объекте даёт undefined.
fn location_of(mutant: &Value) -> (Option<f64>, Option<f64>) {
    let location = get(mutant, "location").filter(|v| js_truthy(v));
    let sub_line = |which: &str| {
        let sub = get(location?, which).filter(|v| v.is_object());
        get(sub?, "line")
    };
    let pick = |first: Option<&Value>| {
        let raw = if first.map(js_truthy).unwrap_or(false) {
            first
        } else {
            get(mutant, "line").filter(|v| js_truthy(v))
        };
        let raw = raw.cloned().unwrap_or(json!(0));
        num_or_null(js_to_number(&raw))
    };
    (pick(sub_line("start")), pick(sub_line("end").or_else(|| sub_line("start"))))
}

/// Привязка мутанта к контракту через вмещающую entity (порт contractForMutant).
fn contract_for_mutant(
    path: &str,
    line: Option<f64>,
    contracts: &[Value],
    entities_by_id: &HashMap<String, &Value>,
) -> Option<Value> {
    let mut candidates: Vec<(&Value, &Value)> = Vec::new();
    for contract in contracts {
        let Some(entity) = get(contract, "entityRef")
            .and_then(|key| entities_by_id.get(&value_key(key)).copied())
        else {
            continue;
        };
        if get(entity, "path").and_then(Value::as_str) != Some(path) {
            continue;
        }
        // guards применяются только когда оба операнда truthy; сравнение без NaN
        if let Some(l) = line {
            let entity_line = js_to_number(get(entity, "line").unwrap_or(&NULL));
            if truthy_opt(get(entity, "line"))
                && entity_line.map(|n| !n.is_nan() && n > l).unwrap_or(false)
            {
                continue;
            }
            let entity_end = js_to_number(get(entity, "endLine").unwrap_or(&NULL));
            if truthy_opt(get(entity, "endLine"))
                && entity_end.map(|n| !n.is_nan() && n < l).unwrap_or(false)
            {
                continue;
            }
        }
        candidates.push((contract, entity));
    }
    // без строки множественная привязка — ложное evidence: только однозначный кейс
    let Some(l) = line else {
        return if candidates.len() == 1 {
            Some(candidates[0].0.clone())
        } else {
            None
        };
    };
    // |entity.line || line - line|: falsy entity.line → дистанция 0; NaN-дистанция
    // (truthy, но не число) — как компаратор 0 у V8
    let distance = |entity: &Value| {
        let raw = get(entity, "line");
        let base = if raw.map(js_truthy).unwrap_or(false) {
            js_to_number(raw.unwrap_or(&Value::Null))
        } else {
            Some(l)
        };
        base.map(|b| (b - l).abs())
    };
    candidates.sort_by(|a, b| match (distance(a.1), distance(b.1)) {
        // JS-компаратор aDistance - bDistance: любая NaN-дистанция → NaN → 0.
        // Option::partial_cmp тут не подходит: None < Some(_) по правилам Option.
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => std::cmp::Ordering::Equal,
    });
    candidates.first().map(|(contract, _)| (*contract).clone())
}

/// Object.entries для `report.files` (массив в JS — тоже object: ключи-индексы).
fn object_entries(v: Option<&Value>) -> Vec<(String, Value)> {
    match v.filter(|x| x.is_object() || x.as_array().is_some()) {
        Some(Value::Object(m)) => m.iter().map(|(k, val)| (k.clone(), val.clone())).collect(),
        Some(Value::Array(a)) => a
            .iter()
            .enumerate()
            .map(|(i, val)| (i.to_string(), val.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// `[...new Set(xs)]` для значений из JSON: скаляры дедуплицируются по значению,
/// объекты — никогда (идентичность). Порядок первого появления сохраняется.
fn unique_values(xs: &[Value]) -> Vec<Value> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (i, x) in xs.iter().enumerate() {
        // объекты получают уникальный ключ с индексом — Set по идентичности
        let key = match x {
            Value::Object(_) | Value::Array(_) => format!("obj#{i}"),
            scalar => value_key(scalar),
        };
        if seen.insert(key) {
            out.push(x.clone());
        }
    }
    out
}

/// Итерируемое значение (`xs || []` в for..of/spread): массив — элементы,
/// строка — посимвольно (JS итерирует строки), falsy — пусто; прочее truthy —
/// TypeError, как в JS.
fn iterable_values(v: Option<&Value>, what: &str) -> Result<Vec<Value>> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()), // null/undefined || []
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(Value::String(s)) => Ok(s.chars().map(|c| json!(c.to_string())).collect()),
        Some(other) if js_truthy(other) => bail!("{what} is not iterable"),
        _ => Ok(Vec::new()),
    }
}

/// Порт parseMutationReport: отчёт mutation-testing-elements → наблюдения.
pub fn parse_mutation_report(
    report: Option<&Value>,
    facts: Option<&Value>,
    scope_contract_refs: &[Value],
) -> Result<Value> {
    let entities_raw = get(facts.unwrap_or(&NULL), "entities");
    let entities = iterable_values(entities_raw.filter(|v| js_truthy(v)), "facts.entities")?;
    let mut entities_by_id: HashMap<String, &Value> = HashMap::new();
    for entity in &entities {
        if let Some(id) = get(entity, "observedId") {
            entities_by_id.insert(value_key(id), entity);
        }
    }
    let allowed: HashSet<String> = scope_contract_refs.iter().map(value_key).collect();
    let contracts = iterable_values(
        get(facts.unwrap_or(&NULL), "contracts").filter(|v| js_truthy(v)),
        "facts.contracts",
    )?;

    let mut observations: Vec<Value> = Vec::new();
    let mut warnings: Vec<Value> = Vec::new();
    let empty_files = json!({});
    let files = match get(report.unwrap_or(&NULL), "files") {
        Some(v) if v.is_object() || v.as_array().is_some() => v,
        _ => &empty_files,
    };
    for (raw_path, file) in object_entries(Some(files)) {
        let path = match normalize_repo_path(&raw_path) {
            Ok(p) => p,
            Err(e) => {
                warnings.push(json!(format!("{e:#}")));
                continue;
            }
        };
        for mutant in iterable_values(get(&file, "mutants").filter(|v| js_truthy(v)), "file.mutants")? {
            let (line, end_line) = location_of(&mutant);
            let contract = contract_for_mutant(&path, line, &contracts, &entities_by_id);
            if !allowed.is_empty() {
                let contract_id = contract.as_ref().and_then(|c| get(c, "id"));
                let Some(id) = contract_id else { continue };
                if !allowed.contains(&value_key(id)) {
                    continue;
                }
            }
            let status_raw = get(&mutant, "status");
            let mutation_status = match status_raw.and_then(Value::as_str) {
                Some(s) => MUTANT_STATUS
                    .iter()
                    .find(|(k, _)| *k == s)
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_else(|| s.to_lowercase()),
                None => match status_raw.filter(|v| js_truthy(v)) {
                    Some(raw) => js_to_string(raw).to_lowercase(),
                    None => "unknown".to_string(),
                },
            };
            let covered_status = if mutation_status == "killed" {
                "covered"
            } else if mutation_status == "survived" || mutation_status == "no_coverage" {
                "uncovered"
            } else {
                "partial"
            };
            // killedBy: Set по raw-значениям (объекты не дедуплицируются), потом
            // map(String) и побайтовый Array.sort (UTF-16 code units == UTF-8 bytes)
            let killed_raw = get(&mutant, "killedByTests")
                .filter(|v| js_truthy(v))
                .or_else(|| get(&mutant, "killedBy").filter(|v| js_truthy(v)));
            let killed_unique = unique_values(&iterable_values(killed_raw, "mutant.killedByTests")?);
            let mut killed_by: Vec<String> = killed_unique.iter().map(js_to_string).collect();
            killed_by.sort();

            let contract_id = contract.as_ref().and_then(|c| get(c, "id"));
            let subject_ref = match contract_id {
                Some(id) if js_truthy(id) => id.clone(),
                _ => json!(format!("file:{path}")),
            };
            let mutator = get(&mutant, "mutatorName")
                .filter(|v| js_truthy(v))
                .or_else(|| get(&mutant, "type").filter(|v| js_truthy(v)));
            let line_part = line.map(f64_js_string).unwrap_or_default();
            let id = stable_id(
                "mutant",
                &[
                    path.clone(),
                    stable_part(get(&mutant, "id")),
                    stable_part(get(&mutant, "mutatorName")),
                    line_part,
                ],
            );
            let mut entry = Map::new();
            entry.insert("id".into(), json!(id));
            entry.insert("level".into(), json!("contract_edge_coverage"));
            entry.insert("subjectRef".into(), subject_ref);
            entry.insert(
                "contractRef".into(),
                match contract_id {
                    Some(id) if js_truthy(id) => id.clone(),
                    _ => Value::Null,
                },
            );
            entry.insert("path".into(), json!(path));
            entry.insert("line".into(), line.map(num_json).unwrap_or(Value::Null));
            entry.insert("endLine".into(), end_line.map(num_json).unwrap_or(Value::Null));
            entry.insert(
                "mutator".into(),
                json!(match mutator {
                    Some(v) => js_to_string(v),
                    None => "unknown".to_string(),
                }),
            );
            entry.insert(
                "replacement".into(),
                match get(&mutant, "replacement") {
                    None | Some(Value::Null) => Value::Null,
                    Some(r) => json!(js_to_string(r)),
                },
            );
            entry.insert("status".into(), json!(covered_status));
            entry.insert("mutationStatus".into(), json!(mutation_status));
            entry.insert("killedBy".into(), json!(killed_by));
            entry.insert(
                "confidence".into(),
                json!(if contract.is_some() { "exact" } else { "heuristic" }),
            );
            entry.insert(
                "evidence".into(),
                json!([format!("mutation:{path}:{}", js_to_string(get(&mutant, "id").unwrap_or(&Value::Null)))]),
            );
            observations.push(Value::Object(entry));
        }
    }
    observations.sort_by(|a, b| {
        locale_compare(
            get(a, "id").map(js_to_string).unwrap_or_default().as_str(),
            get(b, "id").map(js_to_string).unwrap_or_default().as_str(),
        )
    });
    let count = |status: &str| {
        observations
            .iter()
            .filter(|o| get(o, "mutationStatus").and_then(Value::as_str) == Some(status))
            .count()
    };
    Ok(json!({
        "observations": observations,
        "warnings": warnings,
        "summary": {
            "total": observations.len(),
            "killed": count("killed"),
            "survived": count("survived"),
            "noCoverage": count("no_coverage"),
            "timeout": count("timeout"),
        },
    }))
}

/// Порт criticalContractRefs: must-обязательства + highFanIn-хотспоты + god-ноды.
pub fn critical_contract_refs(
    facts: Option<&Value>,
    dependency_risk: Option<&Value>,
    obligations: Option<&Value>,
) -> Result<Value> {
    let mut must: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push_unique = |v: Value, must: &mut Vec<Value>, seen: &mut HashSet<String>| {
        if seen.insert(value_key(&v)) {
            must.push(v);
        }
    };

    for obligation in iterable_values(obligations.filter(|v| js_truthy(v)), "obligations")? {
        let confirmed = strict_eq(get(&obligation, "status"), Some(&json!("confirmed")));
        let must_priority = strict_eq(get(&obligation, "priority"), Some(&json!("must")));
        if !(confirmed && must_priority) {
            continue;
        }
        // flatMap(contractRefs || []): массив — элементы, прочее truthy — один элемент
        let refs = match get(&obligation, "contractRefs") {
            Some(Value::Array(a)) => a.clone(),
            Some(other) if js_truthy(other) => vec![other.clone()],
            _ => Vec::new(),
        };
        for r in refs {
            push_unique(r, &mut must, &mut seen);
        }
    }
    for hotspot in iterable_values(
        get(dependency_risk.unwrap_or(&Value::Null), "highFanIn").filter(|v| js_truthy(v)),
        "dependencyRisk.highFanIn",
    )? {
        // hotspot == null — в JS `hotspot.contractRef` бросает TypeError
        if matches!(hotspot, Value::Null) {
            bail!("Cannot read properties of null (reading 'contractRef')");
        }
        let value = get(&hotspot, "contractRef")
            .filter(|v| js_truthy(v))
            .or_else(|| get(&hotspot, "entityRef").filter(|v| js_truthy(v)));
        push_unique(value.cloned().unwrap_or(Value::Null), &mut must, &mut seen);
    }
    let entities = iterable_values(
        get(facts.unwrap_or(&Value::Null), "entities").filter(|v| js_truthy(v)),
        "facts.entities",
    )?;
    let contracts = iterable_values(
        get(facts.unwrap_or(&Value::Null), "contracts").filter(|v| js_truthy(v)),
        "facts.contracts",
    )?;
    for entity in &entities {
        if !js_truthy(get(entity, "isGodNode").unwrap_or(&Value::Null)) {
            continue;
        }
        for contract in &contracts {
            if strict_eq(get(contract, "entityRef"), get(entity, "observedId")) {
                push_unique(
                    get(contract, "id").cloned().unwrap_or(Value::Null),
                    &mut must,
                    &mut seen,
                );
            }
        }
    }
    // [...must].filter(Boolean).sort() — дефолтный Array.sort по String(x)
    let mut refs: Vec<Value> = must.into_iter().filter(js_truthy).collect();
    refs.sort_by(|a, b| js_to_string(a).cmp(&js_to_string(b)));
    Ok(json!(refs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Value {
        json!({
            "entities": [
                { "observedId": "e1", "path": "src/a.ts", "line": 10, "endLine": 20 },
                { "observedId": "e2", "path": "src/a.ts", "line": 30, "endLine": 40 },
            ],
            "contracts": [{ "id": "c1", "entityRef": "e1" }, { "id": "c2", "entityRef": "e2" }],
        })
    }

    fn report(mutant: Value) -> Value {
        json!({ "files": { "src/a.ts": { "mutants": [mutant] } } })
    }

    #[test]
    fn binds_mutant_to_enclosing_contract() {
        let r = report(json!({
            "id": "m1", "mutatorName": "BooleanLiteral", "status": "Survived",
            "location": { "start": { "line": 35 } },
        }));
        let out = parse_mutation_report(Some(&r), Some(&facts()), &[]).unwrap();
        let obs = &out["observations"][0];
        assert_eq!(obs["contractRef"], json!("c2"));
        assert_eq!(obs["confidence"], json!("exact"));
        assert_eq!(obs["mutationStatus"], json!("survived"));
        assert_eq!(obs["status"], json!("uncovered"));
    }

    #[test]
    fn blind_mutant_binds_only_single_contract() {
        let r = report(json!({ "id": "m1", "mutatorName": "BooleanLiteral", "status": "Survived" }));
        let out = parse_mutation_report(Some(&r), Some(&facts()), &[]).unwrap();
        let obs = &out["observations"][0];
        assert_eq!(obs["line"], Value::Null);
        assert_eq!(obs["contractRef"], Value::Null);
        assert_eq!(obs["subjectRef"], json!("file:src/a.ts"));
        assert_eq!(obs["confidence"], json!("heuristic"));

        let single = json!({
            "entities": [{ "observedId": "e1", "path": "src/a.ts", "line": 10, "endLine": 20 }],
            "contracts": [{ "id": "c1", "entityRef": "e1" }],
        });
        let out = parse_mutation_report(Some(&r), Some(&single), &[]).unwrap();
        assert_eq!(out["observations"][0]["contractRef"], json!("c1"));
    }

    #[test]
    fn applies_scope_contract_refs() {
        let r = report(json!({
            "id": "m1", "mutatorName": "BooleanLiteral", "status": "Killed",
            "location": { "start": { "line": 35 } },
        }));
        let out = parse_mutation_report(Some(&r), Some(&facts()), &[json!("c1")]).unwrap();
        assert_eq!(out["summary"]["total"], json!(0));
        let out = parse_mutation_report(Some(&r), Some(&facts()), &[json!("c2")]).unwrap();
        assert_eq!(out["summary"]["total"], json!(1));
    }

    #[test]
    fn collects_critical_refs_from_all_sources() {
        let refs = critical_contract_refs(
            Some(&json!({
                "entities": [{ "observedId": "g1", "isGodNode": true }],
                "contracts": [{ "id": "god-c", "entityRef": "g1" }],
            })),
            Some(&json!({ "highFanIn": [{ "contractRef": "hot" }, { "entityRef": "hot-e" }] })),
            Some(&json!([
                { "status": "confirmed", "priority": "must", "contractRefs": ["must-a", "must-b"] },
                { "status": "confirmed", "priority": "should", "contractRefs": ["skip"] },
                { "status": "draft", "priority": "must", "contractRefs": ["skip"] },
            ])),
        )
        .unwrap();
        assert_eq!(refs, json!(["god-c", "hot", "hot-e", "must-a", "must-b"]));
    }

    #[test]
    fn number_semantics_of_location() {
        // Number('0') — truthy как строка, но 0 || null → null
        let r = report(json!({
            "id": "m", "location": { "start": { "line": "0" } },
        }));
        let out = parse_mutation_report(Some(&r), None, &[]).unwrap();
        assert_eq!(out["observations"][0]["line"], Value::Null);
        // строка ' 5 ' → 5
        let r = report(json!({
            "id": "m", "line": " 5 ", "status": "UnknownStatus",
        }));
        let out = parse_mutation_report(Some(&r), None, &[]).unwrap();
        assert_eq!(out["observations"][0]["line"], json!(5));
        assert_eq!(out["observations"][0]["mutationStatus"], json!("unknownstatus"));
    }
}
