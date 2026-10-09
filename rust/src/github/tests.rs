//! Тесты rust/src/github.rs против фейкового HTTP-сервера на std::net
//! (прецедент rust/src/llm/tests.rs). Два слоя:
//!
//! 1. транспорт (`gh_raw`/`gh_list`): Link-пагинация (абсолютные и относительные
//!    URL), не-массивная страница, guard 50 страниц, upsert-матрица вручную,
//!    post-then-delete порядок вызовов, коды ошибок (transient 500 + retry-after,
//!    non-transient 400 без ретрая, обрыв соединения, connection refused), заголовки;
//! 2. клиент [`RealGithubClient`] (задача T11) — порт девяти кейсов
//!    `test/github.test.mjs` плюс то, чего в JS-тестах не было: курсорная
//!    пагинация review-тредов, превышение лимита страниц, `loadDiff` для
//!    черновика и обычного PR, ранние возвраты при несконфигурированном GitHub.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

use super::{
    gh_list, gh_raw, GhRequest, GithubConfig, RealGithubClient, MAX_LIST_PAGES, MAX_THREAD_PAGES,
};
use crate::profile::Markers;
use crate::traits::{
    DeleteStaleOpts, GhCallOpts, GithubApi, InlineComment, PostReviewParams, ThreadComment,
};

const MARKER: &str = "<!-- ai-review -->";
const INLINE_MARKER: &str = "<!-- ai-review-inline -->";

struct FakeServer {
    api: String,
    requests: Arc<Mutex<Vec<String>>>,
    #[allow(dead_code)]
    handle: JoinHandle<()>,
}

/// Хендлер на каждое подключение: `None` — разорвать соединение (сетевой сбой).
fn fake_server<F>(handler: F) -> FakeServer
where
    F: Fn(&str) -> Option<(u16, Vec<(String, String)>, Vec<u8>)> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let req_log = Arc::clone(&requests);
    let handle = std::thread::spawn(move || {
        for conn in listener.incoming() {
            let mut conn = match conn {
                Ok(c) => c,
                Err(_) => break,
            };
            let mut buf = [0u8; 65536];
            let mut seen = Vec::new();
            loop {
                let n = conn.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
                // Заголовки дочитаны — добираем тело по Content-Length: клиент
                // вправе прислать его отдельным сегментом, и без дочитывания
                // проверки JSON-тела были бы флаки.
                if let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&seen[..end]).to_lowercase();
                    let want = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if seen.len() >= end + 4 + want {
                        break;
                    }
                }
            }
            if seen.is_empty() {
                continue;
            }
            let raw = String::from_utf8_lossy(&seen).to_string();
            req_log.lock().unwrap().push(raw.clone());
            let Some((status, headers, body)) = handler(&raw) else {
                drop(conn); // обрыв до ответа
                continue;
            };
            let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
            for (k, v) in &headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
            conn.write_all(head.as_bytes()).unwrap();
            conn.write_all(&body).unwrap();
        }
    });
    FakeServer { api: format!("http://{addr}"), requests, handle }
}

fn base_req(api: &str, path: &str) -> GhRequest {
    GhRequest {
        api: api.to_string(),
        token: "t".to_string(),
        tool_name: "ai-reviewer-test".to_string(),
        http_retries: 2,
        method: "GET".to_string(),
        path: path.to_string(),
        body: None,
        accept: None,
    }
}

fn request_line(raw: &str) -> &str {
    raw.lines().next().unwrap_or("")
}

/// method + path без query — для сверки порядка вызовов.
fn method_and_path(raw: &str) -> String {
    let line = request_line(raw);
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("").split('?').next().unwrap_or("");
    format!("{method} {path}")
}

fn body_of(raw: &str) -> Value {
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(body).unwrap_or(Value::Null)
}

fn json_res(v: Value) -> (u16, Vec<(String, String)>, Vec<u8>) {
    (200, vec![("Content-Type".to_string(), "application/json".to_string())], serde_json::to_vec(&v).unwrap())
}

fn reqs(server: &FakeServer) -> Vec<String> {
    server.requests.lock().unwrap().clone()
}

/// ghList + поиск своего sticky (зеркало JS: reverse().find(hasMarker)).
fn find_sticky(comments: &[Value]) -> Option<Value> {
    comments.iter().rev().find(|c| c["body"].as_str().map(|b| b.starts_with(MARKER)).unwrap_or(false)).cloned()
}

// --- заголовки и базовый request ---

#[test]
fn request_шлёт_паритетные_заголовки() {
    let server = fake_server(|_| Some(json_res(json!({"ok": true}))));
    let res = gh_raw(&base_req(&server.api, "/repos/o/r")).unwrap();
    assert_eq!(res.text, r#"{"ok":true}"#);
    assert_eq!(res.link, "");
    let raw = &reqs(&server)[0];
    let lower = raw.to_lowercase();
    assert!(lower.contains("authorization: bearer t"), "{raw}");
    assert!(lower.contains("accept: application/vnd.github+json"), "{raw}");
    assert!(lower.contains("x-github-api-version: 2022-11-28"), "{raw}");
    assert!(lower.contains("user-agent: ai-reviewer-test"), "{raw}");
}

#[test]
fn request_с_body_ставит_content_type() {
    let server = fake_server(|_| Some(json_res(json!({"id": 1}))));
    let mut req = base_req(&server.api, "/repos/o/r/issues/7/comments");
    req.method = "POST".to_string();
    req.body = Some(json!({"body": "привет"}));
    gh_raw(&req).unwrap();
    let raw = &reqs(&server)[0];
    assert!(raw.to_lowercase().contains("content-type: application/json"), "{raw}");
    assert_eq!(body_of(raw)["body"], json!("привет"));
}

#[test]
fn request_абсолютный_url_проходит_как_есть() {
    let server = fake_server(|_| Some(json_res(json!([]))));
    let mut req = base_req(&server.api, "");
    req.path = format!("{}/custom/path", server.api);
    gh_raw(&req).unwrap();
    assert!(request_line(&reqs(&server)[0]).starts_with("GET /custom/path "), "{}", reqs(&server)[0]);
}

// --- Link-пагинация ---

#[test]
fn list_мультистраничный_абсолютные_link() {
    let server = fake_server(|raw| {
        if request_line(raw).contains("page=2") {
            return Some(json_res(json!([{"id": 2}])));
        }
        // Абсолютный Link-URL пересобираем из Host заголовка запроса.
        let host = raw
            .lines()
            .find(|l| l.to_lowercase().starts_with("host:"))
            .map(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()).unwrap_or_default())
            .unwrap_or_default();
        let link = format!("<http://{host}/issues/7/comments?page=2&per_page=100>; rel=\"next\"");
        Some((200, vec![("Content-Type".to_string(), "application/json".to_string()), ("Link".to_string(), link)], serde_json::to_vec(&json!([{"id": 1}])).unwrap()))
    });
    let out = gh_list(&base_req(&server.api, "/issues/7/comments?per_page=100")).unwrap();
    assert_eq!(out, json!([{"id": 1}, {"id": 2}]));
    let seen = reqs(&server);
    assert_eq!(seen.len(), 2);
    assert!(request_line(&seen[1]).starts_with("GET /issues/7/comments?page=2&per_page=100 "), "{}", seen[1]);
}

