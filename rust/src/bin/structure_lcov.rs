//! Бинарный JSON-контракт порта lib/structure-lcov.mjs (Фаза 2, шаг 2).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате JS-возврата (null вместо undefined).
//! Ошибки:        exit code 2 + {"error": "..."} на stderr — тексты доменных
//!                ошибок совпадают с JS (regex-проверки тестов проходят обеими
//!                реализациями).
//!
//! byPath из результата parseLcov — JS Map и JSON'ом не ходит: бинарь его не
//! возвращает, JS-обёртка пересобирает его из files.
//!
//! Операции:
//!   parseLcov           {text, repoRoot?}                 → {hash, files, warnings, totals}
//!   lcovObservations    {lcov, entityEntries?}            → [observation]
//!   matchLcovFunction   {fn: {line, name}, entities?}     → {entity, confidence}

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ai_reviewer::structure_lcov::{lcov_observations, match_lcov_function, parse_lcov};

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
        "parseLcov" => {
            let text = arg_string(obj, "text")?;
            let repo_root = match obj.get("repoRoot") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(_) => anyhow::bail!("аргумент 'repoRoot' должен быть строкой"),
            };
            json!(parse_lcov(&text, &repo_root)?)
        }
        "lcovObservations" => {
            let lcov = obj.get("lcov").cloned().unwrap_or(Value::Null);
            let entries = entity_entries(obj.get("entityEntries"))?;
            json!(lcov_observations(Some(&lcov), &entries))
        }
        "matchLcovFunction" => {
            let fndef = obj.get("fn").cloned().context("аргумент 'fn' обязателен")?;
            let line = fndef.get("line").and_then(Value::as_f64);
            let name = fndef
                .get("name")
                .and_then(Value::as_str)
                .context("'fn.name' должен быть строкой")?;
            let entities = match obj.get("entities") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(a)) => a.clone(),
                Some(_) => anyhow::bail!("аргумент 'entities' должен быть массивом"),
            };
            json!(match_lcov_function(line, name, &entities))
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

/// [[path, [entities...]]] — содержимое entityByPath Map в порядке вставки.
fn entity_entries(v: Option<&Value>) -> Result<Vec<(String, Value)>> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let Some(list) = v.as_array() else {
        anyhow::bail!("аргумент 'entityEntries' должен быть массивом пар");
    };
    let mut out = Vec::new();
    for pair in list {
        let Some(pair) = pair.as_array() else {
            anyhow::bail!("аргумент 'entityEntries' должен быть массивом пар");
        };
        let path = pair
            .first()
            .and_then(Value::as_str)
            .context("entityEntries: путь должен быть строкой")?;
        let entities = pair.get(1).cloned().unwrap_or(json!([]));
        out.push((path.to_string(), entities));
    }
    Ok(out)
}
