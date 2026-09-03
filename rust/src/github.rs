//! Порт транспортного слоя lib/github.mjs (Фаза 3, шаг 4): `ghRaw` (один
//! HTTP-вызов с ретраями/backoff) и `ghList` (Link-пагинация, guard 50).
//! Оркестрация (upsert = list→edit/create, post-then-delete очистка инлайнов,
//! sticky-футер, graphql-пагинация тредов) намеренно осталась в JS-обёртке:
//! маркеры профиля (MARKERS, lib/constants.mjs) и ремонт JSON (lib/json.mjs)
//! процессно-зависимы, а бинарь — одноразовый процесс на op.
//!
//! Паритет с JS (сверен с эталонной копией в lib/github.mjs):
//! - URL: path начинается с "http" → как есть, иначе `${api}${path}`
//!   (относительные Link-URL склеиваются с base api);
//! - заголовки: Authorization Bearer, Accept (default application/vnd.github+json),
//!   X-GitHub-Api-Version: 2022-11-28, User-Agent = toolName, Content-Type при body;
//! - body: JS `body ? JSON.stringify(body) : undefined` — null/falsy не шлются;
//! - таймаут 30с на попытку (аналог AbortController(30_000) в ghRaw);
//! - ретраи: transient (429/5xx) — backoff(attempt, retry-after); сетевые сбои —
//!   backoff(attempt); лимит httpRetries; не-transient статус — сразу ошибка
//!   `GitHub {method} {path} → {status}: {тело, 500 UTF-16 юнитов}` с флагом status;
//! - ghList: страницы склеиваются в массив, первая НЕ-массивная страница
//!   возвращается как есть (квирк ghList), guard 50 страниц, next — первое
//!   совпадение /<([^>]+)>;\s*rel="next"/ по заголовку Link.
//!
//! Отклонения от JS: тексты сетевых ошибок — из reqwest (не из undici); лог
//! ретраев ведёт только JS-ветка (llm-прецедент); сообщение о не-JSON ответе
//! ghList — фиксированный русский текст вместо V8 SyntaxError.

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, USER_AGENT};
use serde::Deserialize;
use serde_json::Value;

use crate::llm::{backoff_ms, is_transient};

/// X-GitHub-Api-Version из ghRaw.
pub const API_VERSION: &str = "2022-11-28";
/// AbortController(30_000) в ghRaw: per-op таймаут на попытку.
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;
/// `guard < 50` в ghList: максимум страниц за один list.
pub const MAX_LIST_PAGES: usize = 50;

#[derive(Debug, Clone, Deserialize)]
pub struct GhRequest {
    pub api: String,
    pub token: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(rename = "httpRetries")]
    pub http_retries: u32,
    pub method: String,
    pub path: String,
    /// JS: `body ? JSON.stringify(body) : undefined` — null пропускается.
    pub body: Option<Value>,
    pub accept: Option<String>,
}

impl GhRequest {
    /// ghRaw: абсолютный URL проходит как есть, относительный клеится к api.
    pub fn url(&self) -> String {
        if self.path.starts_with("http") {
            self.path.clone()
        } else {
            format!("{}{}", self.api, self.path)
        }
    }

    pub fn accept(&self) -> &str {
        self.accept.as_deref().unwrap_or("application/vnd.github+json")
    }
}

/// Доменная ошибка ghRaw/ghList — аналог JS-Error с флагом status.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GhError {
    pub error: String,
    pub status: Option<u16>,
    pub networky: bool,
}

fn gh_error(message: impl Into<String>, status: Option<u16>, networky: bool) -> GhError {
    GhError { error: message.into(), status, networky }
}

/// `text.slice(0, maxUnits)` — обрезка по UTF-16 юнитам, без суффикса.
fn js_slice_utf16(s: &str, max_units: usize) -> String {
    let mut units = 0usize;
    for (i, c) in s.char_indices() {
        let next = units + c.len_utf16();
        if next > max_units {
            return s[..i].to_string();
        }
        units = next;
    }
    s.to_string()
}

