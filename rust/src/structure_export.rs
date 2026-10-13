//! Порт lib/structure-export.mjs: markdown-проекция structure-результата
//! (`build_structure_markdown`) и запись артефактов facts/assessment/property
//! drafts/drift/markdown внутри разрешённого корня (`write_structure_exports`).
//!
//! Паритет с JS:
//! - интерполяция `${x}` — `String(x)`: отсутствующее поле даёт "undefined",
//!   null — "null" (`tmpl` из report.rs);
//! - `esc`/`code` нейтрализуют разметку из репозитория и obligations cod-doc
//!   (перевод строки не становится заголовком, обратная кавычка не рвёт span);
//! - обязательные поля (`hint.subjectRefs.map`, `facts.facts.entities.map`, …)
//!   при отсутствии роняют построение, как TypeError в JS; поля с `|| []` — нет;
//! - JSON-файлы — `JSON.stringify(value, null, 2)` + "\n": ключи в порядке
//!   вставки, числа как `String(number)` у V8, поле со значением undefined
//!   (отсутствующее) из объекта выпадает;
//! - запись ограничена корнем через `util::resolve_contained_path` (symlink
//!   внутри корня, указывающий наружу, проверку не обходит).

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde_json::{Map, Value};

use crate::findings::{js_to_string, js_truthy};
use crate::report::{js_slice_utf16, tmpl};
use crate::structure_evidence::{get, js_json_string, js_number_to_string, value_key};
use crate::util::resolve_contained_path;

/// `ratio == null ? 'n/a' : `${Math.round(ratio * 1000) / 10}%``.
fn percent(ratio: Option<&Value>) -> String {
    match ratio {
        None | Some(Value::Null) => "n/a".into(),
        Some(v) => {
            let x = crate::findings::js_to_number(v).unwrap_or(f64::NAN);
            // Math.round: округление к +∞ на половине (в отличие от f64::round)
            let rounded = (x * 1000.0 + 0.5).floor();
            format!("{}%", js_number_to_string(rounded / 10.0))
        }
    }
}

/// `String(value ?? '')`.
fn str_or_empty(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_to_string_full(v),
    }
}

/// `String(v)` с массивами как `Array.prototype.join(',')`.
fn js_to_string_full(v: &Value) -> String {
    match v {
        Value::Array(a) => js_join(a, ","),
        other => js_to_string(other),
    }
}

/// `Array.prototype.join`: null/undefined-элементы — пустая строка.
fn js_join(items: &[Value], sep: &str) -> String {
    items
        .iter()
        .map(|item| match item {
            Value::Null => String::new(),
            other => js_to_string_full(other),
        })
        .collect::<Vec<_>>()
        .join(sep)
}

/// Текст из репозитория и obligations cod-doc: перевод строки не должен становиться
/// заголовком, а обратная кавычка — рвать разметку.
fn esc_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            '`' => out.push_str("\\`"),
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn esc(value: Option<&Value>) -> String {
    esc_str(&str_or_empty(value))
}

/// Значение внутри code span: длина ограды — по самой длинной серии кавычек в значении.
fn code(value: Option<&Value>) -> String {
    let text: String = str_or_empty(value)
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    if longest > 0 {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// Документ — проекция; молча терять из неё строки нельзя, иначе он врёт о полноте.
fn truncation_note(len: usize, max: usize, what: &str) -> Vec<String> {
    if len > max {
        vec![
            String::new(),
            format!("_Showing {max} of {len} {what}; the JSON assessment holds the rest._"),
            String::new(),
        ]
    } else {
        Vec::new()
    }
}

/// TypeError обращения к свойству undefined/null, как у V8.
fn read_prop_error(v: Option<&Value>, prop: &str) -> anyhow::Error {
    let what = match v {
        Some(Value::Null) => "null",
        _ => "undefined",
    };
    anyhow::anyhow!("Cannot read properties of {what} (reading '{prop}')")
}

/// Обязательный объект (`a.b.c` в JS): null/undefined — TypeError.
fn req<'a>(v: Option<&'a Value>, prop: &str) -> Result<&'a Value> {
    match v {
        None | Some(Value::Null) => Err(read_prop_error(v, prop)),
        Some(x) => Ok(x),
    }
}

