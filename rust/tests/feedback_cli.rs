//! Чёрноящичные тесты подкоманд feedback-семейства (RFC 16, T30): `feedback`,
//! `feedback-batch`, `feedback-migrate`, `feedback-promote` — argv, коды возврата
//! и файлы так, как их увидит workflow потребителя.
//!
//! Сеть — только фейковый GitHub на 127.0.0.1 (`GITHUB_API_URL`); `gh` CLI
//! недоступен: `PATH` указывает на пустой каталог. Окружение очищается
//! (`env_clear`), чтобы токены и профиль разработчика не просочились в прогон.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{json, Value};

const TOKEN: &str = "ghs_secret_token_t30";

/// Фейковый GitHub: `handler(метод_и_путь) → (status, body)`.
struct FakeGithub {
    api: String,
    requests: Arc<Mutex<Vec<String>>>,
}

fn fake_github<F>(handler: F) -> FakeGithub
where
    F: Fn(&str) -> (u16, String) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { break };
            let mut buf = [0u8; 65536];
            let mut seen = Vec::new();
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = conn.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
            }
            let raw = String::from_utf8_lossy(&seen).to_string();
            let line = raw.lines().next().unwrap_or("").to_string();
            let target = line.rsplit_once(' ').map(|(a, _)| a.to_string()).unwrap_or(line);
            log.lock().unwrap().push(raw);
            let (status, body) = handler(&target);
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = conn.write_all(resp.as_bytes());
        }
    });
    FakeGithub { api, requests }
}

/// Бинарь в чистом окружении: cwd — `dir`, `gh` в PATH нет.
fn cmd(dir: &Path) -> Command {
    let bin = dir.join(".no-bin");
    fs::create_dir_all(&bin).unwrap();
    let mut c = Command::cargo_bin("pr-review").unwrap();
    c.env_clear().env("PATH", &bin).env("REVIEW_HTTP_RETRIES", "0").current_dir(dir);
    c
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_no_token(out: &std::process::Output) {
    assert!(!stdout_of(out).contains(TOKEN), "токен в stdout");
    assert!(!String::from_utf8_lossy(&out.stderr).contains(TOKEN), "токен в stderr");
}

// ---------------------------------------------------------------------------
// feedback
// ---------------------------------------------------------------------------

#[test]
fn feedback_без_токена_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["feedback", "--pr", "5", "--repo", "o/r"])
        .assert()
        .code(1)
        .stderr(contains("[pr-review.feedback] нужны GITHUB_TOKEN, GITHUB_REPOSITORY (или --repo), PR_NUMBER (или --pr)"));
}

#[test]
fn feedback_выключен_exit_0() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .env("REVIEW_FEEDBACK", "0")
        .args(["feedback", "--dry-run"])
        .assert()
        .code(0)
        .stdout(contains("REVIEW_FEEDBACK=0 — harvest отключён"));
}

