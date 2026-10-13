//! Порт `chatRequest` из lib/llm.mjs (Фаза 3, шаг 1): OpenAI-compatible
//! `/chat/completions` — HTTP, ручной SSE-стриминг, ретраи с backoff,
//! бюджет таймаута, fail-open по `stream_options/include_usage` на 400.
//!
//! Второй слой (задача T10 RFC 20) — [`RealLlmClient`], порт `createLlmClient`
//! (`lib/llm.mjs:107-241`): два уровня адаптивных семафоров, AIMD-троттлинг обоих
//! на 429, каскад ретраев `chatJson`, `chatText` с повтором на `effort=none`
//! и учёт usage через колбэк `on_usage` (в JS — `costTracker.record`).
//!
//! Транспорт ([`chat`]) остаётся общим для двух вызывающих:
//! - `bin/llm.rs` — JSON-мост до cutover: один процесс на запрос, семафоры и каскад
//!   держит JS-обёртка `lib/llm.mjs`, а сюда приходит уже развёрнутый `ChatRequest`;
//! - [`RealLlmClient`] — единый бинарь после cutover: семафоры, каскад и usage
//!   живут в этом же процессе, JS не участвует.
//!
//! Поэтому одна и та же логика ретраев не продублирована: [`chat_with`] — точка,
//! общая для обоих путей.
//!
//! **Дедлайн прогона клиенту не передаётся** — и это не упущение порта. В JS
//! `ctx.deadline` читает только движок (`deadlineHit`, `lib/engine.mjs:66`,
//! точки проверки — `lib/engine.mjs:269,318,341,370`): он решает, запускать ли
//! ЕЩЁ один вызов модели. У клиента бюджет свой и другой — `o.requestTimeoutMs`
//! на один `chat_request` целиком, вместе с ретраями. Если бы клиент рвал
//! запрос по дедлайну прогона, движок терял бы уже оплаченный ответ модели
//! вместо того, чтобы честно пометить чанк `skipped`.
//!
//! Паритет с JS (сверен с эталонной копией в lib/llm.mjs):
//! - SSE: строки режутся по '\n' в любом месте (chunked-границы посреди
//!   `data:`/`[DONE]` переживаются буферизацией), строка триммится по JS-\s,
//!   не-`data:` строки и битый JSON в data пропускаются, usage берётся из
//!   последнего chunk'а, content триммится по JS-\s;
//! - `reasoningLen` считается в UTF-16 юнитах (JS String#length);
//! - backoff: retry-after > 0 → min(ra*1000, 30000), иначе min(1000*2^attempt, 15000);
//! - ретраи: transient (429/5xx) и сетевые ошибки — до httpRetries, с проверкой
//!   бюджета (elapsed < timeoutMs) ПЕРЕД каждым повтором; 400 на
//!   stream_options/include_usage — повтор без него без расхода attempt и бюджета;
//! - normalizeUsage: JS-семантика Number() (null → 0, ""/пропуск → NaN,
//!   numeric-строки парсятся), total fallback = prompt + completion.
//!
//! Отклонения от JS: тексты сетевых ошибок — из reqwest (не из undici), секреты
//! в теле ошибки не маскируются (это делает JS-обёртка через redactSecretsForLog);
//! отсутствие AbortController компенсируется per-op таймаутом reqwest +
//! ручной проверкой бюджета вокруг чтения SSE.

use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, Read};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::concurrency::{AdaptiveSemaphore, SemaphoreStats};
use crate::json_repair::{extract_json_object, JsonRepairError, RepairedKind};
use crate::util::redact_secrets_for_log;

#[derive(Debug, Clone, Deserialize)]
pub struct ChatRequest {
    pub url: String,
    pub key: String,
    pub model: String,
    pub messages: Value,
    /// Число или null (JSON.stringify в JS роняет undefined — обёртка шлёт null).
    #[serde(rename = "maxTokens")]
    pub max_tokens: Option<Value>,
    pub temperature: Option<f64>,
    pub effort: Option<String>,
    #[serde(rename = "jsonMode")]
    pub json_mode: bool,
    pub seed: Option<Value>,
    #[serde(rename = "httpRetries")]
    pub http_retries: u32,
    #[serde(rename = "timeoutMs")]
    pub timeout_ms: u64,
    #[serde(rename = "includeUsage")]
    pub include_usage: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatOutcome {
    pub content: String,
    /// null | {"prompt_tokens","completion_tokens","total_tokens"} — после normalizeUsage.
    pub usage: Option<Value>,
    /// 429 встречался в ходе ретраев — JS-обёртка режет семафоры (AIMD).
    pub throttled: bool,
}

/// Доменная ошибка chatRequest — аналог JS-Error с флагами status/empty/networky.
/// Сериализуется в JSON-пейлоад stderr при exit 2 (см. bin/llm.rs).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmError {
    pub error: String,
    pub status: Option<u16>,
    pub empty: bool,
    pub networky: bool,
    pub throttled: bool,
}

impl LlmError {
    fn networky(message: impl Into<String>, throttled: bool) -> Self {
        LlmError { error: message.into(), status: None, empty: false, networky: true, throttled }
    }
}

/// JS `\s` (String#trim): Unicode WhiteSpace ∪ LineTerminator ∪ U+FEFF, БЕЗ U+0085 (NEL).
fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0B}'
            | '\u{0C}'
            | '\r'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_ws)
}

