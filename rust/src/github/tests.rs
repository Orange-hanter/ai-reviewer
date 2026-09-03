//! Тесты rust/src/github.rs: контракт ghRaw/ghList против фейкового HTTP-сервера
//! на std::net (прецедент rust/src/llm/tests.rs): Link-пагинация (абсолютные и
//! относительные URL), не-массивная страница, guard 50 страниц, upsert-матрица
//! (create/edit/маркер на второй странице), post-then-delete порядок вызовов,
//! коды ошибок (transient 500 + retry-after, non-transient 400 без ретрая,
//! обрыв соединения, connection refused), заголовки запроса.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

use super::{gh_list, gh_raw, GhRequest, MAX_LIST_PAGES};

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
                if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
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
