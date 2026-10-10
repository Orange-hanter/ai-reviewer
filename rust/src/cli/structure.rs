//! Подкоманда `pr-review structure` — порт bin/pr-review-structure.mjs (RFC 16, T22).
//!
//! Поток как в JS: разбор argv → профиль (`REVIEW_PROFILE` → автопоиск в cwd →
//! встроенный) и `structure`-часть конфига → пути входов от `--repo-root` →
//! `structure::analyze_structure` → `structure_export::write_structure_exports`
//! с корнем `repoRoot` → сводка (`--json` — JSON, иначе строки в stdout/stderr).
//!
//! Коды возврата — свои, а не fail-open диспетчера: неизвестная опция — 2 (ошибка
//! вызывающего, печатается usage), ошибка прогона — 1, `--help` — 0. Structure
//! вне merge-гейта, поэтому exit 1 здесь ничего не блокирует.
//!
//! Разбор аргументов повторяет JS буквально, а не через clap: `--x` без значения
//! даёт `undefined`, значением опции может оказаться следующий флаг
//! (`argv[indexOf(name) + 1]`), позиционные аргументы игнорируются, неизвестный
//! `--…` — ошибка. Clap принял бы другой язык (`--x=v`, отказ на позиционных) —
//! это изменило бы контракт вызова из CI потребителя.
//!
//! Отклонения от JS:
//! - `layout` профиля уходит в `analyze_structure` как JSON того же вида, что
//!   даёт `JSON.stringify` замороженного layout в JS (RegExp → `{}`, `null` — нет
//!   регекса, плюс `baseDir`): от него считается `configHash`. У встроенного
//!   профиля `baseDir` — корень пакета (`profile::package_root`, каталог сборки);
//!   у бинаря, собранного из чекаута пакета, он совпадает с JS, у prebuilt-бинаря —
//!   нет, и `configHash` тогда совпадает с JS только при профиле из файла;
//! - тексты системных ошибок (чтение `--previous-facts`, разбор JSON) — от Rust std
//!   и serde_json, а не от Node; код возврата тот же (1).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

use crate::config::load_review_config_parts;
use crate::profile::Profile;
use crate::structure::{analyze_structure, RealStructureIo};
use crate::structure_export::{js_json_pretty, write_structure_exports, ExportOptions};
use crate::util::js_resolve;

/// Аргументы — сырой argv подкоманды: разбор в [`parse_args`] по правилам JS.
/// Встроенный `--help` clap выключен — справку печатает сама подкоманда (`usage`).
#[derive(Debug, clap::Args)]
#[command(disable_help_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub argv: Vec<String>,
}

/// KNOWN_OPTIONS из bin/pr-review-structure.mjs.
const KNOWN_OPTIONS: [&str; 23] = [
    "--graph", "--graph-report", "--graph-analysis", "--scip", "--lcov", "--obligations",
    "--mutation-report", "--property-report", "--previous-facts", "--repo-root", "--scope",
    "--head-sha", "--branch", "--test-head-sha", "--test-run-id", "--facts-out",
    "--assessment-out", "--property-drafts-out", "--drift-out", "--markdown", "--trust-tier",
    "--include-ambiguous", "--json",
];

/// Разобранные аргументы; `None` — `undefined` в JS.
#[derive(Debug, Default)]
struct Parsed {
    help: bool,
    graph_path: Option<String>,
    graph_report_path: Option<String>,
    graph_analysis_path: Option<String>,
    scip_path: Option<String>,
    lcov_path: Option<String>,
    obligations_path: Option<String>,
    mutation_report_path: Option<String>,
    property_report_path: Option<String>,
    previous_facts_path: Option<String>,
    repo_root: Option<String>,
    scope: Option<String>,
    head_sha: Option<String>,
    branch_ref: Option<String>,
    test_head_sha: Option<String>,
    test_run_id: Option<String>,
    facts_path: Option<String>,
    assessment_path: Option<String>,
    property_drafts_path: Option<String>,
    drift_path: Option<String>,
    markdown_path: Option<String>,
    trust_tier: Option<String>,
    include_ambiguous: bool,
    json: bool,
}