/// `backoffMs` из lib/util.mjs.
pub fn backoff_ms(attempt: u32, retry_after: Option<&str>) -> f64 {
    if let Some(ra) = retry_after.and_then(|s| {
        let t = s.trim();
        if t.is_empty() { None } else { t.parse::<f64>().ok() }
    }) {
        if ra.is_finite() && ra > 0.0 {
            return (ra * 1000.0).min(30_000.0);
        }
    }
    (1000.0 * 2f64.powf(f64::from(attempt))).min(15_000.0)
}

/// `isTransient` из lib/util.mjs: 429 или любой 5xx.
pub fn is_transient(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

/// JS-семантика Number(value) для значений usage-полей: null → 0, пропуск → NaN,
/// numeric-строки парсятся, пустая/пробельная строка → 0, прочее → NaN.
fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => {
            if *b { 1.0 } else { 0.0 }
        }
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() { 0.0 } else { t.parse::<f64>().unwrap_or(f64::NAN) }
        }
        Some(_) => f64::NAN,
    }
}

/// `normalizeUsage` из lib/llm.mjs; None — аналог null (usage отсутствует/непригодно).
pub fn normalize_usage(raw: &Value) -> Option<Value> {
    let prompt = js_number(raw.get("prompt_tokens"));
    let completion = js_number(raw.get("completion_tokens"));
    let total = js_number(raw.get("total_tokens"));
    let usable = [prompt, completion, total].iter().any(|n| n.is_finite() && *n >= 0.0);
    if !usable {
        return None;
    }
    let field = |n: f64| if n.is_finite() && n >= 0.0 { n } else { 0.0 };
    let prompt_tokens = field(prompt);
    let completion_tokens = field(completion);
    let total_tokens = if total.is_finite() && total >= 0.0 {
        total
    } else {
        prompt_tokens + completion_tokens
    };
    Some(json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": total_tokens,
    }))
}

/// JS truthiness для JSON-значений.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0 && x.is_finite()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `content += value`: строки — как есть, скаляры — JS-строкификация, объекты — "[object Object]".
fn append_js_string(acc: &mut String, v: &Value) {
    match v {
        Value::String(s) => acc.push_str(s),
        Value::Number(_) | Value::Bool(_) => acc.push_str(&v.to_string()),
        Value::Array(_) | Value::Object(_) => acc.push_str("[object Object]"),
        Value::Null => {}
    }
}

#[derive(Debug, Default)]
struct SseState {
    content: String,
    reasoning_len: usize,
    finish: Option<Value>,
    usage: Option<Value>,
}

/// `takeLine` из readLlmSseContent: true — встречен [DONE], поток можно не дочитывать.
fn take_line(line: &str, st: &mut SseState) -> bool {
    let line = js_trim(line);
    if !line.starts_with("data:") {
        return false;
    }
    let data = js_trim(&line[5..]);
    if data == "[DONE]" {
        return true;
    }
    let j: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if let Some(d) = j.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()) {
        if let Some(delta) = d.get("delta") {
            if let Some(c) = delta.get("content") {
                if is_truthy(c) {
                    append_js_string(&mut st.content, c);
                }
            }
            // JS: reasoningLen += d.delta.reasoning.length — только строки имеют length;
            // нестроковые truthy в JS дали бы NaN (артефакт, не воспроизводим осмысленно).
            if let Some(Value::String(r)) = delta.get("reasoning") {
                st.reasoning_len += r.encode_utf16().count();
            }
            if let Some(f) = d.get("finish_reason") {
                if is_truthy(f) {
                    st.finish = Some(f.clone());
                }
            }
        }
    }
    // JS: `j.usage && typeof j.usage === 'object'` — массивы тоже object, но
    // normalizeUsage по ним даст NaN → null; в Rust is_object() строже, итог тот же.
    if let Some(u) = j.get("usage") {
        if u.is_object() {
            st.usage = Some(u.clone());
        }
    }
    false
}

#[derive(Debug)]
pub enum SseReadError {
    /// Ошибка чтения/бюджета — JS-классификация: networky (ретрай, если бюджет остался).
    Io(String),
    /// Пустой content — флаг empty (ретрая на уровне chatRequest нет, fallback выше).
    Empty { message: String },
}

/// Результат parse_sse: сырые поля до финальных проверок readLlmSseContent.
#[derive(Debug)]
pub struct RawSse {
    pub content: String,
    pub usage: Option<Value>,
}

/// `readLlmSseContent`: читает SSE-поток, собирает content/usage.
/// `budget` — проверка остатка таймаута перед каждым чтением (аналог AbortController).
pub fn parse_sse(read: impl Read, budget: &dyn Fn() -> bool) -> Result<RawSse, SseReadError> {
    let mut reader = std::io::BufReader::new(read);
    let mut st = SseState::default();
    let mut line = Vec::new();
    loop {
        if !budget() {
            return Err(SseReadError::Io("SSE: бюджет времени запроса исчерпан (таймаут)".to_string()));
        }
        line.clear();
        let n = reader.read_until(b'\n', &mut line).map_err(|e| SseReadError::Io(e.to_string()))?;
        if n == 0 {
            break; // EOF: хвост без '\n' уже обработан последним read_until
        }
        let text = String::from_utf8_lossy(&line);
        if take_line(&text, &mut st) {
            break; // [DONE] — хвост не читаем (как break в JS)
        }
    }
    let trimmed = js_trim(&st.content);
    if trimmed.is_empty() {
        let finish = match &st.finish {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "null".to_string(),
        };
        return Err(SseReadError::Empty {
            message: format!("пустой content (finish={}, reasoning={}симв)", finish, st.reasoning_len),
        });
    }
    Ok(RawSse { content: trimmed.to_string(), usage: st.usage.as_ref().and_then(normalize_usage) })
}

