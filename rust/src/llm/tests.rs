//! Тесты rust/src/llm.rs: паритет SSE-парсинга с readLlmSseContent (lib/llm.mjs)
//! на записанных потоках (chunked-границы посреди `data:`, `[DONE]`, пустые строки,
//! usage в финальном chunk) и контрактные тесты chat() против фейкового HTTP-сервера
//! на std::net (429/backoff, 400 stream_options, retry-бюджет, network-ретрай).

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

use super::{
    backoff_ms, chat, is_transient, normalize_usage, parse_sse, ChatRequest, LlmError, SseReadError,
};

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
    handle: Option<JoinHandle<()>>,
    addr: SocketAddr,
    /// Флаг остановки: взводится в stop()/drop() перед пробуждающим самоподключением.
    stopping: Arc<AtomicBool>,
    /// Сколько ответов из очереди сервер реально отдал (для проверки в stop()).
    served: Arc<AtomicUsize>,
    queued: usize,
}

/// Клиент вправе бросить соединение раньше, чем сервер дописал ответ: исчерпанный
/// бюджет обрывает запрос на полуслове, а на успешном потоке парсер выходит по
/// `[DONE]`, не дочитывая хвост. Закрытый сокет отвечает RST, и первая же запись
/// после этого даёт EPIPE/ECONNRESET — для фейкового сервера это ожидаемый исход,
/// а не сбой. Момент прихода RST — гонка (под нагрузкой падало на любой из записей),
/// поэтому обрыв терпим на всех, а любую ДРУГУЮ ошибку ввода-вывода по-прежнему
/// роняем: она означала бы настоящую поломку.
fn is_client_gone(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
    )
}

/// Ошибку записи различаем: обрыв клиентом → тихо прекращаем ответ, прочее → паника.
fn expect_write(result: std::io::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(e) if is_client_gone(&e) => false,
        Err(e) => panic!("фейковый сервер: непредвиденная ошибка записи: {e}"),
    }
}

/// Одно подключение → один «ответ» из очереди; `None` — сразу разорвать соединение
/// (эмуляция сетевого сбоя между accept и ответом). Тело передаётся chunked
/// с размером чанка 5, чтобы прогнать SSE-парсер по рваным границам.
///
/// Очередь ответов кончилась — сервер не уходит: он продолжает принимать соединения
/// и писать их в лог запросов, обрывая без ответа. Это и делает `requests.len()`
/// доказательным: лишний ретрай клиента будет виден, а не потеряется на закрытом
/// listener'е. Останавливает сервер `stop()` (см. `FakeServer::stop`).
fn fake_server(responses: Vec<Option<(u16, Vec<(String, String)>, Vec<u8>)>>, delay: Duration) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let req_log = Arc::clone(&requests);
    let stopping = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stopping);
    let served = Arc::new(AtomicUsize::new(0));
    let served_count = Arc::clone(&served);
    let queued = responses.len();
    let handle = std::thread::spawn(move || {
        let mut queue = responses.into_iter();
        loop {
            let Ok((mut conn, _)) = listener.accept() else { break };
            // Пробуждающее самоподключение из stop(): запроса не будет, лог не трогаем.
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }
            let mut buf = [0u8; 8192];
            let mut seen = Vec::new();
            loop {
                match conn.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        seen.extend_from_slice(&buf[..n]);
                        if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    // Клиент отвалился на полузапросе — соединение всё равно было, логируем.
                    Err(ref e) if is_client_gone(e) => break,
                    Err(e) => panic!("фейковый сервер: непредвиденная ошибка чтения: {e}"),
                }
            }
            req_log.lock().unwrap().push(String::from_utf8_lossy(&seen).to_string());
            // Очередь пуста — лишнее подключение: рвём без ответа (клиент увидит сетевой сбой).
            let Some(response) = queue.next() else {
                drop(conn);
                continue;
            };
            served_count.fetch_add(1, Ordering::SeqCst);
            let Some((status, headers, body)) = response else {
                drop(conn); // RST: клиент увидит обрыв до ответа
                continue;
            };
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
            for (k, v) in &headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("Transfer-Encoding: chunked\r\n\r\n");
            if !expect_write(conn.write_all(head.as_bytes())) {
                continue;
            }
            let mut alive = true;
            for piece in body.chunks(5) {
                alive = expect_write(conn.write_all(format!("{:x}\r\n", piece.len()).as_bytes()))
                    && expect_write(conn.write_all(piece))
                    && expect_write(conn.write_all(b"\r\n"));
                if !alive {
                    break;
                }
            }
            if alive {
                expect_write(conn.write_all(b"0\r\n\r\n"));
            }
        }
    });
    FakeServer {
        url: format!("http://{addr}/chat/completions"),
        requests,
        handle: Some(handle),
        addr,
        stopping,
        served,
        queued,
    }
}