#[test]
fn feedback_dry_run_на_фейковом_github() {
    let dir = tempfile::tempdir().unwrap();
    let gh = fake_github(|_| (200, "[]".into()));
    let out = cmd(dir.path())
        .env("GITHUB_API_URL", &gh.api)
        .env("GITHUB_TOKEN", TOKEN)
        .env("GITHUB_REPOSITORY", "o/r")
        .env("PR_NUMBER", "5")
        .args(["feedback", "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let s = stdout_of(&out);
    assert!(s.contains("=== pr-review feedback harvest (R8) ==="), "{s}");
    assert!(s.contains("PR #5 · o/r"), "{s}");
    assert!(s.contains("- (нет нового feedback)"), "{s}");
    assert_no_token(&out);
    let reqs = gh.requests.lock().unwrap();
    assert!(reqs.iter().any(|r| r.starts_with("GET /repos/o/r/issues/5/labels")));
    assert!(reqs.iter().all(|r| r.contains(&format!("Bearer {TOKEN}"))));
    // dry-run ничего не пишет
    assert!(!dir.path().join(".ai-review").exists());
}

#[test]
fn feedback_сбой_github_exit_1_без_токена_в_выводе() {
    let dir = tempfile::tempdir().unwrap();
    let gh = fake_github(|_| (500, r#"{"message":"boom"}"#.into()));
    let out = cmd(dir.path())
        .env("GITHUB_API_URL", &gh.api)
        .env("GITHUB_TOKEN", TOKEN)
        .args(["feedback", "--pr", "5", "--repo", "o/r"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("[pr-review.feedback]"));
    assert_no_token(&out);
}

// ---------------------------------------------------------------------------
// feedback-batch
// ---------------------------------------------------------------------------

#[test]
fn feedback_batch_без_env_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    cmd(dir.path())
        .args(["feedback-batch", "--dry-run"])
        .assert()
        .code(1)
        .stderr(contains("[pr-review.feedback-batch] batch harvest требует GITHUB_REPOSITORY и GITHUB_TOKEN"));
}

fn recent_iso() -> String {
    // «вчера» не нужен точно: дискавери берёт PR за 14 дней, 2099 год всегда свежее.
    "2099-01-01T00:00:00Z".to_string()
}

#[test]
fn feedback_batch_дискавери_отбирает_людей_и_харвестит() {
    let dir = tempfile::tempdir().unwrap();
    let pulls = json!([
        { "number": 11, "updated_at": recent_iso(), "user": { "type": "User" } },
        { "number": 12, "updated_at": recent_iso(), "user": { "type": "Bot" } },
        { "number": 13, "updated_at": "2000-01-01T00:00:00Z", "user": { "type": "User" } },
    ])
    .to_string();
    let gh = fake_github(move |target| {
        if target.starts_with("GET /repos/o/r/pulls?state=all") {
            (200, pulls.clone())
        } else {
            (200, "[]".into())
        }
    });
    let out = cmd(dir.path())
        .env("GITHUB_API_URL", &gh.api)
        .env("GITHUB_TOKEN", TOKEN)
        .env("GITHUB_REPOSITORY", "o/r")
        .args(["feedback-batch", "--dry-run"])
        .output()
        .unwrap();
    let s = stdout_of(&out);
    assert_eq!(out.status.code(), Some(0), "{s}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(s.contains("=== pr-review feedback batch ==="), "{s}");
    assert!(s.contains("selected: 1, processed: 1, failed: 0"), "{s}");
    assert!(s.contains("- PR #11: ok; new=0"), "{s}");
    assert_no_token(&out);
    let reqs = gh.requests.lock().unwrap();
    assert!(!reqs.iter().any(|r| r.contains("/issues/12/") || r.contains("/issues/13/")));
}

#[test]
fn feedback_batch_явный_pr_и_провал_harvest_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    let gh = fake_github(|target| {
        if target.contains("/issues/42/labels") {
            (500, "{}".into())
        } else {
            (200, "[]".into())
        }
    });
    let out = cmd(dir.path())
        .env("GITHUB_API_URL", &gh.api)
        .env("GITHUB_TOKEN", TOKEN)
        .env("GITHUB_REPOSITORY", "o/r")
        .args(["feedback-batch", "--pr", "42"])
        .output()
        .unwrap();
    let s = stdout_of(&out);
    assert_eq!(out.status.code(), Some(1), "{s}");
    assert!(s.contains("selected: 1, processed: 1, failed: 1"), "{s}");
    assert!(s.contains("- PR #42: failed; new=0"), "{s}");
    assert_no_token(&out);
    // явный PR — без дискавери
    assert!(!gh.requests.lock().unwrap().iter().any(|r| r.contains("/pulls?state=all")));
}

// ---------------------------------------------------------------------------
// feedback-migrate
// ---------------------------------------------------------------------------

const LEGACY: &str = r#"{"ts":"2026-07-05T00:00:00.000Z","repo":"o/r","pr":7,"source":"label","signal":"fp","finding":{"file":"server/a.ts","line":9,"severity":"major","title":"false alarm"}}"#;

#[test]
fn feedback_migrate_проверка_и_запись() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("fp.jsonl");
    fs::write(&log, format!("{LEGACY}\n")).unwrap();

    let out = cmd(dir.path()).args(["feedback-migrate", "--file", "fp.jsonl"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_str(stdout_of(&out).trim()).unwrap();
    assert_eq!(v["write"], json!(false));
    assert_eq!(v["changed"], json!(true));
    assert_eq!(v["migrated"], json!(1));
    assert_eq!(v["invalid"], json!(0));
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["path", "write", "changed", "total", "valid", "migrated", "invalid"]);
    assert_eq!(fs::read_to_string(&log).unwrap(), format!("{LEGACY}\n"), "без --write файл не трогается");

    cmd(dir.path()).args(["feedback-migrate", "--file", "fp.jsonl", "--write"]).assert().code(0);
    let migrated = fs::read_to_string(&log).unwrap();
    assert!(migrated.contains(r#""schemaVersion":1"#), "{migrated}");

    // идемпотентно
    let out = cmd(dir.path()).args(["feedback-migrate", "--file", "fp.jsonl", "--write"]).output().unwrap();
    let v: Value = serde_json::from_str(stdout_of(&out).trim()).unwrap();
    assert_eq!(v["changed"], json!(false));
    assert_eq!(fs::read_to_string(&log).unwrap(), migrated);
}

#[test]
fn feedback_migrate_невалидные_строки_exit_1_и_отказ_записи() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("fp.jsonl");
    let raw = format!("{LEGACY}\nnot-json\n");
    fs::write(&log, &raw).unwrap();
    let out = cmd(dir.path()).args(["feedback-migrate", "--file", "fp.jsonl"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_str(stdout_of(&out).trim()).unwrap();
    assert_eq!(v["invalid"], json!(1));

    cmd(dir.path())
        .args(["feedback-migrate", "--file", "fp.jsonl", "--write"])
        .assert()
        .code(1)
        .stderr(contains("[migrate-feedback] migration refused: 1 invalid row(s)"));
    assert_eq!(fs::read_to_string(&log).unwrap(), raw);
}

#[test]
fn feedback_migrate_путь_из_конфига_и_пустой_журнал() {
    let dir = tempfile::tempdir().unwrap();
    let out = cmd(dir.path())
        .env("REVIEW_FEEDBACK_LOG", "nested/none.jsonl")
        .args(["feedback-migrate"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_str(stdout_of(&out).trim()).unwrap();
    assert!(v["path"].as_str().unwrap().ends_with("nested/none.jsonl"));
    assert_eq!(v["total"], json!(0));
    assert_eq!(v["changed"], json!(false));
}

// ---------------------------------------------------------------------------
// feedback-promote
// ---------------------------------------------------------------------------

fn roundtrip_candidate(id: &str) -> Value {
    json!({
        "suggestedId": id,
        "meta": {
            "id": id,
            "description": "promote round-trip: file-absent при файле в diff",
            "expected": { "shouldBlock": false, "blockerCount": 0, "findingCount": 0 }
        },
        "findings": [{
            "file": "server/preps.ts",
            "line": 10,
            "severity": "critical",
            "confidence": 0.95,
            "title": "preps.ts не в PR",
            "body": "файл preps.ts отсутствует в диффе PR",
            "model": "m1"
        }],
        "fromFeedback": { "pr": 42 }
    })
}

#[test]
fn feedback_promote_кандидат_в_fixture_через_gh_cli() {
    let dir = tempfile::tempdir().unwrap();
    let cands = dir.path().join("cands");
    let fixtures = dir.path().join("fx");
    fs::create_dir_all(&cands).unwrap();
    fs::write(cands.join("a.json"), roundtrip_candidate("fp-promote-roundtrip").to_string()).unwrap();
    // id с обходом пути — отвергается, за пределы fixtures ничего не пишется
    let mut evil = roundtrip_candidate("../../evil");
    evil["meta"]["id"] = json!("../../evil");
    fs::write(cands.join("b.json"), evil.to_string()).unwrap();
    fs::write(cands.join("c.json"), "{not json").unwrap();

    // фейковый `gh` в PATH: пишет argv в файл и отдаёт полный дифф
    let diff = "diff --git a/server/preps.ts b/server/preps.ts\n--- a/server/preps.ts\n+++ b/server/preps.ts\n@@ -1 +1,2 @@\n const a = 1;\n+export const preps = 1;\n";
    let bin = dir.path().join(".no-bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(dir.path().join("diff.txt"), diff).unwrap();
    let script = bin.join("gh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done > '{0}/gh-args.txt'\n/bin/cat '{0}/diff.txt'\n",
            dir.path().display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let out = cmd(dir.path())
        .env("GITHUB_REPOSITORY", "o/r")
        .env("REVIEW_FEEDBACK_CANDIDATES_DIR", "cands")
        .env("REVIEW_FIXTURES_DIR", "fx")
        .args(["feedback-promote"])
        .output()
        .unwrap();
    let s = stdout_of(&out);
    assert_eq!(out.status.code(), Some(1), "битые кандидаты → exit 1\n{s}");
    assert!(s.contains("=== promote candidates ==="), "{s}");
    assert!(s.contains("promoted: 1, skipped: 0, failed: 2"), "{s}");
    assert!(s.contains("- fp-promote-roundtrip: ok"), "{s}");
    assert!(s.contains("- ../../evil: invalid id or missing file"), "{s}");
    assert!(s.contains("- c.json: parse: "), "{s}");
    // путь из находки идёт после `--` и опцией gh не становится
    assert_eq!(
        fs::read_to_string(dir.path().join("gh-args.txt")).unwrap(),
        "pr\ndiff\n42\n--\nserver/preps.ts\n"
    );

    let case = fixtures.join("fp-promote-roundtrip");
    assert_eq!(fs::read_to_string(case.join("diff.patch")).unwrap(), diff);
    let meta: Value = serde_json::from_str(&fs::read_to_string(case.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["expected"]["findingCount"], json!(0));
    let findings: Value = serde_json::from_str(&fs::read_to_string(case.join("findings.json")).unwrap()).unwrap();
    assert_eq!(findings["findings"][0]["file"], json!("server/preps.ts"));
    assert!(!dir.path().join("evil").exists());
    assert_eq!(fs::read_dir(&fixtures).unwrap().count(), 1);

    // promoted fixture проходит eval-харнес
    cmd(dir.path()).args(["eval", "--fixtures", "fx"]).assert().code(0);

    // повторный прогон — skip, не провал
    fs::remove_file(cands.join("b.json")).unwrap();
    fs::remove_file(cands.join("c.json")).unwrap();
    cmd(dir.path())
        .env("REVIEW_FEEDBACK_CANDIDATES_DIR", "cands")
        .env("REVIEW_FIXTURES_DIR", "fx")
        .args(["feedback-promote"])
        .assert()
        .code(0)
        .stdout(contains("promoted: 0, skipped: 1, failed: 0"))
        .stdout(contains("- fp-promote-roundtrip: skip"));
}

/// API-фолбэк (gh CLI нет): `patch` файла из `/pulls/N/files` — как в JS, без
/// заголовков `diff --git`, поэтому parseDiff не видит файл и FP «файл не в PR» не
/// снимается — кандидат честно проваливает проверку и fixture не пишется.
#[test]
fn feedback_promote_api_фолбэк_с_токеном() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("cands")).unwrap();
    fs::write(dir.path().join("cands/a.json"), roundtrip_candidate("fp-api").to_string()).unwrap();
    let files = json!([{ "filename": "server/preps.ts", "patch": "@@ -1 +1 @@\n+x" }]).to_string();
    let gh = fake_github(move |target| {
        if target == "GET /repos/o/r/pulls/42/files?per_page=100" {
            (200, files.clone())
        } else {
            (404, "{}".into())
        }
    });
    let out = cmd(dir.path())
        .env("GITHUB_API_URL", &gh.api)
        .env("GITHUB_TOKEN", TOKEN)
        .env("GITHUB_REPOSITORY", "o/r")
        .env("REVIEW_FEEDBACK_CANDIDATES_DIR", "cands")
        .env("REVIEW_FIXTURES_DIR", "fx")
        .args(["feedback-promote"])
        .output()
        .unwrap();
    let s = stdout_of(&out);
    assert_eq!(out.status.code(), Some(1), "{s}");
    assert!(s.contains("- fp-api: shouldBlock: ожидали false, получили true"), "{s}");
    assert_no_token(&out);
    assert!(!dir.path().join("fx/fp-api").exists());
    let reqs = gh.requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].starts_with("GET /repos/o/r/pulls/42/files?per_page=100"));
    assert!(reqs[0].to_lowercase().contains(&format!("authorization: bearer {}", TOKEN.to_lowercase())));
}

#[test]
fn feedback_promote_без_сети_пишет_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("cands")).unwrap();
    fs::write(dir.path().join("cands/a.json"), roundtrip_candidate("fp-ph").to_string()).unwrap();
    cmd(dir.path())
        .env("REVIEW_FEEDBACK_CANDIDATES_DIR", "cands")
        .env("REVIEW_FIXTURES_DIR", "fx")
        .args(["feedback-promote"])
        .assert()
        .code(0)
        .stdout(contains("promoted: 1, skipped: 0, failed: 0"));
    let diff = fs::read_to_string(dir.path().join("fx/fp-ph/diff.patch")).unwrap();
    assert!(diff.contains("+# placeholder — replace with gh pr diff 42 -- server/preps.ts"), "{diff}");
}

#[test]
fn feedback_promote_aggregate_паттерн_и_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let row = |pr: u64| {
        json!({
            "schemaVersion": 1, "ts": "2026-07-05T00:00:00.000Z", "repo": "o/r", "pr": pr,
            "source": "label", "signal": "fp",
            "finding": { "file": "app/e2e/x.spec.ts", "line": 3, "severity": "minor", "title": "нет e2e теста" }
        })
        .to_string()
    };
    fs::write(dir.path().join("fp.jsonl"), format!("{}\n{}\n", row(1), row(2))).unwrap();
    let run = || {
        cmd(dir.path())
            .env("REVIEW_FEEDBACK_LOG", "fp.jsonl")
            .env("REVIEW_FP_PATTERNS", "patterns.jsonl")
            .env("REVIEW_FIXTURES_DIR", "fx")
            .args(["feedback-promote", "--aggregate"])
            .output()
            .unwrap()
    };
    let out = run();
    let s = stdout_of(&out);
    assert_eq!(out.status.code(), Some(0), "{s}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(s.contains("=== feedback promote (aggregate) ==="), "{s}");
    assert!(s.contains("groups ≥ threshold: 1"), "{s}");
    assert!(s.contains("patterns appended: 1"), "{s}");
    assert!(s.contains("fixtures promoted: 1"), "{s}");
    let patterns = fs::read_to_string(dir.path().join("patterns.jsonl")).unwrap();
    assert!(patterns.starts_with(r#"{"id":"e2e-in-diff","description":"#), "{patterns}");
    assert_eq!(patterns.lines().count(), 1);

    // второй прогон: паттерн уже есть, fixture — skip
    let s = stdout_of(&run());
    assert!(s.contains("patterns appended: 0"), "{s}");
    assert!(s.contains("fixtures skipped: 1"), "{s}");
    assert_eq!(fs::read_to_string(dir.path().join("patterns.jsonl")).unwrap(), patterns);
}