/// JSON.stringify(payload) из chatRequest: undefined-поля отсутствуют (Option::None).
fn build_payload(req: &ChatRequest, include_usage: bool) -> Value {
    let mut m = Map::new();
    m.insert("model".to_string(), Value::String(req.model.clone()));
    m.insert("messages".to_string(), req.messages.clone());
    if let Some(t) = req.temperature {
        m.insert("temperature".to_string(), json!(t));
    }
    m.insert("stream".to_string(), Value::Bool(true));
    if let Some(mt) = &req.max_tokens {
        m.insert("max_tokens".to_string(), mt.clone());
    }
    if let Some(effort) = &req.effort {
        m.insert("reasoning_effort".to_string(), Value::String(effort.clone()));
    }
    if let Some(seed) = &req.seed {
        m.insert("seed".to_string(), seed.clone());
    }
    if include_usage {
        m.insert("stream_options".to_string(), json!({ "include_usage": true }));
    }
    if req.json_mode {
        m.insert("response_format".to_string(), json!({ "type": "json_object" }));
    }
    Value::Object(m)
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    // redactSecretsForLog: slice(0, maxLen-1) + '…' (в JS — UTF-16 юниты).
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn retry_after_of(res: &Response) -> Option<String> {
    res.headers()
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
}

/// HTTP-клиент под бюджет `timeout_ms`: connect- и per-op таймаут (у JS — AbortController).
///
/// Отдельная функция, потому что бюджет одинаков для всех вызовов одного прогона
/// (`o.requestTimeoutMs`), и [`RealLlmClient`] держит один клиент на весь прогон —
/// пул соединений переиспользуется, как у одного `fetch`-агента в Node.
pub fn build_http_client(timeout_ms: u64) -> Result<Client, LlmError> {
    let timeout = Duration::from_millis(timeout_ms.max(1));
    Client::builder()
        .connect_timeout(timeout)
        // Per-op backstop для зависших connect/read (у JS — AbortController).
        .timeout(timeout)
        .build()
        .map_err(|e| LlmError::networky(e.to_string(), false))
}

/// `chatRequest` из lib/llm.mjs целиком (HTTP + SSE + ретраи + бюджет).
///
/// Создаёт одноразовый HTTP-клиент — форма для бинаря-моста `bin/llm.rs`, где процесс
/// живёт один запрос. Внутри прогона зовите [`chat_with`] с общим клиентом.
pub fn chat(req: &ChatRequest) -> Result<ChatOutcome, LlmError> {
    let client = build_http_client(req.timeout_ms)?;
    chat_with(&client, req)
}

/// [`chat`] на готовом HTTP-клиенте. Клиент обязан быть собран
/// [`build_http_client`] с тем же `timeout_ms`: бюджет ретраев считается здесь,
/// а per-op таймаут — на клиенте.
pub fn chat_with(client: &Client, req: &ChatRequest) -> Result<ChatOutcome, LlmError> {
    let timeout = Duration::from_millis(req.timeout_ms.max(1));
    let t0 = Instant::now();
    let budget_left = |t0: Instant| t0.elapsed() < timeout;

    let mut attempt: u32 = 0;
    let mut include_usage = req.include_usage;
    let mut throttled = false;

    loop {
        let payload = serde_json::to_string(&build_payload(req, include_usage))
            .map_err(|e| LlmError::networky(e.to_string(), throttled))?;
        let res = client
            .post(&req.url)
            .header(AUTHORIZATION, format!("Bearer {}", req.key))
            .header(CONTENT_TYPE, "application/json")
            .body(payload)
            .send();
        let res = match res {
            Ok(r) => r,
            Err(e) => {
                // Сетевой сбой (включая per-op таймаут): ретрай при остатке бюджета.
                if attempt < req.http_retries && budget_left(t0) {
                    std::thread::sleep(Duration::from_secs_f64(backoff_ms(attempt, None) / 1000.0));
                    attempt += 1;
                    continue;
                }
                return Err(LlmError::networky(e.to_string(), throttled));
            }
        };
        let status = res.status().as_u16();
        if !res.status().is_success() {
            let retry_after = retry_after_of(&res);
            let body = res.text().unwrap_or_default();
            if status == 429 {
                throttled = true;
            }
            // 400 на stream_options/include_usage → повтор без него (fail-open):
            // без расхода attempt и без проверки бюджета (как в JS).
            if status == 400
                && include_usage
                && (body.to_lowercase().contains("stream_options") || body.to_lowercase().contains("include_usage"))
            {
                include_usage = false;
                std::thread::sleep(Duration::from_secs_f64(backoff_ms(0, None) / 1000.0));
                continue;
            }
            if is_transient(status) && attempt < req.http_retries && budget_left(t0) {
                let wait = backoff_ms(attempt, retry_after.as_deref());
                std::thread::sleep(Duration::from_secs_f64(wait / 1000.0));
                attempt += 1;
                continue;
            }
            return Err(LlmError {
                error: format!("LLM {status}: {}", truncate_chars(&body, 400)),
                status: Some(status),
                empty: false,
                networky: false,
                throttled,
            });
        }
        let budget_fn = || t0.elapsed() < timeout;
        match parse_sse(res, &budget_fn) {
            Ok(raw) => {
                return Ok(ChatOutcome { content: raw.content, usage: raw.usage, throttled });
            }
            Err(SseReadError::Io(msg)) => {
                if attempt < req.http_retries && budget_left(t0) {
                    std::thread::sleep(Duration::from_secs_f64(backoff_ms(attempt, None) / 1000.0));
                    attempt += 1;
                    continue;
                }
                return Err(LlmError::networky(msg, throttled));
            }
            Err(SseReadError::Empty { message }) => {
                return Err(LlmError {
                    error: message,
                    status: None,
                    empty: true,
                    networky: false,
                    throttled,
                });
            }
        }
    }
}

// ===========================================================================
// Клиент LLM — порт createLlmClient (lib/llm.mjs:107-241)
// ===========================================================================

/// Конфигурация клиента: ровно те поля замороженного конфига, которые читает
/// `createLlmClient` (`lib/llm.mjs:108`: `const { ollama: o, moa, httpRetries } = cfg`).
///
/// Собирается из результата [`crate::config::load_review_config_parts`] через
/// [`LlmConfig::from_cfg`]; отдельная структура, а не `&Value`, потому что клиент
/// живёт весь прогон и разбирать JSON на каждый запрос незачем.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// `o.base` — без хвостового слэша; URL запроса = `{base}/chat/completions`.
    pub base: String,
    /// `o.key` — Bearer-токен провайдера.
    pub api_key: String,
    /// `o.seed`: число или отсутствует (`lib/config.mjs:242`; не-integer → дефолт).
    pub seed: Option<Value>,
    /// `o.jsonMode` (`REVIEW_JSON_MODE`, дефолт on): `response_format: json_object`
    /// в первом запросе `chat_json` и условие ветки 4 каскада.
    pub json_mode: bool,
    /// `o.temperature` (`REVIEW_TEMPERATURE`, дефолт 0.4) — дефолт `chat_json`.
    /// `None` — поле `temperature` в payload отсутствует (JS: `undefined` не сериализуется).
    pub temperature: Option<f64>,
    /// `o.requestTimeoutMs || 420_000` — бюджет одного `chat_request` целиком
    /// (все ретраи внутри) и per-op таймаут HTTP-клиента.
    pub request_timeout_ms: u64,
    /// `o.effort || 'low'` — `reasoning_effort` для `chat_text`.
    /// `chat_json` эффорт НЕ берёт отсюда: у него свой [`LlmConfig::aggregator_effort`].
    pub effort: String,
    /// `o.aggregatorEffort || 'low'` (`REVIEW_AGGREGATOR_REASONING_EFFORT`) — effort
    /// ПЕРВОЙ попытки `chat_json`. Reasoning провайдер считает completion-токенами, и это
    /// самая дорогая строка прогона; эксперименту (бэклог A6) нужен переключатель.
    /// Каскад ретраев остаётся на `none`: там effort снимают, чтобы модель договорила JSON.
    pub aggregator_effort: String,
    /// `o.providerConcurrency || 12` — жёсткий максимум глобального семафора.
    pub provider_concurrency: usize,
    /// `o.providerPerModel || 6` — жёсткий максимум семафора одной модели.
    pub provider_per_model: usize,
    /// `cfg.httpRetries` (`REVIEW_HTTP_RETRIES`, дефолт 3) — потолок ретраев транспорта.
    pub http_retries: u32,
    /// `moa.refMaxTokens` — дефолтный `max_tokens` для `chat_text`.
    pub ref_max_tokens: Option<Value>,
    /// `moa.refTemp` — дефолтная температура `chat_text`.
    pub ref_temp: Option<f64>,
}

