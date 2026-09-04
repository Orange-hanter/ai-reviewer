//! Тесты rust/src/llm.rs.
//!
//! Три слоя:
//! 1. паритет SSE-парсинга с `readLlmSseContent` (lib/llm.mjs) на записанных потоках
//!    (chunked-границы посреди `data:`, `[DONE]`, пустые строки, usage в финальном chunk);
//! 2. контрактные тесты транспорта `chat()` против фейкового HTTP-сервера на std::net
//!    (429/backoff, 400 stream_options, retry-бюджет, network-ретрай) — портированы
//!    из `test/pr-review.engine.test.mjs:188-252,283-297`;
//! 3. тесты клиента [`RealLlmClient`] (каскад `chatJson`, повтор `chatText`,
//!    троттлинг обоих семафоров на 429, лимиты параллелизма) — порт клиентской части
//!    того же JS-файла плюс покрытие веток каскада, которых в JS-тестах нет.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

use super::{
    backoff_ms, chat, is_transient, normalize_usage, parse_sse, ChatJsonOpts, ChatRequest,
    ChatTextOpts, LlmApi, LlmConfig, LlmError, RealLlmClient, SseReadError,
};
use crate::json_repair::RepairedKind;

fn ok_budget() -> bool {
    true
}

/// Reader, отдающий не больше `chunk` байт за вызов read — имитация сетевых chunked-границ.
struct Chunky<'a> {
    data: &'a [u8],
    chunk: usize,
}

impl<'a> Read for Chunky<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.data.is_empty() {
            return Ok(0);
        }
        let n = self.chunk.min(self.data.len()).min(buf.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

fn data_line(v: Value) -> String {
    format!("data: {}\n\n", serde_json::to_string(&v).unwrap())
}

fn ok_chunk(content: &str) -> Value {
    json!({"choices": [{"delta": {"content": content}}]})
}

fn raw(content: &str) -> (String, Option<Value>) {
    let r = parse_sse(Chunky { data: content.as_bytes(), chunk: 7 }, &ok_budget).unwrap();
    (r.content, r.usage)
}

// --- SSE-парсинг (паритет с readLlmSseContent) ---

#[test]
fn sse_собирает_content_из_нескольких_chunk() {
    let stream = format!("{}{}data: [DONE]\n\n", data_line(ok_chunk("привет ")), data_line(ok_chunk("мир")));
    assert_eq!(raw(&stream), ("привет мир".to_string(), None));
}

#[test]
fn sse_chunked_границы_посреди_data_и_внутри_мультибайта() {
    // Границы посреди "data: ", посреди '[DONE]', посреди UTF-8 последовательности (кавычки JSON,
    // 2-байтные UTF-8 в "привет"): парсер обязан пережить любые точки разрезания.
    let mut stream = data_line(ok_chunk("привет"));
    stream.push_str("data: [DONE]\n\n");
    for chunk in 1..=13 {
        let (content, _) = raw(&stream);
        assert_eq!(content, "привет", "chunk={chunk}");
    }
}

#[test]
fn sse_игнорирует_пустые_строки_комментарии_и_битый_json() {
    let stream = format!(
        ": keep-alive\n\ndata:{}\n\ndata: {{not json\n\ndata: [DONE]\n\n",
        serde_json::to_string(&ok_chunk("ok")).unwrap(),
    );
    assert_eq!(raw(&stream).0, "ok");
}

#[test]
fn sse_usage_из_финального_chunk_нормализуется() {
    let mut stream = data_line(ok_chunk("x"));
    stream.push_str(&data_line(json!({
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
    })));
    stream.push_str("data: [DONE]\n\n");
    let (content, usage) = raw(&stream);
    assert_eq!(content, "x");
    assert_eq!(usage, Some(json!({"prompt_tokens": 10.0, "completion_tokens": 5.0, "total_tokens": 15.0})));
}

#[test]
fn sse_usage_total_fallback_и_отбраковка() {
    assert_eq!(
        normalize_usage(&json!({"prompt_tokens": 10, "completion_tokens": 5})),
        Some(json!({"prompt_tokens": 10.0, "completion_tokens": 5.0, "total_tokens": 15.0})),
    );
    // Все поля нечисловые → null (как у JS).
    assert_eq!(normalize_usage(&json!({"prompt_tokens": "abc"})), None);
    // JS Number(null) = 0 — единственное конечное поле проходит.
    assert_eq!(
        normalize_usage(&json!({"prompt_tokens": null})),
        Some(json!({"prompt_tokens": 0.0, "completion_tokens": 0.0, "total_tokens": 0.0})),
    );
}

#[test]
fn sse_поток_без_done_дочитывается_до_eof() {
    let stream = data_line(ok_chunk("хвост без DONE"));
    assert_eq!(raw(&stream).0, "хвост без DONE");
}

#[test]
fn sse_пустой_content_пустая_ошибка_с_finish_и_reasoning() {
    let mut stream = data_line(json!({"choices": [{"delta": {"reasoning": "думаю\u{1F600}"}, "finish_reason": "stop"}]}));
    stream.push_str("data: [DONE]\n\n");
    let err = parse_sse(Chunky { data: stream.as_bytes(), chunk: 64 }, &ok_budget).unwrap_err();
    match err {
        // reasoning "думаю😀" = 5 BMP-символов + 1 не-BMP (2 UTF-16 юнита) = 7 — как JS .length.
        SseReadError::Empty { message } => assert_eq!(message, "пустой content (finish=stop, reasoning=7симв)"),
        other => panic!("ожидалась Empty, получено: {other:?}"),
    }
}

#[test]
fn sse_истощение_бюджета_это_networky_ошибка() {
    let stream = data_line(ok_chunk("ok"));
    let err = parse_sse(Chunky { data: stream.as_bytes(), chunk: 64 }, &|| false).unwrap_err();
    match err {
        SseReadError::Io(_) => {}
        other => panic!("ожидалась Io, получено: {other:?}"),
    }
}

// --- backoff / isTransient (паритет с lib/util.mjs) ---

#[test]
fn backoff_паритет_с_util_mjs() {
    assert_eq!(backoff_ms(0, None), 1000.0);
    assert_eq!(backoff_ms(1, None), 2000.0);
    assert_eq!(backoff_ms(4, None), 15_000.0, "cap 15s");
    assert_eq!(backoff_ms(0, Some("0")), 1000.0, "retry-after=0 → экспонента");
    assert_eq!(backoff_ms(0, Some("2")), 2000.0);
    assert_eq!(backoff_ms(0, Some("120")), 30_000.0, "cap 30s");
    assert_eq!(backoff_ms(3, Some("bad")), 8000.0, "нечисловой retry-after → экспонента");
    assert_eq!(backoff_ms(0, Some("")), 1000.0, "пустой retry-after → Number('')=0 → экспонента");
}

#[test]
fn is_transient_паритет() {
    assert!(is_transient(429));
    assert!(is_transient(500));
    assert!(is_transient(599));
    assert!(!is_transient(400));
    assert!(!is_transient(200));
}

// --- контрактные тесты против фейкового HTTP-сервера (std::net) ---

struct FakeServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: JoinHandle<()>,
}

