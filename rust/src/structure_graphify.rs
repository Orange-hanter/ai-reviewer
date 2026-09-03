//! Порт lib/structure-graphify.mjs: экстрактор структурных фактов из graphify-отчётов
//! (nodes/links) и построение view-деревьев (boundaries/objects/functions).
//!
//! Паритет с JS сохранён, включая:
//! - `||`-семантику (0/''/null → fallback) в label/line/community/scope;
//! - file_type: truthy и не 'code' → узел пропускается;
//! - lineNumber: /(?:^|[:#])L?(\d+)(?::\d+)?$/ — строка, а не хвостовая колонка;
//! - dropped-узлы: unsafe source_file и unparsed source_location считаются и пишутся
//!   в warnings (молча терять узел нельзя);
//! - god-узлы: id/файл+имя однозначны, голое имя при count > 1 понижается
//!   с предупреждением «god node label is ambiguous», а не метится наугад;
//! - confidence эджей (EXTRACTED/INFERRED/AMBIGUOUS → extracted/inferred/ambiguous,
//!   иначе heuristic) и фильтр includeAmbiguous;
//! - boundary-кандидатов: layout.appDir/layerDirs (слой только при parts > 2, иначе
//!   parts[1] — файл), module-префиксы с чёрным списком (node_modules/vendor/dist/
//!   build/coverage); boundary для пути — по самому длинному совпавшему префиксу
//!   (первый выигрывает при равенстве);
//! - отсечение сущностей ДО построения boundaries, рёбер — после сортировки по id;
//! - рёбра дедуплицируются по stableId (одинаковые рёбра иначе удваивают fanIn);
//! - контракты: implements/inherits из рёбер + entrypoint-дедуп по единому ключу,
//!   при обрезке maxContracts первыми уходят entrypoint (rank-сортировка), затем
//!   список пересортировывается по id;
//! - testCases: путь похож на тест И kind function/method; line = entity.line || 1;
//! - сортировки localeCompare (ICU root из structure_scip), дефолтный Array.sort —
//!   побайтовый; stableId — stable_id из structure_scip.

use std::collections::{HashMap, HashSet};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::findings::{js_number_or_zero, js_string_to_number, js_to_string, js_truthy};
use crate::structure_scip::{locale_compare, normalize_repo_path, stable_id};

const DEP_RELATIONS: &[&str] = &[
    "contains", "method", "inherits", "implements", "calls",
    "imports", "imports_from", "re_exports", "dynamic_import", "references",
];

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object().and_then(|o| o.get(key))
}

fn get_opt<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v.and_then(|value| get(value, key))
}

fn as_array(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

/// String(v || '') — truthy-цепочка JS ('' / 0 / false / null → '').
fn string_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    }
}

/// Голый String(v) из JS: undefined → 'undefined' (ключи эджей без fallback).
fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(x) => js_to_string(x),
    }
}

fn line_number_re() -> Regex {
    // /(?:^|[:#])L?(\d+)(?::\d+)?$/ — номер строки, а не хвостовая колонка:
    // 'src/a.ts:10:5' → 10, 'src/a.ts#L10' → 10
    Regex::new(r"(?:^|[:#])L?([0-9]+)(?::[0-9]+)?$").unwrap()
}

/// lineNumber: /(?:^|[:#])L?(\d+)(?::\d+)?$/ на String(value || '').
fn line_number(value: Option<&Value>) -> Value {
    let raw = string_or_empty(value);
    match line_number_re().captures(&raw) {
        Some(caps) => json!(caps.get(1).unwrap().as_str().parse::<u64>().unwrap_or(0)),
        None => Value::Null,
    }
}

/// clampLimit из JS: Number(value); !isFinite → max; min(max(1, floor), max).
/// Number(null) === 0, Number(true) === 1 — как в JS.
pub fn clamp_limit(value: Option<&Value>, max: usize) -> usize {
    let n = match value {
        None => return max,
        Some(Value::Null) => Some(0.0),
        Some(Value::Bool(b)) => Some(u8::from(*b) as f64),
        Some(v) => js_string_to_number(&js_to_string(v)),
    };
    match n {
        Some(n) if n.is_finite() => (n.floor().max(1.0) as usize).min(max),
        _ => max,
    }
}

fn edge_confidence(value: Option<&Value>) -> &'static str {
    match string_or_empty(value).to_uppercase().as_str() {
        "EXTRACTED" => "extracted",
        "INFERRED" => "inferred",
        "AMBIGUOUS" => "ambiguous",
        _ => "heuristic",
    }
}

fn path_in_scope(path: &str, scope: &str) -> bool {
    if scope.is_empty() {
        return true;
    }
    let prefix = format!("{}/", scope.trim_end_matches('/'));
    path == scope || path.starts_with(&prefix)
}

