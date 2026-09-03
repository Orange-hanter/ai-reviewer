//! Порт lib/structure-assessments.mjs: детерминистический движок оценок
//! structure-пайплайна (dependency risk, data quality, contract scenarios,
//! hints, сборка итогового structure_assessment-пейлоада).
//!
//! Паритет с JS сохранён, включая:
//! - ключи JS Map (entityRef/observedId/contract.id): скаляры по значению
//!   (числа через f64, как SameValueZero), объекты по идентичности;
//! - сортировки: localeCompare (ICU root, аппроксимация в structure_scip)
//!   там, где в JS localeCompare, и дефолтный Array.sort по String(x) —
//!   UTF-16 code units (js_default_sort) там, где `.sort()` без компаратора;
//! - дедуп Set'ами по raw-значениям (объекты не дедуплируются), потом сорт;
//! - `||`/truthiness-семантику JS (execution?.status || 'unavailable',
//!   lcov?.hash || sha256(lcov) || sha256('none'));
//! - итерируемость: `xs || []` бросает TypeError на truthy не-итерируемом;
//! - canonicalJson/sha256 из structure_evidence (побайтовый паритет с JS —
//!   от него зависят coverageHash/thresholdsHash/fingerprint/receiptRef).

use std::collections::HashMap;

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_string, js_truthy};
use crate::structure_evidence::{
    get, iterable_values, js_default_sort, opt_number, sha256_str, sha256_value, stable_id_op,
    unique_values, value_key,
};
use crate::structure_scip::locale_compare;

pub const STRUCTURE_ASSESSMENT_SCHEMA: &str = "code-structure/structure-assessment.v1";

/// Значение-заглушка для мест, где JS читал бы undefined из отсутствующего ключа.
const NULL: Value = Value::Null;

fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(js_truthy).unwrap_or(false)
}

/// num_json из соседних модулей: целые — через i64 (5.0 → 5), не-конечные → null.
fn num_json(n: f64) -> Value {
    if n.is_nan() || n.is_infinite() {
        Value::Null
    } else if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// `${v}` в JS-шаблоне: null → "null", undefined → "undefined", строка как есть,
/// скаляры через String, объекты — "[object Object]" (массивы — serde-форма,
/// та же аппроксимация, что в structure_scip::js_template).
fn js_template(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// ratio() из lib/structure-assessments.mjs: при нуле знаменателя — null.
fn ratio(part: f64, total: f64) -> Option<f64> {
    if total > 0.0 {
        Some(part / total)
    } else {
        None
    }
}

/// JS `.map(String)` поверх значений.
fn map_js_string(values: &[Value]) -> Vec<String> {
    values.iter().map(js_to_string).collect()
}

/// temporalAlignment из lib/structure-protocol.mjs (дублируется здесь,
/// чтобы Rust-модуль был автономен; JS-оригинал остаётся в structure-protocol.mjs).
pub fn temporal_alignment(
    facts_head_sha: Option<&Value>,
    test_head_sha: Option<&Value>,
    obligations_head_sha: Option<&Value>,
) -> &'static str {
    let facts = js_string_or_empty(facts_head_sha);
    let test = js_string_or_empty(test_head_sha);
    let docs = js_string_or_empty(obligations_head_sha);
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

/// JS `String(x || '')`.
fn js_string_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    }
}

/// assessmentFingerprint из lib/structure-protocol.mjs.
pub fn assessment_fingerprint(
    facts: Option<&Value>,
    obligations_revision: Option<&Value>,
    coverage_hash: Option<&Value>,
    thresholds_hash: Option<&Value>,
) -> String {
    let join = |v: Option<&Value>| js_string_or_empty(v);
    let parts = [
        "structure_assessment".to_string(),
        join(facts),
        match obligations_revision {
            None | Some(Value::Null) => "none".to_string(), // `|| 'none'`
            Some(v) => js_to_string(v),
        },
        join(coverage_hash),
        join(thresholds_hash),
    ];
    sha256_str(&parts.join("\0"))
}

/// level === 'function_execution' (строгое равенство строк).
fn is_function_execution(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if s == "function_execution")
}

/// subjectRef → observation для level='function_execution' (JS Map: последний
/// выигрывает, ключи скалярные по значению).
fn execution_by_subject(coverage_observations: Option<&Value>) -> Result<HashMap<String, Value>> {
    let mut map = HashMap::new();
    for item in iterable_values(coverage_observations, "coverageObservations")? {
        if is_function_execution(get(&item, "level")) {
            if let Some(subject) = get(&item, "subjectRef") {
                map.insert(value_key(subject), item);
            }
        }
    }
    Ok(map)
}

