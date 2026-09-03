//! Бинарный JSON-контракт порта lib/config.mjs + чистой части lib/profile.mjs —
//! Фаза 1, шаг 6.
//!
//! Вход (stdin):  {"op": "<имя>", ...аргументы}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS.
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! Граница порта (подробно — шапка rust/src/config.rs):
//!   - createMarkers/normalizeProfile/loadReviewConfig — логика приоритетов в Rust;
//!   - fs (resolveProfile/readRepoRules), компиляция RegExp/Map/Set, dataRe getter,
//!     parseCli и frozen-сборка конфига остаются в JS-обёртках.
//!
//! Операции:
//!   createMarkers    {namespace?}                       → {ns, review, inline, dataKey}
//!   normalizeProfile {raw?, baseDir?, source?}          → сериализуемые факты профиля
//!                                                          (регексы — источниками, без
//!                                                          компиляции и repoRules)
//!   loadReviewConfig {env, profile {strict, appDir, paths}}
//!                                                        → plain parts конфигурации
//!                                                          (без cli/profile/markers и
//!                                                          github.repo/prNumber)

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", serde_json::json!({ "error": format!("{e:#}") }));
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

    let out = match op {
        "createMarkers" => ai_reviewer::config::create_markers(
            obj.get("namespace").and_then(Value::as_str),
        ),
        "normalizeProfile" => ai_reviewer::config::normalize_profile_facts(
            obj.get("raw").unwrap_or(&Value::Null),
            obj.get("baseDir").and_then(Value::as_str).unwrap_or(""),
            obj.get("source").and_then(Value::as_str).unwrap_or(""),
        ),
        "loadReviewConfig" => {
            let env = obj
                .get("env")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_else(Map::new);
            let profile = obj.get("profile").cloned().unwrap_or(Value::Null);
            ai_reviewer::config::load_review_config_parts(&env, &profile)
        }
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
