//! Порт DI-тестов `test/pr-review.pipeline.test.mjs`: весь `run_detailed` на фейках
//! `crate::testutil` — без сети и LLM. Профиль — эталонный `profiles/orakul.json`
//! (как `test/helpers/reference-profile.mjs`), корень прогона — корень пакета (как у
//! `node --test` из корня репозитория: окна контекста меряются на реальном lib/config.mjs).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;
use crate::profile::{package_root, Profile};
use crate::testutil::{CallRecord, FakeDeps};
use crate::traits::{BlastMetrics, BlastRadius, EngineApi, LoadedDiff, ReviewAllResult};

const CODE_DIFF: &str = "diff --git a/foo.ts b/foo.ts
--- a/foo.ts
+++ b/foo.ts
@@ -1,2 +1,3 @@
 const a = 1;
-const b = 2;
+const b = 3;
+export function foo() { return a; }
";

const LOCK_DIFF: &str = "diff --git a/package-lock.json b/package-lock.json
--- a/package-lock.json
+++ b/package-lock.json
@@ -1,1 +1,1 @@
-old
+new
";

const DOCS_DIFF: &str = "diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1,1 +1,1 @@
-old
+new
";

const TWO_FILE_DIFF: &str = "diff --git a/foo.ts b/foo.ts
--- a/foo.ts
+++ b/foo.ts
@@ -1,2 +1,3 @@
 const a = 1;
-const b = 2;
+const b = 3;
+export function foo() { return a; }
diff --git a/bar.ts b/bar.ts
--- a/bar.ts
+++ b/bar.ts
@@ -1,1 +1,2 @@
 const c = 1;
+export function bar() { return c; }
";

const WINDOWED_DIFF: &str = "diff --git a/lib/config.mjs b/lib/config.mjs
--- a/lib/config.mjs
+++ b/lib/config.mjs
@@ -30,1 +30,2 @@
 // контекст хунка
+const nearTheTop = 1;
@@ -400,1 +400,2 @@
 // контекст хунка
+const nearTheBottom = 2;
";

fn two_file_v2() -> String {
    TWO_FILE_DIFF.replace("const b = 3;", "const b = 4;")
}

fn orakul() -> Profile {
    Profile::load_file_at(&package_root().join("profiles/orakul.json"), &package_root()).unwrap()
}

fn tmp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("pr-review-pipe-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn review(model: &str, findings: Vec<Value>) -> ModelReview {
    ModelReview { model: model.into(), summary: Some("s".into()), findings, ..ModelReview::default() }
}

fn failed(model: &str, err: &str) -> ModelReview {
    ModelReview { model: model.into(), error: Some(err.into()), ..ModelReview::default() }
}

fn critical_review() -> ModelReview {
    review(
        "m1",
        vec![json!({ "file": "foo.ts", "line": 3, "severity": "critical", "title": "реальный баг", "body": "null pointer", "confidence": 0.9, "model": "m1" })],
    )
}

fn outage() -> Vec<ModelReview> {
    vec![failed("m1", "Ollama 500: boom internal detail"), failed("m2", "Ollama 429: limit")]
}

fn verify_all() -> Vec<Value> {
    (0..40).map(|i| json!({ "candidateIndex": i, "decision": "verified" })).collect()
}

