//! Порт lib/structure-mcp.mjs (Фаза 2, шаг 5): MCP-style JSON-RPC-ish диспетчер
//! structure_context — дескриптор тулза, разбор запросов (initialize / ping /
//! tools/list / notifications / method-not-found) и построение structure context
//! (порт lib/structure-context.mjs: BFS по зависимостям от scope-сеeds, усечение
//! бюджетом токенов/байт, prune висячих ссылок).
//!
//! Граница с JS (как у прочих модулей Фазы 2): fs-I/O остаётся в JS-обёртке —
//! серверный stdio-цикл с Content-Length-фреймингом живёт в
//! bin/pr-review-structure-mcp.mjs без изменений (транспорт, не доменная логика;
//! async-стек — Фаза 3), а бинарь получает ТЕКСТЫ пейлоадов (factsText/
//! assessmentText) уже прочитанными. Контракт опов:
//!   handleRequest         {request} → ответное JSON-RPC-сообщение или null
//!                                     (notifications/initialized)
//!   buildStructureContext {factsText, assessmentText, factsPath, assessmentPath,
//!                          args} → structure context
//!
//! Паритет с JS, включая:
//! - клэмпинг аргументов тулза из собственного inputSchema (сервер не доверяет
//!   чужой валидации): boundedList — slice(0, maxItems) для массивов иначе [],
//!//!   boundedInt — Number(value), нецелое/NaN/Infinity → fallback, иначе
//!   clamp(minimum, maximum); taskRef — `|| null`;
//! - JS truthiness для пинов (`!headSha && !snapshotFingerprint` → ошибка
//!   ДО чтения файлов — эта проверка продублирована в JS-обёртке, чтобы порядок
//!   ошибок не зависел от наличия бинаря);
//! - Set/Map-семантику SameValueZero через value_key (скаляры по значению,
//!   числа через f64: 1 и 1.0 совпадают, как ===);
//! - matchesScope: строгие === по observedId/boundaryId/path и префиксный
//!   матч `path.startsWith(scope.replace(/\/+$/, '') + '/')`;
//! - BFS: очередь FIFO, дедуп рёбер по `edge.id || from→to:relation`, потолки
//!   maxSeeds/maxEdges/maxDepth, truncated = seeds > maxSeeds || edges >= maxEdges;
//! - estimateTokens = ceil(utf16_len(JSON.stringify(result)) / 4), байтовый гейт
//!   Buffer.byteLength(JSON.stringify(result)) > maxBytes (пересчёт сериализации
//!   ПОСЛЕ подстановки tokenEstimate — как в JS), усечение пополам dependencies,
//!   entities до maxSeeds, hints до max(1, floor(maxGaps/2)), затем
//!   pruneDanglingRefs и пересчёт tokenEstimate;
//! - дефолтный sort suggestedFiles — по UTF-16 code units (utf16_cmp);
//! - тексты доменных ошибок посимвольно (pinned headSha / snapshot fingerprint /
//!   assessment link / expected structure_facts|structure_assessment).
//!
//! Отклонения (вердикт совпадает, тексты/значения — нет, все за пределами
//! валидированных пейлоадов):
//! - facts.facts / assessment.assessments и прочие обязательные ветки читаются
//!   как Null при отсутствии: JS бросил бы V8-TypeError «Cannot read properties
//!   of undefined», здесь пустая итерируемая (iterable_values ловит truthy
//!   не-итерируемое как раньше);
//! - нестроковые элементы scopeRefs: в JS `scope.replace` бросил бы TypeError
//!   внутри matchesScope, здесь такие scope молча не матчат префиксно
//!   (строгие ===-ветки работают для любых скаляров);
//! - undefined в JSON не бывает: отсутствующие observedId/path/pinned-поля
//!   становятся Null (в JS-структуре остались бы undefined и выпали бы при
//!   сериализации; после JSON round-trip обе стороны дают null);
//! - экзотические limits (нечисловые maxBytes/maxSeeds): NaN-комбинаторика
//!   slice/сравнений воспроизведена частично (NaN → пустой срез, сравнение
//!   false), Infinity в срезах — как очень большое число;
//! - числа сериализуются через f64 (ECMA Number::toString): лексемы вроде
//!   "1.0" из пейлоада нормализуются, как у V8 после JSON.parse.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_number, js_to_string, js_truthy};
use crate::structure_evidence::{
    get, iterable_values, js_default_sort, js_json_string, js_number_to_string, unique_values,
    value_key,
};
use crate::structure_protocol::parse_structure_payload;

const TOOL_NAME: &str = "structure_context";
const DEFAULT_TOKEN_BUDGET: f64 = 8000.0;
const DEFAULT_DEPENDENCY_DEPTH: f64 = 1.0;
const MAX_LIST_ITEMS: f64 = 20.0;
const TOKEN_BUDGET_RANGE: (f64, f64) = (256.0, 32000.0);
const DEPENDENCY_DEPTH_RANGE: (f64, f64) = (0.0, 3.0);