#[test]
fn list_относительный_link_склеивается_с_api() {
    let server = fake_server(|raw| {
        if request_line(raw).contains("page=2") {
            return Some(json_res(json!([2])));
        }
        let link = r#"</issues/7/comments?page=2>; rel="next""#.to_string();
        Some((200, vec![("Content-Type".to_string(), "application/json".to_string()), ("Link".to_string(), link)], serde_json::to_vec(&json!([1])).unwrap()))
    });
    let out = gh_list(&base_req(&server.api, "/issues/7/comments")).unwrap();
    assert_eq!(out, json!([1, 2]));
    let seen = reqs(&server);
    assert_eq!(seen.len(), 2);
    assert!(request_line(&seen[1]).starts_with("GET /issues/7/comments?page=2 "), "{}", seen[1]);
}

#[test]
fn list_не_массивная_страница_возвращается_как_есть() {
    let server = fake_server(|_| Some(json_res(json!({"total_count": 1}))));
    let out = gh_list(&base_req(&server.api, "/search/issues")).unwrap();
    assert_eq!(out, json!({"total_count": 1}));
    assert_eq!(reqs(&server).len(), 1, "не-массив → возврат с первой страницы");
}

#[test]
fn list_гвард_50_страниц() {
    let server = fake_server(|_| {
        let link = r#"</x?page=next>; rel="next""#.to_string();
        Some((200, vec![("Link".to_string(), link)], serde_json::to_vec(&json!([1])).unwrap()))
    });
    let out = gh_list(&base_req(&server.api, "/x")).unwrap();
    let arr = out.as_array().unwrap();
    assert_eq!(arr.len(), MAX_LIST_PAGES);
    assert_eq!(reqs(&server).len(), MAX_LIST_PAGES, "guard останавливает пагинацию");
}

#[test]
fn list_пустой_тело_это_пустой_массив() {
    let server = fake_server(|_| Some((200, vec![], vec![])));
    let out = gh_list(&base_req(&server.api, "/empty")).unwrap();
    assert_eq!(out, json!([]));
}

// --- upsert-матрица (list → edit/create) ---

#[test]
fn upsert_нет_маркера_create() {
    let server = fake_server(|raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([{"id": 1, "body": "<!-- other -->"}])));
        }
        Some(json_res(json!({"id": 100})))
    });
    let api = server.api.clone();
    let comments = gh_list(&base_req(&api, "/repos/o/r/issues/7/comments?per_page=100")).unwrap();
    let mine = find_sticky(comments.as_array().unwrap());
    assert!(mine.is_none());
    let mut post = base_req(&api, "/repos/o/r/issues/7/comments");
    post.method = "POST".to_string();
    post.body = Some(json!({"body": "sticky"}));
    gh_raw(&post).unwrap();
    let seen = reqs(&server);
    assert_eq!(method_and_path(&seen[0]), "GET /repos/o/r/issues/7/comments");
    assert_eq!(method_and_path(&seen[1]), "POST /repos/o/r/issues/7/comments");
}

#[test]
fn upsert_есть_маркер_edit() {
    let server = fake_server(|raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([{"id": 1, "body": format!("{MARKER}\nold")}])));
        }
        Some(json_res(json!({"id": 42})))
    });
    let api = server.api.clone();
    let comments = gh_list(&base_req(&api, "/repos/o/r/issues/7/comments?per_page=100")).unwrap();
    let mine = find_sticky(comments.as_array().unwrap()).expect("свой комментарий найден");
    assert_eq!(mine["id"], json!(1));
    let mut patch = base_req(&api, format!("/repos/o/r/issues/comments/{}", mine["id"]).as_str());
    patch.method = "PATCH".to_string();
    patch.body = Some(json!({"body": "sticky v2"}));
    gh_raw(&patch).unwrap();
    let seen = reqs(&server);
    assert_eq!(method_and_path(&seen[1]), "PATCH /repos/o/r/issues/comments/1");
    assert_eq!(body_of(&seen[1])["body"], json!("sticky v2"));
}

#[test]
fn upsert_маркер_на_второй_странице() {
    let server = fake_server(|raw| {
        if request_line(raw).starts_with("GET ") {
            if request_line(raw).contains("page=2") {
                return Some(json_res(json!([{"id": 2, "body": format!("{MARKER}\nold")}])));
            }
            let link = r#"</repos/o/r/issues/7/comments?page=2>; rel="next""#.to_string();
            return Some((200, vec![("Link".to_string(), link)], serde_json::to_vec(&json!([{"id": 1, "body": "чужой"}])).unwrap()));
        }
        Some(json_res(json!({"id": 2})))
    });
    let api = server.api.clone();
    let comments = gh_list(&base_req(&api, "/repos/o/r/issues/7/comments?per_page=100")).unwrap();
    let mine = find_sticky(comments.as_array().unwrap()).expect("маркер найден на второй странице");
    let mut patch = base_req(&api, format!("/repos/o/r/issues/comments/{}", mine["id"]).as_str());
    patch.method = "PATCH".to_string();
    patch.body = Some(json!({"body": "v2"}));
    gh_raw(&patch).unwrap();
    let seen = reqs(&server);
    assert_eq!(seen.len(), 3, "GET page1 + GET page2 + PATCH");
    assert_eq!(method_and_path(&seen[2]), "PATCH /repos/o/r/issues/comments/2");
}

// --- post-then-delete порядок вызовов ---

