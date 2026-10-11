//! Чёрноящичные тесты `pr-review review-mcp` (RFC 16, T41) — stdio-сервер ревью-MCP
//! поверх боевой сборки зависимостей (`ReviewWiring`) против фейкового LLM на 127.0.0.1.
//!
//! Что проверяется сверх доменных кейсов `src/review_mcp_tests.rs` (порт
//! test/pr-review.mcp.test.mjs):
//! - review-MCP не гейт: подтверждённый блокер не даёт exit 1, GitHub (фейковый сервер
//!   с настоящим токеном в env) не получает ни одного запроса;
//! - stdout — только протокол: логи прогона приходят `notifications/message`;
//! - `wait: false` + поллинг по `jobId`, один слот (`review already running`), фоновые
//!   notifications в том же формате кадра (Content-Length), что ответы;
//! - EOF stdin при живом фоновом прогоне — сервер дожидается его и дописывает sidecar;
//! - ключ LLM не попадает ни в stdout, ни в stderr, ни в sidecar.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const SECRET: &str = "sekret-llm-key-t41";
const GH_SECRET: &str = "sekret-gh-token-t41";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

// ---------------------------------------------------------------------------
// Фейковые HTTP-серверы
// ---------------------------------------------------------------------------

/// Ворота: пока закрыты, фейковый LLM не отвечает (MoA «идёт»).
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }
    fn wait(&self) {
        let mut open = self.0 .0.lock().unwrap();
        while !*open {
            open = self.0 .1.wait(open).unwrap();
        }
    }
}

struct Fake {
    url: String,
    hits: Arc<AtomicUsize>,
}

fn read_request(conn: &mut TcpStream) -> Option<()> {
    let mut seen = Vec::new();
    let mut buf = [0u8; 8192];
    let header_end = loop {
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => return None,
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
            Ok(0) | Err(_) => return None,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
    }
    Some(())
}

fn blocker_answer() -> Value {
    json!({
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
    })
}

/// SSE-ответ LLM с подтверждённым блокером; ждёт открытия ворот.
fn fake_llm(gate: Gate) -> Fake {
    serve(move |mut conn| {
        if read_request(&mut conn).is_none() {
            return;
        }
        gate.wait();
        let content = blocker_answer().to_string();
        let chunk = json!({ "choices": [{ "delta": { "content": content } }] });
        let usage = json!({ "choices": [], "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 } });
        let body = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = conn.write_all(resp.as_bytes());
    })
}

/// GitHub, который не должен получить ни одного запроса.
fn fake_github() -> Fake {
    serve(|mut conn| {
        let _ = read_request(&mut conn);
        let body = "{}";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = conn.write_all(resp.as_bytes());
    })
}

fn serve(handler: impl Fn(TcpStream) + Send + Sync + Clone + 'static) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { break };
            counter.fetch_add(1, Ordering::SeqCst);
            let h = handler.clone();
            std::thread::spawn(move || h(conn));
        }
    });
    Fake { url, hits }
}

// ---------------------------------------------------------------------------
// Клиент MCP поверх процесса
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Wire {
    Json,
    Lsp,
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Value>,
    wire: Wire,
    /// Всё, что пришло без id (notifications), — в порядке прихода.
    notes: Vec<Value>,
    /// Сырые строки/тела stdout — для проверки утечки секрета.
    raw: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

struct Env {
    dir: tempfile::TempDir,
    llm: Fake,
    github: Fake,
}

impl Env {
    fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }
}

fn env_with(gate: Gate) -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("changes.patch"), DIFF).unwrap();
    std::fs::write(dir.path().join("other.patch"), DIFF).unwrap();
    Env { dir, llm: fake_llm(gate), github: fake_github() }
}

