//! Чёрноящичные тесты `pr-review structure-mcp` (RFC 16, T23) — зеркало
//! test/structure.test.mjs (runMcpServer): Content-Length framing с частичными
//! чтениями, batch, невалидные запросы, oversized-кадр 8 МБ, голый JSON, сигналы,
//! tools/call против эталонного вывода JS (golden из bin/pr-review-structure-mcp.mjs
//! с AI_REVIEWER_STRUCTURE_MCP_FORCE_JS=1 на fixtures/structure/*.v1.json).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_pr-review")
}

/// Корень репозитория (rust/..): там лежат fixtures/structure/.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn frame(body: &str) -> Vec<u8> {
    // регистр заголовка произвольный, неизвестный заголовок игнорируется
    let mut out = format!("content-length: {}\r\nX-Unknown: ignored\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

fn spawn(root: &Path, envs: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(bin());
    cmd.arg("structure-mcp")
        .current_dir(root)
        .env_remove("STRUCTURE_MCP_ROOT")
        .env_remove("REVIEW_MCP_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.spawn().expect("запуск pr-review structure-mcp")
}

/// Ждём выхода с watchdog: зависший сервер не должен вешать прогон.
fn wait(mut child: Child) -> (i32, Vec<u8>, String) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut out = Vec::new();
            child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
            let mut err = String::new();
            child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
            return (status.code().expect("выход по коду, не по сигналу"), out, err);
        }
        if Instant::now() > deadline {
            child.kill().ok();
            panic!("structure-mcp не завершился за 20 с");
        }
        sleep(Duration::from_millis(20));
    }
}

/// Прогон: пишем куски, закрываем stdin, возвращаем stdout.
fn run(chunks: &[Vec<u8>], split: bool) -> Vec<u8> {
    let mut child = spawn(&repo_root(), &[]);
    {
        let mut stdin = child.stdin.take().unwrap();
        for chunk in chunks {
            if split {
                let mid = chunk.len() / 2;
                stdin.write_all(&chunk[..mid]).unwrap();
                stdin.flush().unwrap();
                sleep(Duration::from_millis(30));
                stdin.write_all(&chunk[mid..]).unwrap();
            } else {
                stdin.write_all(chunk).unwrap();
            }
        }
    }
    let (code, out, err) = wait(child);
    assert_eq!(code, 0, "stderr: {err}");
    out
}

fn lsp_out(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

const INVALID: &str = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}"#;