#[test]
fn post_then_delete_порядок_и_фильтры() {
    let server = fake_server(|raw| {
        let line = request_line(raw);
        if line.starts_with("POST /repos/o/r/pulls/7/reviews") {
            return Some(json_res(json!({"id": 555})));
        }
        if line.starts_with("GET ") {
            return Some(json_res(json!([
                {"id": 10, "body": format!("{INLINE_MARKER}\nold"), "pull_request_review_id": 1},
                {"id": 11, "body": format!("{INLINE_MARKER}\nfresh"), "pull_request_review_id": 555},
                {"id": 12, "body": format!("{INLINE_MARKER}\nroot"), "pull_request_review_id": 1},
                {"id": 13, "body": "reply", "in_reply_to_id": 12, "pull_request_review_id": 2},
                {"id": 14, "body": "чужой комментарий"}
            ])));
        }
        Some((204, vec![], vec![]))
    });
    let api = server.api.clone();

    // postReview (event/body — на уровне JS; здесь только контракт вызова).
    let mut post = base_req(&api, "/repos/o/r/pulls/7/reviews");
    post.method = "POST".to_string();
    post.body = Some(json!({"event": "COMMENT", "comments": []}));
    let review = gh_raw(&post).unwrap();
    let review_id = serde_json::from_str::<Value>(&review.text).unwrap()["id"].as_i64().unwrap();

    // deleteStaleInline: маркер + exceptReviewId + треды с ответами.
    let comments = gh_list(&base_req(&api, "/repos/o/r/pulls/7/comments?per_page=100")).unwrap();
    let has_replies: std::collections::HashSet<i64> = comments
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["in_reply_to_id"].as_i64())
        .collect();
    let mut removed = 0;
    for c in comments.as_array().unwrap() {
        let body = c["body"].as_str().unwrap_or("");
        if !body.starts_with(INLINE_MARKER) {
            continue;
        }
        if c["pull_request_review_id"].as_i64() == Some(review_id) {
            continue; // exceptReviewId: свежий review не трогаем
        }
        if has_replies.contains(&c["id"].as_i64().unwrap()) {
            continue; // тред с ответами — не удаляем
        }
        let mut del = base_req(&api, format!("/repos/o/r/pulls/comments/{}", c["id"]).as_str());
        del.method = "DELETE".to_string();
        gh_raw(&del).unwrap();
        removed += 1;
    }
    assert_eq!(removed, 1, "только id=10: 11 excepted, 12 has replies, 14 чужой");

    let seen = reqs(&server);
    let seq: Vec<String> = seen.iter().map(|r| method_and_path(r)).collect();
    assert_eq!(
        seq,
        vec![
            "POST /repos/o/r/pulls/7/reviews",
            "GET /repos/o/r/pulls/7/comments",
            "DELETE /repos/o/r/pulls/comments/10",
        ],
        "post → list → delete: при сбое постинга старые не теряются"
    );
}

// --- коды ошибок и ретраи ---

#[test]
fn transient_500_ретрай_с_retry_after() {
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n2 = Arc::clone(&n);
    let server = fake_server(move |_| {
        if n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Some((500, vec![("retry-after".to_string(), "0".to_string())], b"boom".to_vec()));
        }
        Some(json_res(json!([])))
    });
    let t = std::time::Instant::now();
    let out = gh_list(&base_req(&server.api, "/x")).unwrap();
    assert_eq!(out, json!([]));
    assert!(t.elapsed() >= Duration::from_millis(900), "backoff 1000мс (retry-after 0 → экспонента)");
    assert_eq!(reqs(&server).len(), 2);
}

#[test]
fn non_transient_400_без_ретрая() {
    let server = fake_server(|_| Some((400, vec![], b"bad".to_vec())));
    let err = gh_raw(&base_req(&server.api, "/x")).unwrap_err();
    assert_eq!(err.status, Some(400));
    assert!(!err.networky);
    assert_eq!(err.error, "GitHub GET /x → 400: bad");
    assert_eq!(reqs(&server).len(), 1);
}

#[test]
fn тело_ошибки_обрезается_500_utf16() {
    let server = fake_server(|_| {
        let body = "x".repeat(600);
        Some((400, vec![], body.into_bytes()))
    });
    let err = gh_raw(&base_req(&server.api, "/x")).unwrap_err();
    let prefix = "GitHub GET /x → 400: ";
    assert!(err.error.starts_with(prefix), "{}", err.error);
    assert_eq!(err.error.len() - prefix.len(), 500, "slice(0,500) как в JS");
}

#[test]
fn сетевой_обрыв_ретрай_затем_успех() {
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n2 = Arc::clone(&n);
    let server = fake_server(move |_| {
        if n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return None; // обрыв до ответа
        }
        Some(json_res(json!([])))
    });
    let t = std::time::Instant::now();
    let out = gh_list(&base_req(&server.api, "/x")).unwrap();
    assert_eq!(out, json!([]));
    assert!(t.elapsed() >= Duration::from_millis(900), "backoff 1000мс перед ретраем");
    assert_eq!(reqs(&server).len(), 2);
}

#[test]
fn connection_refused_это_networky_ошибка() {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_api = format!("http://{}", probe.local_addr().unwrap());
    drop(probe);
    let err = gh_raw(&base_req(&dead_api, "/x")).unwrap_err();
    assert!(err.networky);
    assert_eq!(err.status, None);
}

#[test]
fn transient_исчерпание_попыток_отдаёт_ошибку_со_статусом() {
    let server = fake_server(|_| Some((503, vec![], b"down".to_vec())));
    let err = gh_raw(&base_req(&server.api, "/x")).unwrap_err();
    assert_eq!(err.status, Some(503));
    assert!(!err.networky, "исчерпанные ретраи отдают НЕ-сетевую ошибку со статусом");
    assert_eq!(reqs(&server).len(), 3, "попытка + httpRetries=2");
}

// ===========================================================================
// RealGithubClient — порт кейсов test/github.test.mjs (задача T11)
// ===========================================================================

const DATA_KEY: &str = "ai-review-data:";

/// Клиент против фейкового сервера: те же поля конфига, что у CFG в
/// test/github.test.mjs (repo o/r, PR 7, httpRetries 2).
fn client(api: &str) -> RealGithubClient {
    RealGithubClient::new(
        &GithubConfig {
            api: api.to_string(),
            token: "t".to_string(),
            repo: "o/r".to_string(),
            pr_number: 7,
            tool_name: "ai-reviewer-test".to_string(),
            http_retries: 2,
            ..GithubConfig::default()
        },
        Markers::default(),
    )
    .expect("HTTP-клиент собирается")
}

/// Клиент с журналом лога: тексты строк — часть наблюдаемого поведения прогона.
fn client_with_log(api: &str) -> (RealGithubClient, Arc<Mutex<Vec<String>>>) {
    let logged = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&logged);
    let c = client(api).with_log(Box::new(move |m: &str| sink.lock().unwrap().push(m.to_string())));
    (c, logged)
}

/// `stickyBody` из test/github.test.mjs: маркер + текст + машинный футер.
fn sticky_body(data: &Value) -> String {
    format!("{MARKER}\n## Вердикт\nok\n<!-- {DATA_KEY}{data} -->")
}

fn logs(sink: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    sink.lock().unwrap().clone()
}

// --- readPriorStickyData ---