/// parseArgs из JS. `--help` в KNOWN_OPTIONS JS нет, а поиск неизвестных идёт
/// раньше проверки `help`, поэтому `--help` даёт «unknown option» и exit 2 с usage
/// (тот же usage, только в stderr); справку в stdout с exit 0 печатает лишь `-h`.
/// Поведение сохранено ради паритета.
fn parse_args(argv: &[String]) -> std::result::Result<Parsed, String> {
    if let Some(unknown) = argv.iter().find(|a| a.starts_with("--") && !is_known(a)) {
        return Err(format!("unknown option: {unknown}"));
    }
    let value = |name: &str| -> Option<String> {
        let index = argv.iter().position(|a| a == name)?;
        argv.get(index + 1).cloned()
    };
    let has = |name: &str| argv.iter().any(|a| a == name);
    Ok(Parsed {
        help: has("--help") || has("-h"),
        graph_path: value("--graph"),
        graph_report_path: value("--graph-report"),
        graph_analysis_path: value("--graph-analysis"),
        scip_path: value("--scip"),
        lcov_path: value("--lcov"),
        obligations_path: value("--obligations"),
        mutation_report_path: value("--mutation-report"),
        property_report_path: value("--property-report"),
        previous_facts_path: value("--previous-facts"),
        repo_root: value("--repo-root"),
        scope: value("--scope"),
        head_sha: value("--head-sha"),
        branch_ref: value("--branch"),
        test_head_sha: value("--test-head-sha"),
        test_run_id: value("--test-run-id"),
        facts_path: value("--facts-out"),
        assessment_path: value("--assessment-out"),
        property_drafts_path: value("--property-drafts-out"),
        drift_path: value("--drift-out"),
        markdown_path: value("--markdown"),
        trust_tier: value("--trust-tier"),
        include_ambiguous: has("--include-ambiguous"),
        json: has("--json"),
    })
}

/// `KNOWN_OPTIONS.has(arg)`.
fn is_known(arg: &str) -> bool {
    KNOWN_OPTIONS.contains(&arg)
}

/// usage() из JS посимвольно.
fn usage() -> &'static str {
    "Usage: pr-review-structure [options]

Inputs:
  --graph <graph.json>             Graphify node-link graph
  --graph-report <report.md>       Graphify freshness report
  --scip <index.scip>              SCIP semantic index (TS/JS pilot)
  --lcov <lcov.info>               Aggregate LCOV evidence
  --obligations <obligations.json> cod-doc obligations_export.v1
  --mutation-report <report.json>  Mutation Testing Elements/Stryker report
  --property-report <report.json>  Confirmed property-run evidence
  --previous-facts <facts.json>    Prior snapshot for identity lineage

Scope and provenance:
  --repo-root <path> --scope <prefix> --head-sha <sha> --branch <ref>
  --test-head-sha <sha> --test-run-id <id> --trust-tier <tier>

Outputs:
  --facts-out <facts.json> --assessment-out <assessment.json>
  --property-drafts-out <drafts.json> --drift-out <drift.json>
  --markdown <summary.md> --json
"
}

/// `a || b` для строк: пустая строка — falsy.
fn or_str(a: &Option<String>, b: &str) -> String {
    match a.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => b.to_string(),
    }
}

/// `readable(path)`: пустой путь — false, иначе `access(path)` (F_OK).
fn readable(path: &str) -> bool {
    !path.is_empty() && Path::new(path).exists()
}

