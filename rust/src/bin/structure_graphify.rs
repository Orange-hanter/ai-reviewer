//! Бинарный JSON-контракт порта lib/structure-graphify.mjs (Фаза 2, шаг 2).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате JS-возврата (null вместо undefined).
//! Ошибки:        exit code 2 + {"error": "..."} на stderr.
//!
//! Операции:
//!   extractGraphifyFacts {raw, analysis?, layout?, scope?, includeAmbiguous?,
//!                         maxEntities?, maxEdges?} → facts
//!   buildStructureViews  {facts}                  → views

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ai_reviewer::structure_graphify::{build_structure_views, extract_graphify_facts, GraphifyOptions};

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
        "extractGraphifyFacts" => {
            let raw = obj.get("raw").cloned().unwrap_or(Value::Null);
            let options = GraphifyOptions {
                analysis: obj.get("analysis").cloned(),
                layout: obj.get("layout").cloned(),
                scope: match obj.get("scope") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    Some(_) => anyhow::bail!("аргумент 'scope' должен быть строкой"),
                },
                include_ambiguous: matches!(obj.get("includeAmbiguous"), Some(Value::Bool(true))),
                max_entities: arg_usize(obj, "maxEntities", 5000)?,
                max_edges: arg_usize(obj, "maxEdges", 15000)?,
            };
            json!(extract_graphify_facts(Some(&raw), &options))
        }
        "buildStructureViews" => {
            let facts = obj.get("facts").cloned().unwrap_or(Value::Null);
            json!(build_structure_views(Some(&facts)))
        }
        other => anyhow::bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn arg_usize(obj: &serde_json::Map<String, Value>, name: &str, default: usize) -> Result<usize> {
    match obj.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(n) => Ok(n
            .as_u64()
            .with_context(|| format!("аргумент '{name}' должен быть числом"))? as usize),
    }
}