fn read_jsonl(dir: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("pr-review-runs.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Порт `runPipeline`/`mkDeps`: env поверх базового, file-diff прогон с постингом.
struct Harness {
    fakes: FakeDeps,
    env: HashMap<String, String>,
    dry_run: bool,
    cli_head_sha: String,
    repo_files: Option<Box<RepoFilesFn>>,
    engine: Option<Box<dyn EngineApi>>,
    /// Трекер, общий с движком теста (`PipelineDeps::cost`).
    cost: Option<std::sync::Arc<CostTracker>>,
}

impl Harness {
    fn new(reviews: Vec<ModelReview>) -> Self {
        let mut fakes = FakeDeps::new();
        fakes.engine.reviews = reviews;
        let env = [
            ("OLLAMA_API_KEY", "k"),
            ("GITHUB_TOKEN", "t"),
            ("REVIEW_MODE", "consensus"),
            ("OLLAMA_MODELS", "m1,m2"),
            ("REVIEW_OBSERVABILITY", "0"),
            ("REVIEW_EXPORT", "0"),
            ("REVIEW_AUTO_TRIAGE", "0"),
            ("REVIEW_VERIFIER", "0"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        Self { fakes, env, dry_run: false, cli_head_sha: String::new(), repo_files: None, engine: None, cost: None }
    }

    fn env(mut self, k: &str, v: &str) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }

    fn obs(self, dir: &std::path::Path) -> Self {
        self.env("REVIEW_OBSERVABILITY", "1").env("REVIEW_OBSERVABILITY_DIR", &dir.to_string_lossy())
    }

    fn run(&self, diff: &str) -> Result<PipelineOutcome> {
        let dir = tmp_dir("diff");
        let path = dir.join("changes.patch");
        std::fs::write(&path, diff).unwrap();
        let profile = orakul();
        let cli = CliArgs {
            dry_run: self.dry_run,
            diff_file: Some(path.to_string_lossy().into_owned()),
            head_sha: self.cli_head_sha.clone(),
            repo: if self.dry_run { String::new() } else { "o/r".into() },
            pr_number: if self.dry_run { 0 } else { 1 },
        };
        let cfg = load_review_config(&self.env, &cli, &profile);
        // Дерево репозитория не читаем: иначе тест зависел бы от `git ls-files`.
        let no_tree = |_: ()| -> Result<Option<Vec<String>>> { Ok(None) };
        let no_tree_fn = move || no_tree(());
        let repo_files: &RepoFilesFn = match &self.repo_files {
            Some(f) => &**f,
            None => &no_tree_fn,
        };
        let mut deps = self.fakes.deps();
        if let Some(e) = &self.engine {
            deps.engine = &**e;
        }
        if let Some(c) = &self.cost {
            deps.cost = &**c;
        }
        let opts = PipelineOptions { repo_files: Some(repo_files), cwd: Some(package_root()), ..PipelineOptions::default() };
        let out = create_review_pipeline(&cfg, &profile, deps, opts).run_detailed();
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn sticky(&self) -> String {
        self.fakes.calls.sticky_bodies().first().cloned().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Гейт
// ---------------------------------------------------------------------------

#[test]
fn critical_blocks_and_publishes_inline_and_sticky() {
    let h = Harness::new(vec![critical_review()]);
    let out = h.run(CODE_DIFF).unwrap();
    assert_eq!(out.exit_code, 1, "блокирующий finding → exit 1");
    assert!(out.would_block);
    assert_eq!(h.fakes.calls.count("review"), 1);
    let posted = h.fakes.calls.posted_reviews();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].0[0].path, "foo.ts");
    assert_eq!(posted[0].0[0].line, 3);
    assert!(h.sticky().contains("Merge заблокирован"));
}

#[test]
fn absent_path_claim_about_existing_file_is_filtered_as_fp() {
    let absent = || {
        review(
            "m1",
            vec![json!({ "file": "foo.ts", "line": 3, "severity": "critical", "title": "импорт несуществующего модуля",
                "body": "Cannot find module './bar'", "confidence": 0.9, "model": "m1" })],
        )
    };
    let mut with_tree = Harness::new(vec![absent()]);
    with_tree.repo_files = Some(Box::new(|| Ok(Some(vec!["foo.ts".into(), "bar.ts".into()]))));
    assert_eq!(with_tree.run(CODE_DIFF).unwrap().exit_code, 0, "bar.ts есть в дереве — блокер ложный");
    assert!(with_tree.sticky().contains("Отфильтровано как известные FP (1)"));

    let no_tree = Harness::new(vec![absent()]);
    assert_eq!(no_tree.run(CODE_DIFF).unwrap().exit_code, 1, "дерево не прочитано — фильтр выключен");

    let mut broken = Harness::new(vec![absent()]);
    broken.repo_files = Some(Box::new(|| Err(anyhow::anyhow!("git сломан"))));
    assert_eq!(broken.run(CODE_DIFF).unwrap().exit_code, 1, "сбой чтения дерева — fail-open");
}

#[test]
fn post_then_delete_order_excludes_fresh_review() {
    let h = Harness::new(vec![critical_review()]);
    h.run(CODE_DIFF).unwrap();
    let tags: Vec<&str> = h.fakes.calls.tags().into_iter().filter(|t| matches!(*t, "post" | "delete" | "sticky")).collect();
    assert_eq!(&tags[..2], &["post", "delete"], "порядок: postReview → deleteStaleInline");
    assert_eq!(h.fakes.calls.delete_stale_opts(), vec![Some(555)], "свежий review исключён из чистки");
}

#[test]
fn review_without_id_skips_inline_cleanup() {
    let mut h = Harness::new(vec![critical_review()]);
    h.fakes.github.post_review_id = None;
    h.run(CODE_DIFF).unwrap();
    assert!(h.fakes.calls.delete_stale_opts().is_empty(), "без id не стираем только что запощенное");
}

#[test]
fn minor_only_does_not_block() {
    let mut r = critical_review();
    r.findings[0]["severity"] = json!("minor");
    let h = Harness::new(vec![r]);
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 0);
    assert!(h.sticky().contains("есть замечания"));
    assert!(!h.sticky().contains("Merge заблокирован"));
}

#[test]
fn verifier_strict_overflow_beyond_cap_is_downgraded() {
    let findings: Vec<Value> = (1..=13)
        .map(|i| json!({ "file": "foo.ts", "line": i, "severity": "critical", "title": format!("баг {i}"), "body": "", "confidence": 0.9, "model": "m1" }))
        .collect();
    let mut h = Harness::new(vec![review("m1", findings)]).env("REVIEW_VERIFIER", "1");
    h.fakes.engine.verdicts = verify_all();
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 1, "12 верифицированных critical блокируют");
    let md = h.sticky();
    assert!(md.contains("🔴12"), "12 critical после verifier");
    assert!(md.contains("🟡1"), "13-я (сверх капа) → minor");
}

#[test]
fn verifier_failure_is_fail_open() {
    let mut h = Harness::new(vec![critical_review()]).env("REVIEW_VERIFIER", "1").env("REVIEW_VERIFIER_STRICT", "0");
    h.fakes.engine = crate::testutil::FakeEngine::with_log(h.fakes.calls.clone())
        .with_reviews(vec![critical_review()])
        .failing(|r| matches!(r, CallRecord::VerifyFindings { .. }).then(|| "verifier 500".to_string()));
    let out = h.run(CODE_DIFF).unwrap();
    assert_eq!(h.fakes.calls.count("verify"), 1);
    assert_eq!(out.exit_code, 1, "сбой verifier не снимает находку и не роняет прогон");
}

#[test]
fn run_gate_resolution_filters_before_resolving() {
    let finding = json!({ "file": "foo.ts", "line": 3, "severity": "critical", "title": "реальный баг", "body": "", "confidence": 0.9, "model": "m1" });
    let gate = json!({ "blockOn": ["critical", "major"], "blockSingle": ["critical"], "consensusMin": 2, "confidenceMin": 0.8 });
    let log = |_: &str| {};
    let res = run_gate_resolution(
        GateInput {
            findings: vec![finding.clone()],
            changed_paths: &["foo.ts".to_string()],
            gate: &gate,
            prescan_cfg: None,
            patterns_file: None,
            repo_index: None,
            log: &log,
        },
        None,
    )
    .unwrap();
    assert_eq!(res.findings[0]["title"], finding["title"]);
    assert_eq!(res.blockers.len(), 1);
    assert!(res.would_block);
    assert_eq!(res.findings[0]["consensusBlock"], json!(true), "мутации гейта наложены на находку");
}

#[test]
fn before_resolve_runs_between_fp_filter_and_gate() {
    let fp = json!({ "file": "foo.ts", "line": 3, "severity": "critical", "title": "x", "body": "", "confidence": 0.9, "model": "m1" });
    let gate = json!({ "blockOn": ["critical"], "blockSingle": ["critical"], "consensusMin": 1, "confidenceMin": 0.8 });
    let seen = Mutex::new(0usize);
    let mut hook = |f: Vec<Value>| -> Result<BeforeResolved> {
        *seen.lock().unwrap() = f.len();
        // Хук понижает находку — гейт обязан видеть результат хука, а не вход.
        let f = f
            .into_iter()
            .map(|mut x| {
                x["severity"] = json!("minor");
                x
            })
            .collect();
        Ok(BeforeResolved { findings: f, refutation_stats: Some(json!({ "kept": 1 })), verifier_stats: None })
    };
    let log = |_: &str| {};
    let res = run_gate_resolution(
        GateInput {
            findings: vec![fp],
            changed_paths: &["foo.ts".to_string()],
            gate: &gate,
            prescan_cfg: None,
            patterns_file: None,
            repo_index: None,
            log: &log,
        },
        Some(&mut hook),
    )
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), 1);
    assert!(res.blockers.is_empty(), "понижённая хуком находка не блокирует");
    assert_eq!(res.refutation_stats, Some(json!({ "kept": 1 })));
}