/// Обязательный массив, на котором зовут `.map`/`.length`/`.slice`.
fn req_array<'a>(v: Option<&'a Value>, method: &str, what: &str) -> Result<&'a [Value]> {
    match v {
        Some(Value::Array(a)) => Ok(a),
        None | Some(Value::Null) => Err(read_prop_error(v, method)),
        Some(_) => bail!("{what}.{method} is not a function"),
    }
}

/// `xs || []` с последующим `.length`/`.slice`: falsy — пусто.
fn array_or_empty<'a>(v: Option<&'a Value>, what: &str) -> Result<&'a [Value]> {
    match v {
        Some(Value::Array(a)) => Ok(a),
        Some(x) if js_truthy(x) => bail!("{what}.slice is not a function"),
        _ => Ok(&[]),
    }
}

/// Опции `buildStructureMarkdown`: owner по умолчанию 'ai-reviewer',
/// generatedAt — `result.facts.generatedAt`.
#[derive(Debug, Clone, Default)]
pub struct MarkdownOptions {
    pub owner: Option<String>,
    pub generated_at: Option<String>,
}

pub fn build_structure_markdown(result: &Value, options: &MarkdownOptions) -> Result<String> {
    let facts = req(get(result, "facts"), "facts")?;
    let assessment = req(get(result, "assessment"), "assessment")?;
    let assessments = req(get(assessment, "assessments"), "assessments")?;
    let quality = req(get(assessments, "dataQuality"), "dataQuality")?;
    let coverage = req(get(assessments, "codeCoverage"), "codeCoverage")?;
    let totals = get(coverage, "totals");
    let hints = array_or_empty(get(assessment, "hints"), "hints")?;
    let scenarios = array_or_empty(get(assessments, "contractScenarios"), "scenarios")?;
    let inner = req(get(facts, "facts"), "facts")?;
    let deps = array_or_empty(get(inner, "dependencies"), "deps")?;
    let entity_list = req_array(get(inner, "entities"), "map", "facts.facts.entities")?;
    let provenance = req(get(facts, "provenance"), "provenance")?;

    // new Map(entities.map(e => [e.observedId, e])): ключи по SameValueZero
    // (undefined совпадает с undefined, объекты — только по идентичности),
    // при дубликате побеждает последний
    let map_key = |v: Option<&Value>| -> Option<String> {
        match v {
            None => Some("undefined".into()),
            Some(Value::Object(_) | Value::Array(_)) => None,
            Some(scalar) => Some(value_key(scalar)),
        }
    };
    let entity_by_ref = |entity_ref: Option<&Value>| -> Option<&Value> {
        let key = map_key(entity_ref)?;
        entity_list
            .iter()
            .rev()
            .find(|entity| map_key(get(entity, "observedId")).as_deref() == Some(key.as_str()))
    };

    let owner = options.owner.clone().unwrap_or_else(|| "ai-reviewer".into());
    let generated_at = match &options.generated_at {
        Some(s) => s.clone(),
        None => tmpl(get(facts, "generatedAt")),
    };
    let head_sha = tmpl(get(provenance, "headSha"));

    let mut lines: Vec<String> = vec![
        "---".into(),
        "type: architecture".into(),
        "status: draft".into(),
        format!("owner: {owner}"),
        "source_of_truth: false".into(),
        format!("last_updated: {}", js_slice_utf16(&generated_at, 10)),
        format!("head_sha: {head_sha}"),
        format!("facts_fingerprint: {}", tmpl(get(facts, "fingerprint"))),
        format!("assessment_fingerprint: {}", tmpl(get(assessment, "fingerprint"))),
        "generated_by: ai-reviewer".into(),
        "---".into(),
        String::new(),
        "# Code structure, contracts and test scenarios".into(),
        String::new(),
        "## Data quality".into(),
        String::new(),
        format!(
            "- Head: `{head_sha}` on `{}`",
            tmpl(get(provenance, "branchRef"))
        ),
        format!("- Graph: **{}**", tmpl(get(provenance, "graphStatus"))),
        format!(
            "- Temporal alignment: **{}**",
            tmpl(get(assessment, "temporalAlignment"))
        ),
        format!(
            "- Entities: **{}**, dependencies: **{}**",
            entity_list.len(),
            deps.len()
        ),
        format!(
            "- Function joins unresolved: **{}**",
            percent(get(quality, "unresolvedJoinRate"))
        ),
        format!(
            "- Truncated: **{}**",
            if get(provenance, "truncated").is_some_and(js_truthy) { "yes" } else { "no" }
        ),
        String::new(),
    ];

    if let Some(totals) = totals.filter(|t| js_truthy(t)) {
        let pair = |key: &str| -> Result<String> {
            let item = req(get(totals, key), "hit")?;
            Ok(format!("{}/{}", tmpl(get(item, "hit")), tmpl(get(item, "found"))))
        };
        lines.extend([
            "## Coverage".into(),
            String::new(),
            format!("- Lines: **{}**", pair("lines")?),
            format!("- Functions: **{}**", pair("functions")?),
            format!("- Branches: **{}**", pair("branches")?),
            String::new(),
        ]);
    }

    lines.extend(["## Priority hints".into(), String::new()]);
    if hints.is_empty() {
        lines.extend(["No evidence-backed hints for this snapshot.".into(), String::new()]);
    } else {
        for hint in hints.iter().take(50) {
            let priority = tmpl(get(hint, "priority")).to_uppercase();
            let subjects = req_array(get(hint, "subjectRefs"), "map", "hint.subjectRefs")?;
            let evidence = req_array(get(hint, "evidence"), "length", "hint.evidence")?;
            let missing = req_array(get(hint, "missingEvidence"), "length", "hint.missingEvidence")?;
            let joined = |items: &[Value], f: fn(Option<&Value>) -> String| -> String {
                items.iter().map(|item| f(Some(item))).collect::<Vec<_>>().join(", ")
            };
            lines.extend([
                format!("### {} — {}", esc_str(&priority), esc(get(hint, "ruleId"))),
                String::new(),
                esc(get(hint, "remediation")),
                String::new(),
                format!("Subjects: {}", joined(subjects, code)),
                format!(
                    "Evidence: {}",
                    if evidence.is_empty() { "none".into() } else { joined(evidence, code) }
                ),
                format!(
                    "Missing: {}",
                    if missing.is_empty() { "none".into() } else { joined(missing, esc) }
                ),
                String::new(),
            ]);
        }
        lines.extend(truncation_note(hints.len(), 50, "hints"));
    }

    lines.extend([
        "## Contract scenarios".into(),
        String::new(),
        "| Obligation | Kind | Status | Contract | Reason |".into(),
        "|---|---|---|---|---|".into(),
    ]);
    for scenario in scenarios.iter().take(200) {
        let contracts = req_array(get(scenario, "contractRefs"), "join", "scenario.contractRefs")?;
        lines.push(format!(
            "| {} | {} | {} | {} | {} |",
            esc(get(scenario, "obligationRef")),
            esc(get(scenario, "kind")),
            esc(get(scenario, "status")),
            esc_str(&js_join(contracts, ", ")),
            esc(get(scenario, "statusReason")),
        ));
    }
    lines.extend(truncation_note(scenarios.len(), 200, "contract scenarios"));

    lines.extend([String::new(), "## Dependency hotspots".into(), String::new()]);
    let risk = req(get(assessments, "dependencyRisk"), "highFanIn")?;
    let hotspots = array_or_empty(get(risk, "highFanIn"), "hotspots")?;
    if hotspots.is_empty() {
        lines.push("No high fan-in hotspots at the configured threshold.".into());
    } else {
        lines.extend([
            "| Entity | Fan-in | Execution | Cross-boundary callers |".into(),
            "|---|---:|---|---:|".into(),
        ]);
        for hotspot in hotspots.iter().take(100) {
            let entity_ref = get(hotspot, "entityRef");
            // entity?.label || hotspot.entityRef
            let label = entity_by_ref(entity_ref)
                .and_then(|entity| get(entity, "label"))
                .filter(|label| js_truthy(label))
                .or(entity_ref);
            lines.push(format!(
                "| {} | {} | {} | {} |",
                esc(label),
                tmpl(get(hotspot, "fanIn")),
                esc(get(hotspot, "executionStatus")),
                tmpl(get(hotspot, "crossBoundaryCallers")),
            ));
        }
        lines.extend(truncation_note(hotspots.len(), 100, "hotspots"));
    }

    lines.extend([String::new(), "## Boundaries".into(), String::new()]);
    let boundaries = match get(inner, "boundaries") {
        Some(Value::Array(a)) => a.as_slice(),
        other => bail!(
            "{}",
            crate::structure_evidence::v8_not_iterable(other.unwrap_or(&Value::Null))
        ),
    };
    for boundary in boundaries {
        let paths = array_or_empty(get(boundary, "paths"), "paths")?;
        let rendered = paths.iter().map(|path| code(Some(path))).collect::<Vec<_>>().join(", ");
        lines.push(format!(
            "- **{} {}** — {}",
            esc(get(boundary, "kind")),
            esc(get(boundary, "name")),
            rendered
        ));
    }
    lines.extend([
        String::new(),
        "## Notes".into(),
        String::new(),
        "This document is a generated projection. JSON facts and assessment artifacts are the machine-readable source for this observation; cod-doc specifications remain the source of truth for intent.".into(),
        String::new(),
    ]);
    Ok(lines.join("\n"))
}

