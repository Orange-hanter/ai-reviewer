//! Бинарный JSON-контракт порта lib/router.mjs (Фаза 1, шаг 4).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат функции в JSON-формате, эквивалентном возврату JS.
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! Операции:
//!   classifyFilePath       {path}                     → "skip"|"docs"|"logic"|"config"|"other"
//!   extractFileDiffSections {diff}                    → {path: section} (порядок Map)
//!   isRenameOnlySection    {section}                  → bool (null/"" → false)
//!   classifyChangedFile    {path, section?}           → класс как у classifyFilePath
//!   routeDiff              {changedPaths, diff?, enabled?} → {route, reason, files}
//!   formatRouteLog         {route}                    → string

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use ai_reviewer::router::{
    classify_changed_file, classify_file_path, extract_file_diff_sections, format_route_log,
    is_rename_only_section, route_diff, RouteResult,
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

    let arg_str = |name: &str| -> Result<&str> {
        obj.get(name)
            .and_then(Value::as_str)
            .with_context(|| format!("аргумент '{name}' должен быть строкой"))
    };
    let arg_opt_str = |name: &str| -> Option<&str> { obj.get(name).and_then(Value::as_str) };

    let out = match op {
        "classifyFilePath" => json!(classify_file_path(arg_str("path")?)),
        "extractFileDiffSections" => {
            let sections = extract_file_diff_sections(arg_str("diff").unwrap_or(""));
            json!(sections)
        }
        "isRenameOnlySection" => json!(is_rename_only_section(arg_opt_str("section"))),
        "classifyChangedFile" => {
            json!(classify_changed_file(arg_str("path")?, arg_opt_str("section")))
        }
        "routeDiff" => {
            let changed_paths: Vec<String> = serde_json::from_value(
                obj.get("changedPaths")
                    .cloned()
                    .context("аргумент 'changedPaths' обязателен")?,
            )
            .context("changedPaths: нужен массив строк")?;
            let diff = arg_opt_str("diff").unwrap_or("");
            let enabled = match obj.get("enabled") {
                None | Some(Value::Null) => true,
                Some(b) => b.as_bool().context("enabled должен быть bool")?,
            };
            serde_json::to_value(route_diff(&changed_paths, diff, enabled))?
        }
        "formatRouteLog" => {
            let route: RouteResult = serde_json::from_value(
                obj.get("route")
                    .cloned()
                    .context("аргумент 'route' обязателен")?,
            )
            .context("route: нужны route/reason/files[{path,class}]")?;
            json!(format_route_log(&route))
        }
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