fn header_or(value: &str, fallback: &'static str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(fallback))
}

/// `ghRaw` из lib/github.mjs целиком (HTTP + ретраи).
pub fn gh_raw(req: &GhRequest) -> Result<GhResponse, GhError> {
    let timeout = Duration::from_millis(REQUEST_TIMEOUT_MS);
    let client = Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()
        .map_err(|e| gh_error(e.to_string(), None, true))?;
    let method = req
        .method
        .parse::<reqwest::Method>()
        .map_err(|e| gh_error(format!("метод '{}': {e}", req.method), None, false))?;
    let url = req.url();
    let accept = req.accept().to_string();
    let body = req
        .body
        .as_ref()
        .map(|b| serde_json::to_string(b).map_err(|e| gh_error(e.to_string(), None, true)))
        .transpose()?;

    let mut attempt: u32 = 0;
    loop {
        let mut rb = client
            .request(method.clone(), &url)
            .header(AUTHORIZATION, format!("Bearer {}", req.token))
            .header(ACCEPT, header_or(&accept, "application/vnd.github+json"))
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, header_or(&req.tool_name, "ai-reviewer"));
        if let Some(b) = &body {
            rb = rb.header(CONTENT_TYPE, "application/json").body(b.clone());
        }
        let res = match rb.send() {
            Ok(r) => r,
            Err(e) => {
                // Сетевой сбой (включая per-op таймаут): ретрай при остатке попыток.
                if attempt < req.http_retries {
                    std::thread::sleep(Duration::from_secs_f64(backoff_ms(attempt, None) / 1000.0));
                    attempt += 1;
                    continue;
                }
                return Err(gh_error(e.to_string(), None, true));
            }
        };
        let status = res.status().as_u16();
        let retry_after = res
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());
        let link = res
            .headers()
            .get("link")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = res.text().unwrap_or_default();
        if !(200..=299).contains(&status) {
            if is_transient(status) && attempt < req.http_retries {
                let wait = backoff_ms(attempt, retry_after.as_deref());
                std::thread::sleep(Duration::from_secs_f64(wait / 1000.0));
                attempt += 1;
                continue;
            }
            return Err(gh_error(
                format!("GitHub {} {} → {}: {}", req.method, req.path, status, js_slice_utf16(&text, 500)),
                Some(status),
                false,
            ));
        }
        return Ok(GhResponse { text, link });
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GhResponse {
    pub text: String,
    pub link: String,
}

/// `/<([^>]+)>;\s*rel="next"/` — первое совпадение по заголовку Link.
fn next_link(link: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"<([^>]+)>;\s*rel="next""#).expect("валидный регексп"));
    re.captures(link).map(|c| c[1].to_string())
}

/// `ghList` из lib/github.mjs: склейка страниц по Link rel="next".
/// Возвращает массив-склейку; первая не-массивная страница — как есть (квирк JS).
pub fn gh_list(req: &GhRequest) -> Result<Value, GhError> {
    let mut url = req.path.clone();
    let mut out: Vec<Value> = Vec::new();
    let mut guard = 0;
    while !url.is_empty() && guard < MAX_LIST_PAGES {
        guard += 1;
        let page_req = GhRequest {
            method: "GET".to_string(),
            path: url.clone(),
            body: None,
            accept: None,
            ..req.clone()
        };
        let res = gh_raw(&page_req)?;
        // JS: `const page = text ? JSON.parse(text) : []` — пустой текст → [].
        let page: Value = if res.text.is_empty() {
            Value::Array(vec![])
        } else {
            serde_json::from_str(&res.text).map_err(|e| {
                gh_error(format!("GitHub GET {} → не-JSON ответ: {}", page_req.path, e), None, false)
            })?
        };
        let Some(arr) = page.as_array() else {
            return Ok(page);
        };
        out.extend(arr.iter().cloned());
        url = next_link(&res.link).unwrap_or_default();
    }
    Ok(Value::Array(out))
}

#[cfg(test)]
mod tests;