/// `JSON.stringify(value, null, 2)`.
pub fn js_json_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_pretty(value, 0, &mut out);
    out
}

fn write_pretty(value: &Value, depth: usize, out: &mut String) {
    let pad = |n: usize| "  ".repeat(n);
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_f64() {
            Some(x) if x.is_finite() => out.push_str(&js_number_to_string(x)),
            _ => out.push_str("null"),
        },
        Value::String(s) => out.push_str(&js_json_string(s)),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&pad(depth + 1));
                write_pretty(item, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&pad(depth));
            out.push(']');
        }
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            out.push_str("{\n");
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&pad(depth + 1));
                out.push_str(&js_json_string(key));
                out.push_str(": ");
                write_pretty(item, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&pad(depth));
            out.push('}');
        }
    }
}

/// Опции `writeStructureExports`. Пустой путь — как falsy в JS: файл не пишется.
/// `root_dir` по умолчанию — `STRUCTURE_EXPORT_ROOT` (непустой) или рабочий каталог.
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    pub facts_path: Option<String>,
    pub assessment_path: Option<String>,
    pub property_drafts_path: Option<String>,
    pub drift_path: Option<String>,
    pub markdown_path: Option<String>,
    pub owner: Option<String>,
    pub root_dir: Option<PathBuf>,
}

