//! Порт lib/chunk-routing.mjs (RFC 04, решение 2; RFC 16 T36): заслуживает ли чанк
//! полного состава специалистов.
//!
//! Чанк из одних markdown-планов и JSON-фикстур получал тех же трёх специалистов, что и
//! чанк с кодом гейта. Здесь решается, нужен ли чанку полный MoA; потребитель —
//! оркестрация engine (T37).
//!
//! Граница проведена по признаку «исполняемое», а не по классу роутера: `config` у
//! `classify_file_path` одинаково метит `.github/workflows/deploy.yml` и `fixtures/*.json`.
//!
//! Регистронезависимость регексов — только ASCII (`(?i-u)`): JS-регекс без флага `u`
//! канонизирует символ через toUpperCase, но не отображает не-ASCII в ASCII (U+212A KELVIN
//! SIGN не равен `k`, U+017F LONG S не равен `s`), а Unicode-режим regex-крейта их сводит.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::findings::{js_to_string, js_truthy};

/// Запускается или конфигурирует запуск: правка здесь способна сломать прод или утечь секрет.
fn executable_config_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i-u)(?:^|/)(?:Dockerfile|Makefile|Containerfile|Procfile)(?:$|\.)|\.(?:ya?ml|sh|bash|zsh|fish|sql|tf|tfvars|nix|gradle|cmake)$",
        )
        .expect("EXECUTABLE_CONFIG компилируется")
    })
}

/// Всё под `.github/` — workflow, actions, политики репозитория: исполняемая поверхность CI.
fn ci_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i-u)(?:^|/)\.github/").expect("CI_PATH компилируется"))
}

/// Контекст маршрутизации (`{ fileClass, sensitive }` в JS).
#[derive(Default)]
pub struct ChunkRoutingCtx<'a> {
    /// Класс файла по пути (`classifyFilePath`); `None` → всё `other`, как в JS при
    /// отсутствующей функции.
    pub file_class: Option<&'a dyn Fn(&str) -> String>,
    /// `layout.sensitive` профиля; `None` — чувствительных путей профиль не задал.
    pub sensitive: Option<&'a dyn Fn(&str) -> bool>,
}

/// Нужен ли чанку полный состав специалистов (`chunkNeedsSpecialists`).
///
/// `true` — если хотя бы один файл: код (`logic`), исполняемая конфигурация, путь под
/// `.github/` или чувствительный путь профиля. `true` и когда состав чанка неизвестен
/// (`paths` не массив/пуст, `unknownPaths` истинно, пустой путь): решать «кода нет» по
/// неполным данным нельзя — это ровно тот случай, когда ошибка молчалива.
pub fn chunk_needs_specialists(chunk: &Value, ctx: &ChunkRoutingCtx) -> bool {
    let paths = match chunk.get("paths") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return true,
    };
    if js_truthy(chunk.get("unknownPaths").unwrap_or(&Value::Null)) {
        return true;
    }
    paths.iter().any(|p| {
        // String(p || '') — ложное значение даёт пустой путь.
        let path = if js_truthy(p) { js_to_string(p) } else { String::new() };
        if path.is_empty() {
            return true;
        }
        if ctx.file_class.is_some_and(|f| f(&path) == "logic") {
            return true;
        }
        if executable_config_re().is_match(&path) || ci_path_re().is_match(&path) {
            return true;
        }
        ctx.sensitive.is_some_and(|f| f(&path))
    })
}