fn infer_kind(
    node: &Value,
    method_sources: &HashSet<String>,
    method_targets: &HashSet<String>,
    inheritance_sources: &HashSet<String>,
    implementation_targets: &HashSet<String>,
) -> &'static str {
    let id = js_string(node.get("id"));
    let label = string_or_empty(node.get("label"));
    if method_targets.contains(&id) || label.starts_with('.') {
        return "method";
    }
    let callable_class = node.get("_callable_class") == Some(&Value::Bool(true));
    if callable_class || method_sources.contains(&id) || inheritance_sources.contains(&id) {
        return if implementation_targets.contains(&id) {
            "interface"
        } else {
            "class_like"
        };
    }
    let callable = node.get("_callable") == Some(&Value::Bool(true));
    // /\(\)+$/ — один и более хвостовых "()"
    let mut stripped = label.as_str();
    while let Some(rest) = stripped.strip_suffix("()") {
        stripped = rest;
    }
    if callable || stripped.len() != label.len() {
        return "function";
    }
    "symbol"
}

/// boundaryCandidates: repository + layer (по layout профиля) + module (префикс пути).
fn boundary_candidates(paths: &[String], layout: Option<&Value>) -> Vec<Value> {
    let app_dir = string_or_empty(get_opt(layout, "appDir"));
    let layer_dirs = get_opt(layout, "layerDirs");
    let excluded = ["node_modules", "vendor", "dist", "build", "coverage"];

    let mut found: Vec<Value> = Vec::new();
    let mut found_keys: HashSet<String> = HashSet::new();
    for path in paths {
        let parts: Vec<&str> = path.split('/').collect();
        // parts.len() > 2 обязателен: иначе parts[1] — файл (src/index.ts), а не слой
        let layer = if !app_dir.is_empty()
            && parts.first() == Some(&app_dir.as_str())
            && parts.len() > 2
        {
            parts[1].to_string()
        } else {
            String::new()
        };
        // !layout.layerDirs?.length || layerDirs.includes(layer)
        let layered = !layer.is_empty()
            && match layer_dirs {
                None | Some(Value::Null) => true,
                Some(Value::Array(items)) => {
                    items.is_empty() || items.iter().any(|item| js_to_string(item) == layer)
                }
                Some(Value::String(s)) => s.contains(&layer),
                Some(other) => !js_truthy(other),
            };
        if layered {
            let key = format!("layer:{layer}");
            if found_keys.insert(key) {
                found.push(json!({
                    "id": format!("boundary:layer:{layer}"),
                    "kind": "layer",
                    "name": layer,
                    "paths": [format!("{app_dir}/{layer}")],
                    "source": "profile",
                    "confidence": "exact",
                    "evidence": [format!("profile:layout:{app_dir}/{layer}")],
                }));
            }
            continue;
        }
        // путь под appDir с незаявленным слоем раньше проваливался мимо обеих веток
        // и молча оказывался в boundary:repository — теперь у него остаётся модуль
        if parts.len() > 1 {
            let module = parts[0].to_string();
            if !excluded.contains(&module.as_str()) {
                let key = format!("module:{module}");
                if found_keys.insert(key) {
                    found.push(json!({
                        "id": format!("boundary:module:{module}"),
                        "kind": "module",
                        "name": module,
                        "paths": [module.clone()],
                        "source": "path",
                        "confidence": "heuristic",
                        "evidence": [format!("path-prefix:{module}")],
                    }));
                }
            }
        }
    }
    let mut boundaries: Vec<Value> = vec![json!({
        "id": "boundary:repository",
        "kind": "repository",
        "name": "repository",
        "paths": ["."],
        "source": "path",
        "confidence": "exact",
        "evidence": ["repository-root"],
    })];
    boundaries.extend(found);
    boundaries.sort_by(|a, b| {
        locale_compare(
            a.get("id").and_then(Value::as_str).unwrap_or(""),
            b.get("id").and_then(Value::as_str).unwrap_or(""),
        )
    });
    boundaries
}

/// boundaryForPath: самый длинный совпавший префикс; при равенстве — первый.
fn boundary_for_path(path: &str, boundaries: &[Value]) -> String {
    let mut best: Option<(String, usize)> = None;
    for boundary in boundaries {
        if boundary.get("kind").and_then(Value::as_str) == Some("repository") {
            continue;
        }
        let id = boundary.get("id").and_then(Value::as_str).unwrap_or("");
        for prefix in as_array(boundary.get("paths")) {
            let Some(prefix) = prefix.as_str() else { continue };
            if (path == prefix || path.starts_with(&format!("{prefix}/")))
                && best.as_ref().is_none_or(|(_, len)| prefix.len() > *len)
            {
                best = Some((id.to_string(), prefix.len()));
            }
        }
    }
    best.map(|(id, _)| id).unwrap_or_else(|| "boundary:repository".to_string())
}