impl Default for LlmConfig {
    /// Дефолты движка (`lib/config.mjs:236-252`), а не «пустой» объект: клиент,
    /// собранный вручную в тесте, обязан вести себя как боевой.
    fn default() -> Self {
        LlmConfig {
            base: "https://ollama.com/v1".to_string(),
            api_key: String::new(),
            seed: None,
            json_mode: true,
            temperature: Some(0.4),
            request_timeout_ms: 420_000,
            effort: "low".to_string(),
            aggregator_effort: "low".to_string(),
            provider_concurrency: 12,
            provider_per_model: 6,
            http_retries: 3,
            ref_max_tokens: Some(json!(6_000)),
            ref_temp: Some(0.3),
        }
    }
}

/// JS `value || def` для числовых полей конфига: falsy (0/NaN/null/undefined) → `def`.
fn num_or(v: Option<&Value>, def: f64) -> f64 {
    match v.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n != 0.0 => n,
        _ => def,
    }
}

/// `Math.max(1, n)` из `createAdaptiveSemaphore` (`lib/util.mjs:135`), применённый
/// заранее: отрицательный лимит в usize не выражается, а семантика та же.
fn limit_of(n: f64) -> usize {
    if n >= 1.0 {
        n as usize
    } else {
        1
    }
}

impl LlmConfig {
    /// Читает поля из замороженного конфига (`cfg.ollama`, `cfg.moa`, `cfg.httpRetries`).
    ///
    /// Отсутствующие поля дают дефолты движка, а не панику: конфиг собирается
    /// `load_review_config_parts`, но клиент должен переживать урезанный объект
    /// (так же, как JS переживает `undefined` через `||`).
    pub fn from_cfg(cfg: &Value) -> Self {
        let def = LlmConfig::default();
        let o = cfg.get("ollama");
        let moa = cfg.get("moa");
        fn get<'a>(root: Option<&'a Value>, key: &str) -> Option<&'a Value> {
            root.and_then(|v| v.get(key))
        }
        LlmConfig {
            base: get(o, "base")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(def.base),
            api_key: get(o, "key")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(def.api_key),
            // JS кладёт сюда число или null; null в payload не попадает.
            seed: get(o, "seed").filter(|v| !v.is_null()).cloned(),
            json_mode: get(o, "jsonMode").map(is_truthy).unwrap_or(def.json_mode),
            temperature: get(o, "temperature").and_then(Value::as_f64),
            request_timeout_ms: num_or(get(o, "requestTimeoutMs"), def.request_timeout_ms as f64)
                .max(1.0) as u64,
            effort: get(o, "effort")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or(def.effort),
            aggregator_effort: get(o, "aggregatorEffort")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or(def.aggregator_effort),
            provider_concurrency: limit_of(num_or(
                get(o, "providerConcurrency"),
                def.provider_concurrency as f64,
            )),
            provider_per_model: limit_of(num_or(
                get(o, "providerPerModel"),
                def.provider_per_model as f64,
            )),
            // JS: `attempt < httpRetries` — отсутствие поля дало бы 0 ретраев,
            // но конфиг движка его всегда ставит (REVIEW_HTTP_RETRIES, дефолт 3).
            http_retries: cfg
                .get("httpRetries")
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n as u32)
                .unwrap_or(def.http_retries),
            ref_max_tokens: get(moa, "refMaxTokens").filter(|v| !v.is_null()).cloned(),
            ref_temp: get(moa, "refTemp").and_then(Value::as_f64),
        }
    }
}

