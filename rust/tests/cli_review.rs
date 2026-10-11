//! Чёрноящичные тесты `pr-review review` (RFC 16, задача T27): dry-run на диффе-фикстуре
//! против фейкового LLM-сервера на 127.0.0.1. Проверяется контракт кодов возврата, тот же,
//! что у `bin/pr-review.mjs` + `runReview`: exit 1 — только подтверждённый блокер или
//! осознанный strict-режим, всё остальное (включая аварию движка) — exit 0.
//!
//! Конфигурация прогона — consensus на одной модели плюс verifier отдельной моделью:
//! это кратчайший путь до «подтверждённого» блокера (жюри refutation есть только в MoA).
//! Prescan выключен — он зовёт внешние eslint/ast-grep, а тест про CLI, не про линтеры.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use assert_cmd::Command;
use serde_json::{json, Value};

const SECRET: &str = "sekret-llm-key-t27";

const DIFF: &str = "diff --git a/src/db.ts b/src/db.ts
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/src/db.ts
@@ -0,0 +1,4 @@
+export function find(db: any, id: string) {
+  return db.query(\"SELECT * FROM users WHERE id = '\" + id + \"'\");
+}
+
";

/// Что отвечает фейковый LLM.
#[derive(Clone, Copy)]
enum Mode {
    /// Модели не нашли ничего.
    Clean,
    /// Критическая находка, verifier её подтверждает.
    Blocker,
    /// Провайдер отвечает 401 — ошибка не транзиентная, ретраев нет.
    Down,
}

struct FakeLlm {
    url: String,
    hits: Arc<AtomicUsize>,
}

fn answer(mode: Mode) -> Value {
    match mode {
        Mode::Clean => json!({ "summary": "чисто", "findings": [], "verdicts": [] }),
        // Один ответ годится и для ревью (findings), и для verifier (verdicts):
        // каждый вызов читает свой ключ.
        Mode::Blocker => json!({
            "summary": "инъекция",
            "findings": [{
                "file": "src/db.ts",
                "line": 2,
                "severity": "critical",
                "title": "SQL-инъекция через конкатенацию id",
                "body": "id попадает в SQL без параметризации.",
                "quote": "return db.query(\"SELECT * FROM users WHERE id = '\" + id + \"'\");",
                "confidence": 0.95
            }],
            "verdicts": [{ "candidateIndex": 0, "decision": "verified", "reason": "конкатенация видна в строке 2" }]
        }),
        Mode::Down => Value::Null,
    }
}

fn handle(mut conn: TcpStream, mode: Mode) {
    let mut seen = Vec::new();
    let mut buf = [0u8; 8192];
    // Заголовки, затем тело по Content-Length: ответ до конца тела клиент может принять за обрыв.
    let header_end = loop {
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
        if let Some(p) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&seen[..header_end]).to_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0);
    while seen.len() < header_end + len {
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
    }
    let resp = match mode {
        Mode::Down => {
            let body = "{\"error\":\"unauthorized\"}";
            format!("HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        }
        _ => {
            let content = answer(mode).to_string();
            let chunk = json!({ "choices": [{ "delta": { "content": content } }] });
            let usage = json!({ "choices": [], "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 } });
            let body = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        }
    };
    let _ = conn.write_all(resp.as_bytes());
    let _ = conn.flush();
}

/// Сервер живёт до конца процесса теста: поток accept-цикла просто остаётся висеть.
fn fake_llm(mode: Mode) -> FakeLlm {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { break };
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || handle(conn, mode));
        }
    });
    FakeLlm { url, hits }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// `pr-review review --dry-run --diff <фикстура>` в пустом временном каталоге
/// с вычищенным окружением: ни GitHub-кредов, ни чужих REVIEW_*.
fn review(llm: Option<&FakeLlm>, extra_env: &[(&str, &str)], args: &[&str]) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let diff = dir.path().join("changes.patch");
    std::fs::write(&diff, DIFF).unwrap();
    let mut cmd = Command::cargo_bin("pr-review").unwrap();
    cmd.env_clear()
        .current_dir(dir.path())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("OLLAMA_API_KEY", SECRET)
        .env("REVIEW_MODE", "consensus")
        .env("OLLAMA_MODELS", "fake-m1")
        .env("REVIEW_CONSENSUS", "1")
        .env("REVIEW_VERIFIER_MODEL", "fake-ver")
        .env("REVIEW_PRESCAN", "0")
        .env("REVIEW_HTTP_RETRIES", "0")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .arg("review");
    if let Some(l) = llm {
        cmd.env("OLLAMA_BASE_URL", &l.url);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    if args.is_empty() {
        cmd.args(["--dry-run", "--diff"]).arg(&diff);
    } else {
        cmd.args(args.iter().map(|a| if *a == "{diff}" { diff.to_string_lossy().into_owned() } else { a.to_string() }));
    }
    let out = cmd.output().unwrap();
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn assert_no_secret(r: &Run) {
    assert!(!r.stdout.contains(SECRET) && !r.stderr.contains(SECRET), "ключ LLM утёк в вывод");
}

#[test]
fn dry_run_без_находок_exit_0() {
    let llm = fake_llm(Mode::Clean);
    let r = review(Some(&llm), &[], &[]);
    assert_eq!(r.code, 0, "stdout:\n{}\nstderr:\n{}", r.stdout, r.stderr);
    assert!(llm.hits.load(Ordering::SeqCst) >= 1, "LLM не вызывался — прогон не дошёл до движка:\n{}", r.stdout);
    assert!(r.stdout.contains("дифф из файла"), "--diff прочитан:\n{}", r.stdout);
    assert_no_secret(&r);
}

#[test]
fn подтверждённый_блокер_exit_1() {
    let llm = fake_llm(Mode::Blocker);
    let r = review(Some(&llm), &[], &[]);
    assert_eq!(r.code, 1, "stdout:\n{}\nstderr:\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("verifier итог: verified=1"), "блокер прошёл verifier:\n{}", r.stdout);
    assert_no_secret(&r);
}

#[test]
fn блокер_ниже_порога_confidence_не_блокирует() {
    // Тот же ответ модели, но порог REVIEW_CONFIDENCE_MIN выше уверенности находки.
    // Контроль к предыдущему тесту: exit 1 там — от гейта, а не от любого ответа модели.
    let llm = fake_llm(Mode::Blocker);
    let r = review(Some(&llm), &[("REVIEW_CONFIDENCE_MIN", "0.99")], &[]);
    assert_eq!(r.code, 0, "confidence ниже порога не блокирует:\n{}", r.stdout);
}

#[test]
fn падение_llm_fail_open_exit_0() {
    let llm = fake_llm(Mode::Down);
    let r = review(Some(&llm), &[("REVIEW_LLM_REQUIRED", "0")], &[]);
    assert_eq!(r.code, 0, "stdout:\n{}\nstderr:\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("все модели недоступны"), "{}", r.stdout);
    assert!(r.stdout.contains("fail-open"), "{}", r.stdout);
    assert_no_secret(&r);
}

#[test]
fn падение_llm_при_llm_required_не_0() {
    let llm = fake_llm(Mode::Down);
    let r = review(Some(&llm), &[("REVIEW_LLM_REQUIRED", "1")], &[]);
    assert_ne!(r.code, 0, "stdout:\n{}\nstderr:\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("fail-closed (REVIEW_LLM_REQUIRED)"), "{}", r.stdout);
    assert_no_secret(&r);
}

#[test]
fn недоступный_адрес_llm_тоже_fail_open() {
    // Порт 1 закрыт: сетевой сбой, а не HTTP-ответ.
    let r = review(None, &[("REVIEW_LLM_REQUIRED", "0"), ("OLLAMA_BASE_URL", "http://127.0.0.1:1/v1")], &[]);
    assert_eq!(r.code, 0, "stdout:\n{}\nstderr:\n{}", r.stdout, r.stderr);
}

#[test]
fn авария_движка_exit_0() {
    // Несуществующий --diff: в JS readFileSync бросает, runReview ловит → exit 0.
    let llm = fake_llm(Mode::Blocker);
    let r = review(Some(&llm), &[], &["--dry-run", "--diff", "/nonexistent/t27.patch"]);
    assert_eq!(r.code, 0, "stdout:\n{}\nstderr:\n{}", r.stdout, r.stderr);
    assert!(r.stderr.contains("[pr-review]") && r.stderr.contains("не читается дифф"), "{}", r.stderr);
    assert_eq!(llm.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn без_dry_run_и_кредов_мягкий_отказ_exit_0() {
    let llm = fake_llm(Mode::Blocker);
    let r = review(Some(&llm), &[], &["--diff", "{diff}"]);
    assert_eq!(r.code, 0, "{}", r.stdout);
    assert!(r.stdout.contains("SOFT-FAIL: постинг требует"), "{}", r.stdout);
    assert_eq!(llm.hits.load(Ordering::SeqCst), 0, "до LLM не дошло");
}

#[test]
fn без_ключа_мягкий_отказ_exit_0() {
    let llm = fake_llm(Mode::Blocker);
    let r = review(Some(&llm), &[("OLLAMA_API_KEY", "")], &[]);
    assert_eq!(r.code, 0, "{}", r.stdout);
    assert!(r.stdout.contains("SOFT-FAIL: нет LLM API key"), "{}", r.stdout);
}
