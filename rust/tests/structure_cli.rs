//! Чёрноящичные тесты `pr-review structure` (RFC 20, T22): артефакты подкоманды
//! сверяются с эталонным JS bin/pr-review-structure.mjs на одной и той же фикстуре
//! (`tests/fixtures/structure_cli/repo`: граф, отчёт графа, LCOV, obligations,
//! mutation-отчёт, исходник теста, профиль `.ai-reviewer.json`).
//!
//! Эталон считается вживую, а не берётся из файла: `configHash` (а через него
//! receipts) зависит от `layout.baseDir` профиля — абсолютного пути каталога, где
//! лежит профиль, — и от абсолютных путей входов в `uri`. Обе реализации гоняются
//! в одном временном каталоге, и различаются у них только часы: поля
//! `generatedAt`/`observedAt`/`producedAt` маскируются перед сравнением.
//! JS запускается со всеми `AI_REVIEWER_*_FORCE_JS=1`, иначе обёртки structure-*
//! позвали бы собранные Rust-бинари и тест сравнивал бы Rust с Rust.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;

/// Корень пакета (rust/..): там lib/ и bin/ эталона.
fn pkg_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Копия фикстуры во временном каталоге: подкоманда пишет артефакты внутрь корня.
fn fixture_copy() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("repo");
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/structure_cli/repo"), &dst);
    // getcwd отдаёт realpath (/private/var на macOS) — сравниваем в нём же
    let dst = dst.canonicalize().unwrap();
    (tmp, dst)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Окружение без перекрытий structure-конфига и профиля с машины разработчика.
fn clean_env(cmd: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy().into_owned();
        if key.starts_with("REVIEW_") || key == "GRAPH" || key == "GRAPHIFY_REPORT" || key == "STRUCTURE_EXPORT_ROOT" {
            cmd.env_remove(&key);
        }
    }
}

/// Имена `AI_REVIEWER_*_FORCE_JS` из lib/*.mjs.
fn force_js_vars() -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(pkg_root().join("lib")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("mjs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let mut rest = src.as_str();
        while let Some(i) = rest.find("AI_REVIEWER_") {
            let tail = &rest[i..];
            let end = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(tail.len());
            let name = &tail[..end];
            if name.ends_with("_FORCE_JS") && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
            rest = &tail[end.max(1)..];
        }
    }
    names
}

fn run_js(root: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new("node");
    cmd.arg(pkg_root().join("bin/pr-review-structure.mjs")).args(args).current_dir(root);
    clean_env(&mut cmd);
    for name in force_js_vars() {
        cmd.env(name, "1");
    }
    cmd.output().expect("node нужен эталону: пакет — Node-пакет, npm test идёт рядом")
}

fn run_rs(root: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::cargo_bin("pr-review").unwrap();
    cmd.arg("structure").args(args).current_dir(root);
    clean_env(&mut cmd);
    cmd.output().unwrap()
}

const INPUTS: [&str; 16] = [
    "--graph", "graph/graph.json",
    "--graph-report", "graph/GRAPH_REPORT.md",
    "--obligations", "obligations.json",
    "--mutation-report", "mutation.json",
    "--head-sha", "abcdef1234567890",
    "--branch", "main",
    "--test-head-sha", "abcdef1234567890",
    "--test-run-id", "run-1",
];

fn outputs(dir: &str) -> Vec<String> {
    [
        ("--facts-out", "facts.json"),
        ("--assessment-out", "assessment.json"),
        ("--property-drafts-out", "drafts.json"),
        ("--drift-out", "drift.json"),
        ("--markdown", "summary.md"),
    ]
    .iter()
    .flat_map(|(flag, name)| [flag.to_string(), format!("{dir}/{name}")])
    .collect()
}

/// Поля времени прогона — единственное, чем обязаны различаться два запуска.
fn mask_clock(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if matches!(key.as_str(), "generatedAt" | "observedAt" | "producedAt") && value.is_string() {
                    *value = Value::String("<clock>".into());
                } else {
                    mask_clock(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_clock),
        _ => {}
    }
}

fn read_json(path: &Path) -> Value {
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    mask_clock(&mut v);
    v
}

fn args_with(extra: &[String]) -> Vec<&str> {
    INPUTS.iter().copied().chain(extra.iter().map(String::as_str)).collect()
}