/// Ошибка вызова клиента — аналог JS-Error с флагами, по которым ветвится каскад
/// `chatJson` (`lib/llm.mjs:205-220`).
///
/// Склеивает два источника: транспорт ([`LlmError`] → `status`/`empty`/`networky`)
/// и разбор JSON ([`JsonRepairError`] → `truncated`/`bad_escape`). В JS это один
/// и тот же `Error` с набором ad-hoc свойств.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatError {
    pub message: String,
    /// HTTP-статус ответа провайдера (ветка 4 каскада смотрит на 400).
    pub status: Option<u16>,
    /// SSE отдал пустой `content` (ветка 1 каскада, повтор `chat_text`).
    pub empty: bool,
    /// JSON ответа оборван и не чинится (ветка 2 каскада).
    pub truncated: bool,
    /// Невалидные escape-последовательности в JSON (ветка 3 каскада).
    pub bad_escape: bool,
    /// Сетевой класс сбоя; каскад его не читает — только диагностика и логи.
    pub networky: bool,
}

impl fmt::Display for ChatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ChatError {}

impl From<LlmError> for ChatError {
    fn from(e: LlmError) -> Self {
        ChatError {
            // JS-обёртка маскирует секреты в тексте ошибки бинаря (lib/llm.mjs:163);
            // тело ответа провайдера сюда попадает уже усечённым до 400 символов.
            message: redact_secrets_for_log(&e.error, 400),
            status: e.status,
            empty: e.empty,
            truncated: false,
            bad_escape: false,
            networky: e.networky,
        }
    }
}

impl From<JsonRepairError> for ChatError {
    fn from(e: JsonRepairError) -> Self {
        ChatError {
            message: e.message,
            status: None,
            empty: false,
            truncated: e.truncated,
            bad_escape: e.bad_escape,
            networky: false,
        }
    }
}

/// Результат `chatJson`: в JS это сам распарсенный объект с двумя
/// non-enumerable полями (`lib/llm.mjs:191-196`).
///
/// Здесь они вынесены в структуру, а не подмешаны в `Value`: в JS они намеренно
/// не сериализуются (`Object.defineProperty` без `enumerable`), и подмешивание
/// их в объект протекло бы в `JSON.stringify` находок.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatJsonResult {
    /// Распарсенный ответ модели (`{summary, verdict, findings, ...}`).
    pub value: Value,
    /// `__repaired`: 'escapes' | 'truncated' — читает `lib/engine.mjs:245-246`
    /// и пишет событие `json_repair` в observability.
    pub repaired: Option<RepairedKind>,
    /// `__usage` — читает `lib/engine.mjs:252` и кладёт на результат чанка.
    pub usage: Option<Value>,
}