/// Строка для лога/observability (`formatChunkRoutingLog`): какие чанки урезаны и сколько
/// вызовов специалистов сэкономлено. Пустая строка, если урезанных нет.
pub fn format_chunk_routing_log(decisions: &[bool], roles_per_chunk: i64) -> String {
    let lean = decisions.iter().filter(|full| !**full).count();
    if lean == 0 {
        return String::new();
    }
    let saved = lean as i64 * roles_per_chunk.max(0);
    format!(
        "chunk-routing: {lean} из {} чанк(ов) без кода — только агрегатор, −{saved} вызов(ов) специалистов",
        decisions.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::classify_file_path;
    use serde_json::json;

    fn classify(p: &str) -> String {
        classify_file_path(p).to_string()
    }

    fn needs(paths: Value) -> bool {
        let fc: &dyn Fn(&str) -> String = &classify;
        let ctx = ChunkRoutingCtx { file_class: Some(fc), sensitive: None };
        chunk_needs_specialists(&json!({ "paths": paths }), &ctx)
    }

    #[test]
    fn docs_and_data_chunk_is_lean() {
        assert!(!needs(json!(["docs/plan.md", "fixtures/case/findings.json", "README.md"])));
    }

    #[test]
    fn code_file_needs_full_moa() {
        assert!(needs(json!(["docs/plan.md", "lib/engine.mjs"])));
    }

    #[test]
    fn executable_config_and_ci_need_full_moa() {
        for p in [
            ".github/workflows/deploy.yml",
            ".GITHUB/CODEOWNERS",
            "ops/Dockerfile",
            "ops/Dockerfile.prod",
            "Makefile",
            "scripts/run.SH",
            "db/001.sql",
            "infra/main.tf",
            "flake.nix",
        ] {
            assert!(needs(json!([p])), "{p}");
        }
        // Dockerfile-подобное имя без границы — не исполняемое.
        assert!(!needs(json!(["docs/Dockerfiles.md"])));
    }

    #[test]
    fn case_folding_is_ascii_only_like_js() {
        // U+017F LONG S: JS-регекс без `u` не сводит его к `s` — не исполняемое.
        assert!(!needs(json!(["notes/run.\u{17F}h"])));
        // U+212A KELVIN SIGN в `Ma\u{212A}efile` — не Makefile.
        assert!(!needs(json!(["Ma\u{212A}efile"])));
    }

    #[test]
    fn unknown_composition_fails_to_full_moa() {
        let ctx = ChunkRoutingCtx::default();
        assert!(chunk_needs_specialists(&json!({}), &ctx));
        assert!(chunk_needs_specialists(&json!({ "paths": [] }), &ctx));
        assert!(chunk_needs_specialists(&json!({ "paths": "a.md" }), &ctx));
        assert!(chunk_needs_specialists(&json!({ "paths": ["a.md"], "unknownPaths": true }), &ctx));
        assert!(chunk_needs_specialists(&json!({ "paths": ["a.md", ""] }), &ctx));
        assert!(chunk_needs_specialists(&json!({ "paths": ["a.md", null] }), &ctx));
        assert!(!chunk_needs_specialists(&json!({ "paths": ["a.md"], "unknownPaths": false }), &ctx));
    }

    #[test]
    fn without_file_class_everything_is_other() {
        // Без fileClass `.mjs` не распознаётся как код — только исполняемые/CI/sensitive.
        let ctx = ChunkRoutingCtx::default();
        assert!(!chunk_needs_specialists(&json!({ "paths": ["lib/engine.mjs"] }), &ctx));
    }

    #[test]
    fn sensitive_profile_path_needs_full_moa() {
        let re = Regex::new(r"(?i)billing/").unwrap();
        let sens = |p: &str| re.is_match(p);
        let sf: &dyn Fn(&str) -> bool = &sens;
        let ctx = ChunkRoutingCtx { file_class: None, sensitive: Some(sf) };
        assert!(chunk_needs_specialists(&json!({ "paths": ["docs/Billing/rates.md"] }), &ctx));
        assert!(!chunk_needs_specialists(&json!({ "paths": ["docs/rates.md"] }), &ctx));
    }

    #[test]
    fn routing_log() {
        assert_eq!(format_chunk_routing_log(&[true, true], 3), "");
        assert_eq!(
            format_chunk_routing_log(&[true, false, false], 3),
            "chunk-routing: 2 из 3 чанк(ов) без кода — только агрегатор, −6 вызов(ов) специалистов"
        );
        assert_eq!(
            format_chunk_routing_log(&[false], -2),
            "chunk-routing: 1 из 1 чанк(ов) без кода — только агрегатор, −0 вызов(ов) специалистов"
        );
    }
}