pub struct GraphifyOptions {
    pub analysis: Option<Value>,
    pub layout: Option<Value>,
    pub scope: String,
    pub include_ambiguous: bool,
    pub max_entities: usize,
    pub max_edges: usize,
    pub max_contracts: usize,
}

impl Default for GraphifyOptions {
    fn default() -> Self {
        Self {
            analysis: None,
            layout: None,
            scope: String::new(),
            include_ambiguous: false,
            max_entities: 5000,
            max_edges: 15000,
            max_contracts: 5000,
        }
    }
}

/// /(?:^|\/)(?:test|tests|__tests__|e2e)(?:\/|$)|\.(?:test|spec)\.[^.]+$/i —
/// `.`/`[^.]` в JS исключают LineTerminator, в репо-путях недостижимо, но сохранено.
fn test_path_re() -> Regex {
    Regex::new(r"(?i)(?:^|/)(?:test|tests|__tests__|e2e)(?:/|$)|\.(?:test|spec)\.[^.\n]+$").unwrap()
}

/// Строка числа для stableId (String(10) === '10', null/undefined → '').
fn sid_num(v: Option<&Value>) -> String {
    match v {
        Some(Value::Null) | None => String::new(),
        Some(other) => js_to_string(other),
    }
}

/// extractGraphifyFacts из lib/structure-graphify.mjs.
pub fn extract_graphify_facts(raw: Option<&Value>, options: &GraphifyOptions) -> Map<String, Value> {
    let nodes = as_array(get_opt(raw, "nodes"));
    let links: &[Value] = match get_opt(raw, "links") {
        Some(Value::Array(_)) => as_array(get_opt(raw, "links")),
        _ => as_array(get_opt(raw, "edges")),
    };

    let mut method_sources: HashSet<String> = HashSet::new();
    let mut method_targets: HashSet<String> = HashSet::new();
    let mut inheritance_sources: HashSet<String> = HashSet::new();
    let mut implementation_targets: HashSet<String> = HashSet::new();
    for edge in links {
        if get(edge, "relation").and_then(Value::as_str) == Some("method") {
            method_sources.insert(js_string(edge.get("source")));
            method_targets.insert(js_string(edge.get("target")));
        }
        if get(edge, "relation").and_then(Value::as_str) == Some("inherits") {
            inheritance_sources.insert(js_string(edge.get("source")));
        }
        if get(edge, "relation").and_then(Value::as_str) == Some("implements") {
            implementation_targets.insert(js_string(edge.get("target")));
        }
    }

    // god-узел, адресованный id или файлом, однозначен; голое имя — нет:
    // неоднозначное имя понижается с предупреждением, а не метится наугад.
    let mut god_ids: HashSet<String> = HashSet::new();
    let mut god_paths: HashSet<String> = HashSet::new();
    let mut god_labels: HashSet<String> = HashSet::new();
    for god in as_array(get_opt(options.analysis.as_ref(), "gods")) {
        let label = match god {
            Value::String(s) => s.to_lowercase(),
            other => string_or_empty(other.get("label")).to_lowercase(),
        };
        if let Value::Object(o) = god {
            if let Some(id) = o.get("id").filter(|v| !matches!(v, Value::Null)) {
                god_ids.insert(js_to_string(id));
                continue;
            }
            if !label.is_empty() {
                if let Some(src) = o.get("source_file").filter(|v| js_truthy(v)) {
                    match normalize_repo_path(&js_to_string(src)) {
                        Ok(p) => {
                            god_paths.insert(format!("{p}\0{label}"));
                            continue;
                        }
                        Err(_) => { /* небезопасный путь — понижаем до голого имени ниже */ }
                    }
                }
            }
        }
        if !label.is_empty() {
            god_labels.insert(label);
        }
    }

    let mut candidates: Vec<Map<String, Value>> = Vec::new();
    let mut skipped_unsafe_path = 0usize;
    let mut unparsed_location = 0usize;
    for node in nodes {
        if matches!(node, Value::Null) || node.get("id").is_none() {
            continue;
        }
        if matches!(node.get("id"), Some(Value::Null)) {
            continue;
        }
        // node.file_type && node.file_type !== 'code' → пропуск
        let file_type = node.get("file_type");
        if file_type.is_some_and(js_truthy) && file_type.and_then(Value::as_str) != Some("code") {
            continue;
        }
        let raw_path = string_or_empty(node.get("source_file"));
        let path = match normalize_repo_path(&raw_path) {
            Ok(p) => p,
            Err(_) => {
                skipped_unsafe_path += 1;
                continue;
            }
        };
        if !path_in_scope(&path, &options.scope) {
            continue;
        }
        let graph_id = js_string(node.get("id"));
        let label = match node.get("label") {
            Some(x) if js_truthy(x) => js_to_string(x),
            _ => graph_id.clone(),
        };
        let qualified_name = match node.get("norm_label") {
            Some(x) if js_truthy(x) => x.clone(),
            _ => Value::String(label.clone()),
        };
        let kind = infer_kind(node, &method_sources, &method_targets, &inheritance_sources, &implementation_targets);
        let community = match node.get("community") {
            Some(Value::Null) | None => Value::Null,
            Some(other) => other.clone(),
        };
        // node._callable != null || node._callable_class != null → 'extracted'
        let confidence = if node.get("_callable").is_some() || node.get("_callable_class").is_some() {
            "extracted"
        } else {
            "heuristic"
        };
        let line = line_number(node.get("source_location"));
        // String(node.source_location ?? '').trim() — null/undefined → '', иначе голый String
        let location_raw = match node.get("source_location") {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_to_string(v),
        };
        if line.is_null() && !location_raw.trim().is_empty() {
            unparsed_location += 1;
        }
        let mut candidate = Map::new();
        candidate.insert("graphId".into(), json!(graph_id));
        candidate.insert("label".into(), json!(label));
        candidate.insert("qualifiedName".into(), qualified_name);
        candidate.insert("kind".into(), json!(kind));
        candidate.insert("path".into(), json!(path));
        candidate.insert("line".into(), line);
        candidate.insert("community".into(), community);
        candidate.insert("confidence".into(), json!(confidence));
        candidate.insert("evidence".into(), json!([format!("graphify:node:{graph_id}")]));
        candidates.push(candidate);
    }
    candidates.sort_by(|a, b| {
        locale_compare(
            a.get("path").and_then(Value::as_str).unwrap_or(""),
            b.get("path").and_then(Value::as_str).unwrap_or(""),
        )
        .then(
            js_number_or_zero(a.get("line"))
                .partial_cmp(&js_number_or_zero(b.get("line")))
                .unwrap_or(std::cmp::Ordering::Equal),
        )
        .then(locale_compare(
            a.get("label").and_then(Value::as_str).unwrap_or(""),
            b.get("label").and_then(Value::as_str).unwrap_or(""),
        ))
    });
    let mut warnings: Vec<String> = Vec::new();
    if skipped_unsafe_path > 0 {
        warnings.push(format!("{skipped_unsafe_path} node(s) skipped: unsafe source_file path"));
    }
    if unparsed_location > 0 {
        warnings.push(format!("{unparsed_location} node(s) kept without a line: unparsed source_location"));
    }
    let truncated_entities = candidates.len() > options.max_entities;
    if truncated_entities {
        warnings.push(format!("entities truncated at {}", options.max_entities));
    }
    let kept = &candidates[..candidates.len().min(options.max_entities)];

    // имена среди выживших: голое имя god-узла однозначно только при count === 1
    let mut label_counts: HashMap<String, usize> = HashMap::new();
    for candidate in kept {
        let key = candidate
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        *label_counts.entry(key).or_default() += 1;
    }
    let mut ambiguous_gods: HashMap<String, usize> = HashMap::new();

    let kept_paths: Vec<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        kept.iter()
            .filter_map(|c| c.get("path").and_then(Value::as_str).map(String::from))
            .filter(|p| seen.insert(p.clone()))
            .collect()
    };
    let boundaries = boundary_candidates(&kept_paths, options.layout.as_ref());

    let mut entities: Vec<Value> = Vec::new();
    let mut entity_by_graph_id: HashMap<String, Value> = HashMap::new();
    for candidate in kept {
        let path = string_or_empty(candidate.get("path"));
        let observed_id = stable_id("entity", &[
            path.clone(),
            string_or_empty(candidate.get("qualifiedName")),
            string_or_empty(candidate.get("kind")),
            sid_num(candidate.get("line")),
        ]);
        let boundary_id = boundary_for_path(&path, &boundaries);
        let graph_id = string_or_empty(candidate.get("graphId"));
        let label_lower = candidate
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let is_god_node = if god_ids.contains(&graph_id) {
            true
        } else if god_paths.contains(&format!("{path}\0{label_lower}")) {
            true
        } else if !god_labels.contains(&label_lower) {
            false
        } else {
            let count = label_counts.get(&label_lower).copied().unwrap_or(0);
            if count > 1 {
                // имя встречается в нескольких файлах — метить наугад нельзя, понижаем
                ambiguous_gods.insert(label_lower.clone(), count);
                false
            } else {
                true
            }
        };
        let mut entity = Map::new();
        entity.insert("observedId".into(), json!(observed_id));
        entity.insert("lineageId".into(), json!(observed_id));
        entity.insert("boundaryId".into(), json!(boundary_id));
        entity.insert("label".into(), candidate.get("label").cloned().unwrap_or(Value::Null));
        entity.insert("qualifiedName".into(), candidate.get("qualifiedName").cloned().unwrap_or(Value::Null));
        entity.insert("kind".into(), candidate.get("kind").cloned().unwrap_or(Value::Null));
        entity.insert("path".into(), json!(path));
        entity.insert("line".into(), candidate.get("line").cloned().unwrap_or(Value::Null));
        entity.insert("endLine".into(), Value::Null);
        entity.insert("signature".into(), Value::Null);
        entity.insert("community".into(), candidate.get("community").cloned().unwrap_or(Value::Null));
        entity.insert("isGodNode".into(), json!(is_god_node));
        entity.insert("confidence".into(), candidate.get("confidence").cloned().unwrap_or(Value::Null));
        entity.insert("evidence".into(), candidate.get("evidence").cloned().unwrap_or(Value::Null));
        entity.insert(
            "externalIds".into(),
            json!({ "graphify": candidate.get("graphId").and_then(Value::as_str).unwrap_or("") }),
        );
        let value = Value::Object(entity);
        entity_by_graph_id.insert(
            candidate.get("graphId").and_then(Value::as_str).unwrap_or("").to_string(),
            value.clone(),
        );
        entities.push(value);
    }

    let mut ambiguous_sorted: Vec<(String, usize)> = ambiguous_gods.into_iter().collect();
    ambiguous_sorted.sort_by(|a, b| locale_compare(&a.0, &b.0));
    for (label, count) in ambiguous_sorted {
        warnings.push(format!("god node label is ambiguous: {label} ({count} entities)"));
    }

    let mut dependency_candidates: Vec<Value> = Vec::new();
    let mut seen_dependency_ids: HashSet<String> = HashSet::new();
    for (i, edge) in links.iter().enumerate() {
        if matches!(edge, Value::Null) {
            continue;
        }
        let relation = string_or_empty(edge.get("relation"));
        if !DEP_RELATIONS.contains(&relation.as_str()) {
            continue;
        }
        let from = match entity_by_graph_id.get(&js_string(edge.get("source"))) {
            Some(v) => v.clone(),
            None => continue,
        };
        let to = match entity_by_graph_id.get(&js_string(edge.get("target"))) {
            Some(v) => v.clone(),
            None => continue,
        };
        let conf = edge_confidence(edge.get("confidence"));
        if conf == "ambiguous" && !options.include_ambiguous {
            continue;
        }
        let from_id = from.get("observedId").and_then(Value::as_str).unwrap_or("");
        let to_id = to.get("observedId").and_then(Value::as_str).unwrap_or("");
        let cross_community = match (from.get("community"), to.get("community")) {
            (Some(a), Some(b)) => !matches!(a, Value::Null) && !matches!(b, Value::Null) && a != b,
            _ => false,
        };
        let dependency_id = stable_id("edge", &[from_id.into(), to_id.into(), relation.clone()]);
        // одинаковые рёбра дают один и тот же stableId; без дедупа удваивают fanIn
        if !seen_dependency_ids.insert(dependency_id.clone()) {
            continue;
        }
        dependency_candidates.push(json!({
            "id": dependency_id,
            "from": from_id,
            "to":to_id,
            "relation": relation,
            "crossBoundary": from.get("boundaryId") != to.get("boundaryId"),
            "crossCommunity": cross_community,
            "confidence": conf,
            "evidence": [format!("graphify:edge:{i}")],
        }));
    }
    dependency_candidates.sort_by(|a, b| {
        locale_compare(
            a.get("id").and_then(Value::as_str).unwrap_or(""),
            b.get("id").and_then(Value::as_str).unwrap_or(""),
        )
    });
    let truncated_edges = dependency_candidates.len() > options.max_edges;
    if truncated_edges {
        warnings.push(format!("dependencies truncated at {}", options.max_edges));
    }
    let dependencies: Vec<Value> =
        dependency_candidates[..dependency_candidates.len().min(options.max_edges)].to_vec();

    let mut contracts: Vec<Value> = Vec::new();
    let mut contract_keys: HashSet<String> = HashSet::new();
    // единый формат ключа для всех источников контрактов, иначе dedupe мёртвый
    let contract_key =
        |entity_ref: &str, kind: &str, target_ref: &str| format!("{entity_ref}:{kind}:{target_ref}");
    let signature_by_id: HashMap<String, Value> = entities
        .iter()
        .map(|e| {
            (
                e.get("observedId").and_then(Value::as_str).unwrap_or("").to_string(),
                e.get("signature").cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    for edge in &dependencies {
        let relation = edge.get("relation").and_then(Value::as_str).unwrap_or("");
        if !["implements", "inherits"].contains(&relation) {
            continue;
        }
        let from_id = edge.get("from").and_then(Value::as_str).unwrap_or("");
        let to_id = edge.get("to").and_then(Value::as_str).unwrap_or("");
        contract_keys.insert(contract_key(from_id, relation, to_id));
        contracts.push(json!({
            "id": stable_id("contract", &[relation.into(), from_id.into(), to_id.into()]),
            "entityRef": from_id,
            "kind": relation,
            "targetRef": to_id,
            "signature": signature_by_id.get(from_id).cloned().unwrap_or(Value::Null),
            "confidence": edge.get("confidence").cloned().unwrap_or(Value::Null),
            "evidence": edge.get("evidence").cloned().unwrap_or(Value::Null),
        }));
    }
    for entity in &entities {
        let kind = entity.get("kind").and_then(Value::as_str).unwrap_or("");
        if !["function", "method", "class_like", "interface"].contains(&kind) {
            continue;
        }
        let observed_id = entity.get("observedId").and_then(Value::as_str).unwrap_or("");
        if !contract_keys.insert(contract_key(observed_id, "entrypoint", "")) {
            continue;
        }
        let mut evidence = as_array(entity.get("evidence")).to_vec();
        evidence.push(json!("graphify:callable-contract-candidate"));
        contracts.push(json!({
            "id": stable_id("contract", &["entrypoint".into(), observed_id.into()]),
            "entityRef": observed_id,
            "kind": "entrypoint",
            "targetRef": Value::Null,
            "signature": entity.get("signature").cloned().unwrap_or(Value::Null),
            "confidence": "heuristic",
            "evidence": evidence,
        }));
    }
    // при обрезке первыми уходят эвристические entrypoint, доказанные контракты остаются
    let contract_rank = |v: &Value| {
        if v.get("kind").and_then(Value::as_str) == Some("entrypoint") {
            1
        } else {
            0
        }
    };
    contracts.sort_by(|a, b| {
        contract_rank(a)
            .cmp(&contract_rank(b))
            .then(locale_compare(
                a.get("id").and_then(Value::as_str).unwrap_or(""),
                b.get("id").and_then(Value::as_str).unwrap_or(""),
            ))
    });
    let truncated_contracts = contracts.len() > options.max_contracts;
    if truncated_contracts {
        warnings.push(format!("contracts truncated at {}", options.max_contracts));
    }
    let mut kept_contracts = contracts[..contracts.len().min(options.max_contracts)].to_vec();
    kept_contracts.sort_by(|a, b| {
        locale_compare(
            a.get("id").and_then(Value::as_str).unwrap_or(""),
            b.get("id").and_then(Value::as_str).unwrap_or(""),
        )
    });

    let test_re = test_path_re();
    let mut test_cases: Vec<Value> = entities
        .iter()
        .filter(|entity| {
            let path = entity.get("path").and_then(Value::as_str).unwrap_or("");
            let kind = entity.get("kind").and_then(Value::as_str).unwrap_or("");
            test_re.is_match(path) && ["function", "method"].contains(&kind)
        })
        .map(|entity| {
            let path = entity.get("path").and_then(Value::as_str).unwrap_or("");
            let label = entity.get("label").and_then(Value::as_str).unwrap_or("");
            // entity.line || 1 (0/null → 1)
            let line = js_number_or_zero(entity.get("line"));
            json!({
                "id": stable_id("test", &[path.into(), sid_num(entity.get("line")), label.into()]),
                "name": label,
                "path": path,
                "line": if line == 0.0 { json!(1) } else { json!(line as i64) },
                "framework": Value::Null,
                "targetContractRefs": Value::Array(vec![]),
                "assertions": Value::Array(vec![]),
                "confidence": entity.get("confidence").cloned().unwrap_or(Value::Null),
                "evidence": entity.get("evidence").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    test_cases.sort_by(|a, b| {
        locale_compare(
            a.get("id").and_then(Value::as_str).unwrap_or(""),
            b.get("id").and_then(Value::as_str).unwrap_or(""),
        )
    });

    let mut out = Map::new();
    out.insert("boundaries".into(), json!(boundaries));
    out.insert("entities".into(), json!(entities));
    out.insert("contracts".into(), json!(kept_contracts));
    out.insert("dependencies".into(), json!(dependencies));
    out.insert("testCases".into(), json!(test_cases));
    out.insert("warnings".into(), json!(warnings));
    out.insert(
        "truncated".into(),
        json!(truncated_entities || truncated_edges || truncated_contracts),
    );
    out.insert(
        "capabilities".into(),
        json!({
            "symbols": "inferred",
            "exactKinds": false,
            "sourceRanges": false,
            "signatures": false,
            "references": false,
            "implementations": true,
            "imports": true,
            "calls": true,
            "topology": true,
        }),
    );
    out
}

/// buildStructureViews: boundaryTree/objectTree/functionTree/dependencyGraph.
pub fn build_structure_views(facts: Option<&Value>) -> Map<String, Value> {
    let entities = as_array(get_opt(facts, "entities"));
    let dependencies = as_array(get_opt(facts, "dependencies"));

    let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
    for edge in dependencies {
        let relation = edge.get("relation").and_then(Value::as_str).unwrap_or("");
        if !["contains", "method"].contains(&relation) {
            continue;
        }
        let from = edge.get("from").and_then(Value::as_str).unwrap_or("");
        let to = edge.get("to").and_then(Value::as_str).unwrap_or("");
        children_by_parent.entry(from.to_string()).or_default().push(to.to_string());
    }
    let by_id: HashMap<String, &Value> = entities
        .iter()
        .map(|e| (e.get("observedId").and_then(Value::as_str).unwrap_or("").to_string(), e))
        .collect();
    let node = |entity: &Value| {
        json!({
            "id": entity.get("observedId").cloned().unwrap_or(Value::Null),
            "label": entity.get("label").cloned().unwrap_or(Value::Null),
            "kind": entity.get("kind").cloned().unwrap_or(Value::Null),
            "path": entity.get("path").cloned().unwrap_or(Value::Null),
            "line": entity.get("line").cloned().unwrap_or(Value::Null),
        })
    };

    let mut object_tree: Vec<Value> = entities
        .iter()
        .filter(|e| {
            matches!(
                e.get("kind").and_then(Value::as_str),
                Some("class_like") | Some("interface")
            )
        })
        .map(|entity| {
            let observed_id = entity.get("observedId").and_then(Value::as_str).unwrap_or("");
            let mut children: Vec<Value> = children_by_parent
                .get(observed_id)
                .map(|ids| ids.iter().filter_map(|id| by_id.get(id).copied()).map(node).collect())
                .unwrap_or_default();
            children.sort_by(|a, b| {
                locale_compare(
                    a.get("label").and_then(Value::as_str).unwrap_or(""),
                    b.get("label").and_then(Value::as_str).unwrap_or(""),
                )
            });
            let mut entry = node(entity).as_object().cloned().unwrap_or_default();
            entry.insert("bestEffort".into(), Value::Bool(true));
            entry.insert("children".into(), Value::Array(children));
            Value::Object(entry)
        })
        .collect();
    object_tree.sort_by(|a, b| {
        locale_compare(
            a.get("path").and_then(Value::as_str).unwrap_or(""),
            b.get("path").and_then(Value::as_str).unwrap_or(""),
        )
        .then(locale_compare(
            a.get("label").and_then(Value::as_str).unwrap_or(""),
            b.get("label").and_then(Value::as_str).unwrap_or(""),
        ))
    });

    // [...new Set(paths)].sort() — побайтовый порядок
    let mut paths: Vec<String> = Vec::new();
    for entity in entities {
        let path = entity.get("path").and_then(Value::as_str).unwrap_or("");
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_string());
        }
    }
    paths.sort();
    let function_tree: Vec<Value> = paths
        .iter()
        .map(|path| {
            let mut children: Vec<Value> = entities
                .iter()
                .filter(|e| {
                    e.get("path").and_then(Value::as_str) == Some(path.as_str())
                        && matches!(
                            e.get("kind").and_then(Value::as_str),
                            Some("function") | Some("method")
                        )
                })
                .map(node)
                .collect();
            children.sort_by(|a, b| {
                js_number_or_zero(a.get("line"))
                    .partial_cmp(&js_number_or_zero(b.get("line")))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            json!({ "path": path, "children": children })
        })
        .filter(|entry| entry["children"].as_array().is_some_and(|a| !a.is_empty()))
        .collect();

    let boundary_tree: Vec<Value> = as_array(get_opt(facts, "boundaries"))
        .iter()
        .map(|boundary| {
            json!({
                "id": boundary.get("id").cloned().unwrap_or(Value::Null),
                "name": boundary.get("name").cloned().unwrap_or(Value::Null),
                "kind": boundary.get("kind").cloned().unwrap_or(Value::Null),
                "paths": boundary.get("paths").cloned().unwrap_or(Value::Null),
                "entityRefs": entities
                    .iter()
                    .filter(|e| e.get("boundaryId") == boundary.get("id"))
                    .map(|e| e.get("observedId").cloned().unwrap_or(Value::Null))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    let mut out = Map::new();
    out.insert("boundaryTree".into(), json!(boundary_tree));
    out.insert("objectTree".into(), json!(object_tree));
    out.insert("functionTree".into(), json!(function_tree));
    out.insert(
        "dependencyGraph".into(),
        json!(dependencies
            .iter()
            .map(|e| e.get("id").cloned().unwrap_or(Value::Null))
            .collect::<Vec<_>>()),
    );
    out
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Value {
        json!({
            "nodes": [
                {"id": "api", "label": "validate()", "file_type": "code", "source_file": "src/auth.ts", "source_location": "L10", "_callable": true, "community": 1},
                {"id": "caller1", "label": "login()", "file_type": "code", "source_file": "src/login.ts", "source_location": "L4", "_callable": true, "community": 2},
                {"id": "iface", "label": "TokenValidator", "file_type": "code", "source_file": "src/contracts.ts", "source_location": "L1", "_callable_class": true, "community": 1},
                {"id": "impl", "label": "JwtValidator", "file_type": "code", "source_file": "src/auth.ts", "source_location": "L2", "_callable_class": true, "community": 1},
            ],
            "links": [
                {"source": "caller1", "target": "api", "relation": "calls", "confidence": "EXTRACTED"},
                {"source": "impl", "target": "iface", "relation": "implements", "confidence": "EXTRACTED"},
            ],
        })
    }

    #[test]
    fn extracts_entities_contracts_boundaries() {
        let facts = extract_graphify_facts(Some(&graph()), &GraphifyOptions {
            layout: Some(json!({"appDir": "src", "layerDirs": ["auth"]})),
            max_entities: 100,
            max_edges: 100,
            ..Default::default()
        });
        assert_eq!(facts["entities"].as_array().unwrap().len(), 4);
        assert!(facts["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["relation"] == "implements"));
        assert!(facts["contracts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "implements"));
        assert!(facts["contracts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "entrypoint"));
        let api = facts["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["label"] == "validate()")
            .unwrap();
        assert_eq!(api["kind"], json!("function"));
    }

    #[test]
    fn infers_kinds_from_relations() {
        let raw = json!({
            "nodes": [
                {"id": "obj", "label": "obj", "file_type": "code", "source_file": "src/o.ts"},
                {"id": "m1", "label": ".hidden", "file_type": "code", "source_file": "src/o.ts"},
                {"id": "cls", "label": "Cls", "file_type": "code", "source_file": "src/o.ts"},
            ],
            "links": [
                {"source": "obj", "target": "m1", "relation": "method"},
                {"source": "cls", "target": "obj", "relation": "inherits"},
            ],
        });
        let facts = extract_graphify_facts(Some(&raw), &GraphifyOptions::default());
        let kind = |label: &str| {
            facts["entities"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["label"] == label)
                .unwrap()["kind"]
                .clone()
        };
        assert_eq!(kind("obj"), json!("class_like"));
        assert_eq!(kind(".hidden"), json!("method"));
        assert_eq!(kind("Cls"), json!("class_like"));
    }

    #[test]
    fn skips_non_code_and_unsafe_paths() {
        let raw = json!({
            "nodes": [
                {"id": "doc", "label": "doc", "file_type": "doc", "source_file": "src/doc.md"},
                {"id": "esc", "label": "esc", "file_type": "code", "source_file": "../escape.ts"},
                {"id": "ok", "label": "ok", "file_type": "code", "source_file": "src/ok.ts"},
            ],
            "links": [],
        });
        let facts = extract_graphify_facts(Some(&raw), &GraphifyOptions::default());
        assert_eq!(facts["entities"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn entrypoint_dedupes_duplicate_observed_ids() {
        // два graph-узла с одинаковым observedId (дубликаты в выгрузке graphify)
        let raw = json!({
            "nodes": [
                {"id": "dup1", "label": "run()", "file_type": "code", "source_file": "src/run.ts", "source_location": "L10", "_callable": true},
                {"id": "dup2", "label": "run()", "file_type": "code", "source_file": "src/run.ts", "source_location": "L10", "_callable": true},
            ],
            "links": [],
        });
        let facts = extract_graphify_facts(Some(&raw), &GraphifyOptions::default());
        let entrypoints = facts["contracts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["kind"] == "entrypoint")
            .count();
        assert_eq!(entrypoints, 1);
    }

    #[test]
    fn truncation_is_sorted_and_reported() {
        let facts = extract_graphify_facts(Some(&graph()), &GraphifyOptions {
            max_entities: 2,
            max_edges: 1,
            ..Default::default()
        });
        assert_eq!(facts["entities"].as_array().unwrap().len(), 2);
        // оба ребра ведут в обрезанные сущности — в выживших не остаётся ни одного
        assert_eq!(facts["dependencies"].as_array().unwrap().len(), 0);
        assert_eq!(facts["truncated"], json!(true));
        assert!(facts["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("entities truncated at 2")));
    }

    #[test]
    fn builds_views_with_children() {
        let facts = json!({
            "entities": [
                {"observedId": "c1", "label": "Cls", "kind": "class_like", "path": "src/a.ts", "line": 1, "boundaryId": "boundary:repository"},
                {"observedId": "f1", "label": "a()", "kind": "function", "path": "src/a.ts", "line": 5, "boundaryId": "boundary:repository"},
            ],
            "dependencies": [
                {"id": "e1", "from": "c1", "to": "f1", "relation": "contains"},
            ],
            "boundaries": [{"id": "boundary:repository", "name": "repository", "kind": "repository", "paths": ["."]}],
        });
        let views = build_structure_views(Some(&facts));
        let object_tree = views["objectTree"].as_array().unwrap();
        assert_eq!(object_tree.len(), 1);
        assert_eq!(object_tree[0]["children"].as_array().unwrap().len(), 1);
        assert_eq!(views["dependencyGraph"], json!(["e1"]));
        assert_eq!(views["boundaryTree"][0]["entityRefs"].as_array().unwrap().len(), 2);
    }
}