#[test]
fn client_sticky_data_нет_sticky_это_пустая_форма() {
    let server = fake_server(|_| Some(json_res(json!([]))));
    let data = client(&server.api).read_prior_sticky_data().unwrap();
    // Форма одна во ВСЕХ ветках: на первом прогоне PR sticky ещё нет, и «пусто»
    // не должно ронять чтение findings у вызывающего.
    assert!(data.findings.is_empty());
    assert!(data.runs.is_empty());
    assert_eq!(data.sha, "");
}

#[test]
fn client_sticky_data_берёт_последний_футер_игнорируя_цитату() {
    let real = json!({
        "findings": [{"file": "a.ts", "line": 1, "title": "x"}],
        "runs": [{"sha": "abc"}],
        "sha": "abc",
    });
    let quoted = format!("<!-- {DATA_KEY}{} -->", json!({"findings": []}));
    let body = format!("{MARKER}\nПример: {quoted}\n\n<!-- {DATA_KEY}{real} -->");
    let server = fake_server(move |_| Some(json_res(json!([{"id": 1, "user": bot(), "body": body}]))));
    let data = client(&server.api).read_prior_sticky_data().unwrap();
    assert_eq!(data.sha, "abc");
    assert_eq!(data.findings.len(), 1, "цитата маркера в теле не перебивает настоящий футер");
    assert_eq!(data.findings[0]["file"], json!("a.ts"));
    assert_eq!(data.runs.len(), 1);
}

#[test]
fn client_sticky_data_обрезанный_футер_чинится_ремонтом_json() {
    // Футер обрезан лимитом GitHub: закрывающего `-->` нет вовсе.
    let body = format!(
        "{MARKER}\n<!-- {DATA_KEY}{{\"findings\":[{{\"file\":\"b.ts\",\"line\":2,\"title\":\"t\"}}],\"runs\":[],\"sha\":\"dead"
    );
    let server = fake_server(move |_| Some(json_res(json!([{"id": 9, "user": bot(), "body": body}]))));
    let data = client(&server.api).read_prior_sticky_data().unwrap();
    assert_eq!(data.findings[0]["file"], json!("b.ts"));
    assert_eq!(data.sha, "dead");
}

#[test]
fn client_sticky_data_мусор_в_футере_это_пустая_форма() {
    // Единственный catch JS-версии: битый футер = «прошлого прогона не было».
    let body = format!("{MARKER}\n<!-- {DATA_KEY} не json -->");
    let server = fake_server(move |_| Some(json_res(json!([{"id": 1, "user": bot(), "body": body}]))));
    let data = client(&server.api).read_prior_sticky_data().unwrap();
    assert!(data.findings.is_empty());
    assert_eq!(data.sha, "");
}

#[test]
fn client_sticky_data_фильтрует_форму_элементов_и_режет_историю() {
    // Комментарий мог быть отредактирован руками — проверяем форму элементов.
    let runs: Vec<Value> = (0..7).map(|i| json!({"sha": format!("s{i}")})).collect();
    let data = json!({
        "findings": [{"file": "a.ts"}, {"line": 3}, "строка"],
        "runs": runs,
        "sha": 42,
    });
    let body = sticky_body(&data);
    let server = fake_server(move |_| Some(json_res(json!([{"id": 1, "user": bot(), "body": body}]))));
    let got = client(&server.api).read_prior_sticky_data().unwrap();
    assert_eq!(got.findings.len(), 1, "без строкового file элемент не находка");
    assert_eq!(got.runs.len(), 5, "slice(-5): последние пять прогонов");
    assert_eq!(got.runs[0]["sha"], json!("s2"));
    assert_eq!(got.sha, "", "нестроковый sha читается как пустой");
}

#[test]
fn client_read_prior_findings_это_находки_футера() {
    let data = json!({"findings": [{"file": "a.ts"}], "runs": [], "sha": "x"});
    let body = sticky_body(&data);
    let server = fake_server(move |_| Some(json_res(json!([{"id": 1, "user": bot(), "body": body}]))));
    let findings = client(&server.api).read_prior_findings().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["file"], json!("a.ts"));
}

// --- upsertSticky ---

#[test]
fn client_upsert_sticky_нет_prior_это_created() {
    let server = fake_server(|raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([])));
        }
        Some(json_res(json!({"id": 1})))
    });
    let body = sticky_body(&json!({"findings": [], "runs": [], "sha": "1"}));
    let res = client(&server.api).upsert_sticky(&body).unwrap();
    assert_eq!(res, "created");
    let seen = reqs(&server);
    assert_eq!(method_and_path(&seen[0]), "GET /repos/o/r/issues/7/comments");
    assert_eq!(method_and_path(&seen[1]), "POST /repos/o/r/issues/7/comments");
    assert_eq!(body_of(&seen[1])["body"], json!(body));
}

#[test]
fn client_upsert_sticky_есть_маркер_это_updated_патчем() {
    let existing = sticky_body(&json!({"findings": [], "runs": [], "sha": "old"}));
    let server = fake_server(move |raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([{"id": 42, "user": bot(), "body": existing}])));
        }
        Some(json_res(json!({"id": 42})))
    });
    let fresh = sticky_body(&json!({"findings": [], "runs": [], "sha": "new"}));
    let res = client(&server.api).upsert_sticky(&fresh).unwrap();
    assert_eq!(res, "updated");
    let seen = reqs(&server);
    assert_eq!(method_and_path(&seen[1]), "PATCH /repos/o/r/issues/comments/42");
    let patched = body_of(&seen[1])["body"].as_str().unwrap().to_string();
    assert!(patched.contains(r#""sha":"new""#), "{patched}");
}

#[test]
fn client_upsert_sticky_чужой_комментарий_не_перезаписывается() {
    // Маркер в СЕРЕДИНЕ чужого тела: hasMarker смотрит на начало, иначе цитата
    // маркера сделала бы чужой комментарий нашим sticky.
    let server = fake_server(|raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([{"id": 5, "user": bot(), "body": format!("цитирую {MARKER} вот так")}])));
        }
        Some(json_res(json!({"id": 6})))
    });
    assert_eq!(client(&server.api).upsert_sticky("тело").unwrap(), "created");
    assert_eq!(method_and_path(&reqs(&server)[1]), "POST /repos/o/r/issues/7/comments");
}