/// STRUCTURE_CONTEXT_TOOL из lib/structure-mcp.mjs (зеркало inputSchema).
pub fn structure_context_tool() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Return a bounded structure/contracts/test-scenario context pinned to a facts snapshot.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["factsPath", "assessmentPath"],
            "anyOf": [
                { "required": ["headSha"] },
                { "required": ["snapshotFingerprint"] },
            ],
            "properties": {
                "factsPath": { "type": "string", "minLength": 1 },
                "assessmentPath": { "type": "string", "minLength": 1 },
                "headSha": { "type": "string", "pattern": "^[0-9a-fA-F]{7,64}$" },
                "snapshotFingerprint": { "type": "string", "pattern": "^[0-9a-f]{64}$" },
                "scopeRefs": { "type": "array", "maxItems": 20, "items": { "type": "string", "maxLength": 1000 } },
                "taskRef": { "type": ["string", "null"], "maxLength": 500 },
                "contractRefs": { "type": "array", "maxItems": 20, "items": { "type": "string", "maxLength": 500 } },
                "tokenBudget": { "type": "integer", "minimum": 256, "maximum": 32000 },
                "dependencyDepth": { "type": "integer", "minimum": 0, "maximum": 3 },
            },
        },
    })
}

fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(js_truthy).unwrap_or(false)
}

/// JS `String(v)` для опционального значения: None — "undefined".
fn js_to_string_opt(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(x) => js_to_string(x),
    }
}

/// Строгое JS `===` для значений из JSON: скаляры по значению, числа через f64
/// (1 === 1.0), объекты по сериализации (на практике id — скаляры).
fn js_strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn key_of(v: Option<&Value>) -> String {
    value_key(v.unwrap_or(&Value::Null))
}

/// JS `JSON.stringify` (компактно): числа через ECMA Number::toString,
/// не-конечные → null; ключи в порядке вставки (preserve_order).
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

/// JSON.stringify-строка в UTF-16 code units (`str.length` в JS).
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Целое как в JS-ответе: JSON.stringify(Math.ceil(...)) печатает целое без
/// дробной части.
fn int_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.is_finite() && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// `xs.slice(0, n)` для неотрицательного старта: NaN/<=0 → пусто, >=len → всё.
fn slice0(xs: &[Value], n: f64) -> Vec<Value> {
    if !n.is_finite() {
        return if n > 0.0 { xs.to_vec() } else { Vec::new() };
    }
    if n <= 0.0 {
        return Vec::new();
    }
    let take = (n as usize).min(xs.len());
    xs[..take].to_vec()
}

fn bounded_list(v: Option<&Value>, max_items: f64) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => slice0(a, max_items),
        _ => Vec::new(),
    }
}

/// boundedInt из lib/structure-mcp.mjs: Number(value), нецелое/NaN/Infinity →
/// fallback, иначе clamp(minimum, maximum).
fn bounded_int(v: Option<&Value>, range: (f64, f64), fallback: f64) -> f64 {
    match v.and_then(js_to_number) {
        Some(n) if n.is_finite() && n.fract() == 0.0 => n.max(range.0).min(range.1),
        _ => fallback,
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
        js_strict_eq(observed_id, Some(scope))
            || js_strict_eq(boundary_id, Some(scope))
            || js_strict_eq(path, Some(scope))
            || match (path, Some(scope)) {
                (Some(Value::String(p)), Some(Value::String(s))) => {
                    let prefix = format!("{}/", s.trim_end_matches('/'));
                    p.starts_with(&prefix)
                }
                _ => false,
            }
    })
}

/// Один лимит context-капа: {...DEFAULT_CONTEXT_LIMITS, ...limits} с
/// JS-семантикой (нечисловое значение → NaN, как в slice/сравнениях).
fn cap_limit(limits: Option<&Map<String, Value>>, key: &str, default: f64) -> f64 {
    match limits.and_then(|m| m.get(key)) {
        None => default,
        Some(v) => js_to_number(v).unwrap_or(f64::NAN),
    }
}

fn prune_dangling_refs(result: &mut Map<String, Value>) {
    // мутация на месте: JS-присваивание result.x = ... сохраняет порядок
    // ключей, remove+insert у serde_json Map переупорядочил бы их в конец
    let entities = match result.get("entities") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let entity_ids: HashSet<String> = entities
        .iter()
        .map(|e| key_of(get(e, "observedId")))
        .collect();
    let keep_in = |v: Option<&Value>, known: &HashSet<String>| known.contains(&key_of(v));

    retain_arr(result, "dependencies", |edge| {
        keep_in(get(edge, "from"), &entity_ids) && keep_in(get(edge, "to"), &entity_ids)
    });
    retain_arr(result, "contracts", |c| keep_in(get(c, "entityRef"), &entity_ids));
    let contract_ids: HashSet<String> = match result.get("contracts") {
        Some(Value::Array(a)) => a.iter().map(|c| key_of(get(c, "id"))).collect(),
        _ => HashSet::new(),
    };
    retain_arr(result, "scenarios", |s| {
        iterable_values(get(s, "contractRefs"), "contractRefs")
            .map(|refs| refs.iter().any(|id| keep_in(Some(id), &contract_ids)))
            .unwrap_or(false)
    });
    let obligation_ids: HashSet<String> = match result.get("scenarios") {
        Some(Value::Array(a)) => a.iter().map(|s| key_of(get(s, "obligationRef"))).collect(),
        _ => HashSet::new(),
    };
    retain_arr(result, "hints", |h| {
        iterable_values(get(h, "subjectRefs"), "subjectRefs")
            .map(|refs| {
                refs.iter().any(|r| {
                    keep_in(Some(r), &entity_ids)
                        || keep_in(Some(r), &contract_ids)
                        || keep_in(Some(r), &obligation_ids)
                })
            })
            .unwrap_or(false)
    });

    let boundary_ids: HashSet<String> = entities
        .iter()
        .filter_map(|e| get(e, "boundaryId"))
        .filter(|v| js_truthy(v))
        .map(value_key)
        .collect();
    retain_arr(result, "boundaries", |b| keep_in(get(b, "id"), &boundary_ids));
}