#[test]
fn artifacts_match_js_on_fixture() {
    let (_tmp, root) = fixture_copy();
    let js_out = outputs("out-js");
    let rs_out = outputs("out-rs");
    let js = run_js(&root, &args_with(&js_out));
    let rs = run_rs(&root, &args_with(&rs_out));
    assert_eq!(js.status.code(), Some(0), "JS: {}", String::from_utf8_lossy(&js.stderr));
    assert_eq!(rs.status.code(), Some(0), "Rust: {}", String::from_utf8_lossy(&rs.stderr));

    for name in ["facts.json", "assessment.json", "drafts.json", "drift.json"] {
        let expected = read_json(&root.join("out-js").join(name));
        let actual = read_json(&root.join("out-rs").join(name));
        assert_eq!(actual, expected, "{name} расходится с JS");
    }
    // facts.json — побайтно после маскировки часов: порядок ключей и форматирование
    let text = |p: &str| {
        let raw = std::fs::read_to_string(root.join(p)).unwrap();
        let mut v: Value = serde_json::from_str(&raw).unwrap();
        mask_clock(&mut v);
        serde_json::to_string(&v).unwrap()
    };
    assert_eq!(text("out-rs/facts.json"), text("out-js/facts.json"));
    let facts_raw = std::fs::read_to_string(root.join("out-rs/facts.json")).unwrap();
    assert!(facts_raw.starts_with("{\n  \"") && facts_raw.ends_with("}\n"), "формат JSON.stringify(_, null, 2) + \\n");

    // markdown от того же owner (toolName профиля фикстуры)
    let md = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();
    assert_eq!(md("out-rs/summary.md"), md("out-js/summary.md"));
    assert!(md("out-rs/summary.md").contains("fx-review"));

    // текстовая сводка: те же строки с точностью до каталога вывода
    let stdout = |o: &Output, dir: &str| String::from_utf8_lossy(&o.stdout).replace(dir, "OUT");
    assert_eq!(stdout(&rs, "out-rs"), stdout(&js, "out-js"));
    assert_eq!(String::from_utf8_lossy(&rs.stderr), String::from_utf8_lossy(&js.stderr));
}

#[test]
fn json_summary_matches_js() {
    let (_tmp, root) = fixture_copy();
    let mut js_args = outputs("out-js");
    js_args.extend(["--json".to_string(), "--scope".to_string(), "src".to_string()]);
    let mut rs_args = outputs("out-rs");
    rs_args.extend(["--json".to_string(), "--scope".to_string(), "src".to_string()]);
    let js = run_js(&root, &args_with(&js_args));
    let rs = run_rs(&root, &args_with(&rs_args));
    assert_eq!(rs.status.code(), Some(0), "Rust: {}", String::from_utf8_lossy(&rs.stderr));
    let summary = |o: &Output, dir: &str| String::from_utf8_lossy(&o.stdout).replace(dir, "OUT");
    let rs_text = summary(&rs, "out-rs");
    assert_eq!(rs_text, summary(&js, "out-js"));
    let parsed: Value = serde_json::from_str(&rs_text).unwrap();
    assert_eq!(parsed["written"].as_array().unwrap().len(), 5);
}

#[test]
fn default_output_paths_under_export_dir() {
    let (_tmp, root) = fixture_copy();
    let rs = run_rs(&root, &INPUTS);
    assert_eq!(rs.status.code(), Some(0), "Rust: {}", String::from_utf8_lossy(&rs.stderr));
    for name in [
        "structure-facts.json",
        "structure-assessment.json",
        "property-drafts.json",
        "structure-drift.json",
        "code-structure.md",
    ] {
        assert!(root.join(".ai-review/structure").join(name).is_file(), "нет {name}");
    }
}

#[test]
fn unknown_option_exits_2_with_usage() {
    let (_tmp, root) = fixture_copy();
    let rs = run_rs(&root, &["--bogus-flag"]);
    assert_eq!(rs.status.code(), Some(2));
    let err = String::from_utf8_lossy(&rs.stderr);
    assert!(err.contains("unknown option: --bogus-flag"), "{err}");
    assert!(err.contains("Usage: pr-review-structure [options]"), "{err}");
    let js = run_js(&root, &["--bogus-flag"]);
    assert_eq!(String::from_utf8_lossy(&js.stderr), err);
}

#[test]
fn short_help_prints_usage() {
    let (_tmp, root) = fixture_copy();
    let rs = run_rs(&root, &["-h"]);
    assert_eq!(rs.status.code(), Some(0));
    let js = run_js(&root, &["-h"]);
    assert_eq!(rs.stdout, js.stdout);
}

#[test]
fn no_readable_source_exits_1() {
    let (_tmp, root) = fixture_copy();
    // дефолтный --lcov (coverage/lcov.info из профиля) тоже источник — убираем
    std::fs::remove_file(root.join("coverage/lcov.info")).unwrap();
    let rs = run_rs(&root, &["--graph", "missing.json"]);
    assert_eq!(rs.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&rs.stderr),
        "[pr-review.structure] no readable structure source: provide --graph, --scip or --lcov\n"
    );
}

#[test]
fn output_outside_repo_root_is_refused() {
    let (tmp, root) = fixture_copy();
    let escape = tmp.path().join("escape.json");
    let rs = run_rs(&root, &args_with(&["--facts-out".into(), "../escape.json".into()]));
    assert_eq!(rs.status.code(), Some(1), "{}", String::from_utf8_lossy(&rs.stderr));
    assert!(!escape.exists(), "артефакт записан за корнем репозитория");
}
