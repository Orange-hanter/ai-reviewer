//! Подкоманда `pr-review structure-mcp` — порт bin/pr-review-structure-mcp.mjs
//! (RFC 20, T23): stdio-сервер structure-MCP поверх транспорта `mcp_stdio`.
//!
//! Слои как в JS:
//! 1. корень: `STRUCTURE_MCP_ROOT`, иначе `REVIEW_MCP_ROOT` (пустое значение и
//!    неподставленный плейсхолдер `${…}` пропускаются); несуществующий — сообщение
//!    `ai-reviewer MCP: STRUCTURE_MCP_ROOT не существует: <abs>` и exit 1, иначе chdir;
//! 2. handshake без домена (lib/structure-mcp-handshake.mjs, RFC 10): initialize,
//!    notifications/initialized, ping, tools/list, resources/list, prompts/list,
//!    logging/setLevel;
//! 3. домен (handleStructureMcpRequest из lib/structure-mcp.mjs): tools/call
//!    structure_context с чтением facts/assessment внутри корня, прочее — в
//!    `structure_mcp::handle_request`; ошибки → -32601 для `method not found…`,
//!    иначе -32602.
//!
//! Чтение пейлоадов, как в JS, ограничено корнем `STRUCTURE_MCP_ROOT || cwd` —
//! это сырое значение env, а не результат шага 1: при плейсхолдере в
//! STRUCTURE_MCP_ROOT и рабочем REVIEW_MCP_ROOT JS (и этот порт) ищут корень по
//! буквальной строке `${…}`, и чтение падает. Поведение сохранено ради паритета.
//!
//! Отклонения: тексты системных ошибок чтения (ENOENT и т.п.) — от Rust std
//! (`No such file or directory (os error 2)`), а не от Node (`ENOENT: no such
//! file or directory, realpath '…'`); код ответа (-32602) тот же.

use std::path::Path;

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::findings::js_truthy;
use crate::mcp_stdio::{run_mcp_stdio, Notify};
use crate::report::js_json_stringify;
use crate::structure_mcp::{build_structure_context_op, handle_request, structure_context_tool};
use crate::util::{js_resolve, resolve_contained_path};

/// Аргументов нет: конфигурация — только через env, как у JS-входа.
#[derive(Debug, clap::Args)]
pub struct Args {}

/// usableRoot: обрезанное непустое значение без неподставленного `${…}`.
pub(crate) fn usable_root(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() || has_placeholder(trimmed) {
        return None;
    }
    Some(trimmed.to_string())
}

/// `/\$\{[A-Za-z0-9_:.-]+\}/`.
fn has_placeholder(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'$' && b[i + 1] == b'{' {
            let mut j = i + 2;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b"_:.-".contains(&b[j])) {
                j += 1;
            }
            if j > i + 2 && j < b.len() && b[j] == b'}' {
                return true;
            }
        }
        i += 1;
    }
    false
}

pub fn run(_args: Args) -> Result<i32> {
    let root = usable_root(std::env::var("STRUCTURE_MCP_ROOT").ok())
        .or_else(|| usable_root(std::env::var("REVIEW_MCP_ROOT").ok()));
    if let Some(root) = root {
        let abs = js_resolve(&std::env::current_dir()?, &root);
        if !abs.exists() {
            eprintln!("ai-reviewer MCP: STRUCTURE_MCP_ROOT не существует: {}", abs.display());
            return Ok(1);
        }
        // JS: `process.chdir` бросает (ENOTDIR, EACCES) мимо fail-open → exit 1. Через `?`
        // ошибка ушла бы в диспетчер и превратилась в exit 0: клиент не увидел бы сбой конфигурации.
        if let Err(e) = std::env::set_current_dir(&abs) {
            eprintln!("ai-reviewer MCP: не удалось перейти в STRUCTURE_MCP_ROOT {}: {e}", abs.display());
            return Ok(1);
        }
    }
    run_mcp_stdio(|request: &Value, _notify: &Notify<'_>| Ok(handle(request)))?;
    Ok(0)
}

/// handleRequest из bin/pr-review-structure-mcp.mjs: handshake, затем домен.
pub fn handle(request: &Value) -> Option<Value> {
    if let Some(response) = handshake(request) {
        return response;
    }
    domain(request)
}

fn request_id(request: &Value) -> Value {
    match request.get("id") {
        Some(Value::Null) | None => Value::Null,
        Some(v) => v.clone(),
    }
}

/// handleStructureMcpHandshake: `None` — метод не handshake-а (идёт в домен),
/// `Some(None)` — notification без ответа.
pub fn handshake(request: &Value) -> Option<Option<Value>> {
    let id = request_id(request);
    let method = request.get("method").and_then(Value::as_str)?;
    let result = match method {
        "initialize" => {
            let pv = request
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .filter(|v| js_truthy(v))
                .cloned()
                .unwrap_or_else(|| json!("2025-06-18"));
            json!({
                "protocolVersion": pv,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "ai-reviewer-structure", "version": "1.0.0" },
            })
        }
        "notifications/initialized" => return Some(None),
        "ping" | "logging/setLevel" => json!({}),
        "tools/list" => json!({ "tools": [structure_context_tool()] }),
        "resources/list" => json!({ "resources": [] }),
        "prompts/list" => json!({ "prompts": [] }),
        _ => return None,
    };
    Some(Some(json!({ "jsonrpc": "2.0", "id": id, "result": result })))
}

