//! Кейсы `test/pr-review.mcp.test.mjs` поверх домена с фейковыми зависимостями:
//! без сети, без LLM, без git. Транспорт и боевой прогон — в `tests/review_mcp_stdio.rs`.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CODE_DIFF: &str = "diff --git a/foo.ts b/foo.ts
--- a/foo.ts
+++ b/foo.ts
@@ -1,2 +1,3 @@
 const a = 1;
-const b = 2;
+const b = 3;
+export function foo() { return a; }
";

fn critical(title: &str, line: u64) -> Value {
    json!({ "file": "foo.ts", "line": line, "severity": "critical", "title": title, "body": "null pointer", "confidence": 0.9, "model": "m1" })
}

/// Фейковый прогон: артефакт из заданных находок, опционально «держит» MoA до сигнала.
#[derive(Default)]
struct FakeRunner {
    findings: Vec<Value>,
    calls: AtomicUsize,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    hold: Mutex<Option<mpsc::Receiver<()>>>,
    seen_prior: Mutex<Option<StickyData>>,
    fail: Option<String>,
}

impl ReviewRunner for FakeRunner {
    fn run(&self, req: RunRequest) -> Result<RunReport> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.seen_prior.lock().unwrap() = Some(req.prior.clone());
        assert!(req.cli.dry_run, "превью-прогон всегда dry-run");
        assert_eq!(req.env.get("REVIEW_EXPORT_DRY_RUN").map(String::as_str), Some("1"));
        if let Some(tx) = self.entered.lock().unwrap().take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.hold.lock().unwrap().take() {
            let _ = rx.recv();
        }
        if let Some(msg) = &self.fail {
            bail!("{msg}");
        }
        let blockers: Vec<Value> = self.findings.iter().filter(|f| f["severity"] == "critical").cloned().collect();
        Ok(RunReport {
            artifact: Some(json!({
                "headSha": req.head_sha,
                "wouldBlock": !blockers.is_empty(),
                "blockers": blockers,
                "findings": self.findings,
                "counts": { "findings": self.findings.len() },
                "router": { "route": "full" },
            })),
            degraded: false,
            reason: None,
            would_block: !blockers.is_empty(),
            written: vec![],
        })
    }
}

struct FakeGit {
    head: String,
    dirty: String,
}

impl GitProbe for FakeGit {
    fn is_inside_work_tree(&self, _root: &Path) -> bool {
        true
    }
    fn rev_parse_head(&self, _root: &Path) -> Result<String> {
        Ok(self.head.clone())
    }
    fn status_porcelain(&self, _root: &Path) -> Result<String> {
        Ok(self.dirty.clone())
    }
}

struct NoGit;
impl GitProbe for NoGit {
    fn is_inside_work_tree(&self, _root: &Path) -> bool {
        false
    }
    fn rev_parse_head(&self, _root: &Path) -> Result<String> {
        bail!("не git")
    }
    fn status_porcelain(&self, _root: &Path) -> Result<String> {
        bail!("не git")
    }
}

#[derive(Default)]
struct FakeStages {
    prescan_findings: Vec<Value>,
    affected: Vec<String>,
}

impl CheapStages for FakeStages {
    fn prescan(&self, _cfg: &Value, _p: &Profile, _root: &Path, _log: McpLog, _paths: &[String]) -> Result<PrescanResult> {
        Ok(PrescanResult {
            findings: self.prescan_findings.clone(),
            scanned: 1,
            lintable_total: Some(1),
            by_tool: Some(crate::traits::PrescanByTool { eslint: 1, astgrep: 0 }),
            ..Default::default()
        })
    }
    fn blast(&self, _cfg: &Value, _symbols: &[String]) -> Result<BlastRadius> {
        let mut b = BlastRadius::default();
        b.metrics.affected_files = self.affected.clone();
        b.metrics.skip_reason = "none".into();
        Ok(b)
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    mcp: ReviewMcp,
    runner: Arc<FakeRunner>,
    notes: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Fixture {
    fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }
    fn path(&self, name: &str) -> String {
        self.root().join(name).to_string_lossy().into_owned()
    }
    fn call(&self, name: &str, args: Value) -> Value {
        self.mcp
            .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } }))
            .unwrap()
    }
}

