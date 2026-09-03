//! Бинарный JSON-контракт порта lib/actionability.mjs + classify/findingBlock
//! из createFindingsFormatter (lib/findings.mjs) — Фаза 1, шаг 5.
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS.
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr
//!                 (в т.ч. когда extra-регекс чувствительных зон из профиля
//!                 не компилируется regex-крейтом — обёртка тогда деградирует
//!                 на чистый JS, см. шапку lib/actionability.mjs).
//!
//! Мутации routeFindings (actionabilityScore/Reasons/Promoted) НЕ применяются
//! к входным находкам: операции возвращают их списком (index + поля) —
//! JS-обёртка накладывает на исходные объекты, сохраняя семантику разделяемых
//! ссылок (routeFindings вызывается за прогон дважды с разными лимитами).
//!
//! Операции:
//!   signals               {}                          → {key: {weight, why}} (ACTIONABILITY_SIGNALS)
//!   findingLocationKey    {finding}                   → "file:line"
//!   scoreActionability    {finding, ctx?}             → {score, reasons: [{key, weight, why}]}
//!   routeFindings         {findings, files?, cfg?, ctx?} → {scored, mutations}
//!   findingBlock          {finding}                   → string
//!   classifyFindings      {findings, files?, cfg?, ctx?, markerInline} → {inline, outside, mutations}
//!
//! ctx:   {priorKeys?: string[], prescanKeys?: string[], fileClasses?: {path: class},
//!         sensitiveSource?: string|null}   — sensitiveSource = исходник регекса
//!         layout.sensitive профиля (как MARKERS в findings — аргументом).
//! cfg:   {inlineOn?: string[], threshold?, maxAdvisoryInline?, strict?}
//! files: {path: {validLines: number[]}} | null

use std::collections::BTreeSet;
use std::io::Read;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde_json::{json, Value};

use ai_reviewer::actionability::{
    classify_findings, finding_block, finding_location_key, route_findings, score_actionability,
    ClassifyOutcome, FilesMap, RouteCfgArg, ScoreCtxArg,
};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn files_arg(v: Option<&Value>) -> Result<FilesMap> {
    match v {
        None | Some(Value::Null) => Ok(IndexMap::new()),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(path, rec)| {
                let valid: BTreeSet<u64> = rec
                    .get("validLines")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default();
                Ok((path.clone(), valid))
            })
            .collect(),
        Some(_) => bail!("аргумент 'files' должен быть объектом или null"),
    }
}

fn ctx_arg(obj: &serde_json::Map<String, Value>) -> Result<ai_reviewer::actionability::ScoreCtx> {
    serde_json::from_value::<ScoreCtxArg>(
        obj.get("ctx").cloned().unwrap_or(Value::Null),
    )
    .context("ctx: priorKeys/prescanKeys массивы строк, fileClasses объект")?
    .into_ctx()
}

fn cfg_arg(obj: &serde_json::Map<String, Value>) -> ai_reviewer::actionability::RouteCfg {
    serde_json::from_value::<RouteCfgArg>(obj.get("cfg").cloned().unwrap_or(Value::Null))
        .ok()
        .map(RouteCfgArg::into_cfg)
        .unwrap_or_default()
}

/// Мутации routeFindings в формате JS-обёртки: score + reasons-why + promoted
/// (promoted === false означает `actionabilityPromoted = undefined` в JS).
fn mutations(scored: &[ai_reviewer::actionability::ScoredItem]) -> Value {
    json!(scored
        .iter()
        .map(|s| {
            json!({
                "index": s.index,
                "score": s.score,
                "reasons": s.reasons.iter().map(|r| r.why).collect::<Vec<_>>(),
                "promoted": s.promoted,
            })
        })
        .collect::<Vec<_>>())
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
        "signals" => {
            let mut map = serde_json::Map::new();
            for (key, weight, why) in ai_reviewer::actionability::SIGNALS {
                map.insert(
                    key.to_string(),
                    json!({ "weight": weight, "why": why }),
                );
            }
            Value::Object(map)
        }
        "findingLocationKey" => {
            let finding = obj.get("finding").cloned().unwrap_or(Value::Null);
            json!(finding_location_key(&finding))
        }
        "scoreActionability" => {
            let finding = obj.get("finding").cloned().unwrap_or(Value::Null);
            let ctx = ctx_arg(obj)?;
            let (score, reasons) = score_actionability(&finding, &ctx);
            json!({ "score": score, "reasons": reasons })
        }
        "routeFindings" => {
            let findings: Vec<Value> = serde_json::from_value(
                obj.get("findings").cloned().context("аргумент 'findings' обязателен")?,
            )
            .context("findings: нужен массив")?;
            let files = files_arg(obj.get("files"))?;
            let cfg = cfg_arg(obj);
            let ctx = ctx_arg(obj)?;
            let scored = route_findings(&findings, &files, &cfg, &ctx);
            json!({ "scored": scored, "mutations": mutations(&scored) })
        }
        "findingBlock" => {
            let finding = obj.get("finding").cloned().unwrap_or(Value::Null);
            json!(finding_block(&finding))
        }
        "classifyFindings" => {
            let findings: Vec<Value> = serde_json::from_value(
                obj.get("findings").cloned().context("аргумент 'findings' обязателен")?,
            )
            .context("findings: нужен массив")?;
            let files = files_arg(obj.get("files"))?;
            let cfg = cfg_arg(obj);
            let ctx = ctx_arg(obj)?;
            let marker = obj
                .get("markerInline")
                .and_then(Value::as_str)
                .context("аргумент 'markerInline' обязателен")?;
            let ClassifyOutcome { inline, outside, scored } =
                classify_findings(&findings, &files, &cfg, &ctx, marker);
            json!({
                "inline": inline,
                "outside": outside,
                "mutations": mutations(&scored),
            })
        }
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
