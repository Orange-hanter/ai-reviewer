//! Бинарный JSON-контракт decision-моделей (RFC 21): rust/src/decision.rs и
//! rust/src/decision_eval.rs. JS-копии нет — без бинаря shadow и офлайн-замер не работают.
//!
//! Вход (stdin):  {"op": "<операция>", ...аргументы}
//! Выход (stdout): результат JSON.
//! Ошибки:        exit 2 + {"error": "..."} в stderr.
//!
//! Операции:
//!   questions            {}                                   → {fp, chunk} (вход `question` для decide)
//!   fpFamilyOfPattern    {id}                                 → string | null
//!   fpStates             {items: [{finding, section, regexPatternId?}]} → [{state, regexFamily}] (вход Q1, батчем)
//!   chunkStates          {chunks: [{paths, annotated}], classes: {path: class}} → [string]
//!   report               {date, params, corpus, models, rows} → {models, cross, markdown, …}

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use ai_reviewer::decision::{chunk_state, fp_family_of_pattern, fp_state, questions};
use ai_reviewer::decision_eval::report;

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("чтение stdin")?;
    let v: Value = serde_json::from_str(&input).context("вход должен быть JSON")?;
    let op = v.get("op").and_then(Value::as_str).unwrap_or("");
    let out = match op {
        "questions" => questions(),
        "fpFamilyOfPattern" => {
            let id = v.get("id").and_then(Value::as_str).unwrap_or("");
            fp_family_of_pattern(id).map_or(Value::Null, |f| json!(f))
        }
        "fpStates" => {
            let items = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            json!(items
                .iter()
                .map(|it| {
                    let state = fp_state(it.get("finding").unwrap_or(&Value::Null), it.get("section").and_then(Value::as_str).unwrap_or(""));
                    let family = it.get("regexPatternId").and_then(Value::as_str).and_then(fp_family_of_pattern);
                    json!({ "state": state, "regexFamily": family })
                })
                .collect::<Vec<_>>())
        }
        "chunkStates" => {
            let empty = Map::new();
            let classes = v.get("classes").and_then(Value::as_object).unwrap_or(&empty);
            let chunks = v.get("chunks").and_then(Value::as_array).cloned().unwrap_or_default();
            json!(chunks.iter().map(|c| chunk_state(c, classes)).collect::<Vec<_>>())
        }
        "report" => report(&v),
        other => bail!("неизвестная операция: {other:?}"),
    };
    println!("{}", serde_json::to_string(&out).context("сериализация")?);
    Ok(())
}
