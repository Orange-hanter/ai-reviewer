//! Порт lib/router.mjs: классификация diff-путей и маршрутизация PR
//! (skip / light / full).
//!
//! Паритет с JS-кодом сохранён построчно:
//! - те же regex-паттерны (`(?i)` — как флаг `i` в JS; обе стороны делают
//!   Unicode simple case folding, на ASCII-путях идентичны);
//! - порядок вставки в Map секций — IndexMap с preserve_order в JSON;
//! - путь секции берётся из `diff --git` через diff::parse_diff_git_path
//!   (тот же парсер quoted-путей, что в lib/diff.mjs).

use serde::Serialize;

use crate::diff;

// ---------------------------------------------------------------------------
// Классификация одного пути
// ---------------------------------------------------------------------------

struct PathRegexes {
    lockfile: regex::Regex,
    generated: regex::Regex,
    docs: regex::Regex,
    logic: regex::Regex,
    code: regex::Regex,
    config: regex::Regex,
}

fn path_regexes() -> &'static PathRegexes {
    static RES: std::sync::OnceLock<PathRegexes> = std::sync::OnceLock::new();
    RES.get_or_init(|| PathRegexes {
        lockfile: regex::Regex::new(
            r"(?i)(?:^|/)?(?:package-lock\.json|yarn\.lock|pnpm-lock\.yaml|composer\.lock|Cargo\.lock|poetry\.lock|Gemfile\.lock)$",
        )
        .unwrap(),
        generated: regex::Regex::new(
            r"(?i)(?:^|/)(?:dist|graphify-out|node_modules|\.next|coverage|build)/|\.(?:min\.js|map|d\.ts\.map)$|[-.]generated\.|/__generated__/",
        )
        .unwrap(),
        docs: regex::Regex::new(r"(?i)\.(?:md|mdx|rst|txt)$|(?:^|/)docs/").unwrap(),
        logic: regex::Regex::new(
            r"(?i)^(?:app/)?(?:server|shared|client)/.+\.(?:ts|tsx|js|jsx|mjs|cjs)$",
        )
        .unwrap(),
        // Исходники любого языка — logic (паритет с lib/router.mjs: список из PR #17;
        // только JS/TS-семейство отправляет не-JS репозитории в light без ревью).
        code: regex::Regex::new(
            r"(?i)\.(?:ts|tsx|js|jsx|mjs|cjs|cts|mts|vue|svelte|gjs|gts|go|py|pyi|rb|rake|php|java|kt|kts|scala|groovy|rs|c|h|cc|cpp|cxx|hpp|hxx|m|mm|cs|swift|ex|exs|erl|hrl|clj|cljs|dart|lua|pl|pm|r)$",
        )
        .unwrap(),
        config: regex::Regex::new(r"(?i)\.(?:yml|yaml|json|toml|sh|sql|env\.example)$").unwrap(),
    })
}

/// Класс одного изменённого файла: skip | docs | logic | config | other.
pub fn classify_file_path(path: &str) -> &'static str {
    let res = path_regexes();
    if res.lockfile.is_match(path) || path.ends_with(".lock") {
        return "skip";
    }
    if res.generated.is_match(path) {
        return "skip";
    }
    // basename для README-проверки: `path.split('/').pop() || ''`
    let basename = path.split('/').next_back().unwrap_or("");
    if res.docs.is_match(path) || basename.to_uppercase().starts_with("README") {
        return "docs";
    }
    if res.logic.is_match(path) {
        return "logic";
    }
    if res.code.is_match(path) {
        return "logic";
    }
    if res.config.is_match(path) {
        return "config";
    }
    "other"
}

// ---------------------------------------------------------------------------
// Секции диффа
// ---------------------------------------------------------------------------