fn spawn(env: &Env, wire: Wire, root_override: Option<&str>) -> Server {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pr-review"));
    let root = env.root();
    cmd.arg("review-mcp")
        .env_clear()
        .current_dir(std::env::temp_dir())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &root)
        .env("REVIEW_MCP_ROOT", root_override.map(PathBuf::from).unwrap_or(root.clone()))
        .env("OLLAMA_API_KEY", SECRET)
        .env("OLLAMA_BASE_URL", format!("{}/v1", env.llm.url))
        .env("REVIEW_MODE", "consensus")
        .env("OLLAMA_MODELS", "fake-m1")
        .env("REVIEW_CONSENSUS", "1")
        .env("REVIEW_VERIFIER_MODEL", "fake-ver")
        .env("REVIEW_PRESCAN", "0")
        .env("REVIEW_HTTP_RETRIES", "0")
        .env("NO_PROXY", "127.0.0.1,localhost")
        // GitHub сконфигурирован полностью — MCP всё равно не должен его трогать.
        .env("GITHUB_TOKEN", GH_SECRET)
        .env("GITHUB_REPOSITORY", "o/r")
        .env("PR_NUMBER", "7")
        .env("GITHUB_API_URL", &env.github.url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("запуск pr-review review-mcp");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let raw = Arc::new(Mutex::new(String::new()));
    let raw_w = Arc::clone(&raw);
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        loop {
            let body = match wire {
                Wire::Json => {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    line
                }
                Wire::Lsp => {
                    let mut header = String::new();
                    if r.read_line(&mut header).unwrap_or(0) == 0 {
                        break;
                    }
                    let len: usize = header
                        .trim()
                        .strip_prefix("Content-Length: ")
                        .unwrap_or_else(|| panic!("ожидался заголовок кадра, пришло {header:?}"))
                        .parse()
                        .unwrap();
                    let mut blank = String::new();
                    r.read_line(&mut blank).unwrap();
                    assert_eq!(blank, "\r\n");
                    let mut body = vec![0u8; len];
                    r.read_exact(&mut body).unwrap();
                    String::from_utf8(body).unwrap()
                }
            };
            raw_w.lock().unwrap().push_str(&body);
            let v: Value = serde_json::from_str(body.trim())
                .unwrap_or_else(|e| panic!("stdout не протокол ({e}): {body:?}"));
            if tx.send(v).is_err() {
                break;
            }
        }
    });
    let stderr = Arc::new(Mutex::new(String::new()));
    let err_w = Arc::clone(&stderr);
    let mut err_pipe = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        err_w.lock().unwrap().push_str(&s);
    });
    let stdin = child.stdin.take();
    Server { child, stdin, rx, wire, notes: Vec::new(), raw, stderr }
}

impl Server {
    fn send(&mut self, msg: &Value) {
        let body = msg.to_string();
        let bytes = match self.wire {
            Wire::Json => format!("{body}\n").into_bytes(),
            Wire::Lsp => format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes(),
        };
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(&bytes).unwrap();
        stdin.flush().unwrap();
    }