/// Порт assessDependencyRisk: highFanIn-хотспоты, crossBoundary-рёбра, god-ноды.
pub fn assess_dependency_risk(
    facts: Option<&Value>,
    coverage_observations: Option<&Value>,
    options: Option<&Value>,
) -> Result<Value> {
    let high_fan_in = opt_number(options.and_then(|o| get(o, "highFanIn")), 5.0);
    let dependencies = iterable_values(
        facts.and_then(|f| get(f, "dependencies")).filter(|v| js_truthy(v)),
        "facts.dependencies",
    )?;
    let entities = iterable_values(
        facts.and_then(|f| get(f, "entities")).filter(|v| js_truthy(v)),
        "facts.entities",
    )?;
    let contracts = iterable_values(
        facts.and_then(|f| get(f, "contracts")).filter(|v| js_truthy(v)),
        "facts.contracts",
    )?;

    // incoming: edge.to → [edges] (ключи Map по значению)
    let mut incoming: HashMap<String, Vec<Value>> = HashMap::new();
    for edge in &dependencies {
        if let Some(to) = get(edge, "to") {
            incoming.entry(value_key(to)).or_default().push(edge.clone());
        }
    }
    let execution_by_subject = execution_by_subject(coverage_observations)?;
    let mut contracts_by_entity: HashMap<String, Vec<Value>> = HashMap::new();
    for contract in &contracts {
        if let Some(entity_ref) = get(contract, "entityRef") {
            contracts_by_entity
                .entry(value_key(entity_ref))
                .or_default()
                .push(contract.clone());
        }
    }

    let mut hotspots: Vec<Value> = Vec::new();
    for entity in &entities {
        let observed_id = get(entity, "observedId");
        let key = observed_id.map(value_key).unwrap_or_default();
        let edges = incoming.get(&key).cloned().unwrap_or_default();
        if (edges.len() as f64) >= high_fan_in {
            let mut contract_refs: Vec<Value> = contracts_by_entity
                .get(&key)
                .map(|list| list.iter().map(|c| get(c, "id").cloned().unwrap_or(Value::Null)).collect())
                .unwrap_or_default();
            js_default_sort(&mut contract_refs);
            let execution = observed_id
                .and_then(|id| execution_by_subject.get(&value_key(id)));
            let confidence = if edges
                .iter()
                .all(|e| matches!(get(e, "confidence"), Some(Value::String(s)) if s == "exact" || s == "extracted"))
            {
                "extracted"
            } else {
                "inferred"
            };
            hotspots.push(json!({
                "entityRef": observed_id.cloned().unwrap_or(Value::Null),
                "contractRefs": contract_refs,
                "fanIn": edges.len(),
                "executionStatus": get(execution.unwrap_or(&NULL), "status")
                    .filter(|v| js_truthy(v))
                    .cloned()
                    .unwrap_or_else(|| json!("unavailable")),
                "crossBoundaryCallers": edges.iter().filter(|e| truthy_opt(get(e, "crossBoundary"))).count(),
                "confidence": confidence,
            }));
        }
    }
    // b.fanIn - a.fanIn || a.entityRef.localeCompare(b.entityRef)
    hotspots.sort_by(|a, b| {
        let fa = get(a, "fanIn").and_then(Value::as_f64).unwrap_or(0.0);
        let fb = get(b, "fanIn").and_then(Value::as_f64).unwrap_or(0.0);
        fb.partial_cmp(&fa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                locale_compare(
                    &js_template(get(a, "entityRef")),
                    &js_template(get(b, "entityRef")),
                )
            })
    });
    let god_nodes: Vec<Value> = entities
        .iter()
        .filter(|e| truthy_opt(get(e, "isGodNode")))
        .map(|e| get(e, "observedId").cloned().unwrap_or(Value::Null))
        .collect();
    let mut god_nodes = god_nodes;
    js_default_sort(&mut god_nodes);
    Ok(json!({
        "highFanIn": hotspots,
        "crossBoundaryEdges": dependencies.iter().filter(|e| truthy_opt(get(e, "crossBoundary"))).count(),
        "godNodes": god_nodes,
    }))
}

/// Порт assessDataQuality.
pub fn assess_data_quality(
    facts: Option<&Value>,
    coverage_observations: Option<&Value>,
    options: Option<&Value>,
) -> Result<Value> {
    let graph_status = get(options.unwrap_or(&NULL), "graphStatus")
        .filter(|v| js_truthy(v))
        .cloned()
        .unwrap_or_else(|| json!("unknown"));
    let test_inventory_complete = truthy_opt(get(options.unwrap_or(&NULL), "testInventoryComplete"));
    let truncated = truthy_opt(get(options.unwrap_or(&NULL), "truncated"));
    let missing_threshold = opt_number(
        get(options.unwrap_or(&NULL), "missingThreshold"),
        0.1,
    );

    let functions: Vec<Value> = iterable_values(coverage_observations, "coverageObservations")?
        .into_iter()
        .filter(|item| is_function_execution(get(item, "level")))
        .collect();
    let unresolved = functions
        .iter()
        .filter(|item| {
            matches!(get(item, "confidence"), Some(Value::String(s)) if s == "unresolved" || s == "ambiguous")
        })
        .count();
    let join_rate = ratio(unresolved as f64, functions.len() as f64);
    let missing_allowed = graph_status == json!("fresh")
        && !truncated
        && test_inventory_complete
        && join_rate.is_some_and(|r| r <= missing_threshold);
    let entity_count = facts
        .and_then(|f| get(f, "entities"))
        .and_then(|e| match e {
            Value::Array(a) => Some(a.len() as f64),
            Value::String(s) => Some(s.chars().count() as f64),
            _ => None,
        })
        .filter(|n| *n != 0.0)
        .unwrap_or(0.0) as i64;
    let edge_count = facts
        .and_then(|f| get(f, "dependencies"))
        .and_then(|e| match e {
            Value::Array(a) => Some(a.len() as f64),
            Value::String(s) => Some(s.chars().count() as f64),
            _ => None,
        })
        .filter(|n| *n != 0.0)
        .unwrap_or(0.0) as i64;
    Ok(json!({
        "graphStatus": graph_status,
        "truncated": truncated,
        "testInventoryComplete": test_inventory_complete,
        "functionObservations": functions.len(),
        "unresolvedFunctionJoins": unresolved,
        "unresolvedJoinRate": num_json(join_rate.unwrap_or(0.0)),
        "missingAllowed": missing_allowed,
        "entityCount": entity_count,
        "edgeCount": edge_count,
    }))
}

fn scenario_assertion_kinds(kind: Option<&Value>) -> &'static [&'static str] {
    match kind.and_then(Value::as_str) {
        Some("error_path") => &["throws", "rejects", "http_status", "property"],
        Some("boundary_value") => &["property", "equality"],
        Some("invariant") => &["property"],
        Some("integration") => &["http_status", "equality"],
        _ => &["equality", "http_status", "property"],
    }
}

/// scenarioId: stableId('scenario', obligation.id, obligation.kind, contractRefs.join(',')).
fn scenario_id(obligation: &Value, contract_refs: &[Value]) -> String {
    let joined = map_js_string(contract_refs).join(",");
    stable_id_op(
        "scenario",
        &[
            get(obligation, "id").cloned().unwrap_or(Value::Null),
            get(obligation, "kind").cloned().unwrap_or(Value::Null),
            json!(joined),
        ],
    )
}

/// Строгое `===` со строковым литералом.
fn strict_str_eq(v: Option<&Value>, s: &str) -> bool {
    matches!(v, Some(Value::String(x)) if x == s)
}