// ---------------------------------------------------------------------------
// Fail-open / fail-closed
// ---------------------------------------------------------------------------

#[test]
fn all_models_down_fails_closed_with_sanitized_sticky() {
    let h = Harness::new(outage());
    let out = h.run(CODE_DIFF).unwrap();
    assert_eq!(out.exit_code, 1, "REVIEW_LLM_REQUIRED=1 (default): без ревью merge заблокирован");
    assert!(out.degraded);
    assert!(h.fakes.calls.posted_reviews().is_empty(), "inline не постится");
    assert_eq!(h.fakes.calls.tags().into_iter().filter(|t| matches!(*t, "post" | "delete" | "sticky")).collect::<Vec<_>>(), vec!["delete", "sticky"]);
    let md = h.sticky();
    for needle in ["недоступно", "заблокирован", "m1", "HTTP 500", "m2", "HTTP 429"] {
        assert!(md.contains(needle), "в sticky нет «{needle}»");
    }
    assert!(!md.contains("boom internal detail"), "сырой текст ошибки не уходит в PR");
}

#[test]
fn partial_outage_uses_available_model() {
    let reviews = vec![
        failed("m1", "Ollama 500: boom internal detail"),
        review("m2", vec![json!({ "file": "foo.ts", "line": 3, "severity": "minor", "title": "style", "body": "x", "confidence": 0.9, "model": "m2" })]),
    ];
    let h = Harness::new(reviews);
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 0);
    assert!(h.sticky().contains("minor"));
}

#[test]
fn all_models_down_with_llm_optional_is_soft() {
    let h = Harness::new(outage()).env("REVIEW_LLM_REQUIRED", "0");
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 0, "fail-open: инфра-сбой merge не блокирует");
    assert!(h.sticky().contains("временно недоступно"));
    assert!(h.sticky().contains("не блокирует merge"));
    assert!(h.fakes.calls.notifications().is_empty(), "неблокирующий fail-open дежурного не будит");
}

fn prescan_finding(severity: &str, block: bool, title: &str) -> Value {
    json!({ "file": "foo.ts", "line": 3, "severity": severity, "title": title, "body": "x",
        "model": "eslint", "confidence": 0.95, "prescan": true, "prescanBlock": block })
}