    fn recv_id(&mut self, id: i64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = self.rx.recv_timeout(left).unwrap_or_else(|_| panic!("нет ответа на id {id}; stderr:\n{}", self.stderr()));
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
            assert!(v.get("id").is_none(), "чужой ответ {v}");
            self.notes.push(v);
        }
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.recv_id(id)
    }

    fn tool(&mut self, id: i64, name: &str, args: Value) -> Value {
        self.request(id, "tools/call", json!({ "name": name, "arguments": args }))
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Выход процесса с watchdog-ом.
    fn exit_code(&mut self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().expect("выход по коду, не по сигналу");
            }
            if Instant::now() > deadline {
                self.child.kill().ok();
                panic!("review-mcp не завершился за 60 с");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_no_secret(&self) {
        let out = self.raw.lock().unwrap().clone();
        let err = self.stderr();
        for s in [SECRET, GH_SECRET] {
            assert!(!out.contains(s) && !err.contains(s), "секрет утёк в вывод");
        }
    }
}

fn content(res: &Value) -> &Value {
    assert!(res.get("error").is_none(), "ошибка: {res}");
    let sc = &res["result"]["structuredContent"];
    let text: Value = serde_json::from_str(res["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&text, sc, "content[0].text — тот же payload");
    sc
}

fn sidecar(root: &Path, job: &str) -> Value {
    let text = std::fs::read_to_string(root.join(".ai-review/mcp/runs").join(format!("{job}.json"))).unwrap();
    assert!(!text.contains(SECRET) && !text.contains(GH_SECRET), "секрет в sidecar");
    serde_json::from_str(&text).unwrap()
}

// ---------------------------------------------------------------------------
// Кейсы
// ---------------------------------------------------------------------------

#[test]
fn handshake_без_домена_и_каталог_тулзов() {
    let gate = Gate::default();
    let env = env_with(gate);
    let mut s = spawn(&env, Wire::Json, None);
    let init = s.request(1, "initialize", json!({ "protocolVersion": "2024-11-05" }));
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "ai-reviewer");
    s.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let list = s.request(2, "tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["review_run", "review_findings", "review_route", "review_prescan", "fp_check", "fingerprint_check", "review_artifact"]
    );
    assert_eq!(s.request(3, "ping", json!({}))["result"], json!({}));
    let unknown = s.tool(4, "blast_scope", json!({}));
    assert_eq!(unknown["error"]["code"], -32602);
    s.close_stdin();
    assert_eq!(s.exit_code(), 0);
    assert!(s.notes.is_empty(), "notifications/initialized без ответа: {:?}", s.notes);
    assert_eq!(env.llm.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn review_run_блокер_не_гейт_и_github_не_трогается() {
    let gate = Gate::default();
    gate.open();
    let env = env_with(gate);
    let mut s = spawn(&env, Wire::Json, None);
    let res = s.tool(1, "review_run", json!({ "diffPath": "changes.patch", "headSha": HEAD }));
    let p = content(&res).clone();
    assert_eq!(p["kind"], "review_preview");
    assert_eq!(p["status"], "completed", "{p}");
    assert_eq!(p["previewWouldBlock"], true, "{p}");
    assert_eq!(p["headSha"], HEAD);
    assert!(p.get("wouldBlock").is_none());
    let blockers = p["blockers"].as_array().unwrap();
    assert_eq!(blockers.len(), 1, "{p}");
    assert_eq!(blockers[0]["file"], "src/db.ts");
    assert!(env.llm.hits.load(Ordering::SeqCst) >= 2, "MoA и verifier дошли до LLM");
    assert_eq!(env.github.hits.load(Ordering::SeqCst), 0, "review-MCP не ходит в GitHub");
    // Логи прогона — notifications, а не печать в stdout.
    assert!(
        s.notes.iter().any(|n| n["method"] == "notifications/message" && n["params"]["level"] == "info"),
        "логи прогона приходят notifications: {:?}",
        s.notes
    );

    let job = p["jobId"].as_str().unwrap().to_string();
    let side = sidecar(&env.root(), &job);
    assert_eq!(side["status"], "completed");
    assert_eq!(side["runFingerprint"], p["runFingerprint"]);

    let page = content(&s.tool(2, "review_findings", json!({ "runFingerprint": p["runFingerprint"], "headSha": &HEAD[..7] }))).clone();
    assert_eq!(page["status"], "completed");
    assert_eq!(page["findings"][0]["severity"], "critical");
    let art = content(&s.tool(3, "review_artifact", json!({ "jobId": job }))).clone();
    assert_eq!(art["previewWouldBlock"], true);

    s.close_stdin();
    assert_eq!(s.exit_code(), 0, "блокер превью не превращается в exit 1");
    s.assert_no_secret();
}

#[test]
fn wait_false_поллинг_по_job_id_и_один_слот_lsp() {
    let gate = Gate::default();
    let env = env_with(gate.clone());
    let mut s = spawn(&env, Wire::Lsp, None);
    let stub = content(&s.tool(1, "review_run", json!({ "diffPath": "changes.patch", "headSha": HEAD, "wait": false }))).clone();
    assert_eq!(stub["status"], "running");
    assert_eq!(stub["runFingerprint"], "");
    let job = stub["jobId"].as_str().unwrap().to_string();

    let same = content(&s.tool(2, "review_run", json!({ "diffPath": "changes.patch", "headSha": HEAD, "wait": false }))).clone();
    assert_eq!(same["status"], "running");
    assert_eq!(same["jobId"], json!(job));
    let other = s.tool(3, "review_run", json!({ "diffPath": "other.patch", "headSha": HEAD }));
    assert_eq!(other["error"]["code"], -32602);
    assert_eq!(other["error"]["message"], "review already running");
    let page = content(&s.tool(4, "review_findings", json!({ "jobId": job }))).clone();
    assert_eq!(page["status"], "running");
    assert_eq!(page["total"], 0);

    gate.open();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut id = 10;
    let done = loop {
        id += 1;
        let page = content(&s.tool(id, "review_findings", json!({ "jobId": job }))).clone();
        if page["status"] != "running" {
            break page;
        }
        assert!(Instant::now() < deadline, "фоновый прогон не завершился");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["findings"][0]["file"], "src/db.ts");
    // Фоновые логи пришли Content-Length-кадрами (иначе читатель упал бы на разборе).
    assert!(s.notes.iter().any(|n| n["method"] == "notifications/message"));

    let again = content(&s.tool(100, "review_run", json!({ "diffPath": "changes.patch", "headSha": HEAD, "wait": false }))).clone();
    assert_eq!(again["status"], "completed");
    assert_eq!(again["jobId"], json!(job));
    s.close_stdin();
    assert_eq!(s.exit_code(), 0);
    assert_eq!(env.github.hits.load(Ordering::SeqCst), 0);
    s.assert_no_secret();
}

#[test]
fn eof_при_фоновом_прогоне_дожидается_sidecar() {
    let gate = Gate::default();
    let env = env_with(gate.clone());
    let mut s = spawn(&env, Wire::Json, None);
    let stub = content(&s.tool(1, "review_run", json!({ "diffPath": "changes.patch", "headSha": HEAD, "wait": false }))).clone();
    let job = stub["jobId"].as_str().unwrap().to_string();
    assert_eq!(sidecar(&env.root(), &job)["status"], "running");
    s.close_stdin();
    std::thread::sleep(Duration::from_millis(200));
    assert!(s.child.try_wait().unwrap().is_none(), "сервер ждёт фоновый прогон");
    gate.open();
    assert_eq!(s.exit_code(), 0);
    assert_eq!(sidecar(&env.root(), &job)["status"], "completed");
}

#[test]
fn путь_вне_корня_и_несуществующий_корень() {
    let gate = Gate::default();
    gate.open();
    let env = env_with(gate);
    let mut s = spawn(&env, Wire::Json, None);
    let res = s.tool(1, "review_route", json!({ "diffPath": "../../../../etc/hosts", "headSha": HEAD }));
    assert_eq!(res["error"]["code"], -32602);
    assert!(res["error"]["message"].as_str().unwrap().contains("escapes allowed root"), "{res}");
    let route = content(&s.tool(2, "review_route", json!({ "diffPath": "changes.patch", "headSha": HEAD }))).clone();
    assert_eq!(route["route"], "full");
    s.close_stdin();
    assert_eq!(s.exit_code(), 0);

    let missing = env.root().join("nope");
    let mut s = spawn(&env, Wire::Json, Some(missing.to_str().unwrap()));
    s.close_stdin();
    assert_eq!(s.exit_code(), 1);
    std::thread::sleep(Duration::from_millis(50));
    assert!(s.stderr().contains("REVIEW_MCP_ROOT не существует"), "{}", s.stderr());
}