struct Opts {
    runner: FakeRunner,
    git: Arc<dyn GitProbe>,
    stages: FakeStages,
    env: Vec<(&'static str, &'static str)>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            runner: FakeRunner { findings: vec![critical("реальный баг", 3)], ..Default::default() },
            git: Arc::new(NoGit),
            stages: FakeStages::default(),
            env: vec![],
        }
    }
}

fn fixture(opts: Opts) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("changes.patch"), CODE_DIFF).unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let runner = Arc::new(opts.runner);
    let notes: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let sink = Arc::clone(&notes);
    let mut env: HashMap<String, String> =
        [("OLLAMA_API_KEY", "k"), ("GITHUB_TOKEN", "should-not-post"), ("REVIEW_OBSERVABILITY", "0")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    for (k, v) in opts.env {
        env.insert(k.into(), v.into());
    }
    let deps = McpDeps {
        git: opts.git,
        runner: runner.clone(),
        stages: Arc::new(opts.stages),
        notify: Arc::new(move |m: &str, p: Value| sink.lock().unwrap().push((m.to_string(), p))),
    };
    let mcp = ReviewMcp::new(root, env, deps);
    Fixture { dir, mcp, runner, notes }
}

fn ok(res: &Value) -> &Value {
    assert!(res.get("error").is_none(), "ошибка: {res}");
    &res["result"]["structuredContent"]
}

fn err(res: &Value) -> (i64, String) {
    let e = res.get("error").unwrap_or_else(|| panic!("ожидалась ошибка: {res}"));
    (e["code"].as_i64().unwrap(), e["message"].as_str().unwrap().to_string())
}

fn tokens(v: &Value) -> i64 {
    estimate_tokens(v)
}

#[test]
fn tools_list_дешёвый_контур_и_фаза_c() {
    let names = |env: &[(&str, &str)]| -> Vec<String> {
        let env: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let r = handshake(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }), &env).unwrap().unwrap();
        r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    let base = ["review_run", "review_findings", "review_route", "review_prescan", "fp_check", "fingerprint_check", "review_artifact"];
    assert_eq!(names(&[]), base);
    let mut all: Vec<&str> = base.to_vec();
    all.extend(["blast_scope", "actionability_explain", "review_metrics"]);
    assert_eq!(names(&[("REVIEW_MCP_EXTRA", "1")]), all);
}

#[test]
fn handshake_как_в_js() {
    let env = HashMap::new();
    let r = handshake(&json!({ "jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {} }), &env).unwrap().unwrap();
    assert_eq!(r["result"]["capabilities"], json!({ "tools": {}, "logging": {} }));
    assert_eq!(r["result"]["serverInfo"], json!({ "name": "ai-reviewer", "version": "1.0.0" }));
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    assert!(handshake(&json!({ "method": "notifications/initialized" }), &env).unwrap().is_none());
    assert!(handshake(&json!({ "id": 1, "method": "tools/call" }), &env).is_none());
}

#[test]
fn метод_не_найден_и_неизвестный_тулз() {
    let f = fixture(Opts::default());
    let r = f.mcp.handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "nope" })).unwrap();
    assert_eq!(err(&r), (-32601, "method not found: nope".into()));
    assert_eq!(err(&f.call("zzz", json!({}))), (-32602, "unknown tool: zzz".into()));
}

#[test]
fn review_run_без_head_sha_или_diff_path() {
    let f = fixture(Opts::default());
    let (code, msg) = err(&f.call("review_run", json!({ "diffPath": "a.patch" })));
    assert_eq!(code, -32602);
    assert!(msg.contains("headSha"), "{msg}");
    let (code, msg) = err(&f.call("review_run", json!({ "headSha": HEAD })));
    assert_eq!(code, -32602);
    assert!(msg.contains("diffPath"), "{msg}");
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn review_run_путь_вне_корня_не_читается() {
    let f = fixture(Opts::default());
    let outside = tempfile::tempdir().unwrap();
    let patch = outside.path().join("x.patch");
    std::fs::write(&patch, CODE_DIFF).unwrap();
    let (code, msg) = err(&f.call("review_run", json!({ "diffPath": patch, "headSha": HEAD })));
    assert_eq!(code, -32602);
    assert!(msg.contains("escapes allowed root"), "{msg}");
    let (_, msg) = err(&f.call("review_run", json!({ "diffPath": "../x.patch", "headSha": HEAD })));
    assert!(msg.contains("escapes allowed root"), "{msg}");
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn review_run_превью_без_гейт_поля() {
    let f = fixture(Opts::default());
    let p = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD }))).clone();
    assert_eq!(p["kind"], "review_preview");
    assert_eq!(p["headSha"], HEAD);
    assert_eq!(p["previewWouldBlock"], true);
    assert_eq!(p["status"], "completed");
    assert_eq!(p["route"], "full");
    assert!(p.get("wouldBlock").is_none(), "гейт-поле wouldBlock не светим на верхнем уровне");
    assert!(is_run_id(p["jobId"].as_str().unwrap()));
    assert!(is_run_id(p["runFingerprint"].as_str().unwrap()));
    assert!(p["startedAt"].as_str().unwrap().ends_with('Z'));
    // Тот же пин второй раз — кэш, без нового прогона.
    let again = ok(&f.call("review_run", json!({ "diffPath": "changes.patch", "headSha": HEAD }))).clone();
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 2, "относительный diffPath — другой jobId");
    let third = ok(&f.call("review_run", json!({ "diffPath": "changes.patch", "headSha": HEAD }))).clone();
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 2, "повтор пина отдаётся из кэша");
    assert_eq!(third["runFingerprint"], again["runFingerprint"]);
}

