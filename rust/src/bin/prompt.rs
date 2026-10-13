//! Бинарный JSON-контракт чистых частей lib/prompt.mjs (Фаза 3, шаг 3).
//!
//! Вход (stdin):  {"op": "<имя билдера>", "profile": {...}, ...args}
//! Выход (stdout): {"messages": [...]} | {"section": "..."} | {"text": "..."}
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! Оркестрация/LLM остаются в JS (см. шапку rust/src/prompt.rs). Профиль
//! сериализуется обёрткой аргументом profile:
//!   {repoRules: string, prompts: {advisorExtra, specialistExtra,
//!    aggregatorExtra, verifierExtra}, e2eSpecSource: string|null}
//! e2eSpec компилируется fancy-regex (lookahead поддержан); не компилируется → ошибка с маркером
//! E2ESPEC_UNSUPPORTED: обёртка деградирует на чистый JS (как layout.sensitive
//! в actionability), остальные ошибки пробрасываются.

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use ai_reviewer::prompt::{
    build_aggregator_messages, build_changed_files_section, build_messages,
    build_moa_reference_section, build_reference_messages, build_specialist_aggregator_messages,
    build_merged_specialist_system, build_specialist_messages, build_specialist_reference_section,
    build_verifier_messages, PromptCtx, E2ESPEC_UNSUPPORTED,
};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn arr_arg(args: &Value, key: &str) -> Vec<Value> {
    match args.get(key) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
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
    let ctx = PromptCtx::from_json(obj.get("profile").unwrap_or(&Value::Null));

    let out = match op {
        "buildMessages" => json!({ "messages": build_messages(&v, &ctx)? }),
        "buildVerifierMessages" => json!({ "messages": build_verifier_messages(&v, &ctx)? }),
        "buildReferenceMessages" => {
            json!({ "messages": build_reference_messages(&arr_arg(&v, "baseMessages"), &ctx) })
        }
        "buildSpecialistMessages" => {
            let role = v.get("role").and_then(Value::as_str).unwrap_or("");
            json!({ "messages": build_specialist_messages(role, &arr_arg(&v, "baseMessages"), &ctx) })
        }
        "buildAggregatorMessages" => json!({
            "messages": build_aggregator_messages(&arr_arg(&v, "baseMessages"), &arr_arg(&v, "references")),
        }),
        "buildSpecialistAggregatorMessages" => json!({
            "messages": build_specialist_aggregator_messages(&arr_arg(&v, "baseMessages"), &arr_arg(&v, "references")),
        }),
        "buildSpecialistReferenceSection" => {
            json!({ "text": build_specialist_reference_section(&arr_arg(&v, "references")) })
        }
        "buildMoAReferenceSection" => {
            json!({ "text": build_moa_reference_section(&arr_arg(&v, "references")) })
        }
        "buildMergedSpecialistSystem" => {
            let roles: Vec<String> = arr_arg(&v, "roles")
                .iter()
                .filter_map(|r| r.as_str().map(str::to_owned))
                .collect();
            json!({ "text": build_merged_specialist_system(&roles, &ctx) })
        }
        "changedFilesSection" => {
            let paths = arr_arg(&v, "changedPaths");
            let chunk = v.get("chunk").cloned().unwrap_or_else(|| json!({}));
            json!({ "section": build_changed_files_section(&paths, &chunk, &ctx)? })
        }
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

// Маркер используется в контракте с JS-обёрткой — не даём clippy/rustc
// посчитать константу мёртвой, если внутренние вызовы изменятся.
#[allow(dead_code)]
const _: &str = E2ESPEC_UNSUPPORTED;