/// Пишет артефакты внутри корня; возвращает список записанных путей
/// (лексически разрешённых, как `candidate` в JS) в порядке записи.
pub fn write_structure_exports(result: &Value, options: &ExportOptions) -> Result<Vec<PathBuf>> {
    let root: Option<PathBuf> = options.root_dir.clone().or_else(|| {
        std::env::var("STRUCTURE_EXPORT_ROOT")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    });
    let mut written = Vec::new();
    let mut write = |path: &Option<String>, content: String| -> Result<()> {
        let Some(path) = path.as_deref().filter(|p| !p.is_empty()) else {
            return Ok(());
        };
        let absolute = resolve_contained_path(path, root.as_deref(), false)?;
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&absolute, content)?;
        written.push(absolute);
        Ok(())
    };
    // JSON.stringify(undefined) — undefined, `${undefined}\n` даёт "undefined\n"
    let json_text = |value: Option<&Value>| -> String {
        match value {
            Some(v) => format!("{}\n", js_json_pretty(v)),
            None => "undefined\n".into(),
        }
    };

    write(&options.facts_path, json_text(get(result, "facts")))?;
    write(&options.assessment_path, json_text(get(result, "assessment")))?;
    // аргумент writeJson вычисляется всегда, даже без propertyDraftsPath:
    // отсутствие facts/assessment роняет запись, как TypeError в JS
    let facts = req(get(result, "facts"), "fingerprint")?;
    let assessment = req(get(result, "assessment"), "obligationsRevision")?;
    let mut drafts = Map::new();
    drafts.insert("version".into(), Value::from(1));
    drafts.insert("kind".into(), Value::from("property_drafts"));
    // поля со значением undefined JSON.stringify опускает
    for (key, value) in [
        ("factsFingerprint", get(facts, "fingerprint")),
        ("obligationsRevision", get(assessment, "obligationsRevision")),
        ("properties", get(result, "propertyDrafts")),
    ] {
        if let Some(v) = value {
            drafts.insert(key.into(), v.clone());
        }
    }
    write(&options.property_drafts_path, json_text(Some(&Value::Object(drafts))))?;
    // result.drift || null
    let drift = get(result, "drift").filter(|d| js_truthy(d)).cloned().unwrap_or(Value::Null);
    write(&options.drift_path, json_text(Some(&drift)))?;
    if options.markdown_path.as_deref().is_some_and(|p| !p.is_empty()) {
        let markdown = build_structure_markdown(
            result,
            &MarkdownOptions { owner: options.owner.clone(), generated_at: None },
        )?;
        write(&options.markdown_path, format!("{markdown}\n"))?;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Эталоны сняты с lib/structure-export.mjs на том же входе
    // (writeStructureExports с owner 'team', пути внутри корня).
    const INPUT: &str = include_str!("../tests/fixtures/structure_export/input.json");
    const EXPECTED_MD: &str = include_str!("../tests/fixtures/structure_export/structure.md");
    const EXPECTED_FACTS: &str = include_str!("../tests/fixtures/structure_export/facts.json");
    const EXPECTED_ASSESSMENT: &str =
        include_str!("../tests/fixtures/structure_export/assessment.json");
    const EXPECTED_DRAFTS: &str =
        include_str!("../tests/fixtures/structure_export/property-drafts.json");
    const EXPECTED_DRIFT: &str = include_str!("../tests/fixtures/structure_export/drift.json");

    fn input() -> Value {
        serde_json::from_str(INPUT).unwrap()
    }

    fn markdown_input(hints: Vec<Value>, scenarios: Vec<Value>) -> Value {
        json!({
            "facts": {
                "generatedAt": "2026-09-03T00:00:00.000Z",
                "fingerprint": "ff",
                "provenance": { "headSha": "abc", "branchRef": "main", "graphStatus": "ok", "truncated": false },
                "facts": { "entities": [], "boundaries": [], "dependencies": [] },
            },
            "assessment": {
                "fingerprint": "aa",
                "temporalAlignment": "aligned",
                "hints": hints,
                "assessments": {
                    "dataQuality": { "unresolvedJoinRate": 0 },
                    "codeCoverage": { "totals": null },
                    "contractScenarios": scenarios,
                    "dependencyRisk": { "highFanIn": [] },
                },
            },
        })
    }

    fn opts(owner: &str) -> MarkdownOptions {
        MarkdownOptions { owner: Some(owner.into()), generated_at: None }
    }

    #[test]
    fn markdown_совпадает_с_js() {
        let md = build_structure_markdown(&input(), &opts("team")).unwrap();
        assert_eq!(format!("{md}\n"), EXPECTED_MD);
    }

    #[test]
    fn markdown_честно_пишет_об_усечении() {
        let hints = (0..61)
            .map(|i| {
                json!({
                    "priority": "high", "ruleId": format!("rule{i}"), "remediation": format!("do thing {i}"),
                    "subjectRefs": [format!("s{i}")], "evidence": [], "missingEvidence": [],
                })
            })
            .collect();
        let scenarios = (0..250)
            .map(|i| {
                json!({
                    "obligationRef": format!("o{i}"), "kind": "k", "status": "missing",
                    "contractRefs": [], "statusReason": "r",
                })
            })
            .collect();
        let md = build_structure_markdown(&markdown_input(hints, scenarios), &MarkdownOptions::default())
            .unwrap();
        assert_eq!(md.lines().filter(|l| l.starts_with("### ")).count(), 50);
        let rows = regex::Regex::new(r"^\| o\d+ ").unwrap();
        assert_eq!(md.lines().filter(|l| rows.is_match(l)).count(), 200);
        assert!(md.contains("50 of 61"), "документ обязан признать выброшенные hints");
        assert!(md.contains("200 of 250"), "документ обязан признать выброшенные сценарии");
        assert!(md.contains("owner: ai-reviewer"));
        assert!(md.contains("No high fan-in hotspots at the configured threshold."));
        assert!(!md.contains("## Coverage"), "totals: null — секции покрытия нет");
        assert!(md.contains("- Function joins unresolved: **0%**"));
    }

    #[test]
    fn markdown_нейтрализует_разметку_из_ref_и_remediation() {
        let md = build_structure_markdown(
            &markdown_input(
                vec![json!({
                    "priority": "high", "ruleId": "r|id", "remediation": "line one\n## Injected heading",
                    "subjectRefs": ["src/a`.ts"], "evidence": [], "missingEvidence": [],
                })],
                vec![],
            ),
            &MarkdownOptions::default(),
        )
        .unwrap();
        assert!(!md.lines().any(|l| l == "## Injected heading"));
        assert!(!md.lines().any(|l| l == "### HIGH — r|id"));
        assert!(md.lines().any(|l| l == "### HIGH — r\\|id"));
        let subjects = md.lines().find(|l| l.starts_with("Subjects:")).unwrap();
        assert_eq!(subjects, "Subjects: `` src/a`.ts ``");
    }

    #[test]
    fn markdown_без_обязательного_поля_падает_как_typeerror_js() {
        let mut value = markdown_input(
            vec![json!({ "priority": "high", "ruleId": "r", "remediation": "x" })],
            vec![],
        );
        let err = build_structure_markdown(&value, &MarkdownOptions::default()).unwrap_err();
        assert_eq!(err.to_string(), "Cannot read properties of undefined (reading 'map')");
        value["assessment"]["hints"] = json!([]);
        value["facts"]["facts"]["boundaries"] = Value::Null;
        assert!(build_structure_markdown(&value, &MarkdownOptions::default()).is_err());
    }

    #[test]
    fn percent_и_json_pretty_как_в_js() {
        assert_eq!(percent(None), "n/a");
        assert_eq!(percent(Some(&Value::Null)), "n/a");
        assert_eq!(percent(Some(&json!(0.12345))), "12.3%");
        assert_eq!(percent(Some(&json!(0.0005))), "0.1%"); // Math.round(0.5) = 1
        assert_eq!(percent(Some(&json!(1))), "100%");
        assert_eq!(js_json_pretty(&json!({ "a": [], "b": {}, "c": [1, { "d": null }] })),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"d\": null\n    }\n  ]\n}");
    }

    #[test]
    fn write_пишет_те_же_файлы_что_js() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let written = write_structure_exports(
            &input(),
            &ExportOptions {
                facts_path: Some("facts.json".into()),
                assessment_path: Some("a/assessment.json".into()),
                property_drafts_path: Some("property-drafts.json".into()),
                drift_path: Some("drift.json".into()),
                markdown_path: Some("structure.md".into()),
                owner: Some("team".into()),
                root_dir: Some(root.clone()),
            },
        )
        .unwrap();
        let names: Vec<_> = written
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["facts.json", "a/assessment.json", "property-drafts.json", "drift.json", "structure.md"]
        );
        let read = |name: &str| std::fs::read_to_string(root.join(name)).unwrap();
        assert_eq!(read("facts.json"), EXPECTED_FACTS);
        assert_eq!(read("a/assessment.json"), EXPECTED_ASSESSMENT);
        assert_eq!(read("property-drafts.json"), EXPECTED_DRAFTS);
        assert_eq!(read("drift.json"), EXPECTED_DRIFT);
        assert_eq!(read("structure.md"), EXPECTED_MD);
    }

    #[test]
    fn write_без_путей_ничего_не_пишет_а_drafts_опускают_undefined() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut value = input();
        value["assessment"].as_object_mut().unwrap().remove("obligationsRevision");
        value.as_object_mut().unwrap().remove("propertyDrafts");
        let options = ExportOptions { root_dir: Some(root.clone()), ..Default::default() };
        assert!(write_structure_exports(&value, &options).unwrap().is_empty());
        let options = ExportOptions {
            property_drafts_path: Some("d.json".into()),
            facts_path: Some(String::new()), // пустая строка — falsy, как в JS
            root_dir: Some(root.clone()),
            ..Default::default()
        };
        assert_eq!(write_structure_exports(&value, &options).unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("d.json")).unwrap(),
            "{\n  \"version\": 1,\n  \"kind\": \"property_drafts\",\n  \"factsFingerprint\": \"ff01\"\n}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_ограничен_корнем() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();
        let value = input();
        let with = |f: fn(&mut ExportOptions, String), path: String| {
            let mut options = ExportOptions { root_dir: Some(root.clone()), ..Default::default() };
            f(&mut options, path);
            write_structure_exports(&value, &options)
        };
        let written = write_structure_exports(
            &value,
            &ExportOptions {
                facts_path: Some(root.join("facts.json").to_string_lossy().into()),
                assessment_path: Some(root.join("nested/assessment.json").to_string_lossy().into()),
                root_dir: Some(root.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(written.len(), 2);
        assert!(written.iter().all(|p| p.starts_with(&root) && p != &root));
        let escapes = |r: Result<Vec<PathBuf>>| {
            let err = r.unwrap_err().to_string();
            assert!(err.contains("escapes allowed root"), "{err}");
        };
        let up = |name: &str| root.join("..").join(name).to_string_lossy().into_owned();
        escapes(with(|o, p| o.facts_path = Some(p), up("escape.json")));
        escapes(with(|o, p| o.assessment_path = Some(p), up("escape-assessment.json")));
        escapes(with(
            |o, p| o.markdown_path = Some(p),
            std::env::temp_dir().join("escape.md").to_string_lossy().into(),
        ));
        escapes(with(
            |o, p| o.facts_path = Some(p),
            root.join("linked/facts.json").to_string_lossy().into(),
        ));
        assert!(!outside.path().join("facts.json").exists());
        // Висячий symlink: canonicalize цели падает, ближайший существующий предок —
        // root, а запись пошла бы по ссылке и создала файл снаружи (RFC 20 SEC).
        std::os::unix::fs::symlink(outside.path().join("dangling.json"), root.join("dangling.json")).unwrap();
        escapes(with(
            |o, p| o.facts_path = Some(p),
            root.join("dangling.json").to_string_lossy().into(),
        ));
        assert!(!outside.path().join("dangling.json").exists());
        std::os::unix::fs::symlink(outside.path().join("ghost"), root.join("ghost")).unwrap();
        escapes(with(
            |o, p| o.facts_path = Some(p),
            root.join("ghost/facts.json").to_string_lossy().into(),
        ));
    }
}