#[test]
fn review_run_мягкий_отказ_degraded_без_выхода() {
    // Боевой runner без ключа: пайплайн мягко отказывает до любой сети.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("changes.patch"), CODE_DIFF).unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let env: HashMap<String, String> =
        [("REVIEW_MODE", "consensus"), ("OLLAMA_MODELS", "m1"), ("REVIEW_OBSERVABILITY", "0"), ("REVIEW_EXPORT", "0")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    let deps = McpDeps {
        git: Arc::new(NoGit),
        runner: Arc::new(crate::cli::review_mcp::WiringRunner),
        stages: Arc::new(FakeStages::default()),
        notify: Arc::new(|_: &str, _: Value| {}),
    };
    let mcp = ReviewMcp::new(root.clone(), env, deps);
    let r = mcp
        .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "review_run", "arguments": {
            "diffPath": root.join("changes.patch"), "headSha": HEAD } } }))
        .unwrap();
    let p = ok(&r);
    assert_eq!(p["status"], "degraded", "{p}");
    let reason = p["reason"].as_str().unwrap();
    assert!(reason.contains("LLM API key") || reason.contains("OLLAMA_API_KEY"), "{reason}");
}

#[test]
fn тот_же_пин_пока_running_stub_другой_already_running() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (hold_tx, hold_rx) = mpsc::channel();
    let runner = FakeRunner {
        findings: vec![critical("реальный баг", 3)],
        entered: Mutex::new(Some(entered_tx)),
        hold: Mutex::new(Some(hold_rx)),
        ..Default::default()
    };
    let f = fixture(Opts { runner, ..Default::default() });
    std::fs::write(f.root().join("other.patch"), CODE_DIFF).unwrap();
    let mcp = f.mcp.clone();
    let diff = f.path("changes.patch");
    let first = std::thread::spawn(move || {
        mcp.handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "review_run", "arguments": { "diffPath": diff, "headSha": HEAD } } }))
            .unwrap()
    });
    entered_rx.recv().unwrap();
    let same = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD }))).clone();
    assert_eq!(same["status"], "running");
    assert!(is_run_id(same["jobId"].as_str().unwrap()));
    let (code, msg) = err(&f.call("review_run", json!({ "diffPath": f.path("other.patch"), "headSha": HEAD })));
    assert_eq!(code, -32602);
    assert!(msg.contains("already running"), "{msg}");
    hold_tx.send(()).unwrap();
    let done = first.join().unwrap();
    let done = ok(&done);
    assert_eq!(done["status"], "completed");
    assert_eq!(done["jobId"], same["jobId"]);
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 1, "один прогон на слот");
}

