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

use anyhow::{anyhow, Result};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, USER_AGENT};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::constants::{extract_sticky_footer_raw, has_marker};
use crate::json_repair::extract_json_object;
use crate::llm::{backoff_ms, is_transient, LogSink};
use crate::profile::Markers;
use crate::triage::{is_trusted_review_author, parse_trusted_authors};
use crate::traits::{
    DeleteStaleOpts, GhCallOpts, GithubApi, LoadedDiff, PostReviewParams, PostReviewResult,
    PullRequestRef, StickyData, ThreadComment, ThreadRef,
};

/// X-GitHub-Api-Version из ghRaw.
pub const API_VERSION: &str = "2022-11-28";
/// AbortController(30_000) в ghRaw: per-op таймаут на попытку.
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;
/// `guard < 50` в ghList: максимум страниц за один list.
pub const MAX_LIST_PAGES: usize = 50;

#[derive(Clone, Deserialize)]
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

/// Токен в `Debug` не печатается: запрос попадает в `{:?}` ошибок и логов.
impl std::fmt::Debug for GhRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhRequest")
            .field("api", &self.api)
            .field("token", &"***")
            .field("method", &self.method)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// `new URL(x).origin` из JS; `None` — не URL или непрозрачный origin (`"null"`).
fn url_origin(x: &str) -> Option<String> {
    let origin = reqwest::Url::parse(x).ok()?.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

impl GhRequest {
    /// ghRaw: относительный путь клеится к api; абсолютный URL (Link `rel="next"`)
    /// проходит, только если его origin совпадает с origin api.
    ///
    /// SEC-T11: запрос несёт `Authorization: Bearer`, и абсолютный URL из Link
    /// уводил бы токен туда, куда укажет ответ. Перенаправления (3xx) отдельной
    /// проверки не требуют: reqwest снимает Authorization при смене origin.
    pub fn url(&self) -> Result<String, GhError> {
        if !self.path.starts_with("http") {
            return Ok(format!("{}{}", self.api, self.path));
        }
        let api_origin = url_origin(&self.api);
        if api_origin.is_some() && url_origin(&self.path) == api_origin {
            return Ok(self.path.clone());
        }
        Err(gh_error(
            format!(
                "GitHub {} {} → чужой origin, ожидался {}: запрос с токеном не отправлен",
                self.method,
                self.path,
                api_origin.as_deref().unwrap_or(&self.api)
            ),
            None,
            false,
        ))
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

/// HTTP-клиент транспорта: per-op таймаут 30с — аналог `AbortController(30_000)`
/// вокруг каждой попытки в `ghRaw`.
///
/// Отдельная функция, потому что клиентов теперь два вида: одноразовый (бинарь-мост
/// `bin/github.rs` живёт один op) и долгоживущий у [`RealGithubClient`] — пул
/// соединений на весь прогон, как у `RealLlmClient`.
pub fn build_http_client() -> Result<Client, GhError> {
    let timeout = Duration::from_millis(REQUEST_TIMEOUT_MS);
    Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()
        .map_err(|e| gh_error(e.to_string(), None, true))
}

/// `ghRaw` из lib/github.mjs целиком (HTTP + ретраи) на одноразовом клиенте.
pub fn gh_raw(req: &GhRequest) -> Result<GhResponse, GhError> {
    gh_raw_with(&build_http_client()?, req, None)
}

/// `ghRaw` на готовом клиенте и с логом ретраев.
///
/// `log` — точный порт двух строк JS-ветки (`lib/github.mjs:391`, `lib/github.mjs:402`):
/// бинарь-мост их не пишет (лог живёт в JS-процессе), а [`RealGithubClient`] обязан,
/// иначе после cutover исчезнет единственный след деградации сети в CI-логе.
pub fn gh_raw_with(
    client: &Client,
    req: &GhRequest,
    log: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<GhResponse, GhError> {
    let say = |msg: String| {
        if let Some(log) = log {
            log(&msg);
        }
    };
    let method = req
        .method
        .parse::<reqwest::Method>()
        .map_err(|e| gh_error(format!("метод '{}': {e}", req.method), None, false))?;
    let url = req.url()?;
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
                    let wait = backoff_ms(attempt, None);
                    say(format!(
                        "GitHub {} {} сетевой сбой ({}) → повтор через {}мс ({}/{})",
                        req.method,
                        req.path,
                        e,
                        wait,
                        attempt + 1,
                        req.http_retries
                    ));
                    std::thread::sleep(Duration::from_secs_f64(wait / 1000.0));
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
                say(format!(
                    "GitHub {} {} → {}, повтор через {}мс ({}/{})",
                    req.method,
                    req.path,
                    status,
                    wait,
                    attempt + 1,
                    req.http_retries
                ));
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
    gh_list_with(&build_http_client()?, req, None)
}

/// `ghList` на готовом клиенте и с логом ретраев (см. [`gh_raw_with`]).
pub fn gh_list_with(
    client: &Client,
    req: &GhRequest,
    log: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<Value, GhError> {
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
        let res = gh_raw_with(client, &page_req, log)?;
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

// ===========================================================================
// Клиент GitHub — порт createGithubClient (lib/github.mjs:55-275)
// ===========================================================================

/// GraphQL-запрос незакрытых review-тредов (`lib/github.mjs:279-296`).
/// Текст запроса — часть контракта с API, поэтому скопирован дословно.
pub const THREADS_QUERY: &str = r#"query($owner: String!, $name: String!, $pr: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $pr) {
      reviewThreads(first: 100, after: $after) {
        nodes {
          id
          isResolved
          path
          line
          comments(first: 10) {
            nodes { body author { login } }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;

/// Мутация ответа в тред (`lib/github.mjs:298-302`).
pub const REPLY_MUTATION: &str = r#"mutation($threadId: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, body: $body }) {
    comment { id }
  }
}"#;

/// Мутация закрытия треда (`lib/github.mjs:304-306`).
pub const RESOLVE_MUTATION: &str = r#"mutation($threadId: ID!) {
  resolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } }
}"#;

/// `maxPages` пагинации review-тредов (`lib/github.mjs:229`).
pub const MAX_THREAD_PAGES: usize = 50;

/// Поля замороженного конфига, которые читает `createGithubClient`
/// (`lib/github.mjs:59-60`: `cfg.github`, `cfg.httpRetries`, `cfg.profile.toolName`).
///
/// Отдельная структура, а не `&Value`, по тем же соображениям, что
/// [`crate::llm::LlmConfig`]: клиент живёт весь прогон, и разбирать JSON на каждый
/// запрос незачем.
#[derive(Debug, Clone)]
pub struct GithubConfig {
    /// `github.api` — база REST без хвостового слэша (`GITHUB_API_URL`).
    pub api: String,
    /// `github.token` — Bearer (`GITHUB_TOKEN`). Пустой = «GitHub не сконфигурирован».
    pub token: String,
    /// `github.repo` — `owner/name` из CLI. Пустой = «GitHub не сконфигурирован».
    pub repo: String,
    /// `github.prNumber` из CLI. `0` — тот же falsy, что `undefined` в JS-проверках
    /// `if (!repo || !prNumber || !token)`.
    pub pr_number: u64,
    /// `cfg.profile.toolName` — значение заголовка `User-Agent` (`lib/github.mjs:377`).
    pub tool_name: String,
    /// `cfg.httpRetries` (`REVIEW_HTTP_RETRIES`, дефолт 3). `0` — валидное значение
    /// «без ретраев», поэтому дефолт подставляется только при отсутствии поля.
    pub http_retries: u32,
    /// `cfg.github.trustedAuthors` (`REVIEW_TRUSTED_AUTHORS`, алиас
    /// `REVIEW_FEEDBACK_TRUSTED_AUTHORS`): чьи комментарии с маркером движок считает
    /// своими. Пустой список — доверять всем (явный откат `REVIEW_TRUSTED_AUTHORS=`).
    pub trusted_authors: Vec<String>,
}

impl Default for GithubConfig {
    /// Дефолты движка (`lib/config.mjs:230-233`, `lib/profile.mjs` toolName),
    /// а не «пустой» объект: клиент, собранный вручную в тесте, обязан вести себя
    /// как боевой.
    fn default() -> Self {
        GithubConfig {
            api: "https://api.github.com".to_string(),
            token: String::new(),
            repo: String::new(),
            pr_number: 0,
            tool_name: "ai-review".to_string(),
            http_retries: 3,
            trusted_authors: parse_trusted_authors(None),
        }
    }
}

/// JS `Number(v)` для `prNumber`: число или числовая строка, иначе `0` (falsy).
fn as_pr_number(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        Some(other) => other.as_u64().unwrap_or(0),
        None => 0,
    }
}

impl GithubConfig {
    /// Читает поля из замороженного конфига ([`crate::config::load_review_config_parts`]
    /// плюс `cli`/`profile`, которые JS домешивает при сборке).
    ///
    /// Отсутствующие поля дают дефолты движка, а не панику: клиент обязан пережить
    /// урезанный конфиг так же, как JS переживает `undefined` через `||`.
    pub fn from_cfg(cfg: &Value) -> Self {
        let d = GithubConfig::default();
        let str_or = |v: Option<&Value>, def: String| {
            v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or(def)
        };
        GithubConfig {
            api: str_or(cfg.pointer("/github/api"), d.api),
            token: str_or(cfg.pointer("/github/token"), String::new()),
            repo: str_or(cfg.pointer("/github/repo"), String::new()),
            pr_number: as_pr_number(cfg.pointer("/github/prNumber")),
            tool_name: str_or(cfg.pointer("/profile/toolName"), d.tool_name),
            http_retries: cfg
                .get("httpRetries")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(d.http_retries),
            // `ghCfg.trustedAuthors ?? parseTrustedAuthors()`: самодельный cfg без поля
            // получает дефолт, а не «доверять всем».
            trusted_authors: match cfg.pointer("/github/trustedAuthors").and_then(Value::as_array) {
                Some(arr) => arr.iter().filter_map(Value::as_str).map(str::to_string).collect(),
                None => parse_trusted_authors(None),
            },
        }
    }
}

/// Боевой клиент GitHub: транспорт [`gh_raw_with`]/[`gh_list_with`] плюс вся
/// оркестрация, которая до cutover оставалась в JS-обёртке (`lib/github.mjs:55-275`).
///
/// Что здесь важно и легко потерять при перечитывании:
///
/// - **sticky ищется с конца** (`[...comments].reverse().find`), маркер сверяется
///   по НАЧАЛУ тела ([`has_marker`]), а автор — по `trusted_authors` (SEC-T11):
///   ни процитированный, ни вписанный в начало чужого комментария маркер не делает
///   его нашим;
/// - **post-then-delete**: `post_review` идёт раньше `delete_stale_inline`, и id
///   свежего review приходит сюда через `except_review_id` — при сбое постинга старые
///   инлайны остаются на месте;
/// - **два метода глушат ошибку сами** — `read_prior_sticky_data` (нет sticky и битый
///   футер равны «прошлого прогона не было») и `delete_stale_inline` (чистка не
///   критична). Остальные отдают `Err`, а решение принимает пайплайн.
///
/// Отличия от JS, все технические:
///
/// - HTTP-клиент один на весь прогон (в JS `fetch` держит пул сам);
/// - `Err` вместо брошенного `Error`, текст сообщения сохранён дословно — по нему
///   сверяются golden-логи;
/// - не-массивный ответ `ghList` (квирк JS, где он возвращался как есть и падал у
///   вызывающего на `[...x]`) здесь сразу `Err` с внятным текстом.
pub struct RealGithubClient {
    cfg: GithubConfig,
    markers: Markers,
    http: Client,
    log: Option<LogSink>,
}

impl std::fmt::Debug for RealGithubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealGithubClient")
            .field("api", &self.cfg.api)
            .field("repo", &self.cfg.repo)
            .field("pr_number", &self.cfg.pr_number)
            .field("markers", &self.markers.ns)
            .finish_non_exhaustive()
    }
}

impl RealGithubClient {
    /// Собирает клиента и HTTP-пул. Маркеры приходят снимком профиля: в JS это
    /// живые getters `MARKERS`, потому что профиль там process-wide синглтон.
    pub fn new(cfg: &GithubConfig, markers: Markers) -> Result<Self> {
        let http = build_http_client().map_err(|e| anyhow!("{}", e.error))?;
        Ok(RealGithubClient { cfg: cfg.clone(), markers, http, log: None })
    }

    /// Логгер — второй аргумент `createGithubClient`.
    pub fn with_log(mut self, log: LogSink) -> Self {
        self.log = Some(log);
        self
    }

    fn log(&self, message: &str) {
        if let Some(log) = &self.log {
            log(message);
        }
    }

    fn log_sink(&self) -> Option<&(dyn Fn(&str) + Send + Sync)> {
        self.log.as_ref().map(|l| l.as_ref())
    }

    /// `github.repo && github.prNumber && github.token` — сконфигурирован ли GitHub.
    fn configured(&self) -> bool {
        !self.cfg.repo.is_empty() && self.cfg.pr_number != 0 && !self.cfg.token.is_empty()
    }

    fn request(&self, method: &str, path: &str, body: Option<Value>, accept: Option<&str>) -> GhRequest {
        GhRequest {
            api: self.cfg.api.clone(),
            token: self.cfg.token.clone(),
            tool_name: self.cfg.tool_name.clone(),
            http_retries: self.cfg.http_retries,
            method: method.to_string(),
            path: path.to_string(),
            body,
            accept: accept.map(str::to_string),
        }
    }

    /// `gh(path, opts)` (`lib/github.mjs:411-415`): текст для diff-accept, иначе JSON;
    /// пустое тело — `{}`.
    fn gh_call(&self, method: &str, path: &str, body: Option<Value>, accept: Option<&str>) -> Result<Value> {
        let req = self.request(method, path, body, accept);
        let accept = req.accept().to_string();
        let res = gh_raw_with(&self.http, &req, self.log_sink()).map_err(|e| anyhow!("{}", e.error))?;
        if accept.contains("diff") {
            return Ok(Value::String(res.text));
        }
        if res.text.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&res.text)
            .map_err(|e| anyhow!("GitHub {} {} → не-JSON ответ: {}", req.method, req.path, e))
    }

    /// `ghList(path)` (`lib/github.mjs:417-427`) со свёрткой квирка не-массива в `Err`.
    fn gh_list_call(&self, path: &str) -> Result<Vec<Value>> {
        let req = self.request("GET", path, None, None);
        let value =
            gh_list_with(&self.http, &req, self.log_sink()).map_err(|e| anyhow!("{}", e.error))?;
        match value {
            Value::Array(items) => Ok(items),
            _ => Err(anyhow!("GitHub GET {path} → ожидался массив, пришёл объект")),
        }
    }

/// Комментарий наш: тело начинается с маркера И автор доверенный
    /// (`isOwnBotComment` в lib/github.mjs).
    ///
    /// SEC-T11: маркер — публичная строка, и любой участник PR может начать с неё
    /// свой комментарий. Без проверки автора поддельный футер уезжал в prior
    /// findings (промпт, Δ-блок, отчёт), upsert патчил чужой комментарий, а чистка
    /// инлайнов удаляла комментарии людей.
    fn is_own(&self, c: &Value, marker: &str) -> bool {
        if !has_marker(c.get("body").and_then(Value::as_str).unwrap_or(""), marker) {
            return false;
        }
        let user = c.get("user").filter(|u| u.is_object());
        // `c.user?.login` с JS-falsy: null/пустая строка — автора нет.
        let login = user
            .and_then(|u| u.get("login"))
            .filter(|l| l.as_str().is_some_and(|s| !s.is_empty()));
        is_trusted_review_author(login, &self.cfg.trusted_authors, user)
    }

    /// `pickOwnSticky` (lib/github.mjs): последний свой sticky; комментарии с маркером
    /// от чужих авторов — в лог и мимо.
    fn find_sticky<'a>(&self, comments: &'a [Value]) -> Option<&'a Value> {
        let body_of = |c: &Value| c.get("body").and_then(Value::as_str).unwrap_or("").to_string();
        let marked: Vec<&Value> =
            comments.iter().filter(|c| has_marker(&body_of(c), &self.markers.review)).collect();
        let own = marked.iter().rev().find(|c| self.is_own(c, &self.markers.review)).copied();
        let mut who: Vec<String> = Vec::new();
        for c in marked.iter().filter(|c| !self.is_own(c, &self.markers.review)) {
            let login = c
                .pointer("/user/login")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("?")
                .to_string();
            if !who.contains(&login) {
                who.push(login);
            }
        }
        if !who.is_empty() {
            self.log(&format!(
                "sticky-маркер в комментариях недоверенных авторов ({}) — игнорирую",
                who.join(", ")
            ));
        }
        own
    }

    /// Лента issue-комментариев PR — общий источник sticky для трёх методов.
    fn issue_comments(&self) -> Result<Vec<Value>> {
        self.gh_list_call(&format!(
            "/repos/{}/issues/{}/comments?per_page=100",
            self.cfg.repo, self.cfg.pr_number
        ))
    }

    /// `graphql(query, variables)` (`lib/github.mjs:534-541`): `errors` из ответа —
    /// это ошибка вызова, склеенная через `; `, а не «пустые данные».
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables });
        let req = self.request("POST", "/graphql", Some(body), None);
        let res = gh_raw_with(&self.http, &req, self.log_sink()).map_err(|e| anyhow!("{}", e.error))?;
        let parsed: Value = if res.text.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&res.text)
                .map_err(|e| anyhow!("GitHub POST /graphql → не-JSON ответ: {}", e))?
        };
        if let Some(errors) = parsed.get("errors").and_then(Value::as_array) {
            if !errors.is_empty() {
                let joined = errors
                    .iter()
                    .map(|e| e.get("message").and_then(Value::as_str).unwrap_or("").to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(anyhow!("{joined}"));
            }
        }
        Ok(parsed.get("data").cloned().unwrap_or(Value::Null))
    }

    /// `repoParts()` (`lib/github.mjs:543-547`).
    fn repo_parts(&self) -> Result<(String, String)> {
        let mut it = self.cfg.repo.split('/');
        let owner = it.next().unwrap_or("");
        let name = it.next().unwrap_or("");
        if owner.is_empty() || name.is_empty() {
            return Err(anyhow!("github.repo not configured"));
        }
        Ok((owner.to_string(), name.to_string()))
    }

    /// Тело `read_prior_sticky_data` без глушения — вся обработка ошибок снаружи,
    /// одним `catch`, как в JS.
    fn read_prior_sticky_data_inner(&self) -> Result<StickyData> {
        let comments = self.issue_comments()?;
        let Some(mine) = self.find_sticky(&comments) else {
            return Ok(StickyData::default());
        };
        let body = mine.get("body").and_then(Value::as_str).unwrap_or("");
        let Some(raw) = extract_sticky_footer_raw(body, &self.markers) else {
            return Ok(StickyData::default());
        };
        // Обрезанный под лимит GitHub футер чиним ремонтом JSON (P2-12), иначе
        // prior findings молча теряются.
        let data: Value = match serde_json::from_str::<Value>(raw.trim()) {
            Ok(v) => v,
            Err(_) => extract_json_object(&raw).map_err(|e| anyhow!("{}", e.message))?.value,
        };
        // Футер пишет наш же движок, но читаем мы его из чужого текста (комментарий
        // могли отредактировать руками) — проверяем форму элементов, а не только массива.
        let findings = data
            .get("findings")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|f| f.get("file").and_then(Value::as_str).is_some())
                    .take(40)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let runs_all = data
            .get("runs")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|r| r.get("sha").and_then(Value::as_str).is_some())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // `slice(-5)` — последние пять прогонов.
        let runs = runs_all[runs_all.len().saturating_sub(5)..].to_vec();
        let sha = data.get("sha").and_then(Value::as_str).unwrap_or("").to_string();
        Ok(StickyData { findings, runs, sha })
    }
}