/// Читает HTTP-запрос до конца заголовков; `None` — клиент закрыл сокет молча.
///
/// Ошибки чтения глотаются намеренно: в тесте на исчерпанный бюджет клиент рвёт
/// соединение по своему таймауту, и `unwrap()` здесь превращал бы ECONNRESET
/// в панику серверного потока (см. комментарий у [`write_response`]).
fn read_request(conn: &mut TcpStream) -> Option<String> {
    let mut buf = [0u8; 8192];
    let mut seen = Vec::new();
    loop {
        let n = conn.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        seen.extend_from_slice(&buf[..n]);
        if seen.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Some(String::from_utf8_lossy(&seen).to_string())
}

/// Пишет ответ chunked-кусками по 5 байт (гоняет SSE-парсер по рваным границам).
///
/// Каждая запись через `let _ =`: клиент вправе уйти раньше — по бюджету таймаута
/// (`chat_исчерпанный_бюджет_подавляет_ретрай`) или потому, что уже получил
/// `[DONE]` и не дочитывает хвост. Тогда сокет отвечает EPIPE/ECONNRESET, и
/// `unwrap()` ронял серверный поток; падало это не здесь, а в `handle.join()`
/// у теста — то есть флаки́ло под параллельной нагрузкой (BrokenPipe).
fn write_response(conn: &mut TcpStream, status: u16, headers: &[(String, String)], body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Transfer-Encoding: chunked\r\n\r\n");
    if conn.write_all(head.as_bytes()).is_err() {
        return;
    }
    for piece in body.chunks(5) {
        if conn.write_all(format!("{:x}\r\n", piece.len()).as_bytes()).is_err()
            || conn.write_all(piece).is_err()
            || conn.write_all(b"\r\n").is_err()
        {
            return;
        }
    }
    let _ = conn.write_all(b"0\r\n\r\n");
}

/// Одно подключение → один «ответ» из очереди; `None` — сразу разорвать соединение
/// (эмуляция сетевого сбоя между accept и ответом).
fn fake_server(responses: Vec<Option<(u16, Vec<(String, String)>, Vec<u8>)>>, delay: Duration) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let req_log = Arc::clone(&requests);
    let handle = std::thread::spawn(move || {
        for response in responses {
            let Ok((mut conn, _)) = listener.accept() else { break };
            let Some(seen) = read_request(&mut conn) else { continue };
            req_log.lock().unwrap().push(seen);
            let Some((status, headers, body)) = response else {
                drop(conn); // RST: клиент увидит обрыв до ответа
                continue;
            };
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            write_response(&mut conn, status, &headers, &body);
        }
    });
    FakeServer { url: format!("http://{addr}/chat/completions"), requests, handle }
}