#[test]
fn review_run_в_token_budget_хвост_через_review_findings() {
    let findings: Vec<Value> = (0..12).map(|i| critical(&format!("баг {i} {}", "x".repeat(80)), 3 + i)).collect();
    let f = fixture(Opts { runner: FakeRunner { findings, ..Default::default() }, ..Default::default() });
    let p = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD, "tokenBudget": 256 }))).clone();
    assert_eq!(p["truncated"], true);
    assert!(tokens(&p) <= 256, "preview {} > tokenBudget 256", tokens(&p));
    let fp = p["runFingerprint"].as_str().unwrap().to_string();
    let page = ok(&f.call("review_findings", json!({ "runFingerprint": fp, "headSha": HEAD, "limit": 5 }))).clone();
    assert_eq!(page["findings"].as_array().unwrap().len(), 5);
    assert_eq!(page["total"], 12);
    assert!(page["total"].as_u64().unwrap() >= p["blockers"].as_array().unwrap().len() as u64);
    let (code, msg) = err(&f.call("review_findings", json!({ "runFingerprint": fp, "headSha": "bbbbbbb" })));
    assert_eq!(code, -32602);
    assert!(msg.contains("does not match run"), "{msg}");
    let (_, msg) = err(&f.call("review_findings", json!({ "runFingerprint": fp, "headSha": "zz" })));
    assert_eq!(msg, "invalid headSha: zz");
}

#[test]
fn token_budget_клампится_к_схеме() {
    let f = fixture(Opts::default());
    let p = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD, "tokenBudget": 1 }))).clone();
    assert!(tokens(&p) <= 256, "кламп минимума схемы: {}", tokens(&p));
}

#[test]
fn review_findings_после_reset_читает_sidecar() {
    let f = fixture(Opts::default());
    let p = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD }))).clone();
    f.mcp.reset_store();
    let page = ok(&f.call("review_findings", json!({ "runFingerprint": p["runFingerprint"], "headSha": HEAD }))).clone();
    assert!(!page["findings"].as_array().unwrap().is_empty());
    assert_eq!(page["headSha"], HEAD);
    let (_, msg) = err(&f.call("review_findings", json!({ "runFingerprint": "0".repeat(64) })));
    assert_eq!(msg, "unknown runFingerprint");
    let (_, msg) = err(&f.call("review_findings", json!({})));
    assert_eq!(msg, "jobId or runFingerprint is required");
    let (_, msg) = err(&f.call("review_findings", json!({ "jobId": "0".repeat(64), "runFingerprint": "0".repeat(64) })));
    assert_eq!(msg, "provide jobId or runFingerprint, not both");
}

#[test]
fn wait_false_stub_до_moa_затем_completed_по_job_id() {
    let (hold_tx, hold_rx) = mpsc::channel();
    let runner = FakeRunner { findings: vec![critical("реальный баг", 3)], hold: Mutex::new(Some(hold_rx)), ..Default::default() };
    let f = fixture(Opts { runner, ..Default::default() });
    let stub = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD, "wait": false }))).clone();
    assert_eq!(stub["status"], "running");
    let job = stub["jobId"].as_str().unwrap().to_string();
    assert!(is_run_id(&job));
    assert_eq!(stub["runFingerprint"], "");
    assert_eq!(stub["previewWouldBlock"], false);
    let names: Vec<String> = std::fs::read_dir(f.root().join(SIDECAR_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![format!("{job}.json")]);

    let page = ok(&f.call("review_findings", json!({ "jobId": job, "headSha": HEAD }))).clone();
    assert_eq!(page["status"], "running");
    assert_eq!(page["findings"], json!([]));
    assert_eq!(page["total"], 0);
    let art = ok(&f.call("review_artifact", json!({ "jobId": job }))).clone();
    assert_eq!(art["status"], "running");
    assert_eq!(art["previewWouldBlock"], false);

    hold_tx.send(()).unwrap();
    f.mcp.wait_background();
    let done = ok(&f.call("review_findings", json!({ "jobId": job }))).clone();
    assert_eq!(done["status"], "completed");
    assert!(!done["findings"].as_array().unwrap().is_empty());
    assert!(is_run_id(done["runFingerprint"].as_str().unwrap()));

    f.mcp.reset_store();
    let from_disk = ok(&f.call("review_findings", json!({ "runFingerprint": done["runFingerprint"] }))).clone();
    assert!(!from_disk["findings"].as_array().unwrap().is_empty());
    let again = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD, "wait": false }))).clone();
    assert_eq!(again["status"], "completed");
    assert_eq!(again["jobId"], json!(job));
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn wait_false_ошибка_прогона_уходит_notification_и_освобождает_слот() {
    let runner = FakeRunner { fail: Some("движок упал".into()), ..Default::default() };
    let f = fixture(Opts { runner, ..Default::default() });
    let stub = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD, "wait": false }))).clone();
    f.mcp.wait_background();
    let notes = f.notes.lock().unwrap().clone();
    assert!(
        notes.iter().any(|(m, p)| m == "notifications/message" && p["level"] == "error" && p["data"] == "движок упал"),
        "{notes:?}"
    );
    let art = ok(&f.call("review_artifact", json!({ "jobId": stub["jobId"] }))).clone();
    assert_eq!(art["status"], "degraded");
    std::fs::write(f.root().join("other.patch"), CODE_DIFF).unwrap();
    let other = f.call("review_run", json!({ "diffPath": f.path("other.patch"), "headSha": HEAD }));
    assert!(!err(&other).1.contains("already running"), "слот освобождён");
}