/// Опции `chatJson` (`lib/llm.mjs:200`). `None` в поле = аргумент опущен в JS.
#[derive(Debug, Clone, Default)]
pub struct ChatJsonOpts {
    /// Дефолт — `o.temperature`.
    pub temperature: Option<f64>,
    /// Дефолт — `"review_json"`; ключ разбивки стоимости по вызовам.
    pub call_type: Option<String>,
}

/// Колбэк `onUsage` у `chatText`: prose-вызов возвращает строку (контракт, который
/// подменяют DI-фейки), поэтому токены уходят боковым каналом.
#[derive(Clone)]
pub struct UsageCallback(pub std::sync::Arc<dyn Fn(&Value) + Send + Sync>);

impl std::fmt::Debug for UsageCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UsageCallback(..)")
    }
}

/// Опции `chatText` (`lib/llm.mjs:239`). `None` в поле = аргумент опущен в JS.
#[derive(Debug, Clone, Default)]
pub struct ChatTextOpts {
    /// Дефолт — `moa.refMaxTokens`.
    pub max_tokens: Option<Value>,
    /// Дефолт — `moa.refTemp`.
    pub temperature: Option<f64>,
    /// Дефолт — `"advisor"`.
    pub call_type: Option<String>,
    /// `onUsage`: зовётся с usage успешного запроса (если провайдер его вернул).
    /// Без него специалисты — единственная роль, чей расход виден только как
    /// `text_chars`, и атрибуция «токены на роль» невозможна.
    pub on_usage: Option<UsageCallback>,
}

/// Клиент LLM глазами движка — то, что `pipeline.mjs:231` кладёт в `deps.ollama`.
///
/// Отдельный трейт (а не только [`RealLlmClient`]), потому что весь набор тестов
/// движка в JS подменяет именно этот объект: `{ chatJson, chatText }`
/// (`test/pr-review.engine.test.mjs:29,42,56,109,347,413`). Порт engine (T12)
/// принимает `&dyn LlmApi` и получает те же точки подмены без сети.
///
/// Порядок аргументов — как в JS (`messages` первым, `model` вторым): сигнатуры
/// движка и verifier читаются один в один с `lib/engine.mjs`/`lib/verifier.mjs`.
pub trait LlmApi: Send + Sync {
    /// `chatJson` (`lib/llm.mjs:200-223`) — ответ модели, разобранный в JSON,
    /// с каскадом повторов на пустом/оборванном/битом ответе.
    fn chat_json(
        &self,
        messages: &Value,
        model: &str,
        opts: &ChatJsonOpts,
    ) -> Result<ChatJsonResult, ChatError>;

    /// `chatText` (`lib/llm.mjs:225-241`) — прозаический ответ advisor'а
    /// с одним повтором на `effort=none`, если content пуст.
    fn chat_text(
        &self,
        messages: &Value,
        model: &str,
        opts: &ChatTextOpts,
    ) -> Result<String, ChatError>;
}

/// Колбэк учёта стоимости — порт `costTracker.record(usage, model, callType)`
/// (`lib/cost.mjs:46`). В JS он опционален (`createLlmClient(cfg, log, { costTracker })`).
pub type UsageSink = Box<dyn Fn(&str, &str, &Value) + Send + Sync>;

/// Логгер прогона — тот же контракт, что `deps.log` (`traits.rs`, `pipeline.mjs:227`).
pub type LogSink = Box<dyn Fn(&str) + Send + Sync>;

/// Боевой клиент LLM: транспорт [`chat_with`] под двумя адаптивными семафорами.
///
/// Устройство один в один с `createLlmClient`:
/// - глобальный семафор на `providerConcurrency` (`lib/llm.mjs:110`);
/// - ленивая карта семафоров по модели на `providerPerModel` (`lib/llm.mjs:112-115`);
/// - 429 режет **оба** (`throttleFor`, `lib/llm.mjs:122-128`) — иначе одна
///   залимиченная модель продолжала бы выедать глобальный пул.
///
/// Отличие от JS одно и техническое: слоты возвращает `Drop` у `Permit`, а не
/// `finally { m.release(); g.release() }`. Порядок освобождения (сначала модельный,
/// потом глобальный) сохранён порядком объявления переменных.
pub struct RealLlmClient {
    cfg: LlmConfig,
    global_sem: AdaptiveSemaphore,
    model_sems: Mutex<HashMap<String, Arc<AdaptiveSemaphore>>>,
    http: Client,
    on_usage: Option<UsageSink>,
    log: Option<LogSink>,
}

impl fmt::Debug for RealLlmClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealLlmClient")
            .field("cfg", &self.cfg)
            .field("global", &self.global_sem.stats())
            .finish_non_exhaustive()
    }
}

/// `/Bad escaped|Unexpected (?:token|end).*JSON/i` — ветка 3 каскада
/// (`lib/llm.mjs:213`). Это подстраховка под тексты V8: Rust-порт `json_repair`
/// отдаёт фиксированные русские сообщения и попадает в ветку по флагу `bad_escape`.
fn re_bad_escape_message() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)Bad escaped|Unexpected (?:token|end).*JSON").unwrap())
}

/// `/response_format/i` — ветка 4 каскада (`lib/llm.mjs:217`).
fn re_response_format() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)response_format").unwrap())
}

