//! Бинарный JSON-контракт порта lib/json.mjs (Фаза 1, шаг 2).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS.
//! Ошибки:        non-zero exit code + {"error", "badEscape", "truncated"} в stderr —
//!                 JS-обёртка reconstruct'ит Error с теми же флагами.
//!
//! Операции:
//!   repairBadEscapes  {s}     → string
//!   extractJsonObject {text}  → {"value": ..., "repaired": "escapes"|"truncated"|null}

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use ai_reviewer::json_repair::{extract_json_object, repair_bad_escapes};

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

    let out = match op {
        "repairBadEscapes" => json!(repair_bad_escapes(arg_str("s")?)),
        "extractJsonObject" => match extract_json_object(arg_str("text")?) {
            Ok(outcome) => json!({
                "value": outcome.value,
                "repaired": outcome.repaired.map(|k| k.as_str()),
            }),
            Err(e) => {
                eprintln!(
                    "{}",
                    json!({ "error": e.message, "badEscape": e.bad_escape, "truncated": e.truncated })
                );
                std::process::exit(2);
            }
        },
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