fn git(head: &str, dirty: &str) -> Arc<dyn GitProbe> {
    Arc::new(FakeGit { head: head.into(), dirty: dirty.into() })
}

#[test]
fn git_пин_head_грязное_дерево_uncommitted_префикс() {
    let f = fixture(Opts { git: git(HEAD, ""), ..Default::default() });
    let (code, msg) = err(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": "bbbbbbb" })));
    assert_eq!((code, msg.as_str()), (-32602, "headSha does not match HEAD"));
    let p = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": &HEAD[..7] }))).clone();
    assert_eq!(p["headSha"], &HEAD[..7]);

    let f = fixture(Opts { git: git(HEAD, " M foo.ts"), ..Default::default() });
    let (_, msg) = err(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD })));
    assert_eq!(msg, "working tree dirty; pass uncommitted: true");
    let p = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD, "uncommitted": true }))).clone();
    assert_eq!(p["kind"], "review_preview");
}

#[test]
fn prior_export_доезжает_до_немого_github() {
    let f = fixture(Opts::default());
    std::fs::write(
        f.root().join("prior.json"),
        json!({ "findings": [{ "file": "foo.ts", "line": 3, "title": "старый", "fp": "abc123abc123" }], "verdict": { "runs": [{ "sha": HEAD }] }, "headSha": HEAD }).to_string(),
    )
    .unwrap();
    ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD, "priorExportPath": f.path("prior.json") })));
    let prior = f.runner.seen_prior.lock().unwrap().clone().unwrap();
    assert_eq!(prior.findings[0]["title"], "старый");
    assert_eq!(prior.runs.len(), 1);
    assert_eq!(prior.sha, HEAD);
    let silent = SilentGithub::new(prior);
    assert_eq!(silent.read_prior_sticky_data().unwrap().findings[0]["title"], "старый");
    assert_eq!(silent.upsert_sticky("x").unwrap(), "skipped");
}

#[test]
fn prior_export_вне_корня_и_битый_json_освобождают_слот() {
    let f = fixture(Opts::default());
    let outside = tempfile::tempdir().unwrap();
    let prior = outside.path().join("prior.json");
    std::fs::write(&prior, "{\"findings\":[]}").unwrap();
    let (code, msg) = err(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD, "priorExportPath": prior })));
    assert_eq!(code, -32602);
    assert!(msg.contains("escapes allowed root"), "{msg}");

    std::fs::write(f.root().join("other.patch"), CODE_DIFF).unwrap();
    std::fs::write(f.root().join("prior.json"), "{not-json").unwrap();
    let (code, msg) = err(&f.call("review_run", json!({ "diffPath": f.path("other.patch"), "headSha": HEAD, "priorExportPath": f.path("prior.json") })));
    assert_eq!(code, -32602);
    assert!(msg.contains("JSON"), "{msg}");
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 0);
    // Отклонение от JS: слот освобождён, следующий пин идёт в прогон.
    std::fs::write(f.root().join("third.patch"), CODE_DIFF).unwrap();
    ok(&f.call("review_run", json!({ "diffPath": f.path("third.patch"), "headSha": HEAD })));
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn review_findings_limit_клампится() {
    let f = fixture(Opts::default());
    let p = ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD }))).clone();
    let clamped = ok(&f.call("review_findings", json!({ "runFingerprint": p["runFingerprint"], "limit": 150 }))).clone();
    assert_eq!(clamped["limit"], 100);
    let omitted = ok(&f.call("review_findings", json!({ "runFingerprint": p["runFingerprint"] }))).clone();
    assert_eq!(omitted["limit"], 20);
    let filtered = ok(&f.call("review_findings", json!({ "runFingerprint": p["runFingerprint"], "severity": "nit" }))).clone();
    assert_eq!(filtered["total"], 0);
}

