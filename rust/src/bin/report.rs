//! Бинарный JSON-контракт порта lib/report.mjs (Фаза 3, шаг 5).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS
//!                 (null вместо undefined; условные поля пропускаются).
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! Функции чистые: мутаций входных находок нет (JS-оригинал тоже не мутирует).
//! Футер sticky рендерится внутри buildSummary и побайтово совпадает с JS
//! (js_json_stringify с семантикой V8 + replace `-->`/`<!--`).
//!
//! Операции:
//!   sev                       {}                          → {critical, major, minor, nit} (SEV)
//!   publicErrorLabel          {error}                     → string
//!   findingKeys               {finding}                   → {fp, loc, finding, point}
//!   findingIdentity           {finding}                   → string
//!   diffFindings              {current?, prior?}          → {added, gone, kept}
//!   findingPointDelta         {current?, prior?}          → {kept, added, gone, current, prior}
//!   findingsStability         {current?, prior?, priorKnown?} → number | null
//!   verdictReproducibility    {runs?, sha?}              → {total, blocked, flaky} | null
//!   buildDeltaSection         {current?, prior?, priorRuns?, priorSha?, currentRun?} → string
//!   buildWalkthroughSection   {reviews?}                  → string
//!   buildReasoningSection     {reviews?, allFindings?, enabled?, reviewMode?} → {text, truncated}
//!   buildStickyReasoningFooter {reviews?, reviewMode?, excerptLen?} → array
//!   buildLlmUnavailableSummary {llmRequired, whyPublic?, models?, prescanBlockers?,
//!                               prescanAdvisoryFindings?, reviewMarker} → string
//!   buildRouterSummary        {route, headSha?, changedPaths?, routerEnabled?, reviewMarker} → string
//!   buildSummary              {cfg?, input?, fileTexts?, blocking, reviewMarker, dataKey}
//!                             → {md, counts, total, delta, verdict}
//!
//! Маркеры (review/dataKey) приходят аргументом: профиль живёт на JS-стороне,
//! MARKERS — getter'ы активного профиля (как MARKERS.inline в findings).

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use ai_reviewer::report::{
    build_delta_section, build_llm_unavailable_summary, build_reasoning_section, build_router_summary,
    build_sticky_reasoning_footer, build_summary, build_walkthrough_section, diff_findings, finding_keys,
    finding_point_delta, findings_stability, format_cost_sticky_line, public_error_label, sev_map,
    verdict_reproducibility, STICKY_REASONING_EXCERPT,
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
        "sev" => sev_map(),
        "publicErrorLabel" => json!(public_error_label(obj.get("error"))),
        "formatCostStickyLine" => json!(format_cost_sticky_line(obj.get("snapshot"))),
        "findingKeys" => {
            let keys = finding_keys(arg("finding"));
            json!({
                "fp": keys.fp,
                "loc": keys.loc,
                "finding": keys.finding,
                "point": keys.point,
            })
        }
        "findingIdentity" => json!(finding_keys(arg("finding")).finding),
        "diffFindings" => {
            let d = diff_findings(obj.get("current"), obj.get("prior"))?;
            json!({ "added": d.added, "gone": d.gone, "kept": d.kept })
        }
        "findingPointDelta" => {
            let d = finding_point_delta(obj.get("current"), obj.get("prior"))?;
            json!(d)
        }
        "findingsStability" => {
            let prior_known = obj.get("priorKnown").and_then(Value::as_bool).unwrap_or(true);
            match findings_stability(obj.get("current"), obj.get("prior"), prior_known)? {
                Some(x) => json!(x),
                None => Value::Null,
            }
        }
        "verdictReproducibility" => match verdict_reproducibility(obj.get("runs"), obj.get("sha"))? {
            Some(r) => json!(r),
            None => Value::Null,
        },
        "buildDeltaSection" => json!(build_delta_section(
            obj.get("current"),
            obj.get("prior"),
            obj.get("priorRuns"),
            obj.get("priorSha"),
            obj.get("currentRun"),
        )?),
        "buildWalkthroughSection" => json!(build_walkthrough_section(obj.get("reviews"))?),
        "buildReasoningSection" => {
            let section = build_reasoning_section(
                obj.get("reviews"),
                obj.get("allFindings"),
                obj.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                obj.get("reviewMode").and_then(Value::as_str).unwrap_or("consensus"),
            )?;
            json!({ "text": section.text, "truncated": section.truncated })
        }
        "buildStickyReasoningFooter" => {
            let excerpt_len = obj
                .get("excerptLen")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(STICKY_REASONING_EXCERPT);
            json!(build_sticky_reasoning_footer(
                obj.get("reviews"),
                obj.get("reviewMode").and_then(Value::as_str).unwrap_or("consensus"),
                excerpt_len,
            )?)
        }
        "buildLlmUnavailableSummary" => json!(build_llm_unavailable_summary(
            obj.get("llmRequired").and_then(Value::as_bool).unwrap_or(false),
            obj.get("whyPublic"),
            obj.get("models"),
            obj.get("prescanBlockers"),
            obj.get("prescanAdvisoryFindings"),
            obj.get("reviewMarker").and_then(Value::as_str).unwrap_or(""),
        )?),
        "buildRouterSummary" => json!(build_router_summary(
            arg("route"),
            obj.get("headSha"),
            obj.get("changedPaths"),
            obj.get("routerEnabled").and_then(Value::as_bool).unwrap_or(false),
            obj.get("reviewMarker").and_then(Value::as_str).unwrap_or(""),
        )?),
        "buildSummary" => {
            let blocking = obj
                .get("blocking")
                .and_then(Value::as_u64)
                .context("аргумент 'blocking' должен быть числом")? as usize;
            let outcome = build_summary(
                obj.get("cfg"),
                obj.get("input"),
                obj.get("fileTexts"),
                blocking,
                obj.get("reviewMarker").and_then(Value::as_str).unwrap_or(""),
                obj.get("dataKey").and_then(Value::as_str).unwrap_or(""),
            )?;
            json!({
                "md": outcome.md,
                "counts": outcome.counts,
                "total": outcome.total,
                "delta": outcome.delta,
                "verdict": outcome.verdict,
            })
        }
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
