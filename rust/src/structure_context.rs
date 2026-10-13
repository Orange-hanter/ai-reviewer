//! Порт lib/structure-context.mjs (RFC 20, T18): ограниченный structure context,
//! пиннутый к снимку facts.
//!
//! `build_structure_context(facts, assessment, &ContextRequest)` повторяет
//! `buildStructureContext({facts, assessment, headSha, snapshotFingerprint,
//! scopeRefs, taskRef, contractRefs, tokenBudget, dependencyDepth, limits})`:
//! пин-проверки → seeds (scope-матчи, затем entityRef по contractRefs) → BFS по
//! зависимостям (FIFO, дедуп рёбер по `edge.id || from→to:relation`, потолки
//! maxSeeds/maxEdges/maxDepth) → контракты, сценарии, хинты, границы,
//! suggestedFiles → оценка токенов и вторичное усечение с prune висячих ссылок.
//!
//! В отличие от `structure_mcp::build_structure_context`, здесь нет клэмпинга
//! аргументов тулза из inputSchema: это чистая доменная функция, её зовёт
//! structure-контур напрямую, а MCP-сервер — после собственной валидации.
//!
//! Паритет с JS:
//! - Map/Set — SameValueZero через `value_key`; `===` — `strict_eq`;
//! - matchesScope: === по observedId/boundaryId/path и префикс
//!   `path.startsWith(scope.replace(/\/+$/, '') + '/')`;
//! - `estimateTokens = ceil(JSON.stringify(result).length / 4)` в UTF-16 code
//!   units, считается ДО того, как в result появится tokenEstimate; гейт
//!   `Buffer.byteLength(JSON.stringify(result)) > maxBytes` — уже С ним;
//! - дефолтный sort suggestedFiles — по UTF-16 code units;
//! - тексты доменных ошибок посимвольно.
//!
//! Отклонения (за пределами валидированных пейлоадов): отсутствующие ветки
//! facts.facts / assessment.assessments читаются как пустые списки (JS бросил бы
//! TypeError); нестроковый scope не матчит префиксно (в JS — TypeError на
//! `scope.replace`); нечисловые limits → NaN с JS-семантикой срезов и сравнений.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_number, js_to_string, js_truthy};
use crate::structure_evidence::{
    get, iterable_values, js_default_sort, js_json_string, js_number_to_string, unique_values,
    value_key,
};

/// DEFAULT_CONTEXT_LIMITS из lib/structure-context.mjs.
pub const DEFAULT_MAX_SEEDS: f64 = 20.0;
pub const DEFAULT_MAX_EDGES: f64 = 50.0;
pub const DEFAULT_MAX_OBLIGATIONS: f64 = 10.0;
pub const DEFAULT_MAX_GAPS: f64 = 15.0;
pub const DEFAULT_MAX_BYTES: f64 = 32.0 * 1024.0;
pub const DEFAULT_MAX_DEPTH: f64 = 1.0;
pub const DEFAULT_TOKEN_BUDGET: f64 = 8000.0;
pub const DEFAULT_DEPENDENCY_DEPTH: f64 = 1.0;

/// Аргументы buildStructureContext помимо facts/assessment. `Default` даёт
/// JS-дефолты: scopeRefs [], taskRef null, contractRefs [], tokenBudget 8000,
/// dependencyDepth 1, limits {}.
#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub head_sha: Option<Value>,
    pub snapshot_fingerprint: Option<Value>,
    pub scope_refs: Vec<Value>,
    pub task_ref: Value,
    pub contract_refs: Vec<Value>,
    pub token_budget: f64,
    pub dependency_depth: f64,
    pub limits: Map<String, Value>,
}

impl Default for ContextRequest {
    fn default() -> Self {
        Self {
            head_sha: None,
            snapshot_fingerprint: None,
            scope_refs: Vec::new(),
            task_ref: Value::Null,
            contract_refs: Vec::new(),
            token_budget: DEFAULT_TOKEN_BUDGET,
            dependency_depth: DEFAULT_DEPENDENCY_DEPTH,
            limits: Map::new(),
        }
    }
}

