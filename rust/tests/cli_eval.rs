//! Чёрноящичный тест `pr-review eval` (RFC 20, T28): коды возврата так, как их
//! увидит CI. Exit 1 на проваленной фикстуре — осознанный гейт, а не fail-open.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

#[test]
fn eval_on_package_fixtures_passes() {
    Command::cargo_bin("pr-review")
        .unwrap()
        .arg("eval")
        .assert()
        .success()
        .stdout(contains("=== pr-review eval harness ==="))
        .stdout(contains("passed (100%)"))
        .stdout(contains("✅ все fixtures passed"));
}

#[test]
fn eval_json_reports_ok() {
    let out = Command::cargo_bin("pr-review").unwrap().args(["eval", "--json"]).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], serde_json::json!(true));
    assert_eq!(v["metrics"]["failed"], serde_json::json!(0));
}

#[test]
fn eval_failing_fixture_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let case = dir.path().join("must-block");
    fs::create_dir(&case).unwrap();
    fs::write(case.join("meta.json"), r#"{"expected":{"shouldBlock":true}}"#).unwrap();
    fs::write(case.join("diff.patch"), "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -0,0 +1,1 @@\n+x\n").unwrap();
    fs::write(case.join("findings.json"), r#"{"findings":[]}"#).unwrap();
    Command::cargo_bin("pr-review")
        .unwrap()
        .args(["eval", "--fixtures"])
        .arg(dir.path())
        .assert()
        .code(1)
        .stdout(contains("⛔ 1 fixture(s) FAILED"));
}

#[test]
fn eval_argv_as_in_js_unknown_flags_and_bare_fixtures() {
    // bin/pr-review-eval.mjs: argv.includes/indexOf — неизвестный флаг игнорируется,
    // `--fixtures` без значения → undefined → дефолтные фикстуры пакета.
    for args in [&["eval", "--fixtures"][..], &["eval", "--quiet"][..], &["eval", "extra", "--json"][..]] {
        Command::cargo_bin("pr-review")
            .unwrap()
            .args(args)
            .assert()
            .code(0)
            .stdout(contains("passed (100%)").or(contains("\"ok\": true")));
    }
}
