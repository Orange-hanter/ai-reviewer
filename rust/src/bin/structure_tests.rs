//! Бинарный JSON-контракт порта lib/structure-tests.mjs (Фаза 2, шаг 2).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате JS-возврата (null вместо undefined).
//! Ошибки:        exit code 2 + {"error": "..."} на stderr.
//!
//! Операции:
//!   extractTestCasesFromSource {path, source, contracts?, entities?} → [testCase]
//!   mergeTestCases             {groups: [[testCase]]}              → [testCase]
//!   generatePropertyDrafts     {obligations?}                     → [draft]
//!   parsePropertyReport        {report?, drafts?}                 → [result]

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ai_reviewer::structure_tests::{
    extract_test_cases_from_source, generate_property_drafts, merge_test_cases, parse_property_report,
};

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
        "extractTestCasesFromSource" => {
            let path = arg_string(obj, "path")?;
            let source = arg_string(obj, "source")?;
            let contracts = arg_array(obj, "contracts")?;
            let entities = arg_array(obj, "entities")?;
            json!(extract_test_cases_from_source(&path, &source, &contracts, &entities))
        }
        "mergeTestCases" => {
            let groups_v = obj.get("groups").cloned().context("аргумент 'groups' обязателен")?;
            let Some(groups_list) = groups_v.as_array() else {
                anyhow::bail!("аргумент 'groups' должен быть массивом массивов");
            };
            let mut groups: Vec<Vec<Value>> = Vec::new();
            for group in groups_list {
                let Some(cases) = group.as_array() else {
                    anyhow::bail!("аргумент 'groups' должен быть массивом массивов");
                };
                groups.push(cases.clone());
            }
            json!(merge_test_cases(groups))
        }
        "generatePropertyDrafts" => {
            let obligations = arg_array(obj, "obligations")?;
            json!(generate_property_drafts(&obligations))
        }
        "parsePropertyReport" => {
            let report = obj.get("report").cloned();
            let drafts = arg_array(obj, "drafts")?;
            json!(parse_property_report(report.as_ref(), &drafts))
        }
        other => anyhow::bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn arg_string(obj: &serde_json::Map<String, Value>, name: &str) -> Result<String> {
    obj.get(name)
        .and_then(Value::as_str)
        .map(String::from)
        .with_context(|| format!("аргумент '{name}' должен быть строкой"))
}

fn arg_array(obj: &serde_json::Map<String, Value>, name: &str) -> Result<Vec<Value>> {
    match obj.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(_) => anyhow::bail!("аргумент '{name}' должен быть массивом"),
    }
}