/// Путь как строка, как его отдал бы `path.resolve` в JS.
fn path_str(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

/// `JSON.stringify(cfg.profile.layout)`: RegExp сериализуется в `{}`, отсутствующий
/// регекс — `null`; порядок ключей — как у литерала в lib/profile.mjs (на хеш не
/// влияет: canonicalJson сортирует ключи).
fn layout_json(profile: &Profile) -> Value {
    let layout = profile.layout();
    let re = |present: bool| if present { json!({}) } else { Value::Null };
    json!({
        "appDir": layout.app_dir,
        "layerDirs": layout.layer_dirs,
        "appPrefix": re(layout.app_prefix().is_some()),
        "lintable": re(layout.lintable().is_some()),
        "sensitive": re(layout.sensitive().is_some()),
        "e2eSpec": re(layout.e2e_spec().is_some()),
        "logic": re(layout.logic().is_some()),
        "baseDir": path_str(layout.base_dir.clone()),
    })
}

/// Переменные окружения процесса в виде, который ждёт `load_review_config_parts`.
fn env_map() -> Map<String, Value> {
    std::env::vars_os()
        .map(|(k, v)| {
            (k.to_string_lossy().into_owned(), Value::String(v.to_string_lossy().into_owned()))
        })
        .collect()
}

pub fn run(args: Args) -> Result<i32> {
    let parsed = match parse_args(&args.argv) {
        Ok(p) => p,
        Err(message) => {
            eprintln!("[pr-review.structure] {message}");
            eprintln!("{}", usage());
            return Ok(2);
        }
    };
    if parsed.help {
        println!("{}", usage());
        return Ok(0);
    }
    match execute(&parsed) {
        Ok(()) => Ok(0),
        Err(e) => {
            eprintln!("[pr-review.structure] {e:#}");
            // usage-ошибки ушли раньше с exit 2 — здесь только ошибки прогона
            Ok(1)
        }
    }
}

fn execute(args: &Parsed) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let os_env: std::collections::HashMap<String, String> = env_map()
        .into_iter()
        .map(|(k, v)| (k, v.as_str().unwrap_or("").to_string()))
        .collect();
    let profile = Profile::resolve(&os_env, &cwd, |line| eprintln!("[ai-review] {line}"))?;
    let cfg = load_review_config_parts(&env_map(), &profile.config_input());
    let structure = cfg.get("structure").cloned().unwrap_or(json!({}));
    let s = |key: &str| -> String { structure.get(key).and_then(Value::as_str).unwrap_or("").to_string() };

    let repo_root = js_resolve(&cwd, &or_str(&args.repo_root, &path_str(cwd.clone())));
    let output_dir = js_resolve(&repo_root, &s("exportDir"));
    let resolve_root = |p: &str| path_str(js_resolve(&repo_root, p));
    let graph_path = resolve_root(&or_str(&args.graph_path, &s("graph")));
    let graph_report_path = resolve_root(&or_str(&args.graph_report_path, &s("graphReport")));
    let graph_analysis_path = resolve_root(&or_str(&args.graph_analysis_path, &s("graphAnalysis")));
    // опциональный вход: отсутствующий путь → '', присутствующий — resolve от repoRoot
    let optional_input = |cli: &Option<String>, key: &str| -> String {
        let value = or_str(cli, &s(key));
        if value.is_empty() { String::new() } else { resolve_root(&value) }
    };
    let scip_path = optional_input(&args.scip_path, "scip");
    let lcov_path = optional_input(&args.lcov_path, "lcov");
    let obligations_path = optional_input(&args.obligations_path, "obligations");
    let mutation_report_path = optional_input(&args.mutation_report_path, "mutationReport");
    let property_report_path = optional_input(&args.property_report_path, "propertyReport");

    if !readable(&graph_path) && !readable(&scip_path) && !readable(&lcov_path) {
        return Err(anyhow!("no readable structure source: provide --graph, --scip or --lcov"));
    }
    let previous_facts = match args.previous_facts_path.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => {
            let text = std::fs::read_to_string(js_resolve(&repo_root, p))?;
            serde_json::from_str::<Value>(&text)?
        }
        None => Value::Null,
    };
    let if_readable = |p: &str| if readable(p) { p.to_string() } else { String::new() };

    let mut options = Map::new();
    options.insert("repoRoot".into(), json!(path_str(repo_root.clone())));
    options.insert("graphPath".into(), json!(graph_path));
    options.insert("graphReportPath".into(), json!(graph_report_path));
    options.insert("graphAnalysisPath".into(), json!(graph_analysis_path));
    options.insert("scipPath".into(), json!(if_readable(&scip_path)));
    options.insert("lcovPath".into(), json!(if_readable(&lcov_path)));
    options.insert("obligationsPath".into(), json!(if_readable(&obligations_path)));
    options.insert("mutationReportPath".into(), json!(if_readable(&mutation_report_path)));
    options.insert("propertyReportPath".into(), json!(if_readable(&property_report_path)));
    options.insert("previousFacts".into(), previous_facts);
    // args.scope ?? structure.scope: пустая строка из CLI остаётся пустой
    let scope = args.scope.clone().unwrap_or_else(|| s("scope"));
    options.insert("scope".into(), json!(scope));
    // undefined в JS — ключа нет
    for (key, value) in [
        ("headSha", &args.head_sha),
        ("branchRef", &args.branch_ref),
        ("testHeadSha", &args.test_head_sha),
        ("testRunId", &args.test_run_id),
    ] {
        if let Some(v) = value {
            options.insert(key.into(), json!(v));
        }
    }
    let trust_tier = match args.trust_tier.as_deref() {
        Some(t) if !t.is_empty() => json!(t),
        _ => structure.get("trustTier").cloned().unwrap_or(Value::Null),
    };
    options.insert("trustTier".into(), trust_tier);
    let include_ambiguous = args.include_ambiguous
        || structure.get("includeAmbiguous") == Some(&Value::Bool(true));
    options.insert("includeAmbiguous".into(), json!(include_ambiguous));
    for key in ["maxEntities", "maxEdges", "thresholds"] {
        options.insert(key.into(), structure.get(key).cloned().unwrap_or(Value::Null));
    }
    options.insert("layout".into(), layout_json(&profile));

    let result = analyze_structure(&Value::Object(options), &RealStructureIo)?;

    let out_path = |cli: &Option<String>, name: &str| -> Option<String> {
        let default = path_str(js_resolve(&output_dir, name));
        Some(resolve_root(&or_str(cli, &default)))
    };
    let written = write_structure_exports(
        &result,
        &ExportOptions {
            facts_path: out_path(&args.facts_path, "structure-facts.json"),
            assessment_path: out_path(&args.assessment_path, "structure-assessment.json"),
            property_drafts_path: out_path(&args.property_drafts_path, "property-drafts.json"),
            drift_path: out_path(&args.drift_path, "structure-drift.json"),
            markdown_path: out_path(&args.markdown_path, "code-structure.md"),
            owner: Some(profile.tool_name.clone()),
            root_dir: Some(repo_root.clone()),
        },
    )?;

    let len = |v: Option<&Value>| v.and_then(Value::as_array).map_or(0, Vec::len);
    let facts = &result["facts"];
    let assessment = &result["assessment"];
    let entities = len(facts["facts"].get("entities"));
    let dependencies = len(facts["facts"].get("dependencies"));
    let scenarios = len(assessment["assessments"].get("contractScenarios"));
    let hints = len(assessment.get("hints"));
    let written: Vec<String> = written.into_iter().map(path_str).collect();
    if args.json {
        let mut summary = Map::new();
        // undefined-поля JSON.stringify опускает
        if let Some(fp) = facts.get("fingerprint") {
            summary.insert("factsFingerprint".into(), fp.clone());
        }
        if let Some(fp) = assessment.get("fingerprint") {
            summary.insert("assessmentFingerprint".into(), fp.clone());
        }
        summary.insert("entities".into(), json!(entities));
        summary.insert("dependencies".into(), json!(dependencies));
        summary.insert("scenarios".into(), json!(scenarios));
        summary.insert("hints".into(), json!(hints));
        if let Some(d) = result.get("diagnostics") {
            summary.insert("diagnostics".into(), d.clone());
        }
        summary.insert("written".into(), json!(written));
        println!("{}", js_json_pretty(&Value::Object(summary)));
    } else {
        println!(
            "[pr-review.structure] {entities} entities · {dependencies} dependencies · {scenarios} scenarios · {hints} hints"
        );
        for path in &written {
            println!("[pr-review.structure] wrote {path}");
        }
        let warnings = result["diagnostics"].get("warnings").and_then(Value::as_array);
        for warning in warnings.into_iter().flatten() {
            let text = warning.as_str().map(str::to_string).unwrap_or_else(|| warning.to_string());
            eprintln!("[pr-review.structure] warning: {text}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unknown_option_is_rejected() {
        let err = parse_args(&argv(&["--bogus-flag"])).unwrap_err();
        assert_eq!(err, "unknown option: --bogus-flag");
    }

    #[test]
    fn help_is_unknown_like_in_js() {
        // KNOWN_OPTIONS в JS без --help: find находит его первым → exit 2.
        assert_eq!(parse_args(&argv(&["--help"])).unwrap_err(), "unknown option: --help");
        assert!(parse_args(&argv(&["-h"])).unwrap().help);
    }

    #[test]
    fn value_is_next_token_even_if_flag() {
        let p = parse_args(&argv(&["--graph", "--json", "pos"])).unwrap();
        assert_eq!(p.graph_path.as_deref(), Some("--json"));
        assert!(p.json);
        let p = parse_args(&argv(&["--scope"])).unwrap();
        assert_eq!(p.scope, None);
    }
}
