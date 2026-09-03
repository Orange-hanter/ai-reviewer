//! Порт `chatRequest` из lib/llm.mjs (Фаза 3, шаг 1): OpenAI-compatible
//! `/chat/completions` — HTTP, ручной SSE-стриминг, ретраи с backoff,
//! бюджет таймаута, fail-open по `stream_options/include_usage` на 400.
//!
//! Что намеренно НЕ перенесено сюда (и почему):
//! - Адаптивные семафоры (AIMD-троттлинг на 429): бинарь — одноразовый процесс
//!   на запрос, вызываемый из JS-обёртки через execFile; внутрипроцессные семафоры
//!   не имели бы сквозного состояния между запросами. Семафоры живут в JS-обёртке,
//!   бинарь сообщает о 429 флагом `throttled`;
//! - JSON-mode graceful fallback (effort=none, больше токенов, снятие
//!   response_format): живёт в JS-обёртке поверх `extractJsonObject`
//!   (Rust-модуль json_repair через lib/json.mjs, переиспользуется как есть).
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

use std::io::{BufRead, Read};
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::{json, Map, Value};

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

/// `chatRequest` из lib/llm.mjs целиком (HTTP + SSE + ретраи + бюджет).
pub fn chat(req: &ChatRequest) -> Result<ChatOutcome, LlmError> {
    let timeout = Duration::from_millis(req.timeout_ms.max(1));
    let t0 = Instant::now();
    let budget_left = |t0: Instant| t0.elapsed() < timeout;
    let client = Client::builder()
        .connect_timeout(timeout)
        // Per-op backstop для зависших connect/read (у JS — AbortController).
        .timeout(timeout)
        .build()
        .map_err(|e| LlmError::networky(e.to_string(), false))?;

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

#[cfg(test)]
mod tests;