#[test]
fn review_route_logic_дифф_full() {
    let f = fixture(Opts::default());
    let body = ok(&f.call("review_route", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD }))).clone();
    assert_eq!(body["kind"], "review_route");
    assert_eq!(body["route"], "full");
    assert_eq!(body["files"][0]["path"], "foo.ts");
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn review_prescan_не_зовёт_движок() {
    let stages = FakeStages {
        prescan_findings: vec![json!({ "file": "foo.ts", "line": 3, "severity": "major", "title": "lint", "body": "x" })],
        ..Default::default()
    };
    let f = fixture(Opts { stages, ..Default::default() });
    let body = ok(&f.call("review_prescan", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD }))).clone();
    assert_eq!(body["kind"], "review_prescan");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["findings"][0]["title"], "lint");
    assert_eq!(body["byTool"], json!({ "eslint": 1, "astgrep": 0 }));
    assert_eq!(body["skipped"], 0);
    assert_eq!(f.runner.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn fp_check_requires_path_не_матчится_на_чужом_файле() {
    let title = "typedef Finding не описывает blast-radius";
    let body = "softBlast дублирует проверку через два поля";
    let hit = call_fp_check(json!({ "title": title, "body": body, "file": "lib/findings.mjs" }).as_object().unwrap()).unwrap();
    assert_eq!(hit["match"], true);
    assert_eq!(hit["patternId"], "softblast-typedef-cosmetic");
    let miss = call_fp_check(json!({ "title": title, "body": body, "file": "lib/other.mjs" }).as_object().unwrap()).unwrap();
    assert_eq!(miss["match"], false);
    assert_eq!(miss["patternId"], Value::Null);
    let f = fixture(Opts::default());
    let (code, msg) = err(&f.call("fp_check", json!({ "title": title, "body": body, "file": "  " })));
    assert_eq!((code, msg.as_str()), (-32602, "file is required"));
    let (_, msg) = err(&f.call("fp_check", json!({ "title": "x".repeat(8000), "body": "y".repeat(500), "file": "a" })));
    assert_eq!(msg, "text too long");
}

#[test]
fn fingerprint_check_still_present_stale_unknown_line() {
    let f = fixture(Opts::default());
    let parsed = parse_diff(CODE_DIFF);
    let prior_fp = finding_fingerprint(Some(&line_text(&parsed.files["foo.ts"])), &json!(3));
    assert!(!prior_fp.is_empty());
    let present = ok(&f.call("fingerprint_check", json!({ "file": "foo.ts", "line": 3, "priorFp": prior_fp, "diffPath": f.path("changes.patch") }))).clone();
    assert_eq!(present["status"], "still_present");
    assert_eq!(present["line"], 3);
    assert_eq!(present["currentFp"], json!(prior_fp));
    let unknown = ok(&f.call("fingerprint_check", json!({ "file": "foo.ts", "line": 999, "priorFp": "deadbeef0000", "diffPath": f.path("changes.patch") }))).clone();
    assert_eq!(unknown["status"], "unknown_line");
    std::fs::write(f.root().join("stale.patch"), CODE_DIFF.replace("export function foo() { return a; }", "export function bar() { return 0; }")).unwrap();
    let stale = ok(&f.call("fingerprint_check", json!({ "file": "foo.ts", "line": 3, "priorFp": prior_fp, "diffPath": f.path("stale.patch") }))).clone();
    assert_eq!(stale["status"], "stale");
    let other = ok(&f.call("fingerprint_check", json!({ "file": "nope.ts", "line": 3, "priorFp": prior_fp, "diffPath": f.path("changes.patch") }))).clone();
    assert_eq!((other["status"].clone(), other["line"].clone()), (json!("stale"), Value::Null));
    let (_, msg) = err(&f.call("fingerprint_check", json!({ "file": "foo.ts", "line": 0, "priorFp": "x", "diffPath": "a" })));
    assert_eq!(msg, "line is required");
}

#[test]
fn review_artifact_slim_обёртка_и_confine() {
    let f = fixture(Opts::default());
    std::fs::write(
        f.root().join("orakul-ai-review.json"),
        json!({
            "headSha": HEAD, "wouldBlock": true, "blockers": [critical("реальный баг", 3)],
            "findings": [critical("реальный баг", 3), { "file": "foo.ts", "line": 4, "severity": "nit", "title": "nit" }],
            "counts": { "findings": 2, "blockers": 1 }, "router": { "route": "full" }, "swarm": { "strong": true },
        })
        .to_string(),
    )
    .unwrap();
    let body = ok(&f.call("review_artifact", json!({ "path": f.path("orakul-ai-review.json"), "headSha": HEAD }))).clone();
    assert_eq!(body["kind"], "review_artifact");
    assert_eq!(body["previewWouldBlock"], true);
    assert!(body.get("wouldBlock").is_none());
    assert_eq!(body["strong"], true);
    assert_eq!(body["blockers"].as_array().unwrap().len(), 1);
    assert_eq!(body["findingsPreview"].as_array().unwrap().len(), 1, "блокер не дублируется в findingsPreview");
    let (_, msg) = err(&f.call("review_artifact", json!({ "path": f.path("orakul-ai-review.json"), "headSha": "bbbbbbb" })));
    assert!(msg.contains("does not match artifact"), "{msg}");
    let outside = tempfile::tempdir().unwrap();
    let escaped = outside.path().join("x.json");
    std::fs::write(&escaped, "{\"wouldBlock\":true}").unwrap();
    let (_, msg) = err(&f.call("review_artifact", json!({ "path": escaped })));
    assert!(msg.contains("escapes allowed root"), "{msg}");
    let (_, msg) = err(&f.call("review_artifact", json!({})));
    assert_eq!(msg, "provide exactly one of path, jobId, runFingerprint");
}

#[test]
fn фаза_c_без_флага_unknown_tool() {
    let f = fixture(Opts::default());
    let (code, msg) = err(&f.call("blast_scope", json!({ "diffPath": "a.patch", "headSha": HEAD })));
    assert_eq!((code, msg.as_str()), (-32602, "unknown tool: blast_scope"));
}

#[test]
fn blast_scope_suggested_scope_files() {
    let stages = FakeStages { affected: vec!["lib/a.ts".into(), "foo.ts".into()], ..Default::default() };
    let f = fixture(Opts { stages, env: vec![("REVIEW_MCP_EXTRA", "1")], ..Default::default() });
    let body = ok(&f.call("blast_scope", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD }))).clone();
    assert_eq!(body["kind"], "blast_scope");
    assert_eq!(body["files"], json!(["lib/a.ts"]));
    assert!(body.get("skipReason").is_none());
}