/// Вырезает unified-diff секции по пути `+++ b/path` (учитывает quoted-пути с пробелами).
/// Ключ — b-side путь из заголовка `diff --git`; повторный путь перезаписывает
/// секцию на прежней позиции — как Map.set в JS.
pub fn extract_file_diff_sections(diff: &str) -> indexmap::IndexMap<String, String> {
    let mut sections: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
    let mut cur: Option<String> = None;
    let mut buf: Vec<&str> = Vec::new();
    for line in diff.split('\n') {
        if line.starts_with("diff --git") {
            if let Some(c) = cur.take() {
                sections.insert(c, buf.join("\n"));
            }
            cur = diff::parse_diff_git_path(line);
            buf = vec![line];
        } else if cur.is_some() {
            buf.push(line);
        }
    }
    if let Some(c) = cur.take() {
        sections.insert(c, buf.join("\n"));
    }
    sections
}

/// Rename-only или нулевой контентный diff (только метаданные).
/// Пустая секция (falsy в JS) → false.
pub fn is_rename_only_section(section: Option<&str>) -> bool {
    let Some(section) = section else { return false };
    if section.is_empty() {
        return false;
    }
    // `rename from` сам по себе rename-only НЕ делает: «переименовал и поменял логику»
    // приходит с хунками и обязан ревьюиться (RFC 17). Rename без правок либо
    // `similarity index 100%`, либо секция без строк `+`/`-` — оба случая ниже.
    static RE_SIMILARITY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    if RE_SIMILARITY
        .get_or_init(|| regex::Regex::new(r"(?m)^similarity index 100(?:\.0*)?%").unwrap())
        .is_match(section)
    {
        return true;
    }
    for line in section.split('\n') {
        if line.starts_with('+') && !line.starts_with("+++") {
            return false;
        }
        if line.starts_with('-') && !line.starts_with("---") {
            return false;
        }
    }
    true
}

/// Класс файла с учётом rename-only секции diff.
pub fn classify_changed_file(path: &str, section: Option<&str>) -> &'static str {
    if is_rename_only_section(section) {
        return "skip";
    }
    classify_file_path(path)
}

// ---------------------------------------------------------------------------
// Маршрут PR
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct FileClass {
    pub path: String,
    #[serde(rename = "class")]
    pub class: String,
}

/// Маршрут PR: skip | light | full (причина — для лога).
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteResult {
    pub route: String,
    pub reason: String,
    pub files: Vec<FileClass>,
}

fn route_result(route: &str, reason: &str, files: Vec<FileClass>) -> RouteResult {
    RouteResult {
        route: route.to_string(),
        reason: reason.to_string(),
        files,
    }
}

/// Маршрут PR: skip | light | full.
/// - skip: lockfile / generated / rename-only — без LLM
/// - light: docs-only (+ skip-класс) — без LLM, merge не блокируется по коду
/// - full: server/shared/client logic или смешанный код — полный pipeline
pub fn route_diff(changed_paths: &[String], diff: &str, enabled: bool) -> RouteResult {
    if !enabled {
        return route_result("full", "router выключен (REVIEW_ROUTER=0)", Vec::new());
    }
    if changed_paths.is_empty() {
        return route_result("full", "нет файлов в diff", Vec::new());
    }

    let sections = extract_file_diff_sections(diff);
    let files: Vec<FileClass> = changed_paths
        .iter()
        .map(|path| FileClass {
            class: classify_changed_file(path, sections.get(path.as_str()).map(String::as_str)).to_string(),
            path: path.clone(),
        })
        .collect();

    let has_logic = files.iter().any(|f| f.class == "logic");
    let classes: Vec<&str> = files.iter().map(|f| f.class.as_str()).collect();

    if classes.iter().all(|&c| c == "skip") {
        return route_result("skip", "lockfile / generated / rename-only", files);
    }
    if !has_logic && classes.iter().all(|&c| c == "docs" || c == "skip") {
        return route_result("light", "docs-only", files);
    }
    if has_logic {
        return route_result("full", "server/shared/client или иной код", files);
    }
    if classes
        .iter()
        .all(|&c| matches!(c, "skip" | "docs" | "config" | "other"))
    {
        return route_result("light", "нет logic-путей (config/docs/skip)", files);
    }
    route_result("full", "смешанный diff", files)
}

