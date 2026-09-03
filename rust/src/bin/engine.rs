//! Бинарный JSON-контракт чистых частей lib/engine.mjs (Фаза 3, шаг 2).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS.
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! Оркестрация (параллелизм, дедлайн, fallback, MoA, LLM) остаётся в
//! lib/engine.mjs — см. шапку rust/src/engine.rs. Операции — чистые:
//!
//!   planChunks {priorFindings?, chunks} → {scoped: [...]}
//!       scopePriors для каждого чанка; null — «скоуп неполон/пуст, priors
//!       без изменений» (обёртка подставляет оригинал, как в JS).
//!   demote {items: [{res, chunkIndex}], chunks} → {items: [res]}
//!       demoteOutOfChunk для каждого результата чанка.
//!   aggregate {done: [{model, res}], models} → {results, dropped}
//!       aggregateModel для каждой модели; dropped — схлопленные дубли
//!       на модель (обёртка печатает тот же лог, что dedupeSameModelFindings).

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use ai_reviewer::engine::{aggregate_model, demote_out_of_chunk, scope_priors};

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

    let arr_arg = |name: &str| -> Vec<Value> {
        match obj.get(name) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.clone(),
            Some(_) => Vec::new(),
        }
    };

    let out = match op {
        "planChunks" => {
            let priors = arr_arg("priorFindings");
            let chunks = arr_arg("chunks");
            let scoped: Vec<Value> = chunks
                .iter()
                .map(|c| match scope_priors(&priors, c) {
                    Some(list) => Value::Array(list),
                    None => Value::Null,
                })
                .collect();
            json!({ "scoped": scoped })
        }
        "demote" => {
            let chunks = arr_arg("chunks");
            let items = arr_arg("items");
            let out: Vec<Value> = items
                .iter()
                .map(|item| {
                    let res = item.get("res").unwrap_or(&Value::Null);
                    let ci = item
                        .get("chunkIndex")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0) as usize;
                    let chunk = chunks.get(ci).unwrap_or(&Value::Null);
                    demote_out_of_chunk(res, chunk, ci)
                })
                .collect();
            json!({ "items": out })
        }
        "aggregate" => {
            let done = arr_arg("done");
            let models = arr_arg("models");
            let mut results = Vec::new();
            let mut dropped = Vec::new();
            for m in &models {
                let name = m.as_str().unwrap_or("");
                let (agg, d) = aggregate_model(name, &done);
                results.push(agg);
                dropped.push(d);
            }
            json!({ "results": results, "dropped": dropped })
        }
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