#[test]
fn llm_optional_but_prescan_blocker_still_blocks() {
    let mut h = Harness::new(outage()).env("REVIEW_LLM_REQUIRED", "0");
    h.fakes.prescan.result.findings = vec![prescan_finding("critical", true, "no-restricted-globals")];
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 1);
    assert!(h.sticky().contains("no-restricted-globals"), "prescan-блокер виден в sticky");
}

#[test]
fn prescan_without_block_flag_or_low_severity_does_not_block() {
    let mut h = Harness::new(outage()).env("REVIEW_LLM_REQUIRED", "0");
    h.fakes.prescan.result.findings = vec![prescan_finding("critical", false, "style")];
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 0, "prescanBlock обязателен");

    let mut h = Harness::new(outage()).env("REVIEW_LLM_REQUIRED", "0");
    h.fakes.prescan.result.findings = vec![prescan_finding("minor", true, "style")];
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 0, "severity вне blockOn не блокирует");
}

#[test]
fn outage_keeps_prescan_findings_in_sticky_and_export() {
    let dir = tmp_dir("export-outage");
    let mut h = Harness::new(vec![failed("m1", "Ollama 429: limit"), failed("m2", "Ollama 500: boom")])
        .env("REVIEW_EXPORT", "1")
        .env("REVIEW_EXPORT_DIR", &dir.to_string_lossy());
    h.fakes.prescan.result.findings = vec![prescan_finding("critical", true, "no-restricted-globals")];
    let out = h.run(CODE_DIFF).unwrap();
    assert_eq!(out.exit_code, 1);
    let posted = h.fakes.calls.posted_reviews();
    assert_eq!(posted.len(), 1, "prescan-блокер публикуется inline даже при аутедже");
    assert_eq!(posted[0].0[0].path, "foo.ts");
    assert!(h.sticky().contains("Prescan-находки"));
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("orakul-ai-review.json")).unwrap()).unwrap();
    assert_eq!(payload["blockers"].as_array().unwrap().len(), 1, "prescan-блокер попал в экспорт");
    assert_eq!(payload["llmFailed"], json!(true));
    assert_eq!(out.artifact.as_ref().unwrap()["llmFailed"], json!(true), "артефакт в памяти тот же");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fail_closed_logs_observability_and_pings_after_sticky() {
    let dir = tmp_dir("fail-closed");
    let h = Harness::new(vec![failed("m1", "Ollama 429: limit"), failed("m2", "Ollama 500: boom")]).obs(&dir);
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 1);
    let tags = h.fakes.calls.tags();
    assert_eq!(&tags[tags.len() - 2..], &["sticky", "telegram"]);
    let n = h.fakes.calls.notifications();
    assert_eq!(n.len(), 1, "блокирующий LLM-аутедж уведомляет дежурного");
    assert!(n[0].contains("⚠️ fail-closed: LLM outage"), "{}", n[0]);
    let records = read_jsonl(&dir);
    assert!(records.iter().any(|r| r["type"] == "llm_failed"));
    let gate = records.iter().find(|r| r["type"] == "gate").unwrap();
    assert_eq!(gate["wouldBlock"], json!(true));
    let end = records.iter().find(|r| r["type"] == "run_end").unwrap();
    assert_eq!(end["llmFailed"], json!(true));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sticky_failure_after_post_is_not_silent() {
    let mut h = Harness::new(vec![critical_review()]);
    h.fakes.github = crate::testutil::FakeGithub::with_log(h.fakes.calls.clone())
        .failing(|r| matches!(r, CallRecord::UpsertSticky { .. }).then(|| "GitHub 502: sticky upsert failed".to_string()));
    let err = h.run(CODE_DIFF).unwrap_err();
    assert!(format!("{err:#}").contains("sticky upsert failed"));
    let tags = h.fakes.calls.tags();
    let pub_tags: Vec<&str> = tags.into_iter().filter(|t| matches!(*t, "post" | "delete" | "sticky")).collect();
    assert_eq!(&pub_tags[..2], &["post", "delete"], "inline успел уйти до sticky");
    assert!(pub_tags.contains(&"sticky"));
}

#[test]
fn prior_sticky_failure_is_fail_open() {
    let mut h = Harness::new(vec![critical_review()]);
    h.fakes.github = crate::testutil::FakeGithub::with_log(h.fakes.calls.clone())
        .failing(|r| matches!(r, CallRecord::ReadPriorStickyData).then(|| "GitHub 500".to_string()));
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 1, "без prior sticky прогон идёт с нуля");
}

#[test]
fn soft_fail_instead_of_exit() {
    let mut h = Harness::new(vec![critical_review()]);
    h.env.remove("OLLAMA_API_KEY");
    let out = h.run(CODE_DIFF).unwrap();
    assert!(out.soft_fail && out.degraded);
    assert_eq!(out.exit_code, 0);
    assert!(out.reason.unwrap().contains("нет LLM API key"));
    assert_eq!(h.fakes.calls.count("review"), 0);

    let h = Harness::new(vec![critical_review()]);
    let out = h.run("   \n").unwrap();
    assert!(out.soft_fail);
    assert_eq!(out.reason.as_deref(), Some("пустой дифф"));
}

