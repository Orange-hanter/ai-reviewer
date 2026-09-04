//! Бинарный JSON-контракт порта lib/observability.mjs (чистые части) +
//! lib/metrics.mjs (Фаза 3, шаг 6): агрегация метрик из JSONL run-логов.
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS.
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! Операции:
//!   countBySeverity   {findings}                    → object
//!   slimFindings      {findings, limit?}            → array
//!   parseJsonl        {text}                        → array
//!   aggregateMetrics  {records}                     → object
//!   formatMetricsReport {agg, source?}              → string
//!
//! fs-запись логов и часы остаются в JS (createObservability не портирован:
//! runId, ts и appendFile живут на JS-стороне).

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use ai_reviewer::metrics::{
    aggregate_metrics, count_by_severity, format_metrics_report, parse_jsonl, slim_findings,
};

const MAX_SLIM: usize = 40;

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
        "countBySeverity" => count_by_severity(obj.get("findings")),
        "slimFindings" => slim_findings(
            obj.get("findings"),
            arg("limit").as_u64().map(|x| x as usize).unwrap_or(MAX_SLIM),
        ),
        "parseJsonl" => Value::Array(parse_jsonl(arg("text").as_str().unwrap_or(""))),
        "aggregateMetrics" => aggregate_metrics(obj.get("records"))?,
        "formatMetricsReport" => json!(format_metrics_report(
            obj.get("agg"),
            arg("source").as_str().unwrap_or(""),
        )?),
        other => bail!("неизвестный op: {other}"),
    };
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