impl ContextRequest {
    /// Из JSON-объекта с ключами JS-сигнатуры (headSha, snapshotFingerprint,
    /// scopeRefs, taskRef, contractRefs, tokenBudget, dependencyDepth, limits).
    /// Дефолты — только для отсутствующих ключей, как у деструктуризации JS.
    pub fn from_value(args: &Value) -> Self {
        let mut req = Self::default();
        req.head_sha = get(args, "headSha").cloned();
        req.snapshot_fingerprint = get(args, "snapshotFingerprint").cloned();
        if let Some(Value::Array(a)) = get(args, "scopeRefs") {
            req.scope_refs = a.clone();
        }
        if let Some(v) = get(args, "taskRef") {
            req.task_ref = v.clone();
        }
        if let Some(Value::Array(a)) = get(args, "contractRefs") {
            req.contract_refs = a.clone();
        }
        if let Some(v) = get(args, "tokenBudget") {
            req.token_budget = js_to_number(v).unwrap_or(f64::NAN);
        }
        if let Some(v) = get(args, "dependencyDepth") {
            req.dependency_depth = js_to_number(v).unwrap_or(f64::NAN);
        }
        if let Some(Value::Object(m)) = get(args, "limits") {
            req.limits = m.clone();
        }
        req
    }
}

fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(js_truthy).unwrap_or(false)
}

/// JS `String(v)`: undefined → "undefined".
fn to_string_opt(v: Option<&Value>) -> String {
    v.map(js_to_string).unwrap_or_else(|| "undefined".into())
}

/// JS `===` для значений из JSON: undefined === undefined, числа через f64,
/// объекты/массивы — по ссылке, т.е. разные значения никогда не равны.
fn strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(Value::Object(_) | Value::Array(_)), _) => false,
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn key_of(v: Option<&Value>) -> String {
    value_key(v.unwrap_or(&Value::Null))
}