/// `result[key] = result[key].filter(pred)` с сохранением позиции ключа.
fn retain_arr(result: &mut Map<String, Value>, key: &str, pred: impl Fn(&Value) -> bool) {
    if let Some(Value::Array(a)) = result.get_mut(key) {
        a.retain(pred);
    }
}

/// Порт buildStructureContext из lib/structure-context.mjs. facts/assessment —
/// уже распарсенные и валидированные пейлоады, args — сырые аргументы тулза
/// (клэмпинг выполняется здесь, как в callStructureContext). limits — отдельный
/// параметр: callStructureContext собирает вызов литералом и НЕ пробрасывает
/// `limits` из чужих аргументов, op передаёт None (иначе клиент смог бы
/// перекрыть потолки, что JS-стороне недоступно).
pub fn build_structure_context(
    facts: &Value,
    assessment: &Value,
    args: &Value,
    limits: Option<&Map<String, Value>>,
) -> Result<Value> {
    let head_sha = get(args, "headSha");
    let snapshot_fingerprint = get(args, "snapshotFingerprint");
    if !truthy_opt(head_sha) && !truthy_opt(snapshot_fingerprint) {
        bail!("structure_context requires pinned headSha or snapshotFingerprint");
    }
    if truthy_opt(head_sha) {
        let facts_head = get(facts, "provenance").and_then(|p| get(p, "headSha"));
        if !js_strict_eq(facts_head, head_sha) {
            bail!(
                "pinned headSha {} does not match facts {}",
                js_to_string(head_sha.unwrap()),
                js_to_string_opt(facts_head),
            );
        }
    }
    if truthy_opt(snapshot_fingerprint) {
        if !js_strict_eq(get(facts, "fingerprint"), snapshot_fingerprint) {
            bail!(
                "snapshot fingerprint {} does not match facts",
                js_to_string(snapshot_fingerprint.unwrap()),
            );
        }
    }
    if !js_strict_eq(get(assessment, "factsFingerprint"), get(facts, "fingerprint")) {
        bail!("assessment does not reference the supplied facts snapshot");
    }

    let max_seeds = cap_limit(limits, "maxSeeds", 20.0);
    let max_edges = cap_limit(limits, "maxEdges", 50.0);
    let max_gaps = cap_limit(limits, "maxGaps", 15.0);
    let max_bytes = cap_limit(limits, "maxBytes", 32768.0);
    let dependency_depth = bounded_int(
        get(args, "dependencyDepth"),
        DEPENDENCY_DEPTH_RANGE,
        DEFAULT_DEPENDENCY_DEPTH,
    );
    let max_depth = f64::min(dependency_depth, cap_limit(limits, "maxDepth", 1.0));
    let token_budget = bounded_int(
        get(args, "tokenBudget"),
        TOKEN_BUDGET_RANGE,
        DEFAULT_TOKEN_BUDGET,
    );

    let scope_refs = bounded_list(get(args, "scopeRefs"), MAX_LIST_ITEMS);
    let contract_refs = bounded_list(get(args, "contractRefs"), MAX_LIST_ITEMS);
    let task_ref = match get(args, "taskRef") {
        Some(v) if js_truthy(v) => v.clone(),
        _ => Value::Null,
    };

    let facts_inner = get(facts, "facts").cloned().unwrap_or(Value::Null);
    let entities = iterable_values(get(&facts_inner, "entities"), "entities")?;
    let contracts = iterable_values(get(&facts_inner, "contracts"), "contracts")?;
    let dependencies = iterable_values(get(&facts_inner, "dependencies"), "dependencies")?;
    let all_boundaries = iterable_values(get(&facts_inner, "boundaries"), "boundaries")?;
    let test_cases = iterable_values(get(&facts_inner, "testCases"), "testCases")?;
    let assessments_inner = get(assessment, "assessments")
        .cloned()
        .unwrap_or(Value::Null);
    let all_scenarios = iterable_values(
        get(&assessments_inner, "contractScenarios"),
        "contractScenarios",
    )?;
    let all_hints = iterable_values(get(assessment, "hints"), "hints")?;

    let mut entity_by_id: HashMap<String, Value> = HashMap::new();
    for entity in &entities {
        entity_by_id.insert(key_of(get(entity, "observedId")), entity.clone());
    }
    let mut contract_by_id: HashMap<String, Value> = HashMap::new();
    for contract in &contracts {
        contract_by_id.insert(key_of(get(contract, "id")), contract.clone());
    }

    // seeds: scope-матчи в порядке сущностей, затем entityRef по contractRefs
    let mut seed_order: Vec<String> = Vec::new();
    let mut seed_set: HashSet<String> = HashSet::new();
    for entity in &entities {
        if matches_scope(entity, &scope_refs) {
            let k = key_of(get(entity, "observedId"));
            if seed_set.insert(k.clone()) {
                seed_order.push(k);
            }
        }
    }
    for contract_ref in &contract_refs {
        if let Some(contract) = contract_by_id.get(&value_key(contract_ref)) {
            let k = key_of(get(contract, "entityRef"));
            if seed_set.insert(k.clone()) {
                seed_order.push(k);
            }
        }
    }

    // BFS по зависимостям от seeds (FIFO, дедуп рёбер)
    let mut queue: VecDeque<(String, f64)> = VecDeque::new();
    let mut selected_order: Vec<String> = Vec::new();
    let mut selected_set: HashSet<String> = HashSet::new();
    let seed_take = if max_seeds <= 0.0 {
        0
    } else {
        (max_seeds as usize).min(seed_order.len())
    };
    for key in seed_order.iter().take(seed_take).cloned().collect::<Vec<_>>() {
        if selected_set.insert(key.clone()) {
            selected_order.push(key.clone());
            queue.push_back((key, 0.0));
        }
    }
    let mut selected_edges: Vec<Value> = Vec::new();
    let mut selected_edge_ids: HashSet<String> = HashSet::new();
    while let Some((current, depth)) = queue.pop_front() {
        if (selected_edges.len() as f64) >= max_edges {
            break;
        }
        if depth >= max_depth {
            continue;
        }
        for edge in &dependencies {
            if (selected_edges.len() as f64) >= max_edges {
                break;
            }
            let from = get(edge, "from");
            let to = get(edge, "to");
            let touches = key_of(from) == current || key_of(to) == current;
            if !touches {
                continue;
            }
            let edge_id = match get(edge, "id") {
                Some(v) if js_truthy(v) => v.clone(),
                _ => json!(format!(
                    "{}→{}:{}",
                    js_to_string_opt(from),
                    js_to_string_opt(to),
                    js_to_string_opt(get(edge, "relation")),
                )),
            };
            let edge_key = value_key(&edge_id);
            if !selected_edge_ids.insert(edge_key) {
                continue;
            }
            selected_edges.push(edge.clone());
            let other = if key_of(from) == current { to } else { from };
            let other_key = key_of(other);
            if selected_set.insert(other_key.clone()) {
                selected_order.push(other_key.clone());
                queue.push_back((other_key, depth + 1.0));
            }
        }
    }

    let selected_entities: Vec<Value> = selected_order
        .iter()
        .filter_map(|k| entity_by_id.get(k))
        .cloned()
        .collect();
    let selected_contracts: Vec<Value> = contracts
        .iter()
        .filter(|c| {
            selected_set.contains(&key_of(get(c, "entityRef")))
                || contract_refs
                    .iter()
                    .any(|ref_| js_strict_eq(Some(ref_), get(c, "id")))
        })
        .cloned()
        .collect();
    let selected_contract_ids: HashSet<String> = selected_contracts
        .iter()
        .map(|c| key_of(get(c, "id")))
        .collect();
    let scenarios: Vec<Value> = all_scenarios
        .iter()
        .filter(|s| {
            iterable_values(get(s, "contractRefs"), "contractRefs")
                .map(|refs| {
                    refs.iter()
                        .any(|id| selected_contract_ids.contains(&value_key(id)))
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let scenarios = slice0(&scenarios, max_gaps);
    let obligation_ids: HashSet<String> = scenarios
        .iter()
        .map(|s| key_of(get(s, "obligationRef")))
        .collect();
    let hints: Vec<Value> = all_hints
        .iter()
        .filter(|h| {
            iterable_values(get(h, "subjectRefs"), "subjectRefs")
                .map(|refs| {
                    refs.iter().any(|r| {
                        selected_set.contains(&value_key(r))
                            || selected_contract_ids.contains(&value_key(r))
                            || obligation_ids.contains(&value_key(r))
                    })
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let hints = slice0(&hints, max_gaps);
    let boundaries: Vec<Value> = all_boundaries
        .iter()
        .filter(|b| {
            selected_entities
                .iter()
                .any(|e| js_strict_eq(get(e, "boundaryId"), get(b, "id")))
        })
        .cloned()
        .collect();

    let mut suggested_files: Vec<Value> = selected_entities
        .iter()
        .map(|e| get(e, "path").cloned().unwrap_or(Value::Null))
        .collect();
    for test_case in &test_cases {
        let targets_contract = iterable_values(
            get(test_case, "targetContractRefs"),
            "targetContractRefs",
        )
        .map(|refs| {
            refs.iter()
                .any(|id| selected_contract_ids.contains(&value_key(id)))
        })
        .unwrap_or(false);
        if targets_contract {
            suggested_files.push(get(test_case, "path").cloned().unwrap_or(Value::Null));
        }
    }
    let mut suggested_files = unique_values(&suggested_files);
    js_default_sort(&mut suggested_files);

    let mut pinned = Map::new();
    pinned.insert(
        "headSha".into(),
        get(facts, "provenance")
            .and_then(|p| get(p, "headSha"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    for (key, src) in [
        ("factsFingerprint", get(facts, "fingerprint")),
        ("assessmentFingerprint", get(assessment, "fingerprint")),
        ("obligationsRevision", get(assessment, "obligationsRevision")),
    ] {
        pinned.insert(key.into(), src.cloned().unwrap_or(Value::Null));
    }

    let truncated = (seed_set.len() as f64) > max_seeds
        || (selected_edges.len() as f64) >= max_edges;
    let mut result = Map::new();
    result.insert("version".into(), json!(1));
    result.insert("kind".into(), json!("structure_context"));
    result.insert("pinned".into(), Value::Object(pinned));
    if let Some(alignment) = get(assessment, "temporalAlignment") {
        result.insert("temporalAlignment".into(), alignment.clone());
    }
    result.insert("taskRef".into(), task_ref);
    result.insert(
        "scopeRefs".into(),
        Value::Array(scope_refs.clone()),
    );
    result.insert("boundaries".into(), Value::Array(boundaries));
    result.insert("entities".into(), Value::Array(selected_entities));
    result.insert("contracts".into(), Value::Array(selected_contracts));
    result.insert("dependencies".into(), Value::Array(selected_edges));
    result.insert("scenarios".into(), Value::Array(scenarios));
    result.insert("hints".into(), Value::Array(hints));
    result.insert("suggestedFiles".into(), Value::Array(suggested_files));
    result.insert("truncated".into(), json!(truncated));

    // tokenEstimate по сериализации результата БЕЗ tokenEstimate
    let estimate = |v: &Value| -> f64 { (utf16_len(&js_json_stringify(v)) as f64 / 4.0).ceil() };
    // гейт бюджета сравнивает именно эту оценку (result.tokenEstimate в JS), а
    // не пересчёт по result, где поле tokenEstimate уже есть
    let token_estimate = estimate(&Value::Object(result.clone()));
    result.insert("tokenEstimate".into(), int_json(token_estimate));
    let compact = js_json_stringify(&Value::Object(result.clone()));
    if (compact.len() as f64) > max_bytes || token_estimate > token_budget {
        let half = ((get_arr_len(&result, "dependencies") as f64) / 2.0)
            .floor()
            .max(0.0);
        slice_arr(&mut result, "dependencies", half);
        slice_arr(&mut result, "entities", max_seeds);
        let hints_cap = (max_gaps / 2.0).floor().max(1.0);
        slice_arr(&mut result, "hints", hints_cap);
        result.insert("truncated".into(), json!(true));
        prune_dangling_refs(&mut result);
        result.insert(
            "tokenEstimate".into(),
            int_json(estimate(&Value::Object(result.clone()))),
        );
    }
    Ok(Value::Object(result))
}

/// `result[key] = result[key].slice(0, n)` с сохранением позиции ключа.
fn slice_arr(result: &mut Map<String, Value>, key: &str, n: f64) {
    if let Some(Value::Array(a)) = result.get_mut(key) {
        *a = slice0(a, n);
    }
}

fn get_arr_len(m: &Map<String, Value>, key: &str) -> usize {
    match m.get(key) {
        Some(Value::Array(a)) => a.len(),
        _ => 0,
    }
}

/// handleStructureMcpRequest из lib/structure-mcp.mjs для «чистых» методов
/// (initialize / notifications\/initialized / ping / tools\/list и
/// method-not-found). tools/call обслуживается через op buildStructureContext:
/// чтение файлов живёт в JS-обёртке, сюда передаются тексты пейлоадов.
pub fn handle_request(request: &Value) -> Result<Value> {
    let id = get(request, "id").cloned().unwrap_or(Value::Null);
    let method = get(request, "method");
    match method {
        Some(Value::String(m)) if m == "initialize" => {
            let params = get(request, "params");
            let protocol_version = get(params.unwrap_or(&Value::Null), "protocolVersion");
            let protocol_version = match protocol_version {
                Some(v) if js_truthy(v) => v.clone(),
                _ => json!("2025-06-18"),
            };
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": protocol_version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "ai-reviewer-structure", "version": "1.0.0" },
                },
            }))
        }
        Some(Value::String(m)) if m == "notifications/initialized" => Ok(Value::Null),
        Some(Value::String(m)) if m == "ping" => Ok(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
        Some(Value::String(m)) if m == "tools/list" => Ok(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": [structure_context_tool()] },
        })),
        Some(Value::String(m)) if m == "tools/call" => {
            let params = get(request, "params");
            let name = params.and_then(|p| get(p, "name"));
            match name {
                Some(Value::String(n)) if n == TOOL_NAME => {
                    bail!("tools/call structure_context is served via the buildStructureContext op (fs stays in JS)")
                }
                _ => bail!("unknown tool: {}", js_to_string_opt(name)),
            }
        }
        _ => bail!("method not found: {}", js_to_string_opt(method)),
    }
}

/// Op buildStructureContext: тексты пейлоадов из JS-обёртки + сырые args.
pub fn build_structure_context_op(obj: &Map<String, Value>) -> Result<Value> {
    let facts_text = obj
        .get("factsText")
        .and_then(Value::as_str)
        .context("buildStructureContext: factsText required")?;
    let assessment_text = obj
        .get("assessmentText")
        .and_then(Value::as_str)
        .context("buildStructureContext: assessmentText required")?;
    let facts_path = obj.get("factsPath").cloned().unwrap_or(Value::Null);
    let assessment_path = obj.get("assessmentPath").cloned().unwrap_or(Value::Null);
    let args = obj.get("args").cloned().unwrap_or(Value::Null);

    // порядок как в callStructureContext: пин-проверка раньше чтения/парсинга
    if !truthy_opt(get(&args, "headSha")) && !truthy_opt(get(&args, "snapshotFingerprint")) {
        bail!("headSha or snapshotFingerprint is required");
    }
    let facts_payload = parse_structure_payload(facts_text, None)?;
    if get(&facts_payload, "kind").and_then(Value::as_str) != Some("structure_facts") {
        bail!(
            "expected structure_facts at {}",
            js_to_string(&facts_path),
        );
    }
    let assessment_payload = parse_structure_payload(assessment_text, None)?;
    if get(&assessment_payload, "kind").and_then(Value::as_str) != Some("structure_assessment") {
        bail!(
            "expected structure_assessment at {}",
            js_to_string(&assessment_path),
        );
    }
    build_structure_context(&facts_payload, &assessment_payload, &args, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn entity(id: &str, path: &str, boundary: &str) -> Value {
        json!({
            "observedId": id, "lineageId": id, "label": id, "kind": "function",
            "path": path, "boundaryId": boundary,
            "confidence": "extracted", "evidence": [], "externalIds": {},
        })
    }

    fn contract(id: &str, entity_ref: &str) -> Value {
        json!({
            "id": id, "entityRef": entity_ref, "kind": "entrypoint",
            "confidence": "extracted", "evidence": [],
        })
    }

    fn edge(id: &str, from: &str, to: &str) -> Value {
        json!({
            "id": id, "from": from, "to": to, "relation": "calls",
            "confidence": "extracted", "evidence": [],
        })
    }

    fn facts_payload() -> Value {
        json!({
            "schemaRef": "code-structure/structure-facts.v1",
            "version": 1,
            "kind": "structure_facts",
            "generatedAt": "2026-09-02T00:00:00.000Z",
            "fingerprint": sha('a'),
            "provenance": { "headSha": "abcdef1", "branchRef": "main" },
            "facts": {
                "boundaries": [
                    { "id": "b:src", "kind": "module", "name": "src", "paths": ["src"] },
                    { "id": "b:other", "kind": "module", "name": "other", "paths": ["other"] },
                ],
                "entities": [
                    entity("e:a", "src/a.ts", "b:src"),
                    entity("e:b", "src/b.ts", "b:src"),
                    entity("e:c", "src/c.ts", "b:src"),
                ],
                "contracts": [contract("c:a", "e:a"), contract("c:b", "e:b")],
                "dependencies": [edge("d:ab", "e:a", "e:b"), edge("d:bc", "e:b", "e:c")],
                "testCases": [{
                    "id": "t:1", "name": "t", "path": "src/a.test.ts", "line": 1,
                    "targetContractRefs": ["c:a"],
                    "confidence": "extracted", "evidence": [],
                }],
            },
            "identityEvents": [], "views": {}, "receipts": [],
        })
    }

    fn assessment_payload() -> Value {
        json!({
            "schemaRef": "code-structure/structure-assessment.v1",
            "version": 1,
            "kind": "structure_assessment",
            "generatedAt": "2026-09-02T00:00:00.000Z",
            "fingerprint": sha('b'),
            "factsFingerprint": sha('a'),
            "coverageHash": sha('c'),
            "thresholdsHash": sha('d'),
            "temporalAlignment": "aligned",
            "obligationsRevision": "none",
            "coverageObservations": [],
            "assessments": {
                "contractScenarios": [{
                    "id": "s:1", "contractRefs": ["c:a"], "obligationRef": "o:1",
                    "status": "covered", "missingEvidence": [],
                }],
            },
            "hints": [{
                "id": "h:1", "subjectRefs": ["e:a"], "kind": "note", "message": "m",
            }],
            "receipts": [],
        })
    }

    fn args(v: Value) -> Value {
        let mut m = match v {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        m.insert("headSha".into(), json!("abcdef1"));
        Value::Object(m)
    }

    #[test]
    fn tool_descriptor_matches_contract() {
        let tool = structure_context_tool();
        assert_eq!(get(&tool, "name").and_then(Value::as_str), Some("structure_context"));
        let schema = get(&tool, "inputSchema").unwrap();
        assert_eq!(
            get(schema, "required"),
            Some(&json!(["factsPath", "assessmentPath"])),
        );
        let props = get(schema, "properties").unwrap();
        assert_eq!(get(props, "scopeRefs").and_then(|p| get(p, "maxItems")), Some(&json!(20)));
        assert_eq!(
            get(props, "tokenBudget").and_then(|p| get(p, "minimum")),
            Some(&json!(256)),
        );
    }

    #[test]
    fn handle_request_pure_methods() {
        let init = handle_request(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})).unwrap();
        assert_eq!(get(&init, "id"), Some(&json!(1)));
        let result = get(&init, "result").unwrap();
        assert_eq!(
            get(result, "protocolVersion"),
            Some(&json!("2025-06-18")),
        );
        assert_eq!(
            get(result, "serverInfo").and_then(|s| get(s, "name")),
            Some(&json!("ai-reviewer-structure")),
        );
        // protocolVersion из params проходит через, falsy → дефолт
        let init2 = handle_request(&json!({
            "id": 2, "method": "initialize", "params": { "protocolVersion": "2024-11-05" },
        })).unwrap();
        assert_eq!(
            get(&init2, "result").and_then(|r| get(r, "protocolVersion")),
            Some(&json!("2024-11-05")),
        );
        let init3 = handle_request(&json!({
            "id": 3, "method": "initialize", "params": { "protocolVersion": "" },
        })).unwrap();
        assert_eq!(
            get(&init3, "result").and_then(|r| get(r, "protocolVersion")),
            Some(&json!("2025-06-18")),
        );
        // ping / notifications / tools/list
        let ping = handle_request(&json!({"id": "p", "method": "ping"})).unwrap();
        assert_eq!(ping, json!({"jsonrpc": "2.0", "id": "p", "result": {}}));
        assert_eq!(
            handle_request(&json!({"method": "notifications/initialized"})).unwrap(),
            Value::Null,
        );
        let list = handle_request(&json!({"id": 9, "method": "tools/list"})).unwrap();
        assert_eq!(
            get(&list, "result").and_then(|r| get(r, "tools")).and_then(Value::as_array).map(|a| a.len()),
            Some(1),
        );
        // method not found / unknown tool
        let err = handle_request(&json!({"id": 4, "method": "nope"})).unwrap_err();
        assert_eq!(format!("{err:#}"), "method not found: nope");
        let err = handle_request(&json!({"id": 5, "method": "tools/call", "params": {"name": "x"}}))
            .unwrap_err();
        assert_eq!(format!("{err:#}"), "unknown tool: x");
        let err = handle_request(&Value::Null).unwrap_err();
        assert_eq!(format!("{err:#}"), "method not found: undefined");
    }

    #[test]
    fn context_bfs_depth_and_scope() {
        let facts = facts_payload();
        let assessment = assessment_payload();
        // scope e:a, глубина по умолчанию 1: e:a + сосед e:b, дальше не идём
        let ctx = build_structure_context(
            &facts,
            &assessment,
            &args(json!({"scopeRefs": ["e:a"]})),
            None,
        )
        .unwrap();
        assert_eq!(get(&ctx, "kind"), Some(&json!("structure_context")));
        let ids: Vec<&str> = get(&ctx, "entities")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|e| get(e, "observedId").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(ids, vec!["e:a", "e:b"]);
        let deps = get(&ctx, "dependencies").and_then(Value::as_array).unwrap();
        assert_eq!(deps.len(), 1);
        // глубина 0 — только seeds
        let ctx0 = build_structure_context(
            &facts,
            &assessment,
            &args(json!({"scopeRefs": ["e:a"], "dependencyDepth": 0})),
            None,
        )
        .unwrap();
        assert_eq!(
            get(&ctx0, "entities").and_then(Value::as_array).unwrap().len(),
            1,
        );
        // префиксный матч по пути
        let ctxp = build_structure_context(
            &facts,
            &assessment,
            &args(json!({"scopeRefs": ["src/b.ts"]})),
            None,
        )
        .unwrap();
        let ids: Vec<&str> = get(&ctxp, "entities")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|e| get(e, "observedId").and_then(Value::as_str).unwrap())
            .collect();
        assert!(ids.contains(&"e:b"));
    }

    #[test]
    fn context_contract_refs_and_suggested_files() {
        let facts = facts_payload();
        let assessment = assessment_payload();
        let ctx = build_structure_context(
            &facts,
            &assessment,
            &args(json!({"contractRefs": ["c:a"]})),
            None,
        )
        .unwrap();
        // seed по контракту: e:a и его сосед e:b на глубине 1
        let ids: Vec<&str> = get(&ctx, "entities")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|e| get(e, "observedId").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(ids, vec!["e:a", "e:b"]);
        // suggestedFiles: пути сущностей + тест по c:a, отсортированы
        assert_eq!(
            get(&ctx, "suggestedFiles"),
            Some(&json!(["src/a.test.ts", "src/a.ts", "src/b.ts"])),
        );
        // сценарий по c:a подтянулся, хинт на e:a — тоже
        assert_eq!(get(&ctx, "scenarios").and_then(Value::as_array).map(Vec::len), Some(1));
        assert_eq!(get(&ctx, "hints").and_then(Value::as_array).map(Vec::len), Some(1));
        // граница b:src уцелела, b:other отрезана
        let boundaries: Vec<&str> = get(&ctx, "boundaries")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|b| get(b, "id").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(boundaries, vec!["b:src"]);
        assert_eq!(get(&ctx, "truncated"), Some(&json!(false)));
    }

    #[test]
    fn context_pin_mismatch_errors() {
        let facts = facts_payload();
        let assessment = assessment_payload();
        let err = build_structure_context(
            &facts,
            &assessment,
            &json!({"headSha": "deadbeef"}),
            None,
        )
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "pinned headSha deadbeef does not match facts abcdef1",
        );
        let err = build_structure_context(
            &facts,
            &assessment,
            &json!({"snapshotFingerprint": sha('f')}),
            None,
        )
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            format!("snapshot fingerprint {} does not match facts", sha('f')),
        );
        let mut bad_assessment = assessment.clone();
        bad_assessment
            .as_object_mut()
            .unwrap()
            .insert("factsFingerprint".into(), json!(sha('f')));
        let err = build_structure_context(&facts, &bad_assessment, &args(json!({})), None).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "assessment does not reference the supplied facts snapshot",
        );
        let err = build_structure_context(&facts, &assessment, &json!({}), None).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "structure_context requires pinned headSha or snapshotFingerprint",
        );
    }

    #[test]
    fn context_budget_truncation_prunes_dangling() {
        let facts = facts_payload();
        let assessment = assessment_payload();
        // tokenBudget 256: контекст не влезает → усечение и prune
        let ctx = build_structure_context(
            &facts,
            &assessment,
            &args(json!({"scopeRefs": ["e:a"], "tokenBudget": 256})),
            None,
        )
        .unwrap();
        assert_eq!(get(&ctx, "truncated"), Some(&json!(true)));
        let estimate = get(&ctx, "tokenEstimate").and_then(Value::as_f64).unwrap();
        assert!(estimate > 0.0 && estimate.fract() == 0.0);
        // ссылки ведут внутрь payload: dependencies резаны пополам до 0,
        // сценарии/хинты без живых контрактов/сущностей убраны prune
        let deps = get(&ctx, "dependencies").and_then(Value::as_array).unwrap();
        let entities = get(&ctx, "entities").and_then(Value::as_array).unwrap();
        let entity_ids: HashSet<String> = entities
            .iter()
            .map(|e| key_of(get(e, "observedId")))
            .collect();
        for edge in deps {
            assert!(entity_ids.contains(&key_of(get(edge, "from"))));
            assert!(entity_ids.contains(&key_of(get(edge, "to"))));
        }
    }

    #[test]
    fn op_parses_texts_and_checks_kinds() {
        let facts_text = serde_json::to_string(&facts_payload()).unwrap();
        let assessment_text = serde_json::to_string(&assessment_payload()).unwrap();
        let obj = json!({
            "factsText": facts_text,
            "assessmentText": assessment_text,
            "factsPath": "facts.json",
            "assessmentPath": "assessment.json",
            "args": { "headSha": "abcdef1", "scopeRefs": ["e:a"] },
        });
        let ctx = build_structure_context_op(obj.as_object().unwrap()).unwrap();
        assert_eq!(get(&ctx, "kind"), Some(&json!("structure_context")));

        // перепутанные kind'ы
        let obj = json!({
            "factsText": assessment_text,
            "assessmentText": assessment_text,
            "factsPath": "facts.json",
            "args": { "headSha": "abcdef1" },
        });
        let err = build_structure_context_op(obj.as_object().unwrap()).unwrap_err();
        assert_eq!(format!("{err:#}"), "expected structure_facts at facts.json");

        // без пина
        let obj = json!({
            "factsText": facts_text,
            "assessmentText": assessment_text,
            "args": {},
        });
        let err = build_structure_context_op(obj.as_object().unwrap()).unwrap_err();
        assert_eq!(format!("{err:#}"), "headSha or snapshotFingerprint is required");
    }

    #[test]
    fn token_gate_matches_js_at_budget_boundary() {
        // эталон JS (rust/parity/golden-structure/t18.json): без поля tokenEstimate
        // оценка ровно 442 — при tokenBudget 442 усечения нет
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("parity/golden-structure/t18.json");
        let golden: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let pair = &golden["payloads"]["rich"];
        let ctx = build_structure_context(
            &pair[0],
            &pair[1],
            &json!({"headSha": "abcdef1", "scopeRefs": ["e:a"], "taskRef": "T-1", "tokenBudget": 442}),
            None,
        )
        .unwrap();
        assert_eq!(get(&ctx, "truncated"), Some(&json!(false)));
        assert_eq!(get(&ctx, "tokenEstimate"), Some(&json!(442)));
    }

    #[test]
    fn sorting_is_utf16_code_units() {
        // U+E000 (BMP) против astral-символа: по UTF-16 astral идёт раньше
        let mut values = vec![json!("\u{e000}"), json!("\u{10000}")];
        js_default_sort(&mut values);
        assert_eq!(values[0], json!("\u{10000}"));
        assert_eq!(crate::structure_evidence::utf16_cmp("a", "b"), std::cmp::Ordering::Less);
    }
}