#[test]
fn client_список_комментариев_ходит_по_link_next() {
    let server = fake_server(|raw| {
        let line = request_line(raw);
        if line.starts_with("GET ") {
            if line.contains("page=2") {
                return Some(json_res(json!([{"id": 2, "user": bot(), "body": "x"}])));
            }
            let link =
                r#"</repos/o/r/issues/7/comments?page=2&per_page=100>; rel="next""#.to_string();
            return Some((
                200,
                vec![("Link".to_string(), link)],
                serde_json::to_vec(&json!([{"id": 1, "user": bot(), "body": "x"}])).unwrap(),
            ));
        }
        Some(json_res(json!({"id": 1})))
    });
    let res = client(&server.api).upsert_sticky(&format!("{MARKER}\nhi")).unwrap();
    assert_eq!(res, "created", "своего sticky ни на одной странице нет");
    let seen = reqs(&server);
    assert_eq!(seen.len(), 3, "list page1 + page2 + POST");
    assert!(request_line(&seen[1]).contains("page=2"), "{}", seen[1]);
}

// --- ретраи на уровне клиента ---

#[test]
fn client_transient_500_ретрай_затем_успех() {
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n2 = Arc::clone(&n);
    let server = fake_server(move |_| {
        if n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Some((
                500,
                vec![("retry-after".to_string(), "0".to_string())],
                b"boom".to_vec(),
            ));
        }
        Some(json_res(json!([])))
    });
    let (c, logged) = client_with_log(&server.api);
    let data = c.read_prior_sticky_data().unwrap();
    assert!(data.findings.is_empty());
    assert_eq!(reqs(&server).len(), 2);
    assert_eq!(
        logs(&logged),
        vec![
            "GitHub GET /repos/o/r/issues/7/comments?per_page=100 → 500, повтор через 1000мс (1/2)"
        ],
        "строка ретрая — дословный порт lib/github.mjs:391"
    );
}

#[test]
fn client_non_transient_400_сразу_ошибка_без_ретрая() {
    let server = fake_server(|_| Some((400, vec![], b"bad".to_vec())));
    let err = client(&server.api)
        .post_review(&PostReviewParams { inline: vec![], head_sha: "abc".to_string() })
        .unwrap_err();
    assert_eq!(err.to_string(), "GitHub POST /repos/o/r/pulls/7/reviews → 400: bad");
    assert_eq!(reqs(&server).len(), 1);
}

// --- postReview ---

#[test]
fn client_post_review_commit_id_и_id_ревью() {
    let server = fake_server(|_| Some(json_res(json!({"id": 555}))));
    let params = PostReviewParams {
        inline: vec![InlineComment::new("a.ts", 1, "x")],
        head_sha: "deadbeef".to_string(),
    };
    let res = client(&server.api).post_review(&params).unwrap();
    assert_eq!(res.review_id, Some(555));
    let sent = body_of(&reqs(&server)[0]);
    assert_eq!(sent["event"], json!("COMMENT"));
    assert_eq!(sent["commit_id"], json!("deadbeef"));
    assert_eq!(sent["comments"].as_array().unwrap().len(), 1);
    assert_eq!(
        sent["comments"][0],
        json!({"path": "a.ts", "line": 1, "side": "RIGHT", "body": "x"})
    );
    assert_eq!(
        sent["body"],
        json!("🤖 Авто-ревью: инлайн-замечания ниже, общий вывод — в закреплённом комментарии.")
    );
}

#[test]
fn client_post_review_без_инлайнов_меняет_текст_и_не_шлёт_commit_id() {
    let server = fake_server(|_| Some(json_res(json!({}))));
    let res = client(&server.api)
        .post_review(&PostReviewParams { inline: vec![], head_sha: String::new() })
        .unwrap();
    assert_eq!(res.review_id, None, "нет id → чистку старых инлайнов пропускаем");
    let sent = body_of(&reqs(&server)[0]);
    assert!(sent.get("commit_id").is_none(), "пустой headSha — поля нет: {sent}");
    assert_eq!(sent["comments"], json!([]));
    assert_eq!(
        sent["body"],
        json!("🤖 Авто-ревью: инлайн-замечаний нет, общий вывод — в закреплённом комментарии.")
    );
}

// --- deleteStaleInline ---

#[test]
fn client_delete_stale_inline_фильтры_и_лог() {
    let server = fake_server(|raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([
                {"id": 10, "user": bot(), "body": format!("{INLINE_MARKER}\nold"), "pull_request_review_id": 1},
                {"id": 11, "user": bot(), "body": format!("{INLINE_MARKER}\nfresh"), "pull_request_review_id": 99},
                {"id": 12, "user": bot(), "body": format!("{INLINE_MARKER}\nroot"), "pull_request_review_id": 1},
                {"id": 13, "user": bot(), "body": "reply", "in_reply_to_id": 12, "pull_request_review_id": 2},
                {"id": 14, "user": bot(), "body": "чужой комментарий"}
            ])));
        }
        Some(json_res(json!({})))
    });
    let (c, logged) = client_with_log(&server.api);
    let removed = c.delete_stale_inline(&DeleteStaleOpts { except_review_id: Some(99) }).unwrap();
    assert_eq!(removed, 1, "только id=10: 11 excepted, 12 has replies, 14 чужой");
    let seq: Vec<String> = reqs(&server).iter().map(|r| method_and_path(r)).collect();
    assert_eq!(seq, vec!["GET /repos/o/r/pulls/7/comments", "DELETE /repos/o/r/pulls/comments/10"]);
    assert_eq!(logs(&logged), vec!["старых инлайн-тредов с ответами оставлено: 1"]);
}

#[test]
fn client_delete_stale_inline_без_except_чистит_всё_своё() {
    let server = fake_server(|raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([
                {"id": 10, "user": bot(), "body": format!("{INLINE_MARKER}\na"), "pull_request_review_id": 1},
                {"id": 11, "user": bot(), "body": format!("{INLINE_MARKER}\nb"), "pull_request_review_id": 99}
            ])));
        }
        Some(json_res(json!({})))
    });
    let removed = client(&server.api).delete_stale_inline(&DeleteStaleOpts::default()).unwrap();
    assert_eq!(removed, 2, "exceptReviewId=None — свежего review нет, чистим оба");
}

#[test]
fn client_delete_stale_inline_глушит_ошибку_api() {
    let server = fake_server(|_| Some((400, vec![], b"nope".to_vec())));
    let (c, logged) = client_with_log(&server.api);
    // Fail-open: чистка не критична, прогон продолжается.
    let removed = c.delete_stale_inline(&DeleteStaleOpts::default()).unwrap();
    assert_eq!(removed, 0);
    assert_eq!(
        logs(&logged),
        vec![
            "очистка старых инлайнов не удалась (не критично): GitHub GET /repos/o/r/pulls/7/comments?per_page=100 → 400: nope"
        ]
    );
}

// --- loadDiff ---