impl GithubApi for RealGithubClient {
    fn gh(&self, path: &str, opts: &GhCallOpts) -> Result<Value> {
        let method = if opts.method.is_empty() { "GET" } else { &opts.method };
        self.gh_call(method, path, opts.body.clone(), opts.accept.as_deref())
    }

    fn load_diff(&self) -> Result<Option<LoadedDiff>> {
        if !self.configured() {
            return Ok(None);
        }
        let path = format!("/repos/{}/pulls/{}", self.cfg.repo, self.cfg.pr_number);
        let pr = self.gh_call("GET", &path, None, None)?;
        let head_sha =
            pr.pointer("/head/sha").and_then(Value::as_str).unwrap_or("").to_string();
        // Черновик: второй запрос (за диффом) не делаем, pr/headSha не отдаём —
        // ровно как `return { draft: true }` в JS.
        if pr.get("draft").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(Some(LoadedDiff { draft: true, ..LoadedDiff::default() }));
        }
        let diff = self.gh_call("GET", &path, None, Some("application/vnd.github.v3.diff"))?;
        // Поля читаются по одному, а не через `from_value`: JS берёт их лениво, и
        // неожиданный `null` в чужом поле PR не должен обнулять весь объект.
        let pr_ref = PullRequestRef {
            number: pr.get("number").and_then(Value::as_u64).unwrap_or(0),
            title: pr.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            html_url: pr.get("html_url").and_then(Value::as_str).unwrap_or("").to_string(),
        };
        Ok(Some(LoadedDiff {
            draft: false,
            pr: pr_ref,
            head_sha,
            diff: diff.as_str().unwrap_or("").to_string(),
        }))
    }

    fn read_prior_sticky_data(&self) -> Result<StickyData> {
        if !self.configured() {
            return Ok(StickyData::default());
        }
        // Форма возврата одна во ВСЕХ ветках: на первом прогоне PR sticky ещё нет,
        // и «пусто» здесь роняло пайплайн на `priorSticky.findings.length`.
        Ok(self.read_prior_sticky_data_inner().unwrap_or_default())
    }

    fn read_prior_findings(&self) -> Result<Vec<Value>> {
        Ok(self.read_prior_sticky_data()?.findings)
    }

    fn upsert_sticky(&self, body: &str) -> Result<String> {
        let comments = self.issue_comments()?;
        if let Some(mine) = self.find_sticky(&comments) {
            let id = mine.get("id").cloned().unwrap_or(Value::Null);
            let path = format!("/repos/{}/issues/comments/{}", self.cfg.repo, id);
            self.gh_call("PATCH", &path, Some(json!({ "body": body })), None)?;
            return Ok("updated".to_string());
        }
        let path = format!("/repos/{}/issues/{}/comments", self.cfg.repo, self.cfg.pr_number);
        self.gh_call("POST", &path, Some(json!({ "body": body })), None)?;
        Ok("created".to_string())
    }

    fn delete_stale_inline(&self, opts: &DeleteStaleOpts) -> Result<usize> {
        let mut removed = 0usize;
        let mut kept = 0usize;
        let mut foreign = 0usize;
        // Весь блок под одним «catch»: чистка не критична (`lib/github.mjs:178`).
        let outcome = (|| -> Result<()> {
            let comments = self.gh_list_call(&format!(
                "/repos/{}/pulls/{}/comments?per_page=100",
                self.cfg.repo, self.cfg.pr_number
            ))?;
            let has_replies: std::collections::HashSet<u64> = comments
                .iter()
                .filter_map(|c| c.get("in_reply_to_id").and_then(Value::as_u64))
                .collect();
            for c in &comments {
                let body = c.get("body").and_then(Value::as_str).unwrap_or("");
                if !has_marker(body, &self.markers.inline) {
                    continue;
                }
                // `isForeignInline`: маркер есть, автор чужой — не удаляем и сообщаем.
                if !self.is_own(c, &self.markers.inline) {
                    foreign += 1;
                    continue;
                }
                // Только что запощенный review (post-then-delete: при сбое постинга
                // старые не теряем).
                let review_id = c.get("pull_request_review_id").and_then(Value::as_u64);
                if opts.except_review_id.is_some() && review_id == opts.except_review_id {
                    continue;
                }
                // Тред с ответами не трогаем — удаление корня рвёт обсуждение
                // (разрулит auto-triage).
                let id = c.get("id").and_then(Value::as_u64);
                if id.is_some_and(|id| has_replies.contains(&id)) {
                    kept += 1;
                    continue;
                }
                let path = format!(
                    "/repos/{}/pulls/comments/{}",
                    self.cfg.repo,
                    c.get("id").cloned().unwrap_or(Value::Null)
                );
                self.gh_call("DELETE", &path, None, None)?;
                removed += 1;
            }
            Ok(())
        })();
        if let Err(e) = outcome {
            self.log(&format!("очистка старых инлайнов не удалась (не критично): {e}"));
        }
        if kept > 0 {
            self.log(&format!("старых инлайн-тредов с ответами оставлено: {kept}"));
        }
        if foreign > 0 {
            self.log(&format!("инлайнов с маркером от недоверенных авторов не тронуто: {foreign}"));
        }
        Ok(removed)
    }

    fn post_review(&self, params: &PostReviewParams) -> Result<PostReviewResult> {
        let body = if params.inline.is_empty() {
            "🤖 Авто-ревью: инлайн-замечаний нет, общий вывод — в закреплённом комментарии."
        } else {
            "🤖 Авто-ревью: инлайн-замечания ниже, общий вывод — в закреплённом комментарии."
        };
        let mut payload = json!({
            "event": "COMMENT",
            "body": body,
            "comments": params.inline,
        });
        if !params.head_sha.is_empty() {
            payload["commit_id"] = json!(params.head_sha);
        }
        let path = format!("/repos/{}/pulls/{}/reviews", self.cfg.repo, self.cfg.pr_number);
        let res = self.gh_call("POST", &path, Some(payload), None)?;
        // `res?.id || null`: нулевой id для JS тоже falsy.
        let review_id = res.get("id").and_then(Value::as_u64).filter(|id| *id != 0);
        Ok(PostReviewResult { review_id })
    }

    fn list_inline_comments(&self) -> Result<Vec<Value>> {
        if !self.configured() {
            return Ok(vec![]);
        }
        self.gh_list_call(&format!(
            "/repos/{}/pulls/{}/comments?per_page=100",
            self.cfg.repo, self.cfg.pr_number
        ))
    }

    fn list_issue_labels(&self) -> Result<Vec<Value>> {
        if !self.configured() {
            return Ok(vec![]);
        }
        self.gh_list_call(&format!(
            "/repos/{}/issues/{}/labels?per_page=100",
            self.cfg.repo, self.cfg.pr_number
        ))
    }

    fn read_sticky_review(&self) -> Result<Option<Value>> {
        if !self.configured() {
            return Ok(None);
        }
        let comments = self.issue_comments()?;
        Ok(self.find_sticky(&comments).cloned())
    }

    fn list_pull_comment_reactions(&self, comment_id: u64) -> Result<Vec<Value>> {
        // Здесь проверка БЕЗ prNumber: реакции читаются по id комментария
        // (`lib/github.mjs:588`), в том числе вне контекста конкретного PR.
        if self.cfg.repo.is_empty() || self.cfg.token.is_empty() {
            return Ok(vec![]);
        }
        self.gh_list_call(&format!(
            "/repos/{}/pulls/comments/{}/reactions?per_page=100",
            self.cfg.repo, comment_id
        ))
    }

    fn list_unresolved_review_threads(&self) -> Result<Vec<ThreadRef>> {
        let (owner, name) = self.repo_parts()?;
        let mut nodes: Vec<Value> = Vec::new();
        let mut after = Value::Null;
        let mut done = false;
        for _ in 0..MAX_THREAD_PAGES {
            let data = self.graphql(
                THREADS_QUERY,
                json!({ "owner": owner, "name": name, "pr": self.cfg.pr_number, "after": after }),
            )?;
            let conn = data.pointer("/repository/pullRequest/reviewThreads");
            if let Some(page) = conn.and_then(|c| c.get("nodes")).and_then(Value::as_array) {
                nodes.extend(page.iter().cloned());
            }
            let has_next = conn
                .and_then(|c| c.pointer("/pageInfo/hasNextPage"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !has_next {
                done = true;
                break;
            }
            after = conn
                .and_then(|c| c.pointer("/pageInfo/endCursor"))
                .cloned()
                .unwrap_or(Value::Null);
        }
        if !done {
            // Молчаливая обрезка ленты тредов дала бы auto-triage неполную картину.
            return Err(anyhow!("reviewThreads pagination exceeded {MAX_THREAD_PAGES} pages"));
        }
        Ok(nodes
            .iter()
            .filter(|t| !t.get("isResolved").and_then(Value::as_bool).unwrap_or(false))
            .map(|t| ThreadRef {
                id: t.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                path: t.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
                line: t.get("line").and_then(Value::as_u64),
                comments: t
                    .pointer("/comments/nodes")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .map(|c| ThreadComment {
                                body: c.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
                                author: c
                                    .pointer("/author/login")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect())
    }

    fn reply_review_thread(&self, thread_id: &str, body: &str) -> Result<()> {
        self.graphql(REPLY_MUTATION, json!({ "threadId": thread_id, "body": body }))?;
        Ok(())
    }

    fn resolve_review_thread(&self, thread_id: &str) -> Result<()> {
        self.graphql(RESOLVE_MUTATION, json!({ "threadId": thread_id }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