/// Порт assessContractScenarios.
pub fn assess_contract_scenarios(
    facts: Option<&Value>,
    obligations: Option<&Value>,
    coverage_observations: Option<&Value>,
    property_results: Option<&Value>,
    alignment: Option<&Value>,
    data_quality: Option<&Value>,
    evidence_ceiling: Option<&Value>,
) -> Result<Value> {
    let mut contract_by_id: HashMap<String, Value> = HashMap::new();
    for contract in iterable_values(
        facts.and_then(|f| get(f, "contracts")).filter(|v| js_truthy(v)),
        "facts.contracts",
    )? {
        if let Some(id) = get(&contract, "id") {
            contract_by_id.insert(value_key(id), contract);
        }
    }
    let execution_by_entity = execution_by_subject(coverage_observations)?;
    let tests = iterable_values(
        facts.and_then(|f| get(f, "testCases")).filter(|v| js_truthy(v)),
        "facts.testCases",
    )?;
    let mut properties_by_obligation: HashMap<String, Value> = HashMap::new();
    for result in iterable_values(property_results, "propertyResults")? {
        if let Some(obligation_ref) = get(&result, "obligationRef") {
            properties_by_obligation.insert(value_key(obligation_ref), result);
        }
    }
    let alignment_str = match alignment.filter(|v| js_truthy(v)) {
        Some(v) => js_to_string(v),
        None => "unknown".to_string(), // дефолт-параметр JS
    };
    let evidence_ceiling_str = match evidence_ceiling.filter(|v| js_truthy(v)) {
        Some(v) => js_to_string(v),
        None => "static_inventory".to_string(), // дефолт-параметр JS
    };

    let mut out: Vec<Value> = Vec::new();
    for obligation in iterable_values(obligations, "obligations")? {
        // [...new Set(contractRefs.filter(id => contractById.has(id)))].sort()
        let mut contract_refs: Vec<Value> = unique_values(
            &iterable_values(
                get(&obligation, "contractRefs").filter(|v| js_truthy(v)),
                "obligation.contractRefs",
            )?
            .into_iter()
            .filter(|id| contract_by_id.contains_key(&value_key(id)))
            .collect::<Vec<_>>(),
        );
        js_default_sort(&mut contract_refs);
        let resolution = if contract_refs.len() == 1 {
            "unique"
        } else if contract_refs.len() > 1 {
            "ambiguous"
        } else {
            "unresolved"
        };
        let obligation_id = get(&obligation, "id").cloned().unwrap_or(Value::Null);
        let obligation_content_hash = get(&obligation, "contentHash").cloned().unwrap_or(Value::Null);
        let obligation_kind = get(&obligation, "kind").cloned().unwrap_or(Value::Null);
        let edge_case_kind = matches!(
            obligation_kind.as_str(),
            Some("error_path" | "boundary_value" | "invariant")
        );
        let mut scenario = Map::new();
        scenario.insert("id".into(), json!(scenario_id(&obligation, &contract_refs)));
        scenario.insert("obligationRef".into(), obligation_id.clone());
        scenario.insert("contractRefs".into(), json!(contract_refs));
        scenario.insert("testCaseRefs".into(), json!([]));
        scenario.insert("kind".into(), obligation_kind.clone());
        scenario.insert("status".into(), json!("unverifiable"));
        scenario.insert("statusReason".into(), json!("insufficient evidence"));
        scenario.insert("contractResolution".into(), json!(resolution));
        scenario.insert("confidence".into(), json!("unresolved"));
        scenario.insert(
            "evidence".into(),
            json!([format!(
                "obligation:{}:{}",
                js_template(Some(&obligation_id)),
                js_template(Some(&obligation_content_hash))
            )]),
        );
        scenario.insert("missingEvidence".into(), json!([]));

        let push = |out: &mut Vec<Value>, scenario: Map<String, Value>| {
            out.push(Value::Object(scenario));
        };
        if !strict_str_eq(get(&obligation, "status"), "confirmed") {
            scenario.insert("statusReason".into(), json!("obligation is draft"));
            scenario
                .get_mut("missingEvidence")
                .and_then(Value::as_array_mut)
                .expect("массив")
                .push(json!("confirmed_obligation"));
            push(&mut out, scenario);
            continue;
        }
        if alignment_str != "aligned" {
            scenario.insert(
                "statusReason".into(),
                json!(format!("temporal alignment is {alignment_str}")),
            );
            scenario
                .get_mut("missingEvidence")
                .and_then(Value::as_array_mut)
                .expect("массив")
                .push(json!("temporal_alignment"));
            push(&mut out, scenario);
            continue;
        }
        if resolution != "unique" {
            scenario.insert(
                "statusReason".into(),
                json!(format!("contract resolution is {resolution}")),
            );
            scenario
                .get_mut("missingEvidence")
                .and_then(Value::as_array_mut)
                .expect("массив")
                .push(json!(if resolution == "ambiguous" {
                    "unique_contract"
                } else {
                    "contract_link"
                }));
            push(&mut out, scenario);
            continue;
        }

        let contract = contract_by_id
            .get(&value_key(&contract_refs[0]))
            .cloned()
            .unwrap_or(Value::Null);
        let contract_id = get(&contract, "id").cloned().unwrap_or(Value::Null);
        let matching_tests: Vec<Value> = tests
            .iter()
            .filter(|test_case| {
                iterable_values(
                    get(test_case, "targetContractRefs").filter(|v| js_truthy(v)),
                    "testCase.targetContractRefs",
                )
                .map(|refs| refs.iter().any(|r| value_key(r) == value_key(&contract_id)))
                .unwrap_or(false)
            })
            .cloned()
            .collect();
        let execution = get(&contract, "entityRef")
            .and_then(|entity_ref| execution_by_entity.get(&value_key(entity_ref)));
        let accepted = scenario_assertion_kinds(get(&obligation, "kind"));
        let assertion_tests: Vec<Value> = matching_tests
            .iter()
            .filter(|test_case| {
                iterable_values(
                    get(test_case, "assertions").filter(|v| js_truthy(v)),
                    "testCase.assertions",
                )
                .map(|assertions| {
                    assertions.iter().any(|a| {
                        matches!(get(a, "kind").and_then(Value::as_str), Some(k) if accepted.contains(&k))
                    })
                })
                .unwrap_or(false)
            })
            .cloned()
            .collect();
        let property = properties_by_obligation
            .get(&value_key(&obligation_id))
            .cloned();

        let mut test_case_refs: Vec<Value> = matching_tests
            .iter()
            .map(|t| get(t, "id").cloned().unwrap_or(Value::Null))
            .collect();
        js_default_sort(&mut test_case_refs);
        scenario.insert("testCaseRefs".into(), json!(test_case_refs));

        // накапливаем evidence локально — вставка в scenario в конце цикла
        let mut evidence_vec: Vec<Value> = scenario
            .get("evidence")
            .and_then(Value::as_array)
            .map(|a| a.clone())
            .unwrap_or_default();
        if !matching_tests.is_empty() {
            for test_case in &matching_tests {
                evidence_vec.push(json!(format!("test:{}", js_template(get(test_case, "id")))));
            }
        }
        if let Some(execution) = execution {
            // spread строгий: undefined/не-итерируемое — TypeError, как в JS
            match execution.as_object().and_then(|o| o.get("evidence")) {
                Some(Value::Array(items)) => evidence_vec.extend(items.iter().cloned()),
                _ => bail!("execution.evidence is not iterable"),
            }
        }
        if !assertion_tests.is_empty() {
            for test_case in &assertion_tests {
                evidence_vec.push(json!(format!("assertion:{}", js_template(get(test_case, "id")))));
            }
        }
        if strict_str_eq(property.as_ref().and_then(|p| get(p, "status")), "passed") {
            let property_id = js_template(property.as_ref().and_then(|p| get(p, "propertyId")));
            let seed = property
                .as_ref()
                .and_then(|p| get(p, "seed"))
                .filter(|v| js_truthy(v))
                .map(js_to_string)
                .unwrap_or_default();
            evidence_vec.push(json!(format!("property:{property_id}:{seed}")));
        }
        scenario.insert("evidence".into(), json!(evidence_vec));

        let has_test = !matching_tests.is_empty();
        let executed = strict_str_eq(execution.and_then(|e| get(e, "status")), "covered");
        let assertion_bound = !assertion_tests.is_empty();
        let property_bound = strict_str_eq(property.as_ref().and_then(|p| get(p, "status")), "passed")
            && strict_str_eq(property.as_ref().and_then(|p| get(p, "confidence")), "exact");
        let capped_by_aggregate = evidence_ceiling_str == "aggregate_lcov" && edge_case_kind;
        let covered = has_test
            && executed
            && if strict_str_eq(get(&obligation, "kind"), "happy_path") {
                assertion_bound || property_bound
            } else {
                assertion_bound && property_bound
            }
            && !capped_by_aggregate;
        let mut missing_vec: Vec<Value> = Vec::new();
        if covered {
            scenario.insert("status".into(), json!("covered"));
            scenario.insert(
                "statusReason".into(),
                json!("test link, execution and scenario-specific evidence satisfied"),
            );
            scenario.insert(
                "confidence".into(),
                json!(if property_bound { "exact" } else { "extracted" }),
            );
        } else if has_test || executed || assertion_bound || property_bound {
            scenario.insert("status".into(), json!("partial"));
            scenario.insert(
                "statusReason".into(),
                json!(if capped_by_aggregate {
                    "aggregate coverage cannot prove this edge case"
                } else {
                    "some evidence exists but scenario threshold is not met"
                }),
            );
            scenario.insert(
                "confidence".into(),
                json!(if has_test && executed { "inferred" } else { "heuristic" }),
            );
            if !has_test {
                missing_vec.push(json!("linked_test"));
            }
            if !executed {
                missing_vec.push(json!("function_execution"));
            }
            if !assertion_bound {
                missing_vec.push(json!("scenario_assertion"));
            }
            if edge_case_kind && !property_bound {
                missing_vec.push(json!("confirmed_property"));
            }
            if capped_by_aggregate {
                missing_vec.push(json!("per_test_evidence"));
            }
        } else if truthy_opt(get(data_quality.unwrap_or(&NULL), "missingAllowed")) {
            scenario.insert("status".into(), json!("missing"));
            scenario.insert(
                "statusReason".into(),
                json!("fresh complete inventory contains no linked test evidence"),
            );
            scenario.insert("confidence".into(), json!("extracted"));
            missing_vec.push(json!("linked_test"));
            missing_vec.push(json!("function_execution"));
        } else {
            scenario.insert(
                "statusReason".into(),
                json!("inventory quality does not allow a missing conclusion"),
            );
            missing_vec.push(json!("complete_test_inventory"));
        }
        // evidence/missingEvidence: [...new Set()].sort() (строковый дефолтный sort)
        let mut evidence = unique_values(&scenario.get("evidence").and_then(Value::as_array).cloned().unwrap_or_default());
        js_default_sort(&mut evidence);
        scenario.insert("evidence".into(), json!(evidence));
        // [...new Set(missingEvidence)].sort() — сначала дедуп, потом сорт
        let mut missing_evidence = unique_values(&missing_vec);
        js_default_sort(&mut missing_evidence);
        scenario.insert("missingEvidence".into(), json!(missing_evidence));
        push(&mut out, scenario);
    }
    out.sort_by(|a, b| {
        locale_compare(
            &js_template(get(a, "id")),
            &js_template(get(b, "id")),
        )
    });
    Ok(json!(out))
}