#[test]
fn client_load_diff_обычный_pr_даёт_sha_и_дифф() {
    let server = fake_server(|raw| {
        if raw.to_lowercase().contains("accept: application/vnd.github.v3.diff") {
            return Some((200, vec![], b"--- a/x\n+++ b/x\n".to_vec()));
        }
        Some(json_res(json!({
            "number": 7,
            "title": "тест",
            "html_url": "https://gh/pr/7",
            "head": {"sha": "deadbeef"},
            "draft": false,
        })))
    });
    let loaded = client(&server.api).load_diff().unwrap().expect("PR загружен");
    assert!(!loaded.draft);
    assert_eq!(loaded.head_sha, "deadbeef");
    assert_eq!(loaded.diff, "--- a/x\n+++ b/x\n");
    assert_eq!(loaded.pr.number, 7);
    assert_eq!(loaded.pr.title, "тест");
    assert_eq!(loaded.pr.html_url, "https://gh/pr/7");
    assert_eq!(reqs(&server).len(), 2, "метаданные PR + дифф по accept-заголовку");
}

#[test]
fn client_load_diff_черновик_не_запрашивает_дифф() {
    let server =
        fake_server(|_| Some(json_res(json!({"number": 7, "draft": true, "head": {"sha": "abc"}}))));
    let loaded = client(&server.api).load_diff().unwrap().expect("PR загружен");
    assert!(loaded.draft);
    assert_eq!(loaded.diff, "", "у черновика дифф не тянем — LLM не зовётся");
    assert_eq!(reqs(&server).len(), 1);
}

#[test]
fn client_gh_с_diff_accept_отдаёт_текст_а_не_json() {
    let server = fake_server(|_| Some((200, vec![], b"diff --git a b".to_vec())));
    let opts = GhCallOpts {
        method: String::new(),
        body: None,
        accept: Some("application/vnd.github.v3.diff".to_string()),
    };
    let v = client(&server.api).gh("/repos/o/r/pulls/7", &opts).unwrap();
    assert_eq!(v, json!("diff --git a b"));
    assert!(request_line(&reqs(&server)[0]).starts_with("GET "), "пустой method — это GET");
}

#[test]
fn client_gh_пустое_тело_это_пустой_объект() {
    let server = fake_server(|_| Some((204, vec![], vec![])));
    let v = client(&server.api).gh("/x", &GhCallOpts::default()).unwrap();
    assert_eq!(v, json!({}));
}

// --- ранние возвраты при несконфигурированном GitHub ---

#[test]
fn client_без_токена_не_ходит_в_сеть() {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_api = format!("http://{}", probe.local_addr().unwrap());
    drop(probe); // порт мёртв: реальный запрос был бы ошибкой, а не пустотой
    let c = RealGithubClient::new(
        &GithubConfig {
            api: dead_api,
            repo: "o/r".to_string(),
            pr_number: 7,
            ..GithubConfig::default()
        },
        Markers::default(),
    )
    .unwrap();
    assert!(c.load_diff().unwrap().is_none());
    assert!(c.list_inline_comments().unwrap().is_empty());
    assert!(c.list_issue_labels().unwrap().is_empty());
    assert!(c.read_sticky_review().unwrap().is_none());
    assert!(c.list_pull_comment_reactions(5).unwrap().is_empty());
    let sticky = c.read_prior_sticky_data().unwrap();
    assert!(sticky.findings.is_empty() && sticky.runs.is_empty() && sticky.sha.is_empty());
}

#[test]
fn client_реакции_читаются_без_номера_pr() {
    // Проверка реакций в JS — только repo+token: harvest ходит за ними и вне
    // контекста конкретного PR.
    let server = fake_server(|_| Some(json_res(json!([{"content": "+1"}]))));
    let c = RealGithubClient::new(
        &GithubConfig {
            api: server.api.clone(),
            token: "t".to_string(),
            repo: "o/r".to_string(),
            pr_number: 0,
            ..GithubConfig::default()
        },
        Markers::default(),
    )
    .unwrap();
    let out = c.list_pull_comment_reactions(77).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(method_and_path(&reqs(&server)[0]), "GET /repos/o/r/pulls/comments/77/reactions");
}

#[test]
fn client_read_sticky_review_отдаёт_последний_свой_комментарий() {
    let mine = sticky_body(&json!({"findings": [], "runs": [], "sha": "s"}));
    let server = fake_server(move |_| {
        Some(json_res(json!([
            {"id": 1, "user": bot(), "body": format!("{MARKER}\nстарый")},
            {"id": 2, "user": bot(), "body": mine},
            {"id": 3, "user": bot(), "body": "чужой"}
        ])))
    });
    let got = client(&server.api).read_sticky_review().unwrap().expect("sticky найден");
    assert_eq!(got["id"], json!(2), "берём ПОСЛЕДНЕЕ своё вхождение");
}

#[test]
fn client_список_меток_идёт_через_пагинацию() {
    let server = fake_server(|raw| {
        if request_line(raw).contains("page=2") {
            return Some(json_res(json!([{"name": "b"}])));
        }
        let link = r#"</repos/o/r/issues/7/labels?page=2>; rel="next""#.to_string();
        Some((
            200,
            vec![("Link".to_string(), link)],
            serde_json::to_vec(&json!([{"name": "a"}])).unwrap(),
        ))
    });
    let labels = client(&server.api).list_issue_labels().unwrap();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[1]["name"], json!("b"));
}

// --- GraphQL: review-треды ---

/// Ответ GraphQL с одной страницей тредов.
fn threads_page(nodes: Value, has_next: bool, cursor: &str) -> Value {
    json!({
        "data": {"repository": {"pullRequest": {"reviewThreads": {
            "nodes": nodes,
            "pageInfo": {"hasNextPage": has_next, "endCursor": cursor},
        }}}}
    })
}

#[test]
fn client_треды_курсорная_пагинация_и_фильтр_resolved() {
    let server = fake_server(|raw| {
        if raw.contains(r#""after":"CUR1""#) {
            return Some(json_res(threads_page(
                json!([{
                    "id": "T3", "isResolved": false, "path": "b.ts", "line": null,
                    "comments": {"nodes": [{"body": "вторая страница", "author": null}]},
                }]),
                false,
                "CUR2",
            )));
        }
        Some(json_res(threads_page(
            json!([
                {
                    "id": "T1", "isResolved": false, "path": "a.ts", "line": 12,
                    "comments": {"nodes": [{"body": "чиню", "author": {"login": "dev"}}]},
                },
                {"id": "T2", "isResolved": true, "path": "a.ts", "line": 3, "comments": {"nodes": []}}
            ]),
            true,
            "CUR1",
        )))
    });
    let threads = client(&server.api).list_unresolved_review_threads().unwrap();
    assert_eq!(threads.len(), 2, "закрытый тред отфильтрован, обе страницы склеены");
    assert_eq!(threads[0].id, "T1");
    assert_eq!(threads[0].line, Some(12));
    assert_eq!(
        threads[0].comments,
        vec![ThreadComment { body: "чиню".to_string(), author: Some("dev".to_string()) }]
    );
    assert_eq!(threads[1].id, "T3");
    assert_eq!(threads[1].line, None, "тред на удалённой строке переживается");
    assert_eq!(threads[1].comments[0].author, None, "автор скрыт — не ошибка");

    let seen = reqs(&server);
    assert_eq!(seen.len(), 2);
    assert_eq!(method_and_path(&seen[0]), "POST /graphql");
    let vars = body_of(&seen[0])["variables"].clone();
    assert_eq!(vars["owner"], json!("o"));
    assert_eq!(vars["name"], json!("r"));
    assert_eq!(vars["pr"], json!(7));
    assert_eq!(vars["after"], Value::Null, "первая страница идёт без курсора");
    assert_eq!(body_of(&seen[1])["variables"]["after"], json!("CUR1"));
}