#[test]
fn actionability_explain_не_мутирует_находку() {
    let f = fixture(Opts { env: vec![("REVIEW_MCP_EXTRA", "1")], ..Default::default() });
    let finding = json!({ "file": "foo.ts", "line": 3, "severity": "minor", "title": "x", "body": "y", "verifierStatus": "verified" });
    let body = ok(&f.call("actionability_explain", json!({ "finding": finding }))).clone();
    assert_eq!(body["kind"], "actionability_explain");
    assert!(body["score"].is_number());
    assert_eq!(body["reasons"][0]["key"], "verified");
}

#[test]
fn review_metrics_битая_строка_не_роняет() {
    let f = fixture(Opts { env: vec![("REVIEW_MCP_EXTRA", "1")], ..Default::default() });
    std::fs::write(
        f.root().join("obs.jsonl"),
        "{\"type\":\"run_end\",\"wouldBlock\":true}\nnot-json\n{\"type\":\"run_end\",\"wouldBlock\":false}\n",
    )
    .unwrap();
    let body = ok(&f.call("review_metrics", json!({ "path": f.path("obs.jsonl") }))).clone();
    assert_eq!(body["kind"], "review_metrics");
    assert_eq!(body["runs"]["total"], 2);
    assert_eq!(body["runs"]["blocked"], 1);
}

#[test]
fn head_sha_prefix_match_как_в_js() {
    assert!(head_sha_prefix_match("AAAAAAA", HEAD));
    assert!(!head_sha_prefix_match("aaaaaa", HEAD), "короче 7 — не пин");
    assert!(!head_sha_prefix_match("", HEAD));
    assert!(!head_sha_prefix_match("bbbbbbb", HEAD));
}

#[test]
fn sidecar_вытесняется_сверх_disk_cap() {
    let f = fixture(Opts::default());
    let dir = f.root().join(SIDECAR_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..DISK_CAP + 3 {
        std::fs::write(dir.join(format!("{i:064}.json")), "{}").unwrap();
    }
    ok(&f.call("review_run", json!({ "diffPath": f.path("changes.patch"), "headSha": HEAD })));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), DISK_CAP);
}