#[test]
fn github_diff_load_failure_and_draft_are_soft() {
    // Прогон без --diff: дифф берётся из GitHub.
    let profile = orakul();
    let env: HashMap<String, String> =
        [("OLLAMA_API_KEY", "k"), ("GITHUB_TOKEN", "t"), ("REVIEW_OBSERVABILITY", "0"), ("REVIEW_EXPORT", "0")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    let cli = CliArgs { repo: "o/r".into(), pr_number: 7, ..CliArgs::default() };
    let cfg = load_review_config(&env, &cli, &profile);

    let fakes = FakeDeps::new();
    let out = create_review_pipeline(&cfg, &profile, fakes.deps(), PipelineOptions::default()).run_detailed().unwrap();
    assert!(out.soft_fail, "нет диффа от GitHub — мягкий отказ");
    assert_eq!(out.exit_code, 0);

    let mut fakes = FakeDeps::new();
    fakes.github.loaded_diff = Some(LoadedDiff { draft: true, ..LoadedDiff::default() });
    let out = create_review_pipeline(&cfg, &profile, fakes.deps(), PipelineOptions::default()).run_detailed().unwrap();
    assert_eq!(out.reason.as_deref(), Some("PR — черновик, пропускаем"));
    assert_eq!(fakes.calls.count("review"), 0);
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

#[test]
fn router_skip_lockfile_does_not_call_llm() {
    let h = Harness::new(vec![critical_review()]);
    let out = h.run(LOCK_DIFF).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(out.degraded);
    assert_eq!(out.reason.as_deref(), Some("router skip"));
    assert_eq!(h.fakes.calls.count("review"), 0, "engine не вызывался");
    assert!(h.fakes.calls.posted_reviews().is_empty());
    assert!(h.sticky().contains("пропуск LLM"));
    assert_eq!(out.artifact.unwrap()["wouldBlock"], json!(false));
}

#[test]
fn router_light_docs_only() {
    let h = Harness::new(vec![critical_review()]);
    let out = h.run(DOCS_DIFF).unwrap();
    assert_eq!(out.reason.as_deref(), Some("router light"));
    assert_eq!(h.fakes.calls.count("review"), 0);
    assert!(h.sticky().contains("docs/config-only"));
}

// ---------------------------------------------------------------------------
// Окна контекста, blast-radius, дедлайн
// ---------------------------------------------------------------------------

fn run_windowed(margin: Option<&str>) -> Value {
    let dir = tmp_dir("ctx-window");
    let mut h = Harness::new(vec![review("m1", vec![])]).obs(&dir);
    if let Some(m) = margin {
        h = h.env("REVIEW_CONTEXT_MARGIN", m);
    }
    h.run(WINDOWED_DIFF).unwrap();
    let plan = read_jsonl(&dir).into_iter().find(|r| r["type"] == "plan").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    plan
}

#[test]
fn context_windows_cut_chunk_input_but_not_blast_symbols() {
    let on = run_windowed(None);
    let off = run_windowed(Some("0"));
    assert_eq!(on["contextWindows"]["margin"], json!(40), "дефолт RFC 07");
    assert_eq!(on["contextWindows"]["files"], json!(1));
    assert!(on["contextWindows"]["after"].as_u64() < on["contextWindows"]["before"].as_u64());
    assert_eq!(on["chunkPlan"][0]["windowed_paths"], json!(1));
    assert!(on["chunkPlan"][0]["context_chars"].as_u64() < off["chunkPlan"][0]["context_chars"].as_u64());
    // Символы blast-radius — из ПОЛНОГО текста: окна раньше дали бы меньше символов.
    assert_eq!(on["symbols"], off["symbols"]);
}

#[test]
fn context_margin_zero_is_rollback() {
    let off = run_windowed(Some("0"));
    assert_eq!(off["contextWindows"]["margin"], json!(0), "0 — значащее значение");
    assert_eq!(off["contextWindows"]["files"], json!(0));
    assert_eq!(off["contextWindows"]["after"], off["contextWindows"]["before"]);
    assert_eq!(off["chunkPlan"][0]["windowed_paths"], json!(0));
}

#[test]
fn deadline_skipped_chunks_mark_partial_coverage() {
    let mut r = review("m1", vec![]);
    r.skipped_chunks = 2;
    let h = Harness::new(vec![r]);
    assert_eq!(h.run(CODE_DIFF).unwrap().exit_code, 0);
    assert!(h.sticky().contains("охват частичный"));
}

#[test]
fn empty_blast_radius_keeps_skip_reasons_in_plan() {
    let dir = tmp_dir("blast");
    let mut h = Harness::new(vec![review("m1", vec![])]).obs(&dir);
    let mut skip = indexmap::IndexMap::new();
    skip.insert("graph_stale".to_string(), 1);
    skip.insert("symbol_unresolved".to_string(), 1);
    h.fakes.graphify.radius = BlastRadius {
        blocks: vec![],
        metrics: BlastMetrics {
            symbols_total: 1,
            symbols_resolved: Some(0),
            graph_fresh: Some(false),
            skip_reason: "graph_stale".into(),
            skip_reasons: skip,
            ..BlastMetrics::default()
        },
        graph_index: None,
    };
    h.run(CODE_DIFF).unwrap();
    let plan = read_jsonl(&dir).into_iter().find(|r| r["type"] == "plan").unwrap();
    let br = &plan["blast_radius"];
    assert_eq!(br["skip_reason"], json!("graph_stale"));
    assert_eq!(br["skip_reasons"], json!({ "graph_stale": 1, "symbol_unresolved": 1 }));
    assert_eq!(br["graph_fresh"], json!(false));
    assert_eq!(br["symbols_resolved"], json!(0));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Экспорт: отпечаток — одна величина в футере и в артефакте
// ---------------------------------------------------------------------------

#[test]
fn export_carries_fp_verifier_status_and_score_matching_sticky_footer() {
    let dir = tmp_dir("export-fp");
    let base = [
        json!({ "file": "foo.ts", "line": 3, "severity": "critical", "title": "реальный баг", "body": "npe", "confidence": 0.9 }),
        json!({ "file": "foo.ts", "line": 2, "severity": "minor", "title": "магическое число", "body": "", "confidence": 0.9, "suggestion": "const B = 3" }),
    ];
    let reviews = ["m1", "m2"]
        .iter()
        .map(|m| {
            review(m, base.iter().map(|f| {
                let mut f = f.clone();
                f["model"] = json!(m);
                f
            }).collect())
        })
        .collect();
    let mut h = Harness::new(reviews)
        .env("REVIEW_EXPORT", "1")
        .env("REVIEW_EXPORT_DIR", &dir.to_string_lossy())
        .env("REVIEW_VERIFIER", "1");
    h.fakes.engine.verdicts = verify_all();
    h.run(CODE_DIFF).unwrap();
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("orakul-ai-review.json")).unwrap()).unwrap();
    assert_eq!(payload["version"], json!(2));
    let findings = payload["findings"].as_array().unwrap();
    let blocker = findings.iter().find(|f| f["severity"] == "critical").unwrap();
    assert!(blocker["fp"].as_str().is_some_and(|s| !s.is_empty()), "отпечаток в артефакте");
    assert_eq!(blocker["verifierStatus"], json!("verified"));
    assert!(blocker["actionabilityScore"].is_number(), "скор есть и у блокера");
    assert!(payload["blockers"][0]["actionabilityScore"].is_number(), "блокеры материализованы после маршрута");

    let markers = orakul().markers().clone();
    let footer = markers.capture_data(&h.sticky()).expect("машинный футер в sticky");
    let footer: Value = serde_json::from_str(footer.trim()).unwrap();
    let fps: HashMap<String, Value> = footer["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (format!("{}:{}", f["file"].as_str().unwrap(), f["line"]), f["fp"].clone()))
        .collect();
    assert!(!fps.is_empty());
    for f in findings {
        let key = format!("{}:{}", f["file"].as_str().unwrap(), f["line"]);
        assert_eq!(Some(&f["fp"]), fps.get(&key), "артефакт и футер показывают один отпечаток");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_detailed_returns_artifact_with_cli_head_sha_on_dry_run() {
    let mut h = Harness::new(vec![critical_review()]);
    h.dry_run = true;
    h.cli_head_sha = "abc1234deadbeef".into();
    let out = h.run(CODE_DIFF).unwrap();
    assert_eq!(out.artifact.as_ref().unwrap()["headSha"], json!("abc1234deadbeef"));
    assert_eq!(out.exit_code, 1);
    assert!(out.would_block);
    assert!(h.fakes.calls.posted_reviews().is_empty(), "dry-run не постит review");
    assert!(h.fakes.calls.sticky_bodies().is_empty());
}

// ---------------------------------------------------------------------------
// Инкрементальный прогон (RFC 05)
// ---------------------------------------------------------------------------

fn bar_critical() -> ModelReview {
    review(
        "m1",
        vec![json!({ "file": "bar.ts", "line": 2, "severity": "critical", "title": "блокер в bar", "body": "детали",
            "confidence": 0.9, "model": "m1", "quote": "export function bar() { return c; }" })],
    )
}

fn incremental(reviews: Vec<ModelReview>, state: &std::path::Path) -> Harness {
    Harness::new(reviews).env("REVIEW_INCREMENTAL", "1").env("REVIEW_INCREMENTAL_STATE", &state.to_string_lossy())
}

#[test]
fn incremental_carries_blocker_of_unchanged_file() {
    let dir = tmp_dir("incr");
    let state = dir.join("state.json");
    let first = incremental(vec![bar_critical()], &state);
    assert_eq!(first.run(TWO_FILE_DIFF).unwrap().exit_code, 1, "первый прогон блокирует");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(saved["findings"].as_array().unwrap().len(), 1, "находка сохранена для переноса");
    assert!(saved["hashes"]["bar.ts"].is_string() && saved["hashes"]["foo.ts"].is_string());

    let second = incremental(vec![review("m1", vec![])], &state);
    assert_eq!(second.run(&two_file_v2()).unwrap().exit_code, 1, "блокер по неизменившемуся файлу НЕ потерян");
    assert!(second.sticky().contains("блокер в bar"));
    assert!(second.sticky().contains("Инкрементальный прогон"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn incremental_changed_file_drops_prior_finding() {
    let dir = tmp_dir("incr2");
    let state = dir.join("state.json");
    let foo = review(
        "m1",
        vec![json!({ "file": "foo.ts", "line": 3, "severity": "critical", "title": "блокер в foo", "body": "детали", "confidence": 0.9, "model": "m1" })],
    );
    incremental(vec![foo], &state).run(TWO_FILE_DIFF).unwrap();
    let second = incremental(vec![review("m1", vec![])], &state);
    assert_eq!(second.run(&two_file_v2()).unwrap().exit_code, 0);
    assert!(!second.sticky().contains("блокер в foo"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn incremental_config_change_forces_full_run() {
    let dir = tmp_dir("incr3");
    let state = dir.join("state.json");
    incremental(vec![bar_critical()], &state).run(TWO_FILE_DIFF).unwrap();
    let second = incremental(vec![review("m1", vec![])], &state).env("OLLAMA_MODELS", "m3,m4");
    assert_eq!(second.run(TWO_FILE_DIFF).unwrap().exit_code, 0);
    assert!(!second.sticky().contains("Инкрементальный прогон"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn incremental_nothing_changed_skips_llm() {
    let dir = tmp_dir("incr4");
    let state = dir.join("state.json");
    incremental(vec![bar_critical()], &state).run(TWO_FILE_DIFF).unwrap();
    let second = incremental(vec![], &state);
    let out = second.run(TWO_FILE_DIFF).unwrap();
    assert_eq!(second.fakes.calls.count("review"), 0, "ни одного вызова движка");
    assert_eq!(out.exit_code, 1, "перенесённый блокер блокирует, а «ничего не поменялось» — не аутедж");
    assert!(second.sticky().contains("блокер в bar"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn incremental_disabled_by_default_writes_no_state() {
    let dir = tmp_dir("incr5");
    let state = dir.join("state.json");
    Harness::new(vec![bar_critical()])
        .env("REVIEW_INCREMENTAL_STATE", &state.to_string_lossy())
        .run(TWO_FILE_DIFF)
        .unwrap();
    assert!(!state.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn incremental_carried_blocker_survives_strict_verifier_of_second_run() {
    let dir = tmp_dir("incr-ver");
    let state = dir.join("state.json");
    let mut first = incremental(vec![bar_critical()], &state).env("REVIEW_VERIFIER", "1");
    first.fakes.engine.verdicts = verify_all();
    assert_eq!(first.run(TWO_FILE_DIFF).unwrap().exit_code, 1, "verifier подтвердил блокер");

    // Пустые вердикты + REVIEW_VERIFIER_STRICT: будь перенос ДО Pass B, блокер стал бы minor.
    let second = incremental(vec![review("m1", vec![])], &state).env("REVIEW_VERIFIER", "1");
    assert_eq!(second.run(&two_file_v2()).unwrap().exit_code, 1);
    assert!(second.sticky().contains("блокер в bar"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn carried_blocker_survives_strict_refutation_in_moa() {
    // MoA со специалистами: strict-refutation понижает block-tier без advisorClaims. Перенос
    // идёт после неё — перенесённый блокер (claims из прошлого прогона нет) не трогается.
    let dir = tmp_dir("incr-moa");
    let state = dir.join("state.json");
    let moa = |reviews: Vec<ModelReview>| {
        incremental(reviews, &state).env("REVIEW_MODE", "moa").env("REVIEW_VERIFIER", "0")
    };
    let with_claims = review(
        "moa",
        vec![json!({ "file": "bar.ts", "line": 2, "severity": "critical", "title": "блокер в bar", "body": "детали",
            "confidence": 0.95, "model": "moa", "advisorClaims": [
                { "role": "security", "model": "a", "status": "verified" },
                { "role": "correctness", "model": "b", "status": "verified" },
            ] })],
    );
    assert_eq!(moa(vec![with_claims]).run(TWO_FILE_DIFF).unwrap().exit_code, 1);
    let second = moa(vec![review("moa", vec![])]);
    assert_eq!(second.run(&two_file_v2()).unwrap().exit_code, 1, "перенос после refutation");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Порог допуска advisory к verifier'у (P1) и observability actionability
// ---------------------------------------------------------------------------

fn advisory_minor() -> ModelReview {
    review(
        "m1",
        vec![json!({ "file": "foo.ts", "line": 2, "severity": "minor", "title": "магическое число", "body": "вынести в константу",
            "confidence": 0.9, "suggestion": "const B = 3;", "model": "m1" })],
    )
}

/// Движок-шпион: запоминает кандидатов verifier'а в момент вызова (скор — как его видел
/// verifier, до пересчёта маршрутом).
struct SpyEngine {
    reviews: ReviewAllResult,
    seen: std::sync::Arc<Mutex<Vec<Value>>>,
}

impl EngineApi for SpyEngine {
    fn review_all_models(&self, _: &[Chunk], _: &ReviewContext) -> Result<ReviewAllResult> {
        Ok(self.reviews.clone())
    }
    fn verify_findings(&self, candidates: &[Value], _: &VerifyContext) -> Result<Vec<Value>> {
        self.seen.lock().unwrap().extend(candidates.iter().cloned());
        Ok((0..candidates.len()).map(|i| json!({ "candidateIndex": i, "decision": "verified" })).collect())
    }
}

fn run_with_verifier_spy(extra: &[(&str, &str)]) -> (PipelineOutcome, Harness, Vec<Value>) {
    let mut h = Harness::new(vec![]).env("REVIEW_VERIFIER", "1");
    for (k, v) in extra {
        h = h.env(k, v);
    }
    let seen = std::sync::Arc::new(Mutex::new(vec![]));
    h.engine = Some(Box::new(SpyEngine { reviews: vec![advisory_minor()], seen: seen.clone() }));
    let out = h.run(CODE_DIFF).unwrap();
    let seen = seen.lock().unwrap().clone();
    (out, h, seen)
}

#[test]
fn advisory_below_route_threshold_is_admitted_and_goes_inline_after_verified() {
    let (out, h, seen) = run_with_verifier_spy(&[]);
    assert_eq!(out.exit_code, 0, "minor не блокирует даже подтверждённым");
    assert_eq!(seen.len(), 1, "находка дошла до verifier");
    assert_eq!(seen[0]["line"], json!(2));
    assert_eq!(seen[0]["actionabilityScore"], json!(3), "до verifier ровно 3 балла при пороге 4");
    let posted = h.fakes.calls.posted_reviews();
    let lines: Vec<(String, u64)> = posted[0].0.iter().map(|c| (c.path.clone(), c.line)).collect();
    assert_eq!(lines, vec![("foo.ts".to_string(), 2)], "3 + verified(3) ≥ 4: находка в строке");
}

#[test]
fn unreachable_verify_threshold_keeps_advisory_out_of_line() {
    let (out, h, seen) = run_with_verifier_spy(&[("REVIEW_VERIFY_ADVISORY_MIN", "99")]);
    assert_eq!(out.exit_code, 0);
    assert!(seen.is_empty(), "порог допуска недостижим — кандидатов нет");
    assert!(h.fakes.calls.posted_reviews()[0].0.is_empty());
    assert!(h.sticky().contains("магическое число"), "из отчёта не исчезает");
}

#[test]
fn actionability_event_has_histogram_and_reason_keys() {
    let dir = tmp_dir("actionability");
    let h = Harness::new(vec![advisory_minor()]).obs(&dir);
    h.run(CODE_DIFF).unwrap();
    let ev = read_jsonl(&dir).into_iter().find(|r| r["type"] == "actionability").unwrap();
    assert_eq!(ev["threshold"], json!(4));
    assert_eq!(ev["inline"], json!(0));
    assert_eq!(ev["outside"], json!(1));
    assert_eq!(ev["histogram"], json!({ "3": 1 }));
    let mut keys: Vec<String> =
        ev["topOutsideReasons"].as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap().to_string()).collect();
    keys.sort();
    assert_eq!(keys, vec!["confidence", "logicFile", "suggestion"], "ключи сигналов, не формулировки");
    let end = read_jsonl(&dir).into_iter().find(|r| r["type"] == "run_end").unwrap();
    assert_eq!(end["exitCode"], json!(0));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Находки ревью T26 (FIX-T26)
// ---------------------------------------------------------------------------

/// Огромный `REVIEW_TIME_BUDGET_MIN` (numEnv его пропускает: конечное и > 0) не должен
/// переполнять дедлайн: в JS это дедлайн далеко в будущем и полное ревью.
#[test]
fn huge_time_budget_does_not_overflow_deadline() {
    let h = Harness::new(vec![critical_review()]).env("REVIEW_TIME_BUDGET_MIN", "999999999999999");
    let out = h.run(CODE_DIFF).unwrap();
    assert_eq!(h.fakes.calls.count("review"), 1, "бюджет не сработал — чанки ушли в движок");
    assert_eq!(out.exit_code, 1, "critical дошла до гейта");
}

/// Движок, чей LLM-клиент пишет usage в трекер — как `RealLlmClient::with_usage_sink`.
struct UsageEngine {
    reviews: ReviewAllResult,
    tracker: std::sync::Arc<CostTracker>,
}

impl EngineApi for UsageEngine {
    fn review_all_models(&self, _: &[Chunk], _: &ReviewContext) -> Result<ReviewAllResult> {
        let usage = json!({ "prompt_tokens": 1200, "completion_tokens": 300, "total_tokens": 1500 });
        self.tracker.record(Some(&usage), "m1", "review");
        Ok(self.reviews.clone())
    }
    fn verify_findings(&self, _: &[Value], _: &VerifyContext) -> Result<Vec<Value>> {
        Ok(vec![])
    }
}

/// Трекер, в который пишет движок, и трекер, из которого пайплайн читает efficiency и
/// строку стоимости, — один и тот же (в JS `costTracker` общий с `createLlmClient`).
#[test]
fn engine_usage_reaches_run_end_and_sticky_cost_line() {
    let dir = tmp_dir("cost");
    let mut h = Harness::new(vec![]).obs(&dir);
    let tracker = std::sync::Arc::new(CostTracker::new(None));
    h.engine = Some(Box::new(UsageEngine { reviews: vec![critical_review()], tracker: tracker.clone() }));
    h.cost = Some(tracker);
    h.run(CODE_DIFF).unwrap();
    let end = read_jsonl(&dir).into_iter().find(|r| r["type"] == "run_end").unwrap();
    assert_eq!(end["tokens_total"], json!(1500), "efficiencyMetrics в run_end: {end}");
    assert!(h.sticky().contains("💰 2k tokens"), "строка стоимости в sticky: {}", h.sticky());
    let _ = std::fs::remove_dir_all(&dir);
}
