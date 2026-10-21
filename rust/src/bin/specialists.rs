//! Бинарный JSON-контракт порта lib/specialists.mjs (Фаза 3, шаг 2).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS
//!                 (null вместо undefined; условные поля пропускаются).
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! LLM-вызовов здесь нет — оркестрация advisor/aggregator остаётся в
//! lib/engine.mjs; этот бинарь — чистая R5-логика jury/refutation.
//!
//! Операции:
//!   parseAssignments        {env, moaReferences?}        → [{role, model}]
//!   normalizeAdvisorClaims  {raw}                        → claims | null
//!   decideRefutation        {claims}                     → {action, severity?}
//!   applyRefutation         {findings, strict?, blockOn?, skipBlast?} → {kept, mutations, stats, logs}
//!   mergeSpecialistReferences {chunkResults}             → [reference]
//!   formatRefutationLog     {stats}                      → string
//!
//! applyRefutation НЕ мутирует вход: возвращает kept-индексы и мутации
//! (index + severity/refutationStatus) — JS-обёртка накладывает на исходные
//! объекты, сохраняя семантику разделяемых ссылок в пайплайне.

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use ai_reviewer::findings::normalize_advisor_claims;
use ai_reviewer::specialists::{
    apply_refutation, decide_refutation, format_refutation_log, merge_specialist_references,
    parse_specialist_assignments,
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
    let arg = |name: &str| obj.get(name).unwrap_or(&Value::Null);

    let arr_arg = |name: &str| -> Vec<Value> {
        match obj.get(name) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.clone(),
            Some(_) => Vec::new(), // как falsy/пусто в JS-вызывающем коде
        }
    };

    let out = match op {
        "parseAssignments" => {
            let env = arg("env");
            let refs = arr_arg("moaReferences");
            json!(parse_specialist_assignments(env, &refs))
        }
        "normalizeAdvisorClaims" => match normalize_advisor_claims(Some(arg("raw"))) {
            Some(claims) => Value::Array(claims),
            None => Value::Null,
        },
        "decideRefutation" => decide_refutation(&arr_arg("claims")),
        "applyRefutation" => {
            let outcome = apply_refutation(
                &arr_arg("findings"),
                obj.get("strict").and_then(Value::as_bool).unwrap_or(false),
                &arr_arg("blockOn"),
                obj.get("skipBlast").and_then(Value::as_bool).unwrap_or(false),
            );
            json!({
                "kept": outcome.kept,
                "mutations": outcome.mutations,
                "stats": outcome.stats,
                "logs": outcome.logs,
            })
        }
        "mergeSpecialistReferences" => {
            json!(merge_specialist_references(&arr_arg("chunkResults")))
        }
        "formatRefutationLog" => json!(format_refutation_log(arg("stats"))),
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
