//! Бинарный JSON-контракт чистых частей lib/verifier.mjs (Фаза 3, шаг 3).
//!
//! Вход (stdin):  {"op": "select"|"apply", ...args}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS.
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! LLM-runner (createVerifierRunner) остаётся в JS — см. шапку
//! rust/src/verifier.rs. Мутации возвращаются индексами: JS-обёртка
//! накладывает их на исходные объекты находок, сохраняя семантику
//! разделяемых ссылок в пайплайне.
//!
//!   select {findings, blockOn, max, advisory: {candidateIndices, max}}
//!       → {indices} — ИНДЕКСЫ findings (blockers + advisory без повторов)
//!   apply  {allFindings, candidateIndices, verdicts, strictUnverified}
//!       → {kept, mutations, stats, logs}
//!       kept — индексы allFindings после отсева rejected; mutations —
//!       [{index, set}] поля verifierStatus/verifierReason/severity.

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use ai_reviewer::verifier::{apply_verifier_verdicts, select_verifier_candidates};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn arr_arg(args: &Value, key: &str) -> Vec<Value> {
    match args.get(key) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
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
        "select" => {
            let findings = arr_arg(&v, "findings");
            let block_on = arr_arg(&v, "blockOn");
            let max = v.get("max").and_then(Value::as_u64).unwrap_or(12) as usize;
            let (adv_idx, adv_max) = match v.get("advisory") {
                Some(a) => {
                    let idx: Vec<usize> = arr_arg(a, "candidateIndices")
                        .iter()
                        .filter_map(|x| x.as_u64().map(|n| n as usize))
                        .collect();
                    (idx, a.get("max").and_then(Value::as_u64).unwrap_or(0) as usize)
                }
                None => (Vec::new(), 0),
            };
            let indices = select_verifier_candidates(&findings, &block_on, max, &adv_idx, adv_max);
            json!({ "indices": indices })
        }
        "apply" => {
            let all = arr_arg(&v, "allFindings");
            let candidates: Vec<usize> = arr_arg(&v, "candidateIndices")
                .iter()
                .filter_map(|x| x.as_u64().map(|n| n as usize))
                .collect();
            let verdicts = arr_arg(&v, "verdicts");
            let strict = v
                .get("strictUnverified")
                .map(|s| matches!(s, Value::Bool(true)))
                .unwrap_or(false);
            let applied = apply_verifier_verdicts(&all, &candidates, &verdicts, strict);
            let mutations: Vec<Value> = applied
                .mutations
                .iter()
                .map(|m| {
                    let mut o = Map::new();
                    o.insert("index".into(), json!(m.index));
                    o.insert("set".into(), Value::Object(m.set.clone()));
                    Value::Object(o)
                })
                .collect();
            json!({
                "kept": applied.kept,
                "mutations": mutations,
                "stats": applied.stats,
                "logs": applied.logs,
            })
        }
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