/// Строка лога маршрута: `full (причина): путь→класс, ...`.
pub fn format_route_log(route: &RouteResult) -> String {
    let breakdown = route
        .files
        .iter()
        .map(|f| format!("{}→{}", f.path, f.class))
        .collect::<Vec<_>>()
        .join(", ");
    if breakdown.is_empty() {
        format!("{} ({})", route.route, route.reason)
    } else {
        format!("{} ({}): {}", route.route, route.reason, breakdown)
    }
}

// ---------------------------------------------------------------------------
// Тесты — ключевые случаи из test/pr-review.eval.test.mjs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_file_path_classes() {
        assert_eq!(classify_file_path("package-lock.json"), "skip");
        assert_eq!(classify_file_path("app/yarn.lock"), "skip");
        assert_eq!(classify_file_path("dist/bundle.js"), "skip");
        assert_eq!(classify_file_path("node_modules/x/index.js"), "skip");
        assert_eq!(classify_file_path("src/x.generated.ts"), "skip");
        assert_eq!(classify_file_path("docs/guide.md"), "docs");
        assert_eq!(classify_file_path("src/docs/inner.md"), "docs");
        assert_eq!(classify_file_path("README.ru.md"), "docs");
        assert_eq!(classify_file_path("app/server/api.ts"), "logic");
        assert_eq!(classify_file_path("shared/foo.ts"), "logic");
        assert_eq!(classify_file_path("client/x.tsx"), "logic");
        assert_eq!(classify_file_path("src/deep/util.ts"), "logic");
        assert_eq!(classify_file_path("config.yml"), "config");
        assert_eq!(classify_file_path(".env.example"), "config");
        assert_eq!(classify_file_path("Cargo.toml"), "config");
        assert_eq!(classify_file_path("LICENSE"), "other");
        assert_eq!(classify_file_path("assets/logo.png"), "other");
    }

    #[test]
    fn classify_file_path_lock_suffix_and_case() {
        // LOCKFILE-регекс регистронезависим, а вот `path.endsWith('.lock')` в JS
        // регистрозависимый — COMPOSE.LOCK не skip (паритет с lib/router.mjs).
        assert_eq!(classify_file_path("app/COMPOSE.LOCK"), "other");
        assert_eq!(classify_file_path("app/composer.lock"), "skip");
        assert_eq!(classify_file_path("APP/SERVER/API.TS"), "logic");
        assert_eq!(classify_file_path("DOCS/GUIDE.MD"), "docs");
    }

    #[test]
    fn extract_sections_quoted_and_plain() {
        let d = "diff --git \"a/Price copy/f.ts\" \"b/Price copy/f.ts\"\n--- \"a/Price copy/f.ts\"\n+++ \"b/Price copy/f.ts\"\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/plain.ts b/plain.ts\n--- a/plain.ts\n+++ b/plain.ts\n@@ -1 +1 @@\n-a\n+b";
        let sections = extract_file_diff_sections(d);
        let keys: Vec<&String> = sections.keys().collect();
        assert_eq!(keys, vec!["Price copy/f.ts", "plain.ts"]);
        assert!(sections["Price copy/f.ts"].starts_with("diff --git \"a/Price copy/f.ts\""));
        assert!(sections["plain.ts"].contains("+b"));
    }

    #[test]
    fn extract_sections_preserves_map_semantics() {
        // повторный заголовок того же пути перезаписывает секцию на прежней позиции
        let d = "diff --git a/x.ts b/x.ts\n+one\ndiff --git a/y.ts b/y.ts\n+two\ndiff --git a/x.ts b/x.ts\n+three";
        let sections = extract_file_diff_sections(d);
        let keys: Vec<&String> = sections.keys().collect();
        assert_eq!(keys, vec!["x.ts", "y.ts"]);
        assert!(sections["x.ts"].contains("+three"));
        assert!(!sections["x.ts"].contains("+one"));
    }

    #[test]
    fn rename_only_section_detection() {
        let rename = "diff --git a/app/shared/old.ts b/app/shared/new.ts\nsimilarity index 100%\nrename from app/shared/old.ts\nrename to app/shared/new.ts\n";
        assert!(is_rename_only_section(Some(rename)));
        assert!(is_rename_only_section(Some(
            "similarity index 100.0000%\nrename from a\nrename to b"
        )));
        assert!(!is_rename_only_section(Some("@@ -1 +1 @@\n-old\n+new")));
        assert!(!is_rename_only_section(Some("+add only")));
        assert!(!is_rename_only_section(None));
        assert!(!is_rename_only_section(Some("")));
        // rename без хунков — rename-only при любой similarity
        assert!(is_rename_only_section(Some("similarity index 90%\nrename from a\nrename to b")));
        // rename с правками — ревьюится (RFC 17)
        assert!(!is_rename_only_section(Some(
            "similarity index 90%\nrename from a.ts\nrename to b.ts\n--- a/a.ts\n+++ b/b.ts\n@@ -1 +1 @@\n-return check(user);\n+return true;"
        )));
    }

    #[test]
    fn route_diff_routes() {
        let lock = "diff --git a/package-lock.json b/package-lock.json\n+++ b/package-lock.json\n@@ -1 +1 @@\n+  \"version\": \"2\"";
        assert_eq!(route_diff(&["package-lock.json".to_string()], lock, true).route, "skip");

        let docs = "diff --git a/docs/guide.md b/docs/guide.md\n+++ b/docs/guide.md\n@@ -1 +1 @@\n+# Title";
        assert_eq!(route_diff(&["docs/guide.md".to_string()], docs, true).route, "light");

        let server = "diff --git a/app/server/api.ts b/app/server/api.ts\n+++ b/app/server/api.ts\n@@ -1 +1 @@\n+export const x = 1;";
        assert_eq!(route_diff(&["app/server/api.ts".to_string()], server, true).route, "full");

        // docs + logic → full; rename-only → skip; router off → full
        let mixed = route_diff(
            &["docs/a.md".to_string(), "app/server/x.ts".to_string()],
            "diff",
            true,
        );
        assert_eq!(mixed.route, "full");
        let rename = "diff --git a/app/shared/old.ts b/app/shared/new.ts\nsimilarity index 100%\nrename from app/shared/old.ts\nrename to app/shared/new.ts\n";
        assert_eq!(route_diff(&["app/shared/new.ts".to_string()], rename, true).route, "skip");
        assert_eq!(route_diff(&["package-lock.json".to_string()], "", false).route, "full");
        assert_eq!(route_diff(&[], "", true).route, "full");

        // config/docs/skip без logic → light; docs + skip → light
        assert_eq!(
            route_diff(
                &["config.yml".to_string(), "docs/a.md".to_string()],
                "diff",
                true
            )
            .route,
            "light"
        );
    }

    #[test]
    fn format_route_log_output() {
        let r = route_diff(
            &["config.yml".to_string(), "docs/a.md".to_string()],
            "diff",
            true,
        );
        assert_eq!(
            format_route_log(&r),
            "light (нет logic-путей (config/docs/skip)): config.yml→config, docs/a.md→docs"
        );
        let docs_only = route_diff(
            &["package-lock.json".to_string(), "docs/a.md".to_string()],
            "diff",
            true,
        );
        assert_eq!(
            format_route_log(&docs_only),
            "light (docs-only): package-lock.json→skip, docs/a.md→docs"
        );
        let off = route_diff(&["x.ts".to_string()], "diff", false);
        assert_eq!(format_route_log(&off), "full (router выключен (REVIEW_ROUTER=0))");
    }
}
