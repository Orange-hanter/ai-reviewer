//! Бинарный JSON-контракт порта lib/structure-mcp.mjs (Фаза 2, шаг 5).
//!
//! Вход (stdin):  {"op": "<имя операции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате JS-возврата (null вместо undefined).
//! Ошибки:        exit code 2 + {"error": "..."} на stderr.
//!
//! Операции:
//!   handleRequest         {request} → ответное JSON-RPC-сообщение или null
//!                         (notifications/initialized). Чистые методы:
//!                         initialize / ping / tools/list / method-not-found /
//!                         unknown-tool. tools/call structure_context НЕ
//!                         обслуживается здесь: fs-I/O живёт в JS-обёртке
//!                         (см. шапку structure_mcp.rs), серверный stdio-цикл
//!                         с Content-Length-фреймингом — в
//!                         bin/pr-review-structure-mcp.mjs.
//!   buildStructureContext {factsText, assessmentText, factsPath,
//!                          assessmentPath, args} → structure context
//!                         (пин-проверка → parseStructurePayload обоих текстов
//!                         → проверка kind → buildStructureContext с клэмпингом
//!                         аргументов из inputSchema).
//!
//! (аргументы шлются вложенным объектом args, как у JS-функций)

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ai_reviewer::structure_mcp::{build_structure_context_op, handle_request};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("чтение stdin")?;
    let v: Value = serde_json::from_str(&input).context("вход не является JSON")?;
    let obj = v.as_object().context("вход должен быть JSON-объектом")?;
    let op = obj.get("op").and_then(Value::as_str).unwrap_or("");

    let out = match op {
        "handleRequest" => {
            let request = arg(&v, "request");
            handle_request(&request)?
        }
        "buildStructureContext" => build_structure_context_op(obj)?,
        other => anyhow::bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// Аргумент как значение (None → Null): поля в JSON не бывают undefined.
fn arg(v: &Value, name: &str) -> Value {
    v.as_object()
        .and_then(|o| o.get(name))
        .cloned()
        .unwrap_or(Value::Null)
}