impl RealLlmClient {
    /// Собирает клиента и HTTP-пул под бюджет `cfg.request_timeout_ms`.
    pub fn new(cfg: LlmConfig) -> Result<Self, LlmError> {
        let http = build_http_client(cfg.request_timeout_ms)?;
        Ok(RealLlmClient {
            global_sem: AdaptiveSemaphore::new(cfg.provider_concurrency),
            model_sems: Mutex::new(HashMap::new()),
            cfg,
            http,
            on_usage: None,
            log: None,
        })
    }

    /// Учёт usage — аналог `{ costTracker }` третьим аргументом `createLlmClient`.
    /// Вызывается только на успешном ответе и только при непустом usage
    /// (`recordUsage`, `lib/llm.mjs:117-120`).
    pub fn with_usage_sink(mut self, sink: UsageSink) -> Self {
        self.on_usage = Some(sink);
        self
    }

    /// Логгер — второй аргумент `createLlmClient`.
    pub fn with_log(mut self, log: LogSink) -> Self {
        self.log = Some(log);
        self
    }

    /// Снимок глобального семафора — то же, что уходит в лог при 429.
    pub fn global_stats(&self) -> SemaphoreStats {
        self.global_sem.stats()
    }

    /// Снимок семафора модели; `None` — к модели ещё не обращались
    /// (в JS карта тоже наполняется лениво).
    pub fn model_stats(&self, model: &str) -> Option<SemaphoreStats> {
        lock_sems(&self.model_sems).get(model).map(|s| s.stats())
    }

    fn log(&self, message: &str) {
        if let Some(log) = &self.log {
            log(message);
        }
    }

    /// `semFor` (`lib/llm.mjs:112-115`): семафор модели, создаётся при первом обращении.
    fn sem_for(&self, model: &str) -> Arc<AdaptiveSemaphore> {
        let mut map = lock_sems(&self.model_sems);
        if let Some(sem) = map.get(model) {
            return Arc::clone(sem);
        }
        let sem = Arc::new(AdaptiveSemaphore::new(self.cfg.provider_per_model));
        map.insert(model.to_string(), Arc::clone(&sem));
        sem
    }

    /// `throttleFor` (`lib/llm.mjs:122-128`): 429 режет оба семафора и пишет лог.
    fn throttle_for(&self, model: &str) {
        let m = self.sem_for(model);
        self.global_sem.on_429();
        m.on_429();
        self.log(&format!(
            "[{model}] 429 → снижение параллелизма (global: {}, model: {})",
            stats_json(&self.global_sem.stats()),
            stats_json(&m.stats()),
        ));
    }

    /// `chatRequest` (`lib/llm.mjs:130-176`): захват обоих семафоров, транспорт,
    /// троттлинг на 429, учёт usage. Слоты держатся на всё время запроса.
    #[allow(clippy::too_many_arguments)]
    fn chat_request(
        &self,
        messages: &Value,
        model: &str,
        max_tokens: Option<Value>,
        temperature: Option<f64>,
        effort: Option<&str>,
        json_mode: bool,
        call_type: &str,
    ) -> Result<ChatOutcome, ChatError> {
        let req = ChatRequest {
            url: format!("{}/chat/completions", self.cfg.base),
            key: self.cfg.api_key.clone(),
            model: model.to_string(),
            messages: messages.clone(),
            max_tokens,
            temperature,
            effort: effort.map(str::to_string),
            json_mode,
            seed: self.cfg.seed.clone(),
            http_retries: self.cfg.http_retries,
            timeout_ms: self.cfg.request_timeout_ms,
            // JS всегда просит usage и снимает stream_options только на 400.
            include_usage: true,
        };

        // Порядок как в JS: сперва глобальный слот, затем модельный; освобождение
        // (Drop в обратном порядке объявления) — сперва модельный, затем глобальный.
        let model_sem = self.sem_for(model);
        let _global_permit = self.global_sem.acquire();
        let _model_permit = model_sem.acquire();

        match chat_with(&self.http, &req) {
            Ok(out) => {
                if out.throttled {
                    self.throttle_for(model);
                }
                if let Some(usage) = &out.usage {
                    if let Some(sink) = &self.on_usage {
                        sink(model, call_type, usage);
                    }
                }
                Ok(out)
            }
            Err(e) => {
                if e.throttled {
                    self.throttle_for(model);
                }
                Err(ChatError::from(e))
            }
        }
    }

    /// `chatJsonOnce` (`lib/llm.mjs:178-198`): один запрос + толерантный разбор JSON.
    #[allow(clippy::too_many_arguments)]
    fn chat_json_once(
        &self,
        messages: &Value,
        model: &str,
        max_tokens: u32,
        effort: &str,
        json_mode: bool,
        temperature: Option<f64>,
        call_type: &str,
    ) -> Result<ChatJsonResult, ChatError> {
        let out = self.chat_request(
            messages,
            model,
            Some(json!(max_tokens)),
            temperature,
            Some(effort),
            json_mode,
            call_type,
        )?;
        let parsed = extract_json_object(&out.content)?;
        if let Some(kind) = parsed.repaired {
            self.log(match kind {
                RepairedKind::Escapes => {
                    "JSON: исправлены невалидные escape-последовательности в ответе модели"
                }
                RepairedKind::Truncated => {
                    "JSON восстановлен из оборванного ответа модели (усечение)"
                }
            });
        }
        Ok(ChatJsonResult {
            value: parsed.value,
            repaired: parsed.repaired,
            usage: out.usage,
        })
    }
}