/// Порт buildStructureHints.
pub fn build_structure_hints(
    dependency_risk: Option<&Value>,
    data_quality: Option<&Value>,
    scenarios: Option<&Value>,
    mutation_observations: Option<&Value>,
) -> Result<Value> {
    let mut hints: Vec<Value> = Vec::new();
    let mut add = |
        rule_id: &str,
        subject_refs: Vec<Value>,
        priority: &str,
        remediation_target: &str,
        remediation: String,
        evidence: Vec<Value>,
        missing_evidence: Vec<Value>,
        confidence: Value,
    | {
        let mut subjects = unique_values(&subject_refs);
        js_default_sort(&mut subjects);
        let mut evidence = unique_values(&evidence);
        js_default_sort(&mut evidence);
        let mut missing = unique_values(&missing_evidence);
        js_default_sort(&mut missing);
        let mut parts: Vec<Value> = vec![json!(rule_id)];
        parts.extend(subject_refs.iter().cloned());
        let id = stable_id_op("hint", &parts);
        hints.push(json!({
            "id": id,
            "ruleId": rule_id,
            "priority": priority,
            "subjectRefs": subjects,
            "evidence": evidence,
            "missingEvidence": missing,
            "remediationTarget": remediation_target,
            "remediation": remediation.clone(),
            "promptSnippet": remediation,
            "confidence": confidence,
        }));
    };

    // JS: dataQuality.graphStatus — строгое разыменование без ?.;
    // null/undefined бросают TypeError как в V8.
    match data_quality {
        Some(Value::Null) => bail!("Cannot read properties of null (reading 'graphStatus')"),
        None => bail!("Cannot read properties of undefined (reading 'graphStatus')"),
        _ => {}
    }
    if !strict_str_eq(get(data_quality.unwrap_or(&NULL), "graphStatus"), "fresh") {
        let status = js_template(get(data_quality.unwrap_or(&NULL), "graphStatus"));
        add(
            "structure.graph_stale",
            vec![json!("structure:graph")],
            "info",
            "code",
            "Rebuild the Graphify graph at the pinned commit before strict conclusions.".to_string(),
            vec![json!(format!("graph-status:{status}"))],
            vec![json!("fresh_graph")],
            json!("exact"),
        );
    }
    for scenario in iterable_values(scenarios, "scenarios")? {
        if strict_str_eq(get(&scenario, "status"), "covered") {
            continue;
        }
        let priority = if strict_str_eq(get(&scenario, "status"), "missing") {
            "high"
        } else {
            "medium"
        };
        let obligation_ref = js_template(get(&scenario, "obligationRef"));
        let kind = js_template(get(&scenario, "kind"));
        let mut subjects = vec![get(&scenario, "obligationRef").cloned().unwrap_or(Value::Null)];
        if let Some(refs) = get(&scenario, "contractRefs").and_then(Value::as_array) {
            subjects.extend(refs.iter().cloned());
        }
        add(
            "scenario.must_obligation_gap",
            subjects,
            priority,
            "test",
            format!("Add or strengthen a focused {kind} test for {obligation_ref}."),
            get(&scenario, "evidence").and_then(Value::as_array).cloned().unwrap_or_default(),
            get(&scenario, "missingEvidence").and_then(Value::as_array).cloned().unwrap_or_default(),
            get(&scenario, "confidence").cloned().unwrap_or(Value::Null),
        );
    }
    for hotspot in iterable_values(
        get(dependency_risk.unwrap_or(&NULL), "highFanIn").filter(|v| js_truthy(v)),
        "dependencyRisk.highFanIn",
    )? {
        if strict_str_eq(get(&hotspot, "executionStatus"), "covered") {
            continue;
        }
        let entity_ref = get(&hotspot, "entityRef").cloned().unwrap_or(Value::Null);
        let mut subjects = vec![entity_ref.clone()];
        if let Some(refs) = get(&hotspot, "contractRefs").and_then(Value::as_array) {
            subjects.extend(refs.iter().cloned());
        }
        let fan_in = js_template(get(&hotspot, "fanIn"));
        add(
            "coverage.function_unexecuted_high_fan_in",
            subjects,
            "high",
            "test",
            format!("Add a regression test for the high fan-in entity ({fan_in} incoming dependencies)."),
            vec![json!(format!("fan-in:{}", js_template(get(&hotspot, "fanIn"))))],
            vec![json!("function_execution")],
            get(&hotspot, "confidence").cloned().unwrap_or(Value::Null),
        );
    }
    for mutant in iterable_values(mutation_observations, "mutationObservations")? {
        if !matches!(
            get(&mutant, "mutationStatus").and_then(Value::as_str),
            Some("survived" | "no_coverage")
        ) {
            continue;
        }
        let evidence = iterable_values(
            get(&mutant, "evidence").filter(|v| js_truthy(v)),
            "mutant.evidence",
        )?;
        add(
            "scenario.surviving_contract_mutant",
            vec![get(&mutant, "subjectRef").cloned().unwrap_or(Value::Null)],
            "high",
            "test",
            format!(
                "Add an assertion that kills the {} mutant at {}:{}.",
                js_template(get(&mutant, "mutator")),
                js_template(get(&mutant, "path")),
                js_template(get(&mutant, "line"))
            ),
            evidence,
            vec![json!("mutation_killed")],
            get(&mutant, "confidence").cloned().unwrap_or(Value::Null),
        );
    }
    let mut hints_json: Vec<Value> = hints;
    hints_json.sort_by(|a, b| locale_compare(&js_template(get(a, "id")), &js_template(get(b, "id"))));
    Ok(json!(hints_json))
}

