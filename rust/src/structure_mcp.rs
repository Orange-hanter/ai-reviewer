//! Порт lib/structure-mcp.mjs (Фаза 2, шаг 5): MCP-style JSON-RPC-ish диспетчер
//! structure_context — дескриптор тулза, разбор запросов (initialize / ping /
//! tools/list / notifications / method-not-found) и построение structure context.
//! Сама сборка контекста (BFS по зависимостям от scope-seeds, усечение бюджетом
//! токенов/байт, prune висячих ссылок) — общий доменный путь
//! `structure_context::build_structure_context` (порт lib/structure-context.mjs,
//! T18); здесь — только клэмпинг аргументов тулза, как в callStructureContext.
//! Пункты «паритет» ниже про BFS и усечение описывают поведение того модуля.
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
//!   boundedInt — Number(value), нецелое/NaN/Infinity → fallback, иначе
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

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_number, js_to_string, js_truthy};
use crate::structure_evidence::get;
use crate::structure_context::{self, ContextRequest};
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

/// Порт callStructureContext/buildStructureContext из lib/structure-mcp.mjs:
/// facts/assessment — уже распарсенные и валидированные пейлоады, args — сырые
/// аргументы тулза. Здесь — только клэмпинг из собственного inputSchema
/// (boundedList/boundedInt); сама сборка контекста — общий доменный путь
/// [`crate::structure_context::build_structure_context`], как в JS, где
/// structure-mcp зовёт buildStructureContext из lib/structure-context.mjs.
/// limits — отдельный параметр: callStructureContext собирает вызов литералом и
/// НЕ пробрасывает `limits` из чужих аргументов, op передаёт None (иначе клиент
/// смог бы перекрыть потолки, что JS-стороне недоступно).
pub fn build_structure_context(
    facts: &Value,
    assessment: &Value,
    args: &Value,
    limits: Option<&Map<String, Value>>,
) -> Result<Value> {
    let request = ContextRequest {
        head_sha: get(args, "headSha").cloned(),
        snapshot_fingerprint: get(args, "snapshotFingerprint").cloned(),
        scope_refs: bounded_list(get(args, "scopeRefs"), MAX_LIST_ITEMS),
        task_ref: match get(args, "taskRef") {
            Some(v) if js_truthy(v) => v.clone(),
            _ => Value::Null,
        },
        contract_refs: bounded_list(get(args, "contractRefs"), MAX_LIST_ITEMS),
        token_budget: bounded_int(get(args, "tokenBudget"), TOKEN_BUDGET_RANGE, DEFAULT_TOKEN_BUDGET),
        dependency_depth: bounded_int(
            get(args, "dependencyDepth"),
            DEPENDENCY_DEPTH_RANGE,
            DEFAULT_DEPENDENCY_DEPTH,
        ),
        limits: limits.cloned().unwrap_or_default(),
    };
    structure_context::build_structure_context(facts, assessment, &request)
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
    use std::collections::HashSet;

    use crate::structure_evidence::{js_default_sort, value_key};

    fn key_of(v: Option<&Value>) -> String {
        value_key(v.unwrap_or(&Value::Null))
    }

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
