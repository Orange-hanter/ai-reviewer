//! Бинарный JSON-контракт порта lib/structure-mutation.mjs (Фаза 2, шаг 3).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате JS-возврата (null вместо undefined).
//! Ошибки:        exit code 2 + {"error": "..."} на stderr.
//!
//! Операции:
//!   parseMutationReport   {report?, facts?, scopeContractRefs?} → {observations, warnings, summary}
//!   criticalContractRefs  {facts?, dependencyRisk?, obligations?} → [ref]

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ai_reviewer::structure_mutation::{critical_contract_refs, parse_mutation_report};

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
        "parseMutationReport" => {
            let report = obj.get("report").cloned();
            let facts = obj.get("facts").cloned();
            let scope = arg_array(obj, "scopeContractRefs")?;
            parse_mutation_report(report.as_ref(), facts.as_ref(), &scope)?
        }
        "criticalContractRefs" => {
            let facts = obj.get("facts").cloned();
            let dependency_risk = obj.get("dependencyRisk").cloned();
            let obligations = obj.get("obligations").cloned();
            critical_contract_refs(facts.as_ref(), dependency_risk.as_ref(), obligations.as_ref())?
        }
        other => anyhow::bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn arg_array(obj: &serde_json::Map<String, Value>, name: &str) -> Result<Vec<Value>> {
    match obj.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(_) => anyhow::bail!("аргумент '{name}' должен быть массивом"),
    }
}