/// Порт buildStructureAssessment. `generated_at` обязателен (JS-дефолт
/// new Date().toISOString() применяет обёртка — часы живут в JS).
pub fn build_structure_assessment(
    facts_payload: Option<&Value>,
    obligations_export: Option<&Value>,
    lcov: Option<&Value>,
    coverage_observations: Option<&Value>,
    property_results: Option<&Value>,
    mutation: Option<&Value>,
    receipts: Option<&Value>,
    test_run: Option<&Value>,
    thresholds: Option<&Value>,
    generated_at: Option<&Value>,
) -> Result<Value> {
    // JS: testRun = {} — дефолт деструктурирования только для undefined;
    // явный null бросает TypeError при testRun.headSha.
    if matches!(test_run, Some(Value::Null)) {
        bail!("Cannot read properties of null (reading 'headSha')");
    }
    let facts_payload = facts_payload.filter(|v| js_truthy(v));
    let provenance = facts_payload.and_then(|p| get(p, "provenance"));
    let head_sha = get(provenance.unwrap_or(&NULL), "headSha");
    let head_sha_string = match head_sha {
        Some(Value::String(s)) => s.clone(),
        Some(other) if js_truthy(other) => {
            // JS: headSha.slice(...) на не-строке — TypeError; upstream
            // (validateStructureFacts) гарантирует строку.
            bail!("factsPayload.provenance.headSha must be a string");
        }
        _ => bail!("factsPayload.provenance.headSha must be a string"),
    };
    let obligations_export = obligations_export.filter(|v| js_truthy(v));
    let obligations = iterable_values(
        obligations_export
            .and_then(|o| get(o, "obligations"))
            .filter(|v| js_truthy(v)),
        "obligationsExport.obligations",
    )?;
    let coverage_observations_vec = iterable_values(coverage_observations, "coverageObservations")?;
    let property_results = property_results.filter(|v| js_truthy(v));
    let thresholds = thresholds.filter(|v| js_truthy(v));
    let test_run = test_run.filter(|v| js_truthy(v));
    let mutation = mutation.filter(|v| js_truthy(v));
    let mutation_observations = iterable_values(
        get(mutation.unwrap_or(&NULL), "observations").filter(|v| js_truthy(v)),
        "mutation.observations",
    )?;
    let receipts = iterable_values(receipts, "receipts")?;

    let alignment = temporal_alignment(
        head_sha,
        // temporalAlignment({testHeadSha: testRun.headSha || headSha, ...})
        get(test_run.unwrap_or(&NULL), "headSha")
            .filter(|v| js_truthy(v))
            .or(head_sha),
        obligations_export.and_then(|o| get(o, "headSha")),
    );
    let facts = facts_payload.and_then(|p| get(p, "facts"));
    // testInventoryComplete — строгое `=== true` (не truthiness)
    let test_inventory_complete = matches!(
        get(provenance.unwrap_or(&NULL), "providerCapabilities")
            .and_then(|c| get(c, "testInventory")),
        Some(Value::Bool(true))
    );
    let data_quality = assess_data_quality(
        facts,
        Some(&json!(coverage_observations_vec)),
        Some(&json!({
            "graphStatus": get(provenance.unwrap_or(&NULL), "graphStatus"),
            "testInventoryComplete": test_inventory_complete,
            "truncated": get(provenance.unwrap_or(&NULL), "truncated"),
            "missingThreshold": get(thresholds.unwrap_or(&NULL), "unresolvedJoinRate"),
        })),
    )?;

    let dependency_risk = assess_dependency_risk(
        facts,
        Some(&json!(coverage_observations_vec)),
        Some(thresholds.unwrap_or(&json!({}))),
    )?;
    // evidenceCeiling: lcov ? 'aggregate_lcov' : 'static_inventory'
    let evidence_ceiling = if lcov.map(js_truthy).unwrap_or(false) {
        json!("aggregate_lcov")
    } else {
        json!("static_inventory")
    };
    let scenarios = assess_contract_scenarios(
        facts,
        Some(&json!(obligations)),
        Some(&json!(coverage_observations_vec)),
        property_results,
        Some(&json!(alignment)),
        Some(&data_quality),
        Some(&evidence_ceiling),
    )?;
    let hints = build_structure_hints(
        Some(&dependency_risk),
        Some(&data_quality),
        Some(&scenarios),
        Some(&json!(mutation_observations)),
    )?;

    let obligations_revision = obligations_export
        .and_then(|o| get(o, "revision"))
        .filter(|v| js_truthy(v))
        .cloned()
        .unwrap_or_else(|| json!("none"));
    // coverageHash: lcov.hash || (lcov ? sha256(lcov) : sha256('none'))
    let lcov_truthy = lcov.map(js_truthy).unwrap_or(false);
    let coverage_hash = match lcov.and_then(|l| get(l, "hash")).filter(|v| js_truthy(v)) {
        Some(hash) => hash.clone(),
        None if lcov_truthy => json!(sha256_value(lcov.unwrap())),
        None => json!(sha256_str("none")),
    };
    let thresholds_hash = json!(sha256_value(thresholds.unwrap_or(&json!({}))));
    let facts_fingerprint = get(facts_payload.unwrap_or(&NULL), "fingerprint").cloned().unwrap_or(Value::Null);
    let fingerprint = assessment_fingerprint(
        Some(&facts_fingerprint),
        Some(&obligations_revision),
        Some(&coverage_hash),
        Some(&thresholds_hash),
    );

    // obligationContentHashes: entries [id, contentHash], sort по localeCompare(id)
    let mut entries: Vec<(String, Value)> = obligations
        .iter()
        .map(|o| {
            (
                js_template(get(o, "id")),
                get(o, "contentHash").cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    entries.sort_by(|a, b| locale_compare(&a.0, &b.0));
    let obligation_hashes: Map<String, Value> = entries.into_iter().collect();

    let test_run_id = match get(test_run.unwrap_or(&NULL), "id").filter(|v| js_truthy(v)) {
        Some(id) => json!(js_to_string(id)),
        // headSha.slice(0, 12) — по UTF-16 code units
        None => json!(format!("local-{}", head_sha_string.chars().take(12).collect::<String>())),
    };
    let test_run_head = match get(test_run.unwrap_or(&NULL), "headSha").filter(|v| js_truthy(v)) {
        Some(h) => json!(js_to_string(h)),
        None => json!(head_sha_string),
    };
    let produced_at = match get(test_run.unwrap_or(&NULL), "producedAt").filter(|v| js_truthy(v)) {
        Some(p) => p.clone(),
        None => generated_at.cloned().unwrap_or(Value::Null),
    };

    let mut all_observations = coverage_observations_vec;
    all_observations.extend(mutation_observations);
    all_observations.sort_by(|a, b| locale_compare(&js_template(get(a, "id")), &js_template(get(b, "id"))));

    let mut out = Map::new();
    out.insert("schemaRef".into(), json!(STRUCTURE_ASSESSMENT_SCHEMA));
    out.insert("version".into(), json!(1));
    out.insert("kind".into(), json!("structure_assessment"));
    out.insert(
        "generatedAt".into(),
        generated_at.cloned().unwrap_or(Value::Null),
    );
    out.insert("fingerprint".into(), json!(fingerprint));
    out.insert("factsFingerprint".into(), facts_fingerprint);
    out.insert("obligationsRevision".into(), obligations_revision);
    out.insert(
        "obligationContentHashes".into(),
        Value::Object(obligation_hashes),
    );
    out.insert("coverageHash".into(), coverage_hash);
    out.insert("thresholdsHash".into(), thresholds_hash);
    out.insert(
        "testRun".into(),
        json!({ "id": test_run_id, "headSha": test_run_head, "producedAt": produced_at }),
    );
    out.insert("temporalAlignment".into(), json!(alignment));
    out.insert("coverageObservations".into(), json!(all_observations));
    out.insert(
        "assessments".into(),
        json!({
            "codeCoverage": {
                "totals": lcov
                    .and_then(|l| get(l, "totals"))
                    .filter(|v| js_truthy(v))
                    .cloned()
                    .unwrap_or(Value::Null),
                "files": lcov
                    .and_then(|l| get(l, "files"))
                    .and_then(|f| f.as_array().map(|a| a.len()))
                    .filter(|n| *n > 0)
                    .unwrap_or(0),
            },
            "dependencyRisk": dependency_risk,
            "contractScenarios": scenarios,
            "dataQuality": data_quality,
        }),
    );
    out.insert("hints".into(), hints);
    out.insert("receipts".into(), json!(receipts));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "abcdef1234567890";
    const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn sha(s: &str) -> String {
        sha256_str(s)
    }

    fn facts_payload() -> Value {
        json!({
            "fingerprint": sha("facts"),
            "provenance": {
                "headSha": HEAD,
                "graphStatus": "fresh",
                "truncated": false,
                "providerCapabilities": { "testInventory": true },
            },
            "facts": {
                "entities": [
                    { "observedId": "e-hot", "isGodNode": false },
                    { "observedId": "e-god", "isGodNode": true },
                ],
                "contracts": [
                    { "id": "c-hot", "entityRef": "e-hot" },
                    { "id": "c-god", "entityRef": "e-god" },
                ],
                "dependencies": [
                    { "from": "a", "to": "e-hot", "confidence": "exact", "crossBoundary": true },
                    { "from": "b", "to": "e-hot", "confidence": "exact" },
                    { "from": "c", "to": "e-hot", "confidence": "inferred" },
                    { "from": "d", "to": "e-hot" },
                    { "from": "e", "to": "e-hot", "confidence": "extracted" },
                    { "from": "f", "to": "e-hot" },
                ],
                "testCases": [
                    {
                        "id": "t-1",
                        "targetContractRefs": ["c-hot"],
                        "assertions": [{ "kind": "throws" }],
                    },
                ],
            },
        })
    }

    #[test]
    fn dependency_risk_hotspots_and_god_nodes() {
        let coverage = json!([
            { "level": "function_execution", "subjectRef": "e-hot", "status": "covered", "confidence": "exact", "evidence": ["lcov:x"] },
        ]);
        let out = assess_dependency_risk(
            facts_payload().get("facts"),
            Some(&coverage),
            Some(&json!({ "highFanIn": 5 })),
        )
        .unwrap();
        let hotspots = out["highFanIn"].as_array().unwrap();
        assert_eq!(hotspots.len(), 1);
        assert_eq!(hotspots[0]["fanIn"], json!(6));
        assert_eq!(hotspots[0]["executionStatus"], json!("covered"));
        assert_eq!(hotspots[0]["crossBoundaryCallers"], json!(1));
        // не все confidence exact/extracted → inferred
        assert_eq!(hotspots[0]["confidence"], json!("inferred"));
        assert_eq!(out["crossBoundaryEdges"], json!(1));
        assert_eq!(out["godNodes"], json!(["e-god"]));
    }

    #[test]
    fn data_quality_missing_requires_join_rate() {
        let facts = json!({ "entities": [], "dependencies": [] });
        let none = assess_data_quality(Some(&facts), Some(&json!([])), Some(&json!({
            "graphStatus": "fresh",
            "testInventoryComplete": true,
            "truncated": false,
        })))
        .unwrap();
        assert_eq!(none["missingAllowed"], json!(false));
        assert_eq!(none["unresolvedJoinRate"], json!(0));
        let unresolved = assess_data_quality(
            Some(&facts),
            Some(&json!([
                { "level": "function_execution", "confidence": "unresolved" },
                { "level": "function_execution", "confidence": "exact" },
                { "level": "function_execution", "confidence": "ambiguous" },
            ])),
            Some(&json!({ "graphStatus": "fresh", "testInventoryComplete": true })),
        )
        .unwrap();
        assert_eq!(unresolved["unresolvedFunctionJoins"], json!(2));
        // joinRate 2/3 > 0.1 по умолчанию — missing запрещён
        assert_eq!(unresolved["missingAllowed"], json!(false));
        let lenient = assess_data_quality(
            Some(&facts),
            Some(&json!([
                { "level": "function_execution", "confidence": "unresolved" },
                { "level": "function_execution", "confidence": "exact" },
            ])),
            Some(&json!({
                "graphStatus": "fresh",
                "testInventoryComplete": true,
                "missingThreshold": 0.5,
            })),
        )
        .unwrap();
        assert_eq!(lenient["unresolvedJoinRate"], json!(0.5));
        assert_eq!(lenient["missingAllowed"], json!(true));
    }

    #[test]
    fn temporal_alignment_semantics() {
        assert_eq!(
            temporal_alignment(Some(&json!(HEAD)), Some(&json!(HEAD)), Some(&json!(HEAD))),
            "aligned"
        );
        assert_eq!(
            temporal_alignment(Some(&json!(HEAD)), Some(&json!("other")), None),
            "mismatch"
        );
        assert_eq!(
            temporal_alignment(Some(&json!(HEAD)), Some(&json!(HEAD)), Some(&json!("docs"))),
            "mismatch"
        );
        assert_eq!(temporal_alignment(Some(&json!(HEAD)), None, None), "unknown");
    }

    #[test]
    fn scenario_covered_end_to_end() {
        let obligations = json!([
            {
                "id": "obl:1",
                "kind": "error_path",
                "status": "confirmed",
                "contentHash": sha("MUST reject"),
                "contractRefs": ["c-hot", "missing-contract", "c-hot"],
            },
        ]);
        let coverage = json!([
            { "level": "function_execution", "subjectRef": "e-hot", "status": "covered", "confidence": "exact", "evidence": ["lcov:fn"] },
        ]);
        let scenarios = assess_contract_scenarios(
            facts_payload().get("facts"),
            Some(&obligations),
            Some(&coverage),
            Some(&json!([
                { "obligationRef": "obl:1", "status": "passed", "confidence": "exact", "propertyId": "p-1", "seed": 42 },
            ])),
            Some(&json!("aligned")),
            Some(&json!({ "missingAllowed": false })),
            Some(&json!("static_inventory")),
        )
        .unwrap();
        let scenario = &scenarios.as_array().unwrap()[0];
        assert_eq!(scenario["status"], json!("covered"));
        assert_eq!(scenario["confidence"], json!("exact"));
        assert_eq!(scenario["contractResolution"], json!("unique"));
        assert!(scenario["evidence"].as_array().unwrap().contains(&json!("property:p-1:42")));
        assert!(scenario["evidence"].as_array().unwrap().contains(&json!("assertion:t-1")));
        assert!(scenario["evidence"].as_array().unwrap().contains(&json!("lcov:fn")));
    }

    #[test]
    fn hints_collect_stale_gap_hotspot_mutant() {
        let data_quality = json!({ "graphStatus": "stale" });
        let scenarios = json!([
            {
                "status": "missing",
                "obligationRef": "obl:1",
                "kind": "error_path",
                "contractRefs": ["c1"],
                "evidence": ["obligation:obl:1:x"],
                "missingEvidence": ["linked_test"],
                "confidence": "extracted",
            },
        ]);
        let risk = json!({ "highFanIn": [{ "entityRef": "e1", "fanIn": 7, "executionStatus": "uncovered", "confidence": "extracted" }] });
        let mutants = json!([
            { "mutationStatus": "survived", "subjectRef": "e1", "mutator": "BooleanLiteral", "path": "src/a.ts", "line": 3, "confidence": "exact", "evidence": ["mutation:src/a.ts:m1"] },
            { "mutationStatus": "killed", "subjectRef": "e2" },
        ]);
        let hints = build_structure_hints(Some(&risk), Some(&data_quality), Some(&scenarios), Some(&mutants)).unwrap();
        let mut rule_ids: Vec<&str> = hints
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["ruleId"].as_str().unwrap())
            .collect();
        // порядок hints — по id (stableId-хеш), ruleId сверяем как множество
        rule_ids.sort();
        assert_eq!(rule_ids, vec![
            "coverage.function_unexecuted_high_fan_in",
            "scenario.must_obligation_gap",
            "scenario.surviving_contract_mutant",
            "structure.graph_stale",
        ]);
    }

    #[test]
    fn build_assessment_full_shape() {
        let out = build_structure_assessment(
            Some(&facts_payload()),
            Some(&json!({
                "revision": "docs-1",
                "headSha": HEAD,
                "obligations": [
                    { "id": "obl:b", "contentHash": sha("b"), "status": "draft", "kind": "happy_path" },
                    { "id": "obl:a", "contentHash": sha("a"), "status": "confirmed", "kind": "error_path", "contractRefs": ["c-hot"] },
                ],
            })),
            Some(&json!({
                "hash": sha("lcov"),
                "totals": { "lines": { "found": 10, "hit": 5 } },
                "files": [{ "path": "src/a.ts" }],
            })),
            Some(&json!([])),
            Some(&json!([])),
            Some(&json!({ "observations": [] })),
            Some(&json!([])),
            Some(&json!({ "id": "run-1", "producedAt": "2026-09-02T00:00:00.000Z" })),
            Some(&json!({ "unresolvedJoinRate": 0.2 })),
            Some(&json!("2026-09-03T00:00:00.000Z")),
        )
        .unwrap();
        assert_eq!(out["schemaRef"], json!("code-structure/structure-assessment.v1"));
        assert_eq!(out["temporalAlignment"], json!("aligned"));
        assert_eq!(out["coverageHash"], json!(sha("lcov")));
        assert_eq!(out["testRun"]["id"], json!("run-1"));
        assert_eq!(out["testRun"]["headSha"], json!(HEAD));
        // obligationContentHashes отсортированы по id
        let hashes = out["obligationContentHashes"].as_object().unwrap();
        let keys: Vec<&str> = hashes.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["obl:a", "obl:b"]);
        // fingerprint — sha256 от join с '\0'
        let expected_fp = sha256_str(&[
            "structure_assessment".to_string(),
            sha("facts"),
            "docs-1".to_string(),
            sha("lcov"),
            sha256_value(&json!({ "unresolvedJoinRate": 0.2 })),
        ].join("\0"));
        assert_eq!(out["fingerprint"], json!(expected_fp));
        assert_eq!(out["assessments"]["codeCoverage"]["files"], json!(1));
        assert_eq!(out["assessments"]["dataQuality"]["entityCount"], json!(2));
        assert_eq!(out["assessments"]["dataQuality"]["edgeCount"], json!(6));
    }

    #[test]
    fn coverage_hash_without_lcov_is_sha256_none() {
        let out = build_structure_assessment(
            Some(&facts_payload()),
            None,
            None,
            Some(&json!([])),
            Some(&json!([])),
            None,
            None,
            None,
            None,
            Some(&json!("2026-09-03T00:00:00.000Z")),
        )
        .unwrap();
        assert_eq!(out["coverageHash"], json!(sha("none")));
        assert_eq!(out["obligationsRevision"], json!("none"));
        assert_eq!(out["assessments"]["codeCoverage"]["totals"], Value::Null);
        assert_eq!(out["assessments"]["codeCoverage"]["files"], json!(0));
        // lcov-объект без hash: coverageHash = canonical sha256(lcov)
        let out2 = build_structure_assessment(
            Some(&facts_payload()),
            None,
            Some(&json!({ "files": [] })),
            Some(&json!([])),
            Some(&json!([])),
            None,
            None,
            None,
            None,
            Some(&json!("2026-09-03T00:00:00.000Z")),
        )
        .unwrap();
        assert_eq!(out2["coverageHash"], json!(sha256_value(&json!({ "files": [] }))));
    }
}