/// `JSON.stringify(sem.stats())` из строки лога `throttleFor` (`lib/llm.mjs:127`).
fn stats_json(stats: &SemaphoreStats) -> String {
    serde_json::to_string(stats).unwrap_or_else(|_| "{}".to_string())
}

/// Блокировка карты семафоров, игнорирующая poisoning (как в `concurrency.rs`):
/// паника одного запроса не должна ронять весь клиент.
fn lock_sems(
    m: &Mutex<HashMap<String, Arc<AdaptiveSemaphore>>>,
) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AdaptiveSemaphore>>> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl LlmApi for RealLlmClient {
    /// Каскад `chatJson` (`lib/llm.mjs:200-223`) — порядок веток значим, он же
    /// приоритет диагнозов: пустой ответ → усечение → битые escape → JSON-mode.
    ///
    /// Первый запрос: `max_tokens=24000`, `effort=low`, `json_mode=o.jsonMode`.
    /// Повтор ровно один — вложенных каскадов в JS нет (`chatJsonOnce` зовётся
    /// напрямую, вне `try`).
    fn chat_json(
        &self,
        messages: &Value,
        model: &str,
        opts: &ChatJsonOpts,
    ) -> Result<ChatJsonResult, ChatError> {
        let jm = self.cfg.json_mode;
        let temperature = opts.temperature.or(self.cfg.temperature);
        let call_type = opts.call_type.as_deref().unwrap_or("review_json");
        // JS: `o.aggregatorEffort || 'low'`.
        let first_effort = if self.cfg.aggregator_effort.is_empty() {
            "low"
        } else {
            self.cfg.aggregator_effort.as_str()
        };

        let e = match self.chat_json_once(
            messages,
            model,
            24_000,
            first_effort,
            jm,
            temperature,
            call_type,
        ) {
            Ok(ok) => return Ok(ok),
            Err(e) => e,
        };

        // Ветка 1 (lib/llm.mjs:205-208): пустой content — модель ушла в reasoning.
        if e.empty {
            self.log(&format!(
                "[{model}] content пуст на effort={first_effort} → повтор effort=none: {}",
                e.message
            ));
            return self
                .chat_json_once(messages, model, 24_000, "none", jm, temperature, call_type);
        }
        // Ветка 2 (lib/llm.mjs:209-212): JSON оборван — больше токенов.
        if e.truncated {
            self.log(&format!(
                "[{model}] JSON оборван → повтор effort=none + больше токенов: {}",
                e.message
            ));
            return self
                .chat_json_once(messages, model, 32_000, "none", jm, temperature, call_type);
        }
        // Ветка 3 (lib/llm.mjs:213-216): битые escape — снимаем ещё и JSON-mode.
        if e.bad_escape || re_bad_escape_message().is_match(&e.message) {
            self.log(&format!(
                "[{model}] битые escape → повтор effort=none + больше токенов (без JSON-mode): {}",
                e.message
            ));
            return self
                .chat_json_once(messages, model, 32_000, "none", false, temperature, call_type);
        }
        // Ветка 4 (lib/llm.mjs:217-220): провайдер не понял response_format.
        // Условие `jm &&` обязательно: без JSON-mode повторять нечего, и 400 по
        // любой другой причине превратился бы в лишний запрос.
        if jm && (e.status == Some(400) || re_response_format().is_match(&e.message)) {
            self.log(&format!(
                "[{model}] ошибка, возможно из-за JSON-mode → повтор без него: {}",
                e.message
            ));
            return self
                .chat_json_once(messages, model, 24_000, "low", false, temperature, call_type);
        }
        Err(e)
    }

    /// `chatText` (`lib/llm.mjs:225-241`): один повтор на `effort=none`, только
    /// если content пуст. Любая другая ошибка уходит наверх без повтора.
    fn chat_text(
        &self,
        messages: &Value,
        model: &str,
        opts: &ChatTextOpts,
    ) -> Result<String, ChatError> {
        let max_tokens = opts
            .max_tokens
            .clone()
            .or_else(|| self.cfg.ref_max_tokens.clone());
        let temperature = opts.temperature.or(self.cfg.ref_temp);
        let call_type = opts.call_type.as_deref().unwrap_or("advisor");
        // JS: `effort: o.effort || 'low'`.
        let effort = if self.cfg.effort.is_empty() {
            "low"
        } else {
            self.cfg.effort.as_str()
        };

        let report = |usage: &Option<Value>| {
            if let (Some(u), Some(cb)) = (usage, &opts.on_usage) {
                (cb.0)(u);
            }
        };
        match self.chat_request(
            messages,
            model,
            max_tokens.clone(),
            temperature,
            Some(effort),
            false,
            call_type,
        ) {
            Ok(out) => {
                report(&out.usage);
                Ok(out.content)
            }
            Err(e) if e.empty => {
                self.log(&format!(
                    "[{model}] advisor content пуст на effort=low → effort=none"
                ));
                self.chat_request(
                    messages,
                    model,
                    max_tokens,
                    temperature,
                    Some("none"),
                    false,
                    call_type,
                )
                .map(|out| {
                    report(&out.usage);
                    out.content
                })
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests;