impl FakeServer {
    /// Гасит сервер и проверяет, что вся очередь ответов израсходована (эквивалент
    /// прежнего `handle.join()`), а заодно поднимает панику серверного потока в тест.
    fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Будим accept(): без этого поток стоял бы в блокирующем accept вечно.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
        assert_eq!(
            self.served.load(Ordering::SeqCst),
            self.queued,
            "фейковый сервер: очередь ответов израсходована не полностью",
        );
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        // Тест мог упасть до stop() — поток не должен оставаться висеть в accept().
        self.stopping.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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
    let mut server = fake_server(vec![Some((200, vec![(r#"Content-Type"#.to_string(), r#"text/event-stream"#.to_string())], sse_body("привет мир")))], Duration::ZERO);
    let out = chat(&base_req(&server.url)).unwrap();
    assert_eq!(out.content, "привет мир");
    assert!(!out.throttled);
    let payload = payload_of(&server.requests.lock().unwrap()[0]);
    assert_eq!(payload["stream_options"], json!({"include_usage": true}));
    assert_eq!(payload["reasoning_effort"], json!("low"));
    assert_eq!(payload["seed"], json!(1));
    server.stop();
}

#[test]
fn chat_429_throttled_и_ретрай_с_retry_after() {
    // retry-after: 0 → backoff(0, "0") = 1000мс (релевантный путь, а не ожидание).
    let mut server = fake_server(
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
    server.stop();
}

#[test]
fn chat_429_недельная_квота_не_ретраится_и_не_режет_параллелизм() {
    let body = br#"{"error":{"message":"you (x) have reached your weekly usage limit, upgrade for higher limits"}}"#;
    let mut server = fake_server(vec![Some((429, vec![], body.to_vec()))], Duration::ZERO);
    let err = chat(&base_req(&server.url)).unwrap_err();
    assert!(err.quota, "квота опознана: {err:?}");
    assert!(!err.throttled, "квота — не повод снижать параллелизм");
    assert_eq!(err.status, Some(429));
    assert!(err.error.contains("квота провайдера исчерпана"));
    assert_eq!(server.requests.lock().unwrap().len(), 1, "без ретраев");
    server.stop();
}

#[test]
fn chat_402_openrouter_кредиты_кончились_это_квота() {
    let body = br#"{"error":{"message":"Insufficient credits. Add more using https://openrouter.ai/settings/credits","code":402}}"#;
    let mut server = fake_server(vec![Some((402, vec![], body.to_vec()))], Duration::ZERO);
    let err = chat(&base_req(&server.url)).unwrap_err();
    assert!(err.quota);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    server.stop();
}

#[test]
fn is_quota_exhausted_отличает_квоту_от_rate_limit() {
    use crate::llm::is_quota_exhausted;
    assert!(is_quota_exhausted(402, ""));
    assert!(is_quota_exhausted(429, "You have reached your weekly USAGE LIMIT"));
    assert!(is_quota_exhausted(429, "Key limit exceeded"));
    assert!(!is_quota_exhausted(429, "rate limited, retry later"));
    assert!(!is_quota_exhausted(500, "quota"));
}

#[test]
fn chat_400_stream_options_повтор_без_include_usage() {
    let mut server = fake_server(
        vec![
            Some((400, vec![], b"unknown field stream_options".to_vec())),
            Some((200, vec![], sse_body("ok"))),
        ],
        Duration::ZERO,
    );
    let out = chat(&base_req(&server.url)).unwrap();
    assert_eq!(out.content, "ok");
    let reqs = server.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert!(payload_of(&reqs[0])["stream_options"].is_object(), "первый запрос — с include_usage");
    assert!(payload_of(&reqs[1]).get("stream_options").is_none(), "повтор — без stream_options");
    server.stop();
}

#[test]
fn chat_400_без_stream_options_в_теле_не_ретраится() {
    let mut server = fake_server(vec![Some((400, vec![], b"bad request: max_tokens".to_vec()))], Duration::ZERO);
    let err = chat(&base_req(&server.url)).unwrap_err();
    match err {
        LlmError { status: Some(400), networky: false, .. } => {}
        other => panic!("ожидался LLM 400 без ретрая: {other:?}"),
    }
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    server.stop();
}

#[test]
fn chat_500_transient_ретрай_затем_успех() {
    let mut server = fake_server(
        vec![Some((500, vec![], b"err".to_vec())), Some((200, vec![], sse_body("ok")))],
        Duration::ZERO,
    );
    let out = chat(&base_req(&server.url)).unwrap();
    assert_eq!(out.content, "ok");
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    server.stop();
}

#[test]
fn chat_исчерпанный_бюджет_подавляет_ретрай() {
    // Сервер отвечает 500 через 300мс; бюджет 100мс → per-op таймаут сработает раньше
    // ответа (networky), бюджет израсходован → ретрая нет. Аналог JS-теста на budgetLeft().
    let mut server = fake_server(vec![Some((500, vec![], b"err".to_vec()))], Duration::from_millis(300));
    let mut req = base_req(&server.url);
    req.timeout_ms = 100;
    let err = chat(&req).unwrap_err();
    assert!(err.networky, "per-op таймаут — сетевой класс ошибки");
    assert_eq!(err.status, None);
    assert_eq!(server.requests.lock().unwrap().len(), 1, "ретрай подавлен бюджетом");
    server.stop();
}

#[test]
fn chat_network_error_ретрай_затем_успех() {
    // Первое подключение сервер разрывает сразу после accept (обрыв до ответа) —
    // клиент видит сетевой сбой, ретраит с backoff и доходит до живого ответа.
    let mut server = fake_server(
        vec![None, Some((200, vec![], sse_body("ok")))],
        Duration::ZERO,
    );
    let t = std::time::Instant::now();
    let out = chat(&base_req(&server.url)).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(900), "backoff 1000мс перед ретраем");
    assert_eq!(out.content, "ok");
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    server.stop();
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
    let mut server = fake_server(vec![Some((200, vec![], body.into_bytes()))], Duration::ZERO);
    let err = chat(&base_req(&server.url)).unwrap_err();
    assert!(err.empty, "флаг empty для JS-fallback");
    assert!(!err.networky);
    assert_eq!(err.error, "пустой content (finish=stop, reasoning=0симв)");
    server.stop();
}