#[test]
fn client_треды_превышение_лимита_страниц_это_ошибка() {
    let server = fake_server(|_| Some(json_res(threads_page(json!([]), true, "CUR"))));
    let err = client(&server.api).list_unresolved_review_threads().unwrap_err();
    // Молчаливая обрезка дала бы auto-triage неполную ленту тредов.
    assert_eq!(err.to_string(), "reviewThreads pagination exceeded 50 pages");
    assert_eq!(reqs(&server).len(), MAX_THREAD_PAGES);
}

#[test]
fn client_треды_ошибки_graphql_склеиваются_в_сообщение() {
    let server = fake_server(|_| {
        Some(json_res(json!({"errors": [{"message": "нет прав"}, {"message": "и ещё"}]})))
    });
    let err = client(&server.api).list_unresolved_review_threads().unwrap_err();
    assert_eq!(err.to_string(), "нет прав; и ещё");
}

#[test]
fn client_репозиторий_без_слеша_это_ошибка_конфига() {
    let c = RealGithubClient::new(
        &GithubConfig { repo: "просто-имя".to_string(), ..GithubConfig::default() },
        Markers::default(),
    )
    .unwrap();
    let err = c.list_unresolved_review_threads().unwrap_err();
    assert_eq!(err.to_string(), "github.repo not configured");
}

#[test]
fn client_reply_и_resolve_шлют_свои_мутации() {
    let server = fake_server(|_| Some(json_res(json!({"data": {}}))));
    let c = client(&server.api);
    c.reply_review_thread("T1", "готово").unwrap();
    c.resolve_review_thread("T1").unwrap();
    let seen = reqs(&server);
    let reply = body_of(&seen[0]);
    assert!(reply["query"].as_str().unwrap().contains("addPullRequestReviewThreadReply"));
    assert_eq!(reply["variables"], json!({"threadId": "T1", "body": "готово"}));
    let resolve = body_of(&seen[1]);
    assert!(resolve["query"].as_str().unwrap().contains("resolveReviewThread"));
    assert_eq!(resolve["variables"], json!({"threadId": "T1"}));
}

// --- маркеры чужого namespace и конфиг ---

#[test]
fn client_маркеры_берутся_из_профиля_а_не_из_дефолта() {
    let markers = Markers::new("orakul-ai-review");
    let body = format!("{}\n<!-- {}{} -->", markers.review, markers.data_key, json!({"sha": "ns"}));
    let server = fake_server(move |raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([{"id": 3, "user": bot(), "body": body}])));
        }
        Some(json_res(json!({"id": 3})))
    });
    let c = RealGithubClient::new(
        &GithubConfig {
            api: server.api.clone(),
            token: "t".to_string(),
            repo: "o/r".to_string(),
            pr_number: 7,
            ..GithubConfig::default()
        },
        markers,
    )
    .unwrap();
    assert_eq!(c.read_prior_sticky_data().unwrap().sha, "ns");
    assert_eq!(c.upsert_sticky("новое тело").unwrap(), "updated");
    // GET (sticky-данные) + GET (upsert) + PATCH: правим найденный чужой namespace.
    let seen = reqs(&server);
    assert_eq!(method_and_path(seen.last().unwrap()), "PATCH /repos/o/r/issues/comments/3");
}

#[test]
fn config_from_cfg_читает_замороженный_конфиг() {
    let cfg = json!({
        "httpRetries": 5,
        "github": {"api": "https://gh.example/api/v3", "token": "sec", "repo": "o/r", "prNumber": 7},
        "profile": {"toolName": "orakul-ai-review"},
    });
    let got = GithubConfig::from_cfg(&cfg);
    assert_eq!(got.api, "https://gh.example/api/v3");
    assert_eq!(got.token, "sec");
    assert_eq!(got.repo, "o/r");
    assert_eq!(got.pr_number, 7);
    assert_eq!(got.tool_name, "orakul-ai-review");
    assert_eq!(got.http_retries, 5);

    // Урезанный конфиг — дефолты движка, а не паника (JS переживает undefined через ||).
    let bare = GithubConfig::from_cfg(&json!({}));
    assert_eq!(bare.api, "https://api.github.com");
    assert_eq!(bare.tool_name, "ai-review");
    assert_eq!(bare.http_retries, 3);
    assert_eq!(bare.pr_number, 0);

    // httpRetries: 0 — валидное «без ретраев», дефолт подставлять нельзя.
    assert_eq!(GithubConfig::from_cfg(&json!({"httpRetries": 0})).http_retries, 0);
}

// ===========================================================================
// SEC-T11: аудит безопасности — токен, доверие к автору, недоверенные тела
// ===========================================================================

/// Автор комментария, которого движок считает собой (дефолт
/// `REVIEW_FEEDBACK_TRUSTED_AUTHORS`): логин из allowlist И `user.type == Bot`.
fn bot() -> Value {
    json!({"login": "github-actions[bot]", "type": "Bot"})
}

fn evil() -> Value {
    json!({"login": "evil", "type": "User"})
}

#[test]
fn sec_link_на_чужой_хост_не_получает_токен() {
    // Второй сервер — «чужой хост» на другом порту (другой origin).
    let foreign = fake_server(|_| Some(json_res(json!([{"id": 99}]))));
    let foreign_api = foreign.api.clone();
    let server = fake_server(move |_| {
        let link = format!("<{foreign_api}/steal?page=2>; rel=\"next\"");
        Some((200, vec![("Link".to_string(), link)], serde_json::to_vec(&json!([{"id": 1}])).unwrap()))
    });
    let err = gh_list(&base_req(&server.api, "/repos/o/r/issues/7/comments")).unwrap_err();
    assert!(reqs(&foreign).is_empty(), "Bearer-токен ушёл на чужой origin: {:?}", reqs(&foreign));
    assert!(err.error.contains("чужой origin"), "{}", err.error);
    assert_eq!(err.status, None);
    assert!(!err.networky, "отказ по origin не ретраится");
}