fn domain(request: &Value) -> Option<Value> {
    let id = request_id(request);
    let outcome = if request.get("method").and_then(Value::as_str) == Some("tools/call") {
        tools_call(request).map(|context| {
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": js_json_pretty(&context) }],
                    "structuredContent": context,
                },
            }))
        })
    } else {
        handle_request(request).map(|r| if r.is_null() { None } else { Some(r) })
    };
    match outcome {
        Ok(response) => response,
        Err(error) => {
            let message = error.to_string();
            let code = if message.starts_with("method not found") { -32601 } else { -32602 };
            Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        }
    }
}

/// `String(value)` для аргумента пути (undefined → "undefined").
fn js_string_arg(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(v) => crate::findings::js_to_string(v),
    }
}

fn tools_call(request: &Value) -> Result<Value> {
    let params = request.get("params");
    let name = params.and_then(|p| p.get("name"));
    if name.and_then(Value::as_str) != Some(structure_context_tool()["name"].as_str().unwrap_or_default()) {
        anyhow::bail!("unknown tool: {}", js_string_arg(name));
    }
    let args = params
        .and_then(|p| p.get("arguments"))
        .filter(|v| js_truthy(v))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let truthy = |k: &str| args.get(k).is_some_and(js_truthy);
    // пин-проверка до чтения файлов — порядок ошибок как в JS
    if !truthy("headSha") && !truthy("snapshotFingerprint") {
        anyhow::bail!("headSha or snapshotFingerprint is required");
    }
    let facts_text = read_payload_text(&js_string_arg(args.get("factsPath")))?;
    let assessment_text = read_payload_text(&js_string_arg(args.get("assessmentPath")))?;
    let mut op = Map::new();
    op.insert("factsText".into(), Value::String(facts_text));
    op.insert("assessmentText".into(), Value::String(assessment_text));
    op.insert("factsPath".into(), args.get("factsPath").cloned().unwrap_or(Value::Null));
    op.insert("assessmentPath".into(), args.get("assessmentPath").cloned().unwrap_or(Value::Null));
    op.insert("args".into(), args);
    build_structure_context_op(&op)
}

/// readPayloadText: путь внутри `STRUCTURE_MCP_ROOT || cwd`, realpath внутри,
/// текст — UTF-8 с заменой битых последовательностей (как readFile 'utf8').
fn read_payload_text(target: &str) -> Result<String> {
    let root = std::env::var("STRUCTURE_MCP_ROOT").ok().filter(|s| !s.is_empty());
    let absolute = resolve_contained_path(target, root.as_deref().map(Path::new), true)?;
    let bytes = std::fs::read(absolute)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `JSON.stringify(value, null, 2)`.
pub fn js_json_pretty(v: &Value) -> String {
    let mut out = String::new();
    pretty_into(v, 0, &mut out);
    out
}

fn pretty_into(v: &Value, depth: usize, out: &mut String) {
    let pad = |out: &mut String, d: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(d));
    };
    match v {
        Value::Array(a) if !a.is_empty() => {
            out.push('[');
            for (i, el) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pad(out, depth + 1);
                pretty_into(el, depth + 1, out);
            }
            pad(out, depth);
            out.push(']');
        }
        Value::Object(m) if !m.is_empty() => {
            out.push('{');
            for (i, (k, val)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pad(out, depth + 1);
                js_json_stringify(&Value::String(k.clone()), out);
                out.push_str(": ");
                pretty_into(val, depth + 1, out);
            }
            pad(out, depth);
            out.push('}');
        }
        _ => js_json_stringify(v, out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_root_skips_placeholders() {
        assert_eq!(usable_root(Some("  /x  ".into())), Some("/x".into()));
        assert_eq!(usable_root(Some("   ".into())), None);
        assert_eq!(usable_root(Some("${workspaceFolder}".into())), None);
        assert_eq!(usable_root(Some("/a/${env:HOME}/b".into())), None);
        assert_eq!(usable_root(Some("/a/${}/b".into())), Some("/a/${}/b".into()));
        assert_eq!(usable_root(None), None);
    }

    #[test]
    fn pretty_matches_json_stringify_indent_2() {
        let v = json!({ "a": [], "b": {}, "c": [1, { "d": "x" }], "e": null });
        assert_eq!(
            js_json_pretty(&v),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"d\": \"x\"\n    }\n  ],\n  \"e\": null\n}"
        );
    }

    #[test]
    fn handshake_and_domain_routing() {
        let r = handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" })).unwrap();
        assert_eq!(r["result"], json!({ "resources": [] }));
        assert!(handle(&json!({ "method": "notifications/initialized" })).is_none());
        let r = handle(&json!({ "id": 2, "method": "nope" })).unwrap();
        assert_eq!(r["error"], json!({ "code": -32601, "message": "method not found: nope" }));
        let r = handle(&json!({ "id": 3, "method": "tools/call", "params": { "name": "x" } })).unwrap();
        assert_eq!(r["error"], json!({ "code": -32602, "message": "unknown tool: x" }));
        let r = handle(&json!({ "id": 4, "method": "tools/call", "params": { "name": "structure_context", "arguments": { "factsPath": "f" } } })).unwrap();
        assert_eq!(r["error"]["message"], json!("headSha or snapshotFingerprint is required"));
    }
}
