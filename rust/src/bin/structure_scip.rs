//! Бинарный JSON-контракт порта lib/structure-scip.mjs (Фаза 2, шаг 1).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат функции в JSON-формате, эквивалентном возврату JS
//!                 (null вместо undefined).
//! Ошибки:        exit code 2 + {"error": "..."} на stderr — текст сообщений
//!                 совпадает с JS, чтобы regex-проверки тестов проходили обеими
//!                 реализациями.
//!
//! SCIP-индекс — бинарный protobuf; JSON-строку в stdin испортить нельзя, поэтому
//! бинарный ввод передаётся как base64:
//!
//! Операции:
//!   decodeScipIndex        {dataBase64}                        → {documents, externalSymbols}
//!   extractScipFacts       {index, scope?, maxEntities?, maxEdges?} → facts
//!   mergeScipWithGraphify  {scip, graph}                       → merged facts

use std::io::Read;

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use ai_reviewer::structure_scip::{
    decode_scip_index, extract_scip_facts, merge_scip_with_graphify, ExtractOptions,
};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("чтение stdin")?;
    let v: Value = serde_json::from_str(&input).context("вход не является JSON")?;
    let obj = v.as_object().context("вход должен быть JSON-объектом")?;
    let op = obj.get("op").and_then(Value::as_str).unwrap_or("");

    let out = match op {
        "decodeScipIndex" => {
            let data_base64 = obj
                .get("dataBase64")
                .and_then(Value::as_str)
                .context("аргумент 'dataBase64' должен быть строкой")?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .context("dataBase64: не валидный base64")?;
            json!(decode_scip_index(&bytes)?)
        }
        "extractScipFacts" => {
            let index = obj.get("index").cloned().context("аргумент 'index' обязателен")?;
            let options = ExtractOptions {
                scope: match obj.get("scope") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => bail!("аргумент 'scope' должен быть строкой"),
                },
                max_entities: arg_usize(obj, "maxEntities", 5000)?,
                max_edges: arg_usize(obj, "maxEdges", 15000)?,
            };
            json!(extract_scip_facts(Some(&index), &options)?)
        }
        "mergeScipWithGraphify" => {
            let scip = obj.get("scip").cloned().context("аргумент 'scip' обязателен")?;
            let graph = obj.get("graph").cloned().context("аргумент 'graph' обязателен")?;
            json!(merge_scip_with_graphify(Some(&scip), Some(&graph))?)
        }
        other => bail!("unknown op: '{other}'"),
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