#[test]
fn sec_абсолютный_path_на_чужой_хост_отклоняется() {
    let foreign = fake_server(|_| Some(json_res(json!({}))));
    let server = fake_server(|_| Some(json_res(json!({}))));
    let mut req = base_req(&server.api, "");
    req.path = format!("{}/x", foreign.api);
    let err = gh_raw(&req).unwrap_err();
    assert!(reqs(&foreign).is_empty());
    assert_eq!(
        err.error,
        format!("GitHub GET {}/x → чужой origin, ожидался {}: запрос с токеном не отправлен", foreign.api, server.api)
    );
}

#[test]
fn sec_redirect_на_чужой_хост_не_несёт_authorization() {
    // Опровержение: reqwest снимает Authorization при редиректе на другой origin.
    let foreign = fake_server(|_| Some(json_res(json!({"ok": true}))));
    let target = format!("{}/landing", foreign.api);
    let server = fake_server(move |_| Some((302, vec![("Location".to_string(), target.clone())], vec![])));
    gh_raw(&base_req(&server.api, "/repos/o/r")).unwrap();
    let seen = reqs(&foreign);
    assert_eq!(seen.len(), 1);
    assert!(!seen[0].to_lowercase().contains("authorization:"), "{}", seen[0]);
}

#[test]
fn sec_debug_запроса_не_печатает_токен() {
    let mut req = base_req("http://x", "/y");
    req.token = "ghs_SECRET".to_string();
    let dbg = format!("{req:?}");
    assert!(!dbg.contains("ghs_SECRET"), "{dbg}");
}

#[test]
fn sec_sticky_чужого_автора_с_маркером_не_читается() {
    // Любой участник PR может оставить комментарий, начинающийся с маркера, с
    // поддельным футером: его findings уехали бы в промпт и в отчёт.
    let own = sticky_body(&json!({"findings": [{"file": "own.ts"}], "runs": [], "sha": "own"}));
    let forged = sticky_body(&json!({"findings": [{"file": "forged.ts"}], "runs": [], "sha": "forged"}));
    let server = fake_server(move |_| {
        Some(json_res(json!([
            {"id": 1, "body": own, "user": bot()},
            {"id": 2, "body": forged, "user": evil()},
        ])))
    });
    let data = client(&server.api).read_prior_sticky_data().unwrap();
    assert_eq!(data.sha, "own");
    assert_eq!(data.findings[0]["file"], json!("own.ts"));
}

#[test]
fn sec_sticky_с_логином_бота_но_не_bot_типом_чужой() {
    // Пользователь может зарегистрировать логин, похожий на бота, но не тип Bot.
    let forged = sticky_body(&json!({"findings": [], "runs": [], "sha": "forged"}));
    let server = fake_server(move |_| {
        Some(json_res(json!([{"id": 2, "body": forged, "user": {"login": "github-actions[bot]", "type": "User"}}])))
    });
    assert_eq!(client(&server.api).read_prior_sticky_data().unwrap().sha, "");
}

#[test]
fn sec_sticky_без_автора_чужой() {
    // `user: null` (удалённый аккаунт) — доверять нечему.
    let forged = sticky_body(&json!({"findings": [], "runs": [], "sha": "forged"}));
    let server = fake_server(move |_| Some(json_res(json!([{"id": 2, "body": forged, "user": null}]))));
    assert_eq!(client(&server.api).read_prior_sticky_data().unwrap().sha, "");
}

#[test]
fn sec_upsert_не_патчит_чужой_комментарий_с_маркером() {
    let forged = format!("{MARKER}\nподделка");
    let server = fake_server(move |raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([{"id": 5, "body": forged, "user": evil()}])));
        }
        Some(json_res(json!({"id": 6})))
    });
    assert_eq!(client(&server.api).upsert_sticky("тело").unwrap(), "created");
    assert_eq!(method_and_path(&reqs(&server)[1]), "POST /repos/o/r/issues/7/comments");
}

#[test]
fn sec_read_sticky_review_пропускает_чужой() {
    let forged = format!("{MARKER}\nподделка");
    let mine = format!("{MARKER}\nсвой");
    let server = fake_server(move |_| {
        Some(json_res(json!([
            {"id": 1, "body": mine, "user": bot()},
            {"id": 2, "body": forged, "user": evil()},
        ])))
    });
    let got = client(&server.api).read_sticky_review().unwrap().expect("свой sticky");
    assert_eq!(got["id"], json!(1));
}

#[test]
fn sec_delete_stale_inline_не_удаляет_чужие_комментарии() {
    let server = fake_server(|raw| {
        if request_line(raw).starts_with("GET ") {
            return Some(json_res(json!([
                {"id": 10, "body": format!("{INLINE_MARKER}\nсвой"), "pull_request_review_id": 1, "user": bot()},
                {"id": 20, "body": format!("{INLINE_MARKER}\nчеловек"), "pull_request_review_id": 3, "user": evil()},
            ])));
        }
        Some(json_res(json!({})))
    });
    let removed = client(&server.api).delete_stale_inline(&DeleteStaleOpts::default()).unwrap();
    assert_eq!(removed, 1);
    let seq: Vec<String> = reqs(&server).iter().map(|r| method_and_path(r)).collect();
    assert_eq!(seq, vec!["GET /repos/o/r/pulls/7/comments", "DELETE /repos/o/r/pulls/comments/10"]);
}

#[test]
fn sec_пустой_allowlist_доверяет_всем() {
    // Явный отказ от проверки (как у harvest/triage): `trustedAuthors: []`.
    let body = sticky_body(&json!({"findings": [], "runs": [], "sha": "any"}));
    let server = fake_server(move |_| Some(json_res(json!([{"id": 2, "body": body, "user": evil()}]))));
    let cfg = GithubConfig {
        api: server.api.clone(),
        token: "t".to_string(),
        repo: "o/r".to_string(),
        pr_number: 7,
        trusted_authors: vec![],
        ..GithubConfig::default()
    };
    let c = RealGithubClient::new(&cfg, Markers::default()).unwrap();
    assert_eq!(c.read_prior_sticky_data().unwrap().sha, "any");
}

#[test]
fn sec_trusted_authors_читается_из_конфига() {
    let cfg = GithubConfig::from_cfg(&json!({"feedback": {"trustedAuthors": "a[bot], b[bot]"}}));
    assert_eq!(cfg.trusted_authors, vec!["a[bot]".to_string(), "b[bot]".to_string()]);
    let d = GithubConfig::from_cfg(&json!({}));
    assert_eq!(d.trusted_authors, vec!["github-actions[bot]".to_string()]);
}
