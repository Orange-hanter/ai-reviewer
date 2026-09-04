//! Бинарный JSON-контракт порта lib/export.mjs (Фаза 3, шаг 6).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS
//!                 (null вместо undefined; условные поля пропускаются).
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! Функции чистые: мутаций входных находок нет (JS-оригинал тоже не мутирует).
//! SARIF — побайтовый внешний контракт (GitHub Code Scanning): порядок ключей
//! объекта значим, JS-обёртка сравнивает JSON.stringify(sarif, null, 2).
//!
//! Операции:
//!   sarifAutomationGuid  {seed, tool}                          → string
//!   sarifArtifactUri     {file}                                → string
//!   slimFinding          {finding}                             → object
//!   extractReasoningEntries {reviews?, reviewMode, excerptLen?, errorMax?} → array
//!   buildSwarmVerdict    {input, generatedAt}                  → object
//!   buildReviewJson      {input, generatedAt}                  → object
//!   selectSarifFindings  {payload, sarifAll?}                  → array
//!   buildSarif           {payload, repositoryUri?, sarifAll?, profile} → object
//!
//! Часы НЕ портированы: generatedAt (top-level и swarm) приходит из JS
//! (fs/часы живут на JS-стороне). Профиль (toolName/sarifRulePrefix/
//! repositoryUri) приходит аргументом — профиль живёт на JS-стороне.

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use ai_reviewer::export::{
    build_review_json, build_sarif, build_swarm_verdict, sarif_artifact_uri, sarif_automation_guid,
    select_sarif_findings, slim_finding, REASONING_EXPORT_MAX,
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
    let arg = |name: &str| obj.get(name).unwrap_or(&Value::Null);

    let out = match op {
        "sarifAutomationGuid" => json!(sarif_automation_guid(
            &js_str(arg("seed")),
            &js_str(arg("tool")),
        )),
        "sarifArtifactUri" => json!(sarif_artifact_uri(obj.get("file"))),
        "slimFinding" => slim_finding(arg("finding")),
        "extractReasoningEntries" => {
            let excerpt_len = arg("excerptLen").as_u64().map(|x| x as usize).unwrap_or(REASONING_EXPORT_MAX);
            let error_max = arg("errorMax").as_u64().map(|x| x as usize).unwrap_or(200);
            Value::Array(ai_reviewer::report::extract_reasoning_entries(
                obj.get("reviews"),
                arg("reviewMode").as_str().unwrap_or("consensus"),
                excerpt_len,
                error_max,
            )?)
        }
        "buildSwarmVerdict" => build_swarm_verdict(
            obj.get("input"),
            arg("generatedAt").as_str().unwrap_or(""),
        ),
        "buildReviewJson" => build_review_json(
            obj.get("input"),
            arg("generatedAt").as_str().unwrap_or(""),
        )?,
        "selectSarifFindings" => Value::Array(select_sarif_findings(
            obj.get("payload"),
            arg("sarifAll") == &json!(true),
        )),
        "buildSarif" => build_sarif(
            obj.get("payload"),
            obj.get("repositoryUri").and_then(Value::as_str),
            arg("sarifAll") == &json!(true),
            obj.get("profile"),
        ),
        other => bail!("неизвестный op: {other}"),
    };
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => ai_reviewer::findings::js_to_string(other),
    }
}