/// `JSON.stringify` (компактно): числа по ECMA Number::toString, не-конечные → null.
fn js_json_stringify(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(x) if x.is_finite() => js_number_to_string(x),
            _ => "null".into(),
        },
        Value::String(s) => js_json_string(s),
        Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(js_json_stringify).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(o) => {
            let inner: Vec<String> = o
                .iter()
                .map(|(k, val)| format!("{}:{}", js_json_string(k), js_json_stringify(val)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

/// estimateTokens: ceil(JSON.stringify(value).length / 4), длина в UTF-16.
fn estimate_tokens(result: &Map<String, Value>) -> f64 {
    let text = js_json_stringify(&Value::Object(result.clone()));
    (text.encode_utf16().count() as f64 / 4.0).ceil()
}

/// Целое без дробной части, как печатает JSON.stringify(Math.ceil(...)).
fn int_json(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// `xs.slice(0, n)`: NaN/<=0 → пусто, +Infinity/>=len → всё.
fn slice0(xs: &[Value], n: f64) -> Vec<Value> {
    if n.is_nan() || n <= 0.0 {
        return Vec::new();
    }
    if !n.is_finite() {
        return xs.to_vec();
    }
    xs[..(n as usize).min(xs.len())].to_vec()
}

/// `{...DEFAULT_CONTEXT_LIMITS, ...limits}[key]` с JS-приведением к числу.
fn cap_limit(limits: &Map<String, Value>, key: &str, default: f64) -> f64 {
    match limits.get(key) {
        None => default,
        Some(v) => js_to_number(v).unwrap_or(f64::NAN),
    }
}

fn matches_scope(entity: &Value, scope_refs: &[Value]) -> bool {
    if scope_refs.is_empty() {
        return false;
    }
    let observed_id = get(entity, "observedId");
    let boundary_id = get(entity, "boundaryId");
    let path = get(entity, "path");
    scope_refs.iter().any(|scope| {
        strict_eq(observed_id, Some(scope))
            || strict_eq(boundary_id, Some(scope))
            || strict_eq(path, Some(scope))
            || match (path, scope) {
                (Some(Value::String(p)), Value::String(s)) => {
                    p.starts_with(&format!("{}/", s.trim_end_matches('/')))
                }
                _ => false,
            }
    })
}

fn refs_of(v: &Value, key: &str) -> Vec<Value> {
    iterable_values(get(v, key), key).unwrap_or_default()
}

fn any_ref_in(v: &Value, key: &str, sets: &[&HashSet<String>]) -> bool {
    refs_of(v, key)
        .iter()
        .any(|r| sets.iter().any(|set| set.contains(&value_key(r))))
}

/// `result[key] = result[key].filter(pred)` с сохранением позиции ключа.
fn retain_arr(result: &mut Map<String, Value>, key: &str, pred: impl Fn(&Value) -> bool) {
    if let Some(Value::Array(a)) = result.get_mut(key) {
        a.retain(pred);
    }
}

/// `result[key] = result[key].slice(0, n)` с сохранением позиции ключа.
fn slice_arr(result: &mut Map<String, Value>, key: &str, n: f64) {
    if let Some(Value::Array(a)) = result.get_mut(key) {
        *a = slice0(a, n);
    }
}

fn arr<'a>(result: &'a Map<String, Value>, key: &str) -> &'a [Value] {
    match result.get(key) {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

fn id_set(values: &[Value], key: &str) -> HashSet<String> {
    values.iter().map(|v| key_of(get(v, key))).collect()
}

/// pruneDanglingRefs: после вторичного усечения ссылки обязаны вести внутрь
/// payload; suggestedFiles не трогаем — это пути в репозитории.
fn prune_dangling_refs(result: &mut Map<String, Value>) {
    let entity_ids = id_set(arr(result, "entities"), "observedId");
    retain_arr(result, "dependencies", |edge| {
        entity_ids.contains(&key_of(get(edge, "from")))
            && entity_ids.contains(&key_of(get(edge, "to")))
    });
    retain_arr(result, "contracts", |c| entity_ids.contains(&key_of(get(c, "entityRef"))));
    let contract_ids = id_set(arr(result, "contracts"), "id");
    retain_arr(result, "scenarios", |s| any_ref_in(s, "contractRefs", &[&contract_ids]));
    let obligation_ids = id_set(arr(result, "scenarios"), "obligationRef");
    retain_arr(result, "hints", |h| {
        any_ref_in(h, "subjectRefs", &[&entity_ids, &contract_ids, &obligation_ids])
    });
    let boundary_ids: HashSet<String> = arr(result, "entities")
        .iter()
        .filter_map(|e| get(e, "boundaryId"))
        .filter(|v| js_truthy(v))
        .map(value_key)
        .collect();
    retain_arr(result, "boundaries", |b| boundary_ids.contains(&key_of(get(b, "id"))));
}

/// Порт buildStructureContext из lib/structure-context.mjs.
pub fn build_structure_context(
    facts: &Value,
    assessment: &Value,
    req: &ContextRequest,
) -> Result<Value> {
    let head_sha = req.head_sha.as_ref();
    let snapshot_fingerprint = req.snapshot_fingerprint.as_ref();
    if !truthy_opt(head_sha) && !truthy_opt(snapshot_fingerprint) {
        bail!("structure_context requires pinned headSha or snapshotFingerprint");
    }
    let facts_head = get(facts, "provenance").and_then(|p| get(p, "headSha"));
    if truthy_opt(head_sha) && !strict_eq(facts_head, head_sha) {
        bail!(
            "pinned headSha {} does not match facts {}",
            to_string_opt(head_sha),
            to_string_opt(facts_head),
        );
    }
    if truthy_opt(snapshot_fingerprint) && !strict_eq(get(facts, "fingerprint"), snapshot_fingerprint)
    {
        bail!(
            "snapshot fingerprint {} does not match facts",
            to_string_opt(snapshot_fingerprint),
        );
    }
    if !strict_eq(get(assessment, "factsFingerprint"), get(facts, "fingerprint")) {
        bail!("assessment does not reference the supplied facts snapshot");
    }

    let max_seeds = cap_limit(&req.limits, "maxSeeds", DEFAULT_MAX_SEEDS);
    let max_edges = cap_limit(&req.limits, "maxEdges", DEFAULT_MAX_EDGES);
    let max_gaps = cap_limit(&req.limits, "maxGaps", DEFAULT_MAX_GAPS);
    let max_bytes = cap_limit(&req.limits, "maxBytes", DEFAULT_MAX_BYTES);
    // Math.min с NaN даёт NaN; f64::min вернул бы второй аргумент
    let cap_depth = cap_limit(&req.limits, "maxDepth", DEFAULT_MAX_DEPTH);
    let max_depth = if req.dependency_depth.is_nan() || cap_depth.is_nan() {
        f64::NAN
    } else {
        req.dependency_depth.min(cap_depth)
    };

    let inner = get(facts, "facts").cloned().unwrap_or(Value::Null);
    let entities = iterable_values(get(&inner, "entities"), "entities")?;
    let contracts = iterable_values(get(&inner, "contracts"), "contracts")?;
    let dependencies = iterable_values(get(&inner, "dependencies"), "dependencies")?;
    let all_boundaries = iterable_values(get(&inner, "boundaries"), "boundaries")?;
    let test_cases = iterable_values(get(&inner, "testCases"), "testCases")?;
    let assessments = get(assessment, "assessments").cloned().unwrap_or(Value::Null);
    let all_scenarios =
        iterable_values(get(&assessments, "contractScenarios"), "contractScenarios")?;
    let all_hints = iterable_values(get(assessment, "hints"), "hints")?;

    // new Map(...): при дубликате ключа значение последнее, позиция — первая
    let mut entity_by_id: HashMap<String, &Value> = HashMap::new();
    for entity in &entities {
        entity_by_id.insert(key_of(get(entity, "observedId")), entity);
    }
    let mut contract_by_id: HashMap<String, &Value> = HashMap::new();
    for contract in &contracts {
        contract_by_id.insert(key_of(get(contract, "id")), contract);
    }

    let mut seed_order: Vec<String> = Vec::new();
    let mut seed_set: HashSet<String> = HashSet::new();
    let mut add_seed = |k: String| {
        if seed_set.insert(k.clone()) {
            seed_order.push(k);
        }
    };
    for entity in &entities {
        if matches_scope(entity, &req.scope_refs) {
            add_seed(key_of(get(entity, "observedId")));
        }
    }
    for contract_ref in &req.contract_refs {
        if let Some(contract) = contract_by_id.get(&value_key(contract_ref)) {
            add_seed(key_of(get(contract, "entityRef")));
        }
    }
    let seed_count = seed_set.len();

    let seed_values: Vec<Value> = seed_order.iter().map(|k| json!(k)).collect();
    let mut queue: VecDeque<(String, f64)> = slice0(&seed_values, max_seeds)
        .into_iter()
        .map(|v| (v.as_str().unwrap_or_default().to_string(), 0.0))
        .collect();
    let mut selected_order: Vec<String> = queue.iter().map(|(k, _)| k.clone()).collect();
    let mut selected_set: HashSet<String> = selected_order.iter().cloned().collect();
    let mut selected_edges: Vec<Value> = Vec::new();
    let mut selected_edge_ids: HashSet<String> = HashSet::new();
    // while (queue.length && selectedEdges.length < cap.maxEdges)
    while !queue.is_empty() && (selected_edges.len() as f64) < max_edges {
        let (current, depth) = queue.pop_front().unwrap();
        // `depth >= NaN` ложно: при NaN-глубине JS обходит без ограничения
        if depth >= max_depth {
            continue;
        }
        for edge in &dependencies {
            if (selected_edges.len() as f64) >= max_edges {
                break;
            }
            let from = get(edge, "from");
            let to = get(edge, "to");
            let from_hit = key_of(from) == current;
            if !from_hit && key_of(to) != current {
                continue;
            }
            let edge_id = match get(edge, "id") {
                Some(v) if js_truthy(v) => value_key(v),
                _ => value_key(&json!(format!(
                    "{}→{}:{}",
                    to_string_opt(from),
                    to_string_opt(to),
                    to_string_opt(get(edge, "relation")),
                ))),
            };
            if !selected_edge_ids.insert(edge_id) {
                continue;
            }
            selected_edges.push(edge.clone());
            let other = key_of(if from_hit { to } else { from });
            if selected_set.insert(other.clone()) {
                selected_order.push(other.clone());
                queue.push_back((other, depth + 1.0));
            }
        }
    }

    let selected_entities: Vec<Value> = selected_order
        .iter()
        .filter_map(|k| entity_by_id.get(k).map(|e| (*e).clone()))
        .collect();
    let selected_contracts: Vec<Value> = contracts
        .iter()
        .filter(|c| {
            selected_set.contains(&key_of(get(c, "entityRef")))
                || req.contract_refs.iter().any(|r| strict_eq(Some(r), get(c, "id")))
        })
        .cloned()
        .collect();
    let selected_contract_ids = id_set(&selected_contracts, "id");
    let scenarios: Vec<Value> = all_scenarios
        .iter()
        .filter(|s| any_ref_in(s, "contractRefs", &[&selected_contract_ids]))
        .cloned()
        .collect();
    let scenarios = slice0(&scenarios, max_gaps);
    let obligation_ids = id_set(&scenarios, "obligationRef");
    let hints: Vec<Value> = all_hints
        .iter()
        .filter(|h| {
            any_ref_in(h, "subjectRefs", &[&selected_set, &selected_contract_ids, &obligation_ids])
        })
        .cloned()
        .collect();
    let hints = slice0(&hints, max_gaps);
    let boundaries: Vec<Value> = all_boundaries
        .iter()
        .filter(|b| {
            selected_entities
                .iter()
                .any(|e| strict_eq(get(e, "boundaryId"), get(b, "id")))
        })
        .cloned()
        .collect();

    let mut files: Vec<Value> = selected_entities
        .iter()
        .map(|e| get(e, "path").cloned().unwrap_or(Value::Null))
        .collect();
    for test_case in &test_cases {
        if any_ref_in(test_case, "targetContractRefs", &[&selected_contract_ids]) {
            files.push(get(test_case, "path").cloned().unwrap_or(Value::Null));
        }
    }
    let mut suggested_files = unique_values(&files);
    js_default_sort(&mut suggested_files);

    let mut pinned = Map::new();
    pinned.insert("headSha".into(), facts_head.cloned().unwrap_or(Value::Null));
    for (key, src) in [
        ("factsFingerprint", get(facts, "fingerprint")),
        ("assessmentFingerprint", get(assessment, "fingerprint")),
        ("obligationsRevision", get(assessment, "obligationsRevision")),
    ] {
        pinned.insert(key.into(), src.cloned().unwrap_or(Value::Null));
    }

    let truncated =
        (seed_count as f64) > max_seeds || (selected_edges.len() as f64) >= max_edges;
    let mut result = Map::new();
    result.insert("version".into(), json!(1));
    result.insert("kind".into(), json!("structure_context"));
    result.insert("pinned".into(), Value::Object(pinned));
    // undefined-поле JSON.stringify пропускает
    if let Some(alignment) = get(assessment, "temporalAlignment") {
        result.insert("temporalAlignment".into(), alignment.clone());
    }
    result.insert("taskRef".into(), req.task_ref.clone());
    result.insert("scopeRefs".into(), Value::Array(req.scope_refs.clone()));
    result.insert("boundaries".into(), Value::Array(boundaries));
    result.insert("entities".into(), Value::Array(selected_entities));
    result.insert("contracts".into(), Value::Array(selected_contracts));
    result.insert("dependencies".into(), Value::Array(selected_edges));
    result.insert("scenarios".into(), Value::Array(scenarios));
    result.insert("hints".into(), Value::Array(hints));
    result.insert("suggestedFiles".into(), Value::Array(suggested_files));
    result.insert("truncated".into(), json!(truncated));

    let token_estimate = estimate_tokens(&result);
    result.insert("tokenEstimate".into(), int_json(token_estimate));
    let bytes = js_json_stringify(&Value::Object(result.clone())).len() as f64;
    if bytes > max_bytes || token_estimate > req.token_budget {
        let half = (arr(&result, "dependencies").len() as f64 / 2.0).floor();
        slice_arr(&mut result, "dependencies", half);
        slice_arr(&mut result, "entities", max_seeds);
        // Math.max(1, NaN) === NaN
        let hints_cap = (max_gaps / 2.0).floor();
        let hints_cap = if hints_cap.is_nan() { f64::NAN } else { hints_cap.max(1.0) };
        slice_arr(&mut result, "hints", hints_cap);
        result.insert("truncated".into(), json!(true));
        prune_dangling_refs(&mut result);
        // пересчёт по result, где tokenEstimate уже есть (как в JS)
        let estimate = estimate_tokens(&result);
        result.insert("tokenEstimate".into(), int_json(estimate));
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    fn load(rel: &str) -> Value {
        let text = std::fs::read_to_string(repo_root().join(rel)).expect(rel);
        serde_json::from_str(&text).expect(rel)
    }

    fn fixture_pair() -> (Value, Value) {
        (
            load("fixtures/structure/structure-facts.v1.json"),
            load("fixtures/structure/structure-assessment.v1.json"),
        )
    }

    fn head(facts: &Value) -> Value {
        facts["provenance"]["headSha"].clone()
    }

    #[test]
    fn fixture_pinned_by_head_gives_empty_context() {
        let (facts, assessment) = fixture_pair();
        let req = ContextRequest { head_sha: Some(head(&facts)), ..Default::default() };
        let ctx = build_structure_context(&facts, &assessment, &req).unwrap();
        assert_eq!(ctx["kind"], json!("structure_context"));
        assert_eq!(ctx["pinned"]["headSha"], head(&facts));
        assert_eq!(ctx["pinned"]["factsFingerprint"], facts["fingerprint"]);
        assert_eq!(ctx["pinned"]["assessmentFingerprint"], assessment["fingerprint"]);
        assert_eq!(ctx["pinned"]["obligationsRevision"], json!("docs-1"));
        assert_eq!(ctx["temporalAlignment"], json!("aligned"));
        assert_eq!(ctx["entities"], json!([]));
        assert_eq!(ctx["boundaries"], json!([]));
        assert_eq!(ctx["truncated"], json!(false));
    }

    #[test]
    fn fixture_pin_errors() {
        let (facts, assessment) = fixture_pair();
        let err = |req: ContextRequest, a: &Value| {
            format!("{:#}", build_structure_context(&facts, a, &req).unwrap_err())
        };
        assert_eq!(
            err(ContextRequest::default(), &assessment),
            "structure_context requires pinned headSha or snapshotFingerprint"
        );
        assert_eq!(
            err(ContextRequest { head_sha: Some(json!("fffffff")), ..Default::default() }, &assessment),
            "pinned headSha fffffff does not match facts abcdef1"
        );
        let mut foreign = assessment.clone();
        foreign["factsFingerprint"] = json!("9".repeat(64));
        assert_eq!(
            err(ContextRequest { head_sha: Some(head(&facts)), ..Default::default() }, &foreign),
            "assessment does not reference the supplied facts snapshot"
        );
    }

    /// Эталон JS: rust/parity/golden-structure/t18.json (генератор рядом).
    #[test]
    fn golden_parity_with_js() {
        let golden = load("rust/parity/golden-structure/t18.json");
        let cases = golden["context"].as_array().unwrap();
        assert!(cases.len() >= 19);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let pair = &golden["payloads"][case["payload"].as_str().unwrap()];
            let req = ContextRequest::from_value(&case["args"]);
            let got = build_structure_context(&pair[0], &pair[1], &req);
            match case.get("error") {
                Some(Value::String(msg)) => {
                    assert_eq!(&format!("{:#}", got.unwrap_err()), msg, "кейс {name}");
                }
                _ => {
                    let got = got.unwrap_or_else(|e| panic!("кейс {name}: {e:#}"));
                    // сравнение текстом: порядок ключей тоже часть контракта
                    assert_eq!(
                        js_json_stringify(&got),
                        js_json_stringify(&case["expected"]),
                        "кейс {name}"
                    );
                }
            }
        }
    }

    #[test]
    fn token_gate_uses_estimate_without_own_field() {
        let golden = load("rust/parity/golden-structure/t18.json");
        let pair = &golden["payloads"]["rich"];
        let req = ContextRequest {
            head_sha: Some(head(&pair[0])),
            scope_refs: vec![json!("e:a")],
            task_ref: json!("T-1"),
            token_budget: 442.0,
            ..Default::default()
        };
        let ctx = build_structure_context(&pair[0], &pair[1], &req).unwrap();
        assert_eq!(ctx["tokenEstimate"], json!(442));
        assert_eq!(ctx["truncated"], json!(false));
    }
}