fn base_req(url: &str) -> ChatRequest {
    ChatRequest {
        url: url.to_string(),
        key: "k".to_string(),
        model: "m1".to_string(),
        messages: json!([{"role": "user", "content": "x"}]),
        max_tokens: Some(json!(100)),
        temperature: Some(0.4),
        effort: Some("low".to_string()),
        json_mode: false,
        seed: Some(json!(1)),
        http_retries: 3,
        timeout_ms: 240_000,
        include_usage: true,
    }
}

fn sse_body(text: &str) -> Vec<u8> {
    let mut s = data_line(ok_chunk(text));
    s.push_str("data: [DONE]\n\n");
    s.into_bytes()
}

fn payload_of(request: &str) -> Value {
    let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(body).unwrap()
}

#[test]
fn chat_успешный_sse_с_usage() {
    let server = fake_server(vec![Some((200, vec![(r#"Content-Type"#.to_string(), r#"text/event-stream"#.to_string())], sse_body("привет мир")))], Duration::ZERO);
    let out = chat(&base_req(&server.url)).unwrap();
    assert_eq!(out.content, "привет мир");
    assert!(!out.throttled);
    let payload = payload_of(&server.requests.lock().unwrap()[0]);
    assert_eq!(payload["stream_options"], json!({"include_usage": true}));
    assert_eq!(payload["reasoning_effort"], json!("low"));
    assert_eq!(payload["seed"], json!(1));
    server.handle.join().unwrap();
}

#[test]
fn chat_429_throttled_и_ретрай_с_retry_after() {
    // retry-after: 0 → backoff(0, "0") = 1000мс (релевантный путь, а не ожидание).
    let server = fake_server(
        vec![
            Some((429, vec![(r#"retry-after"#.to_string(), r#"0"#.to_string())], b"rate limited".to_vec())),
            Some((200, vec![], sse_body("ok"))),
        ],
        Duration::ZERO,
    );
    let t = std::time::Instant::now();
    let out = chat(&base_req(&server.url)).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(900), "backoff 1000мс перед ретраем");
    assert_eq!(out.content, "ok");
    assert!(out.throttled, "429 всплыл флагом throttled для JS-семафоров");
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    server.handle.join().unwrap();
}

#[test]
fn chat_400_stream_options_повтор_без_include_usage() {
    let server = fake_server(
        vec![
            Some((400, vec![], b"unknown field stream_options".to_vec())),
            Some((200, vec![], sse_body("ok"))),
        ],
        Duration::ZERO,
    );
    let out = chat(&base_req(&server.url)).unwrap();
    assert_eq!(out.content, "ok");
    let reqs = server.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(payload_of(&reqs[0])["stream_options"].is_object(), "первый запрос — с include_usage");
    assert!(payload_of(&reqs[1]).get("stream_options").is_none(), "повтор — без stream_options");
    server.handle.join().unwrap();
}

#[test]
fn chat_400_без_stream_options_в_теле_не_ретраится() {
    let server = fake_server(vec![Some((400, vec![], b"bad request: max_tokens".to_vec()))], Duration::ZERO);
    let err = chat(&base_req(&server.url)).unwrap_err();
    match err {
        LlmError { status: Some(400), networky: false, .. } => {}
        other => panic!("ожидался LLM 400 без ретрая: {other:?}"),
    }
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    server.handle.join().unwrap();
}

#[test]
fn chat_500_transient_ретрай_затем_успех() {
    let server = fake_server(
        vec![Some((500, vec![], b"err".to_vec())), Some((200, vec![], sse_body("ok")))],
        Duration::ZERO,
    );
    let out = chat(&base_req(&server.url)).unwrap();
    assert_eq!(out.content, "ok");
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    server.handle.join().unwrap();
}

#[test]
fn chat_исчерпанный_бюджет_подавляет_ретрай() {
    // Сервер отвечает 500 через 300мс; бюджет 100мс → per-op таймаут сработает раньше
    // ответа (networky), бюджет израсходован → ретрая нет. Аналог JS-теста на budgetLeft().
    let server = fake_server(vec![Some((500, vec![], b"err".to_vec()))], Duration::from_millis(300));
    let mut req = base_req(&server.url);
    req.timeout_ms = 100;
    let err = chat(&req).unwrap_err();
    assert!(err.networky, "per-op таймаут — сетевой класс ошибки");
    assert_eq!(err.status, None);
    assert_eq!(server.requests.lock().unwrap().len(), 1, "ретрай подавлен бюджетом");
    server.handle.join().unwrap();
}

#[test]
fn chat_network_error_ретрай_затем_успех() {
    // Первое подключение сервер разрывает сразу после accept (обрыв до ответа) —
    // клиент видит сетевой сбой, ретраит с backoff и доходит до живого ответа.
    let server = fake_server(
        vec![None, Some((200, vec![], sse_body("ok")))],
        Duration::ZERO,
    );
    let t = std::time::Instant::now();
    let out = chat(&base_req(&server.url)).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(900), "backoff 1000мс перед ретраем");
    assert_eq!(out.content, "ok");
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    server.handle.join().unwrap();
}

#[test]
fn chat_connection_refused_это_networky_ошибка() {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}/chat/completions", probe.local_addr().unwrap());
    drop(probe); // порт закрыт — connection refused
    let err = chat(&base_req(&dead_url)).unwrap_err();
    assert!(err.networky);
    assert_eq!(err.status, None);
}

#[test]
fn chat_пустой_content_пустая_ошибка() {
    let body = data_line(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})) + "data: [DONE]\n\n";
    let server = fake_server(vec![Some((200, vec![], body.into_bytes()))], Duration::ZERO);
    let err = chat(&base_req(&server.url)).unwrap_err();
    assert!(err.empty, "флаг empty для JS-fallback");
    assert!(!err.networky);
    assert_eq!(err.error, "пустой content (finish=stop, reasoning=0симв)");
    server.handle.join().unwrap();
}

// ===========================================================================
// Клиент RealLlmClient — порт клиентской части test/pr-review.engine.test.mjs
// ===========================================================================

/// `base` для клиента: он сам дописывает `/chat/completions` (lib/llm.mjs:148).
fn base_of(url: &str) -> String {
    url.trim_end_matches("/chat/completions").to_string()
}

/// Аналог `OLLAMA_CFG` из test/pr-review.engine.test.mjs:169-176.
///
/// Отличие одно: `http_retries = 0`. Ретраи транспорта уже покрыты тестами `chat()`
/// выше, а здесь важно точное число запросов на каждую ветку каскада — лишний
/// transient-повтор сделал бы счётчик неотличимым от повтора каскада.
fn client_cfg(url: &str) -> LlmConfig {
    LlmConfig {
        base: base_of(url),
        api_key: "k".to_string(),
        seed: Some(json!(1)),
        json_mode: false,
        temperature: Some(0.4),
        request_timeout_ms: 5_000,
        effort: "low".to_string(),
        provider_concurrency: 12,
        provider_per_model: 6,
        http_retries: 0,
        ref_max_tokens: Some(json!(100)),
        ref_temp: Some(0.6),
    }
}

fn client(cfg: LlmConfig) -> RealLlmClient {
    RealLlmClient::new(cfg).unwrap()
}

/// Клиент с журналом строк лога — как `(...a) => logs.push(a.join(' '))` в JS-тесте.
fn client_with_logs(cfg: LlmConfig) -> (RealLlmClient, Arc<Mutex<Vec<String>>>) {
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&logs);
    let c = client(cfg).with_log(Box::new(move |m| sink.lock().unwrap().push(m.to_string())));
    (c, logs)
}

fn msgs() -> Value {
    json!([{"role": "user", "content": "x"}])
}

fn ok(status: u16, body: Vec<u8>) -> Option<(u16, Vec<(String, String)>, Vec<u8>)> {
    Some((status, vec![], body))
}

/// SSE без единого delta.content — путь `empty` (lib/llm.mjs:343-345).
fn empty_body() -> Vec<u8> {
    let mut s = data_line(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
    s.push_str("data: [DONE]\n\n");
    s.into_bytes()
}

fn sse_body_with_usage(text: &str, usage: Value) -> Vec<u8> {
    let mut s = data_line(ok_chunk(text));
    s.push_str(&data_line(json!({
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "usage": usage,
    })));
    s.push_str("data: [DONE]\n\n");
    s.into_bytes()
}

fn payloads(server: &FakeServer) -> Vec<Value> {
    server.requests.lock().unwrap().iter().map(|r| payload_of(r)).collect()
}

// --- chatText (lib/llm.mjs:225-241) ---

#[test]
fn client_chat_text_собирает_sse_и_шлёт_дефолты_moa() {
    // Порт «ollama: SSE-стрим собирается в content» (test/pr-review.engine.test.mjs:187).
    let server = fake_server(vec![ok(200, sse_body("привет мир"))], Duration::ZERO);
    let c = client(client_cfg(&server.url));
    let text = c.chat_text(&msgs(), "m1", &ChatTextOpts::default()).unwrap();
    assert_eq!(text, "привет мир");

    let p = payloads(&server).remove(0);
    assert_eq!(p["max_tokens"], json!(100), "дефолт moa.refMaxTokens");
    assert_eq!(p["temperature"], json!(0.6), "дефолт moa.refTemp");
    assert_eq!(p["reasoning_effort"], json!("low"), "o.effort || 'low'");
    assert!(p.get("response_format").is_none(), "chatText всегда без JSON-mode");
    assert!(p["stream_options"].is_object());
    server.handle.join().unwrap();
}

#[test]
fn client_chat_text_пустой_content_повтор_на_effort_none() {
    // lib/llm.mjs:231-238 — единственная ветка повтора chatText.
    let server = fake_server(
        vec![ok(200, empty_body()), ok(200, sse_body("ok"))],
        Duration::ZERO,
    );
    let (c, logs) = client_with_logs(client_cfg(&server.url));
    let text = c.chat_text(&msgs(), "m1", &ChatTextOpts::default()).unwrap();
    assert_eq!(text, "ok");

    let p = payloads(&server);
    assert_eq!(p.len(), 2);
    assert_eq!(p[0]["reasoning_effort"], json!("low"));
    assert_eq!(p[1]["reasoning_effort"], json!("none"));
    assert_eq!(
        p[1]["max_tokens"],
        json!(100),
        "токены при повторе НЕ растут (в отличие от chatJson)"
    );
    assert!(logs.lock().unwrap().iter().any(|l| l.contains("advisor content пуст")));
    server.handle.join().unwrap();
}

#[test]
fn client_chat_text_не_ретраит_прочие_ошибки() {
    // Порт «не-transient 400 без stream_options в теле — сразу стоп»
    // (test/pr-review.engine.test.mjs:195): у chatText ветка только на empty.
    let server = fake_server(vec![ok(400, b"bad request: max_tokens".to_vec())], Duration::ZERO);
    let c = client(client_cfg(&server.url));
    let err = c.chat_text(&msgs(), "m1", &ChatTextOpts::default()).unwrap_err();
    assert_eq!(err.status, Some(400));
    assert!(err.message.starts_with("LLM 400:"), "{}", err.message);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    server.handle.join().unwrap();
}

// --- chatJson: happy path и __repaired (lib/llm.mjs:178-198) ---

#[test]
fn client_chat_json_разбирает_ответ_и_отдаёт_usage() {
    let body = sse_body_with_usage(
        r#"{"summary":"s","verdict":"approve","findings":[]}"#,
        json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}),
    );
    let server = fake_server(vec![ok(200, body)], Duration::ZERO);

    let seen = Arc::new(Mutex::new(Vec::<(String, String, Value)>::new()));
    let sink = Arc::clone(&seen);
    let c = client(client_cfg(&server.url)).with_usage_sink(Box::new(move |model, call, usage| {
        sink.lock().unwrap().push((model.to_string(), call.to_string(), usage.clone()));
    }));

    let res = c.chat_json(&msgs(), "m1", &ChatJsonOpts::default()).unwrap();
    assert_eq!(res.value["verdict"], json!("approve"));
    assert_eq!(res.repaired, None);
    assert_eq!(
        res.usage,
        Some(json!({"prompt_tokens": 10.0, "completion_tokens": 5.0, "total_tokens": 15.0})),
    );

    // costTracker.record(usage, model, callType) — lib/cost.mjs:46.
    let calls = seen.lock().unwrap();
    assert_eq!(calls.len(), 1, "usage учтён ровно один раз");
    assert_eq!(calls[0].0, "m1");
    assert_eq!(calls[0].1, "review_json", "дефолтный callType chatJson");

    let p = payloads(&server).remove(0);
    assert_eq!(p["max_tokens"], json!(24000), "первый запрос каскада");
    assert_eq!(p["reasoning_effort"], json!("low"));
    assert_eq!(p["temperature"], json!(0.4), "дефолт o.temperature");
    server.handle.join().unwrap();
}

#[test]
fn client_chat_json_сообщает_repaired_escapes() {
    // lib/llm.mjs:180-193: onRepaired → __repaired + строка лога.
    let server = fake_server(
        vec![ok(200, sse_body(r#"{"summary":"bad \q escape"}"#))],
        Duration::ZERO,
    );
    let (c, logs) = client_with_logs(client_cfg(&server.url));
    let res = c.chat_json(&msgs(), "m1", &ChatJsonOpts::default()).unwrap();
    assert_eq!(res.repaired, Some(RepairedKind::Escapes));
    assert_eq!(res.value["summary"], json!("bad \\q escape"));
    assert!(logs
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.contains("исправлены невалидные escape-последовательности")));
    assert_eq!(
        server.requests.lock().unwrap().len(),
        1,
        "починенный ответ повтора не требует"
    );
    server.handle.join().unwrap();
}

// --- chatJson: каскад ретраев (lib/llm.mjs:204-221), по ветке на тест ---

#[test]
fn client_chat_json_ветка1_пустой_content_повтор_effort_none() {
    // lib/llm.mjs:205-208: max_tokens не растёт, JSON-mode сохраняется.
    let server = fake_server(
        vec![ok(200, empty_body()), ok(200, sse_body(r#"{"summary":"s"}"#))],
        Duration::ZERO,
    );
    let mut cfg = client_cfg(&server.url);
    cfg.json_mode = true;
    let (c, logs) = client_with_logs(cfg);
    let res = c.chat_json(&msgs(), "m1", &ChatJsonOpts::default()).unwrap();
    assert_eq!(res.value["summary"], json!("s"));

    let p = payloads(&server);
    assert_eq!(p.len(), 2);
    assert_eq!(p[1]["reasoning_effort"], json!("none"));
    assert_eq!(p[1]["max_tokens"], json!(24000), "ветка 1 токены не поднимает");
    assert!(p[1]["response_format"].is_object(), "ветка 1 JSON-mode не снимает");
    assert!(logs.lock().unwrap().iter().any(|l| l.contains("content пуст на effort=low")));
    server.handle.join().unwrap();
}

#[test]
fn client_chat_json_ветка2_оборванный_json_повтор_с_32000() {
    // lib/llm.mjs:209-212. `{"findings":[{"line":` не чинится балансировкой →
    // truncated=true (json_repair::tests::hopeless_throws_with_flags).
    let server = fake_server(
        vec![
            ok(200, sse_body(r#"{"findings":[{"line":"#)),
            ok(200, sse_body(r#"{"findings":[]}"#)),
        ],
        Duration::ZERO,
    );
    let mut cfg = client_cfg(&server.url);
    cfg.json_mode = true;
    let (c, logs) = client_with_logs(cfg);
    let res = c.chat_json(&msgs(), "m1", &ChatJsonOpts::default()).unwrap();
    assert_eq!(res.value["findings"], json!([]));

    let p = payloads(&server);
    assert_eq!(p.len(), 2);
    assert_eq!(p[1]["max_tokens"], json!(32000), "ветка 2 просит больше токенов");
    assert_eq!(p[1]["reasoning_effort"], json!("none"));
    assert!(p[1]["response_format"].is_object(), "ветка 2 JSON-mode не снимает");
    assert!(logs.lock().unwrap().iter().any(|l| l.contains("JSON оборван")));
    server.handle.join().unwrap();
}

#[test]
fn client_chat_json_ветка3_битые_escape_повтор_без_json_mode() {
    // lib/llm.mjs:213-216. Сбалансированный, но непарсимый объект: ремонт escape
    // не помогает → bad_escape=true при truncated=false (json_repair.rs:416-422).
    let server = fake_server(
        vec![
            ok(200, sse_body(r#"{"a": undefined}"#)),
            ok(200, sse_body(r#"{"a":1}"#)),
        ],
        Duration::ZERO,
    );
    let mut cfg = client_cfg(&server.url);
    cfg.json_mode = true;
    let (c, logs) = client_with_logs(cfg);
    let res = c.chat_json(&msgs(), "m1", &ChatJsonOpts::default()).unwrap();
    assert_eq!(res.value["a"], json!(1));

    let p = payloads(&server);
    assert_eq!(p.len(), 2);
    assert_eq!(p[1]["max_tokens"], json!(32000));
    assert_eq!(p[1]["reasoning_effort"], json!("none"));
    assert!(p[1].get("response_format").is_none(), "ветка 3 снимает JSON-mode");
    assert!(logs.lock().unwrap().iter().any(|l| l.contains("битые escape")));
    server.handle.join().unwrap();
}

#[test]
fn client_chat_json_ветка4_400_повтор_без_json_mode() {
    // lib/llm.mjs:217-220: 400 при включённом JSON-mode → повтор без response_format;
    // effort остаётся low и токены остаются 24000 — это НЕ ветка усечения.
    let server = fake_server(
        vec![
            ok(400, b"unsupported parameter".to_vec()),
            ok(200, sse_body(r#"{"a":1}"#)),
        ],
        Duration::ZERO,
    );
    let mut cfg = client_cfg(&server.url);
    cfg.json_mode = true;
    let (c, logs) = client_with_logs(cfg);
    let res = c.chat_json(&msgs(), "m1", &ChatJsonOpts::default()).unwrap();
    assert_eq!(res.value["a"], json!(1));

    let p = payloads(&server);
    assert_eq!(p.len(), 2);
    assert!(p[0]["response_format"].is_object());
    assert!(p[1].get("response_format").is_none());
    assert_eq!(p[1]["reasoning_effort"], json!("low"));
    assert_eq!(p[1]["max_tokens"], json!(24000));
    assert!(logs.lock().unwrap().iter().any(|l| l.contains("возможно из-за JSON-mode")));
    server.handle.join().unwrap();
}

#[test]
fn client_chat_json_400_без_json_mode_не_ретраится() {
    // Условие `jm &&` в lib/llm.mjs:217: без JSON-mode повторять нечего.
    let server = fake_server(vec![ok(400, b"bad request".to_vec())], Duration::ZERO);
    let c = client(client_cfg(&server.url)); // json_mode: false
    let err = c.chat_json(&msgs(), "m1", &ChatJsonOpts::default()).unwrap_err();
    assert_eq!(err.status, Some(400));
    assert_eq!(server.requests.lock().unwrap().len(), 1, "лишнего запроса нет");
    server.handle.join().unwrap();
}

#[test]
fn client_chat_json_повтор_ровно_один() {
    // В JS повтор идёт ВНЕ try (lib/llm.mjs:207): вложенного каскада нет,
    // вторая неудача уходит наверх как есть.
    let server = fake_server(
        vec![ok(200, empty_body()), ok(200, empty_body())],
        Duration::ZERO,
    );
    let c = client(client_cfg(&server.url));
    let err = c.chat_json(&msgs(), "m1", &ChatJsonOpts::default()).unwrap_err();
    assert!(err.empty);
    assert_eq!(server.requests.lock().unwrap().len(), 2, "ровно один повтор");
    server.handle.join().unwrap();
}

#[test]
fn client_chat_json_сеть_упала_каскад_не_срабатывает() {
    // networky-ошибка не совпадает ни с одной веткой каскада (status=None,
    // empty/truncated/bad_escape = false) — уходит наверх сразу.
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}/chat/completions", probe.local_addr().unwrap());
    drop(probe);
    let mut cfg = client_cfg(&dead);
    cfg.json_mode = true;
    let err = client(cfg).chat_json(&msgs(), "m1", &ChatJsonOpts::default()).unwrap_err();
    assert!(err.networky);
    assert_eq!(err.status, None);
}

// --- семафоры: троттлинг на 429 и лимиты параллелизма ---

#[test]
fn client_429_троттлит_оба_семафора_и_логирует() {
    // Порт «ollama: 429 вызывает throttle» (test/pr-review.engine.test.mjs:283).
    // Отличие от JS-теста: http_retries=0, поэтому 429 сразу становится ошибкой —
    // проверяется именно throttleFor (lib/llm.mjs:122-128), а не ретрай транспорта
    // (он покрыт chat_429_throttled_и_ретрай_с_retry_after выше).
    let server = fake_server(
        vec![Some((
            429,
            vec![("retry-after".to_string(), "0".to_string())],
            b"rate limited".to_vec(),
        ))],
        Duration::ZERO,
    );
    let (c, logs) = client_with_logs(client_cfg(&server.url));
    assert_eq!(c.global_stats().limit, 12);
    assert_eq!(c.model_stats("m1"), None, "карта семафоров наполняется лениво");

    let err = c.chat_text(&msgs(), "m1", &ChatTextOpts::default()).unwrap_err();
    assert_eq!(err.status, Some(429));

    // floor(12*0.7)=8 и floor(6*0.7)=4 — режутся ОБА (lib/llm.mjs:124-125).
    assert_eq!(c.global_stats().limit, 8, "глобальный семафор");
    assert_eq!(c.model_stats("m1").unwrap().limit, 4, "семафор модели");
    assert_eq!(c.model_stats("m2"), None, "чужая модель не тронута");

    let logs = logs.lock().unwrap();
    let line = logs
        .iter()
        .find(|l| l.contains("снижение параллелизма"))
        .expect("throttle залогирован");
    assert!(line.contains("[m1] 429"), "{line}");
    assert!(line.contains(r#"model: {"limit":4,"#), "{line}");
    server.handle.join().unwrap();
}

/// Сервер, обслуживающий подключения параллельно и меряющий пик одновременных
/// запросов «в полёте». Нужен там, где проверяется cap семафора: последовательный
/// [`fake_server`] дал бы пик 1 при любых лимитах.
struct BusyServer {
    url: String,
    peak: Arc<AtomicUsize>,
    handle: JoinHandle<()>,
}

fn busy_server(conns: usize, hold: Duration) -> BusyServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let now = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (now_srv, peak_srv) = (Arc::clone(&now), Arc::clone(&peak));
    let handle = std::thread::spawn(move || {
        let mut workers = Vec::new();
        for _ in 0..conns {
            let Ok((mut conn, _)) = listener.accept() else { break };
            let (now, peak) = (Arc::clone(&now_srv), Arc::clone(&peak_srv));
            workers.push(std::thread::spawn(move || {
                if read_request(&mut conn).is_none() {
                    return;
                }
                let cur = now.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(hold);
                now.fetch_sub(1, Ordering::SeqCst);
                write_response(&mut conn, 200, &[], &sse_body("ok"));
            }));
        }
        for w in workers {
            let _ = w.join();
        }
    });
    BusyServer { url: format!("http://{addr}/chat/completions"), peak, handle }
}

#[test]
fn client_держит_лимит_по_модели() {
    // Порт «semaphore: cap параллелизма соблюдается» (test/pr-review.engine.test.mjs:255),
    // но на уровне клиента: глобальный лимит заведомо шире, режет per-model.
    let server = busy_server(6, Duration::from_millis(60));
    let mut cfg = client_cfg(&server.url);
    cfg.provider_concurrency = 10;
    cfg.provider_per_model = 2;
    let c = client(cfg);
    std::thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| {
                assert_eq!(c.chat_text(&msgs(), "m1", &ChatTextOpts::default()).unwrap(), "ok");
            });
        }
    });
    server.handle.join().unwrap();
    let peak = server.peak.load(Ordering::SeqCst);
    assert!(peak <= 2, "per-model cap не соблюдён: пик {peak}");
    assert!(peak >= 2, "параллелизм вообще не состоялся: пик {peak}");
}

#[test]
fn client_держит_глобальный_лимит_поверх_разных_моделей() {
    // Глобальный семафор — единственное, что связывает разные модели
    // (lib/llm.mjs:110): по 2 запроса на 3 модели при per-model=6 и global=2.
    let server = busy_server(6, Duration::from_millis(60));
    let mut cfg = client_cfg(&server.url);
    cfg.provider_concurrency = 2;
    cfg.provider_per_model = 6;
    let c = client(cfg);
    let c = &c;
    std::thread::scope(|scope| {
        for model in ["m1", "m1", "m2", "m2", "m3", "m3"] {
            scope.spawn(move || {
                assert_eq!(c.chat_text(&msgs(), model, &ChatTextOpts::default()).unwrap(), "ok");
            });
        }
    });
    server.handle.join().unwrap();
    let peak = server.peak.load(Ordering::SeqCst);
    assert!(peak <= 2, "глобальный cap не соблюдён: пик {peak}");
    assert!(peak >= 2, "параллелизм вообще не состоялся: пик {peak}");
}

// --- LlmConfig::from_cfg ---

#[test]
fn llm_config_читает_замороженный_конфиг() {
    let cfg = crate::config::load_review_config_parts(&serde_json::Map::new(), &json!({}));
    let llm = LlmConfig::from_cfg(&cfg);
    assert_eq!(llm.base, "https://ollama.com/v1");
    assert_eq!(llm.provider_concurrency, 12, "REVIEW_PROVIDER_CONCURRENCY по умолчанию");
    assert_eq!(llm.provider_per_model, 6, "REVIEW_PROVIDER_PER_MODEL по умолчанию");
    assert_eq!(llm.request_timeout_ms, 240_000);
    assert_eq!(llm.http_retries, 3);
    assert_eq!(llm.effort, "low");
    assert!(llm.json_mode, "REVIEW_JSON_MODE по умолчанию включён");
    assert_eq!(llm.temperature, Some(0.4));
    assert_eq!(llm.ref_temp, Some(0.3));
    assert_eq!(llm.ref_max_tokens.as_ref().and_then(Value::as_f64), Some(12_000.0));
}

#[test]
fn llm_config_falsy_поля_дают_дефолты_как_в_js() {
    // JS: `o.providerConcurrency || 12` — 0 и null падают на дефолт (lib/llm.mjs:110,113).
    let llm = LlmConfig::from_cfg(&json!({
        "ollama": {
            "providerConcurrency": 0,
            "providerPerModel": Value::Null,
            "requestTimeoutMs": 0,
            "effort": "",
            "seed": Value::Null,
        },
    }));
    assert_eq!(llm.provider_concurrency, 12);
    assert_eq!(llm.provider_per_model, 6);
    assert_eq!(llm.request_timeout_ms, 240_000);
    assert_eq!(llm.effort, "low", "o.effort || 'low'");
    assert_eq!(llm.seed, None, "null seed в payload не попадает");
}