#[test]
fn framing_partial_reads_batches_and_invalid_requests() {
    let out = run(
        &[
            frame(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#),
            frame(r#"[{"jsonrpc":"2.0","id":2,"method":"ping"},42,{"method":"notifications/initialized"},{"jsonrpc":"2.0","id":4,"method":"prompts/list"}]"#),
            frame("[]"),
            frame("42"),
            frame("{not json"),
            frame(r#"{"id":5,"method":"bogus"}"#),
            frame(r#"{"id":6,"method":"tools/call","params":{"name":"q"}}"#),
            frame(r#"{"method":"notifications/initialized"}"#),
        ],
        true,
    );
    let want = [
        lsp_out(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"ai-reviewer-structure","version":"1.0.0"}}}"#),
        lsp_out(&format!(
            r#"[{{"jsonrpc":"2.0","id":2,"result":{{}}}},{INVALID},{{"jsonrpc":"2.0","id":4,"result":{{"prompts":[]}}}}]"#
        )),
        lsp_out(INVALID),
        lsp_out(INVALID),
        lsp_out(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#),
        lsp_out(r#"{"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"method not found: bogus"}}"#),
        lsp_out(r#"{"jsonrpc":"2.0","id":6,"error":{"code":-32602,"message":"unknown tool: q"}}"#),
    ]
    .concat();
    assert_eq!(String::from_utf8(out).unwrap(), want);
}

#[test]
fn oversized_frame_is_rejected_and_stream_stays_in_sync() {
    let size = 8 * 1024 * 1024 + 1;
    let mut big = format!("Content-Length: {size}\r\n\r\n").into_bytes();
    big.extend(vec![b'x'; size]);
    let out = run(&[big, frame(r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#)], false);
    let want = [
        lsp_out(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"message exceeds 8388608 bytes"}}"#),
        lsp_out(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#),
    ]
    .concat();
    assert_eq!(String::from_utf8(out).unwrap(), want);
}

#[test]
fn bare_json_answers_newline_delimited_until_next_lsp_frame() {
    let out = run(
        &[
            b"{\"id\":1,\"method\":\"ping\"}\n[{\"id\":2,\"method\":\"resources/list\"}]".to_vec(),
            frame(r#"{"id":3,"method":"ping"}"#),
        ],
        false,
    );
    let want = format!(
        "{}\n{}\n{}",
        r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        r#"[{"jsonrpc":"2.0","id":2,"result":{"resources":[]}}]"#,
        lsp_out(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#),
    );
    assert_eq!(String::from_utf8(out).unwrap(), want);
}

#[test]
fn eof_inside_frame_exits_cleanly_without_output() {
    let out = run(&[b"Content-Length: 100\r\n\r\n{\"id\":".to_vec()], false);
    assert!(out.is_empty());
}

fn tools_call(facts: &str, assessment: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"structure_context","arguments":{{"factsPath":"{facts}","assessmentPath":"{assessment}","snapshotFingerprint":"{}"}}}}}}"#,
        "0".repeat(64)
    )
}

#[test]
fn tools_call_matches_js_golden() {
    let out = run(
        &[frame(&tools_call(
            "fixtures/structure/structure-facts.v1.json",
            "fixtures/structure/structure-assessment.v1.json",
        ))],
        false,
    );
    let golden = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/structure_mcp_stdio/tools_call.golden"),
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), String::from_utf8(golden).unwrap());
}

#[test]
fn tools_call_reads_are_confined_to_root() {
    let fixtures = repo_root().join("fixtures/structure");
    let mut child = spawn(
        &repo_root(),
        &[("STRUCTURE_MCP_ROOT", fixtures.to_str().unwrap())],
    );
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(&frame(&tools_call("structure-facts.v1.json", "../structure/structure-assessment.v1.json")))
            .unwrap();
        stdin.write_all(&frame(&tools_call("/etc/passwd", "structure-assessment.v1.json"))).unwrap();
    }
    let (code, out, _) = wait(child);
    assert_eq!(code, 0);
    let text = String::from_utf8(out).unwrap();
    // первый — внутри корня (../structure возвращает в тот же каталог), второй — наружу
    assert!(text.contains(r#""kind":"structure_context""#), "{text}");
    assert!(text.contains(r#""message":"path escapes allowed root: /etc/passwd""#), "{text}");
}

#[test]
fn missing_root_is_fatal_with_exit_1() {
    let child = spawn(&repo_root(), &[("STRUCTURE_MCP_ROOT", "/nonexistent/t23-root")]);
    let (code, out, err) = wait(child);
    assert_eq!(code, 1);
    assert!(out.is_empty());
    assert!(err.contains("ai-reviewer MCP: STRUCTURE_MCP_ROOT не существует: /nonexistent/t23-root"), "{err}");
}

#[test]
fn placeholder_root_falls_back_to_review_mcp_root() {
    let child = spawn(
        &repo_root(),
        &[("STRUCTURE_MCP_ROOT", "${workspaceFolder}"), ("REVIEW_MCP_ROOT", "/nonexistent/t23-review")],
    );
    let (code, _, err) = wait(child);
    assert_eq!(code, 1);
    assert!(err.contains("/nonexistent/t23-review"), "{err}");
}

#[cfg(unix)]
#[test]
fn sigterm_and_sigint_exit_zero_after_answering() {
    for sig in ["-TERM", "-INT"] {
        let mut child = spawn(&repo_root(), &[]);
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&frame(r#"{"id":1,"method":"ping"}"#)).unwrap();
        stdin.flush().unwrap();
        // сервер ответил — значит, обработчики сигналов уже стоят
        let want = lsp_out(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        let mut got = vec![0u8; want.len()];
        child.stdout.as_mut().unwrap().read_exact(&mut got).unwrap();
        assert_eq!(String::from_utf8(got).unwrap(), want);
        let status = Command::new("kill").arg(sig).arg(child.id().to_string()).status().unwrap();
        assert!(status.success());
        // stdin остаётся открытым: выход — только по сигналу
        let (code, _, _) = wait(child);
        drop(stdin);
        assert_eq!(code, 0, "{sig}");
    }
}
