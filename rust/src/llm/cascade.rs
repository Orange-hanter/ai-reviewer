//! Каскад ретраев поверх [`chat`](super::chat): `chatJson` / `chatText` из lib/llm.mjs.
//!
//! До 0.6 каскад жил только в JS-обёртке, бинарь делал один запрос. Теперь это
//! источник истины: JS-копия каскада в lib/llm.mjs — deprecated fallback на случай,
//! когда бинаря нет, и новых правил не получает (proposals/20-rust-primary.md).
//!
//! Правила первой ступени — те же, что в JS (порядок важен, срабатывает первое):
//! - json: пустой content → effort=none, 24000; оборванный JSON → none, 32000;
//!   битые escape → none, 32000, без JSON-mode; 400 при JSON-mode → low, 24000, без него;
//! - prose: пустой content → effort=none, тот же max_tokens.
//!
//! Новое правило (docs/probe-openrouter-2026-10-14.md, п. 1): часть моделей на OpenRouter
//! (gemini-3.8-flash, glm-5.3-flash) отвечает на effort=none `HTTP 400: Reasoning is
//! mandatory for this endpoint and cannot be disabled`. Ступень `none` для них —
//! гарантированный отказ, а JS-каскад отдавал его наверх как сбой всего вызова. Здесь
//! такой 400 после попытки на `none` даёт ещё одну: effort=low и max_tokens + 8000 —
//! запас на рассуждение, которое модель всё равно потратит. Правило проверяется ПЕРЕД
//! веткой «400 при JSON-mode»: иначе тот же 400 снял бы JSON-mode, не вернув reasoning.
//!
//! Предел — три попытки: обычные правила срабатывают только на первой, правило
//! mandatory-reasoning — только после `none` и всегда уводит на `low`, поэтому
//! зациклиться каскад не может.

use serde::Deserialize;
use serde_json::{json, Value};

use super::{ChatOutcome, ChatRequest, LlmError};
use crate::json_repair::{extract_json_object, JsonRepairError};

/// Добавка к max_tokens при повторе после «Reasoning is mandatory».
pub const MANDATORY_REASONING_EXTRA_TOKENS: u64 = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CascadeKind {
    /// Ответ — JSON-объект (агрегатор, verifier): extractJsonObject поверх content.
    Json,
    /// Ответ — текст (advisors, специалисты).
    Prose,
}

/// Параметры одной попытки — то, что каскад меняет между ними.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    pub effort: Option<String>,
    pub max_tokens: Option<u64>,
    pub json_mode: bool,
}

/// Чем закончилась попытка, если не успехом.
#[derive(Debug, Clone)]
pub enum Failure {
    Llm(LlmError),
    Json(JsonRepairError),
}

/// 400 от провайдера, у которого reasoning нельзя выключить.
pub fn is_mandatory_reasoning(e: &LlmError) -> bool {
    if e.status != Some(400) {
        return false;
    }
    let m = e.error.to_lowercase();
    m.contains("reasoning is mandatory") || m.contains("cannot be disabled")
}

fn attempt(effort: &str, max_tokens: Option<u64>, json_mode: bool) -> Attempt {
    Attempt { effort: Some(effort.to_string()), max_tokens, json_mode }
}

/// Следующая попытка каскада и строка лога для неё; `None` — сдаёмся.
/// `step` — номер попытки, которая только что упала (0 — первая).
pub fn next_attempt(kind: CascadeKind, step: usize, prev: &Attempt, failure: &Failure) -> Option<(Attempt, String)> {
    if let Failure::Llm(e) = failure {
        if e.quota {
            // Квота кончилась у провайдера, а не у попытки: другой effort её не вернёт.
            return None;
        }
        if prev.effort.as_deref() == Some("none") && is_mandatory_reasoning(e) {
            let max_tokens = prev.max_tokens.map(|n| n + MANDATORY_REASONING_EXTRA_TOKENS);
            return Some((
                attempt("low", max_tokens, prev.json_mode),
                format!(
                    "reasoning обязателен у модели (400 на effort=none) → повтор effort=low, max_tokens={}",
                    max_tokens.map_or("—".to_string(), |n| n.to_string()),
                ),
            ));
        }
    }
    if step > 0 {
        return None;
    }
    let first_effort = prev.effort.as_deref().unwrap_or("low");
    // Модель, настроенная сразу на effort=none (LLM_MODEL_SETTINGS), получила бы «повтор»
    // с теми же параметрами — тот же счёт за тот же ответ. Такой повтор не делаем.
    first_step_rule(kind, prev, failure, first_effort).filter(|(next, _)| next != prev)
}

fn first_step_rule(kind: CascadeKind, prev: &Attempt, failure: &Failure, first_effort: &str) -> Option<(Attempt, String)> {
    match (kind, failure) {
        // Пустой ответ на low — почти всегда рассуждение съело весь max_tokens (finish=length).
        // Больший бюджет не помогает: замер 2026-10-16 на чанке 49 КБ — deepseek-v4.1-flash
        // съедает и 6000, и 14000 (RFC 20, § 5). Остаётся effort=none; причина — в логе.
        (CascadeKind::Prose, Failure::Llm(e)) if e.empty => Some((
            attempt("none", prev.max_tokens, false),
            format!("advisor content пуст на effort={first_effort} ({}) → effort=none", e.error),
        )),
        (CascadeKind::Prose, _) => None,
        (CascadeKind::Json, Failure::Llm(e)) if e.empty => Some((
            attempt("none", Some(24000), prev.json_mode),
            format!("content пуст на effort={first_effort} → повтор effort=none: {}", e.error),
        )),
        (CascadeKind::Json, Failure::Json(e)) if e.truncated => Some((
            attempt("none", Some(32000), prev.json_mode),
            format!("JSON оборван → повтор effort=none + больше токенов: {}", e.message),
        )),
        (CascadeKind::Json, Failure::Json(e)) if e.bad_escape => Some((
            attempt("none", Some(32000), false),
            format!("битые escape → повтор effort=none + больше токенов (без JSON-mode): {}", e.message),
        )),
        (CascadeKind::Json, Failure::Llm(e))
            if prev.json_mode && (e.status == Some(400) || e.error.to_lowercase().contains("response_format")) =>
        {
            Some((
                attempt("low", Some(24000), false),
                format!("ошибка, возможно из-за JSON-mode → повтор без него: {}", e.error),
            ))
        }
        _ => None,
    }
}

/// Вход бинаря в режиме каскада: обычный ChatRequest (параметры ПЕРВОЙ попытки) + вид ответа.
#[derive(Debug, Clone, Deserialize)]
pub struct CascadeRequest {
    #[serde(flatten)]
    pub base: ChatRequest,
    pub cascade: CascadeKind,
}

/// Итог каскада. `usages` — usage каждого дошедшего ответа, включая те, чей JSON не
/// разобрался: провайдер их тоже выставил в счёт, и JS-копия учитывала их так же.
#[derive(Debug, Clone)]
pub struct CascadeOutcome {
    pub result: Result<Value, Failure>,
    pub usages: Vec<Value>,
    pub throttled: bool,
    pub log: Vec<String>,
}

impl CascadeOutcome {
    /// JSON-контракт бинаря. Успех json: `{value, repaired, usage, …}`, prose: `{content, usage, …}`;
    /// сбой — флаги последней ошибки (как у одиночного запроса) плюс truncated/badEscape.
    pub fn to_json(&self, kind: CascadeKind) -> (bool, Value) {
        let usage = self.usages.last().cloned().unwrap_or(Value::Null);
        match &self.result {
            Ok(v) => {
                let mut out = json!({
                    "usage": usage,
                    "usages": self.usages,
                    "throttled": self.throttled,
                    "log": self.log,
                });
                let o = out.as_object_mut().expect("объект");
                match kind {
                    CascadeKind::Json => {
                        o.insert("value".into(), v.get("value").cloned().unwrap_or(Value::Null));
                        o.insert("repaired".into(), v.get("repaired").cloned().unwrap_or(Value::Null));
                    }
                    CascadeKind::Prose => {
                        o.insert("content".into(), v.clone());
                    }
                }
                (true, out)
            }
            Err(f) => {
                let (error, status, empty, networky, truncated, bad_escape, quota) = match f {
                    Failure::Llm(e) => (e.error.clone(), e.status, e.empty, e.networky, false, false, e.quota),
                    Failure::Json(e) => (e.message.clone(), None, false, false, e.truncated, e.bad_escape, false),
                };
                (
                    false,
                    json!({
                        "error": error,
                        "status": status,
                        "empty": empty,
                        "networky": networky,
                        "truncated": truncated,
                        "badEscape": bad_escape,
                        "quota": quota,
                        "throttled": self.throttled,
                        "usages": self.usages,
                        "log": self.log,
                    }),
                )
            }
        }
    }
}

fn request_for(base: &ChatRequest, a: &Attempt) -> ChatRequest {
    let mut r = base.clone();
    r.effort = a.effort.clone();
    r.max_tokens = a.max_tokens.map(|n| json!(n));
    r.json_mode = a.json_mode;
    r
}

fn repaired_log(kind: &str) -> String {
    if kind == "escapes" {
        "JSON: исправлены невалидные escape-последовательности в ответе модели".to_string()
    } else {
        "JSON восстановлен из оборванного ответа модели (усечение)".to_string()
    }
}

/// Каскад с подменяемым транспортом: в бинаре это [`super::chat`], в тестах — сценарий.
pub fn run_cascade_with(
    req: &CascadeRequest,
    chat: &mut dyn FnMut(&ChatRequest) -> Result<ChatOutcome, LlmError>,
) -> CascadeOutcome {
    let mut cur = Attempt {
        effort: req.base.effort.clone(),
        max_tokens: req.base.max_tokens.as_ref().and_then(Value::as_u64),
        json_mode: req.base.json_mode,
    };
    let mut usages = Vec::new();
    let mut throttled = false;
    let mut log = Vec::new();
    let mut step = 0;
    loop {
        let outcome = match chat(&request_for(&req.base, &cur)) {
            Ok(out) => {
                throttled |= out.throttled;
                if let Some(u) = out.usage.clone() {
                    usages.push(u);
                }
                match req.cascade {
                    CascadeKind::Prose => Ok(Value::String(out.content)),
                    CascadeKind::Json => match extract_json_object(&out.content) {
                        Ok(x) => {
                            let repaired = x.repaired.map(|k| k.as_str());
                            if let Some(k) = repaired {
                                log.push(repaired_log(k));
                            }
                            Ok(json!({ "value": x.value, "repaired": repaired }))
                        }
                        Err(e) => Err(Failure::Json(e)),
                    },
                }
            }
            Err(e) => {
                throttled |= e.throttled;
                // Пустой ответ оплачен так же, как полный: рассуждение — completion-токены.
                if let Some(u) = e.usage.clone() {
                    usages.push(u);
                }
                Err(Failure::Llm(e))
            }
        };
        let failure = match outcome {
            Ok(v) => return CascadeOutcome { result: Ok(v), usages, throttled, log },
            Err(f) => f,
        };
        match next_attempt(req.cascade, step, &cur, &failure) {
            Some((next, line)) => {
                log.push(line);
                cur = next;
                step += 1;
            }
            None => return CascadeOutcome { result: Err(failure), usages, throttled, log },
        }
    }
}

/// Каскад поверх настоящего HTTP-транспорта.
pub fn run_cascade(req: &CascadeRequest) -> CascadeOutcome {
    run_cascade_with(req, &mut |r| super::chat(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(kind: CascadeKind, effort: &str, max_tokens: u64, json_mode: bool) -> CascadeRequest {
        CascadeRequest {
            base: ChatRequest {
                url: "http://x".into(),
                key: "k".into(),
                model: "m".into(),
                messages: json!([]),
                max_tokens: Some(json!(max_tokens)),
                temperature: None,
                effort: Some(effort.into()),
                json_mode,
                seed: None,
                http_retries: 0,
                timeout_ms: 1000,
                include_usage: true,
                provider: None,
            },
            cascade: kind,
        }
    }

    fn ok(content: &str) -> Result<ChatOutcome, LlmError> {
        Ok(ChatOutcome { content: content.into(), usage: Some(json!({"total_tokens": 1})), throttled: false })
    }

    fn err(status: Option<u16>, msg: &str, empty: bool) -> Result<ChatOutcome, LlmError> {
        Err(LlmError { error: msg.into(), status, empty, networky: false, throttled: false, quota: false, usage: None })
    }

    const MANDATORY: &str =
        r#"LLM 400: {"error":{"message":"Reasoning is mandatory for this endpoint and cannot be disabled."}}"#;

    /// Прогоняет каскад по сценарию ответов; возвращает итог и параметры каждой попытки.
    fn run(req: &CascadeRequest, script: Vec<Result<ChatOutcome, LlmError>>) -> (CascadeOutcome, Vec<Attempt>) {
        let mut script = script.into_iter();
        let mut seen = Vec::new();
        let out = run_cascade_with(req, &mut |r| {
            seen.push(Attempt {
                effort: r.effort.clone(),
                max_tokens: r.max_tokens.as_ref().and_then(Value::as_u64),
                json_mode: r.json_mode,
            });
            script.next().expect("каскад сделал попытку сверх сценария")
        });
        (out, seen)
    }

    #[test]
    fn json_пустой_затем_mandatory_reasoning_уходит_на_low_с_запасом_токенов() {
        let req = base(CascadeKind::Json, "low", 24000, true);
        let (out, seen) = run(&req, vec![err(None, "пустой content", true), err(Some(400), MANDATORY, false), ok(r#"{"findings":[]}"#)]);
        assert_eq!(out.result.unwrap()["value"], json!({"findings": []}));
        assert_eq!(seen[1], attempt("none", Some(24000), true));
        assert_eq!(seen[2], attempt("low", Some(32000), true), "JSON-mode сохраняется, +8000 токенов");
        assert_eq!(out.log.len(), 2);
    }

    #[test]
    fn json_оборванный_хвост_чинится_без_второй_попытки() {
        let req = base(CascadeKind::Json, "low", 24000, true);
        let (out, seen) = run(&req, vec![ok(r#"{"findings":[{"a":"x""#)]);
        assert_eq!(out.result.unwrap()["repaired"], json!("truncated"));
        assert_eq!(seen.len(), 1);
        assert_eq!(out.usages.len(), 1);
    }

    #[test]
    fn json_неремонтируемый_обрыв_затем_mandatory_reasoning() {
        let req = base(CascadeKind::Json, "low", 24000, true);
        let (out, seen) = run(&req, vec![ok(r#"{"findings":[{"a":"#), err(Some(400), MANDATORY, false), ok("{}")]);
        assert!(out.result.is_ok());
        assert_eq!(seen[1], attempt("none", Some(32000), true));
        assert_eq!(seen[2], attempt("low", Some(40000), true));
        assert_eq!(out.usages.len(), 2, "оборванный ответ тоже оплачен");
    }

    #[test]
    fn json_битые_escape_затем_mandatory_reasoning_без_json_mode() {
        let failure = Failure::Json(JsonRepairError { message: "esc".into(), bad_escape: true, truncated: false });
        let (next, _) = next_attempt(CascadeKind::Json, 0, &attempt("low", Some(24000), true), &failure).unwrap();
        assert_eq!(next, attempt("none", Some(32000), false));
        let mandatory = Failure::Llm(LlmError { error: MANDATORY.into(), status: Some(400), empty: false, networky: false, throttled: false, quota: false, usage: None });
        let (third, _) = next_attempt(CascadeKind::Json, 1, &next, &mandatory).unwrap();
        assert_eq!(third, attempt("low", Some(40000), false));
    }

    #[test]
    fn json_первая_попытка_на_none_и_mandatory_не_снимает_json_mode() {
        // REVIEW_AGGREGATOR_REASONING_EFFORT=none: без нового правила этот 400 ушёл бы в
        // ветку «400 при JSON-mode» и снял бы JSON-mode, оставив reasoning выключенным.
        let req = base(CascadeKind::Json, "none", 24000, true);
        let (out, seen) = run(&req, vec![err(Some(400), MANDATORY, false), ok("{}")]);
        assert!(out.result.is_ok());
        assert_eq!(seen[1], attempt("low", Some(32000), true));
    }

    #[test]
    fn json_400_не_про_reasoning_снимает_json_mode_как_раньше() {
        let req = base(CascadeKind::Json, "low", 24000, true);
        let (out, seen) = run(&req, vec![err(Some(400), "LLM 400: response_format unsupported", false), ok("{}")]);
        assert!(out.result.is_ok());
        assert_eq!(seen[1], attempt("low", Some(24000), false));
    }

    #[test]
    fn json_вторая_ступень_не_ретраится_по_обычным_правилам() {
        let req = base(CascadeKind::Json, "low", 24000, true);
        let (out, seen) = run(&req, vec![err(None, "пусто", true), err(None, "пусто", true)]);
        assert_eq!(seen.len(), 2);
        match out.result {
            Err(Failure::Llm(e)) => assert!(e.empty),
            other => panic!("ожидался сбой: {other:?}"),
        }
    }

    #[test]
    fn mandatory_на_low_не_ретраится() {
        // 400 «mandatory» на low не про effort — повторять тот же запрос бессмысленно.
        let req = base(CascadeKind::Prose, "low", 6000, false);
        let (out, seen) = run(&req, vec![err(Some(400), MANDATORY, false)]);
        assert_eq!(seen.len(), 1);
        assert!(out.result.is_err());
    }

    #[test]
    fn prose_пустой_затем_mandatory_reasoning() {
        let req = base(CascadeKind::Prose, "low", 6000, false);
        let (out, seen) = run(&req, vec![err(None, "пусто", true), err(Some(400), MANDATORY, false), ok("текст")]);
        assert_eq!(out.result.unwrap(), json!("текст"));
        assert_eq!(seen[1], attempt("none", Some(6000), false));
        assert_eq!(seen[2], attempt("low", Some(14000), false));
    }

    #[test]
    fn json_нет_json_в_ответе_это_сбой_без_ретрая_но_usage_учтён() {
        let req = base(CascadeKind::Json, "low", 24000, true);
        let (out, seen) = run(&req, vec![ok("просто текст")]);
        assert_eq!(seen.len(), 1);
        assert_eq!(out.usages.len(), 1, "провайдер выставил счёт и за неразобранный ответ");
        let (ok_flag, payload) = out.to_json(CascadeKind::Json);
        assert!(!ok_flag);
        assert_eq!(payload["truncated"], json!(false));
        assert_eq!(payload["badEscape"], json!(false));
    }

    #[test]
    fn контракт_успеха_json() {
        let req = base(CascadeKind::Json, "low", 24000, true);
        let (out, _) = run(&req, vec![ok("```json\n{\"a\":1}\n```")]);
        let (ok_flag, payload) = out.to_json(CascadeKind::Json);
        assert!(ok_flag);
        assert_eq!(payload["value"], json!({"a": 1}));
        assert_eq!(payload["repaired"], Value::Null);
        assert_eq!(payload["usage"], json!({"total_tokens": 1}));
    }

    #[test]
    fn квота_не_ретраится_ни_одним_правилом() {
        let req = base(CascadeKind::Prose, "none", 6000, false);
        let quota = || -> Result<ChatOutcome, LlmError> {
            Err(LlmError { error: "LLM 429 (квота провайдера исчерпана): weekly usage limit".into(), status: Some(429), empty: false, networky: false, throttled: false, quota: true, usage: None })
        };
        let (out, seen) = run(&req, vec![quota()]);
        assert_eq!(seen.len(), 1);
        let (ok_flag, payload) = out.to_json(CascadeKind::Prose);
        assert!(!ok_flag);
        assert_eq!(payload["quota"], json!(true));
    }

    #[test]
    fn пустой_ответ_оплачен_и_его_usage_учтён() {
        let req = base(CascadeKind::Prose, "low", 6000, false);
        let empty_paid = || -> Result<ChatOutcome, LlmError> {
            Err(LlmError {
                error: "пустой content (finish=length, reasoning=24000симв)".into(),
                status: None, empty: true, networky: false, throttled: false, quota: false,
                usage: Some(json!({"prompt_tokens": 16000, "completion_tokens": 6000, "total_tokens": 22000})),
            })
        };
        let (out, _) = run(&req, vec![empty_paid(), ok("текст")]);
        assert_eq!(out.usages.len(), 2, "холостой вызов тоже в учёте цены");
        assert_eq!(out.usages[0]["completion_tokens"], json!(6000));
        assert!(out.log[0].contains("finish=length"), "причина пустого ответа видна в логе: {:?}", out.log);
    }

    #[test]
    fn effort_none_с_первой_попытки_не_повторяется_тем_же_запросом() {
        let req = base(CascadeKind::Prose, "none", 6000, false);
        let (out, seen) = run(&req, vec![err(None, "пусто", true)]);
        assert_eq!(seen.len(), 1, "повтор на none с теми же параметрами бессмыслен");
        assert!(out.result.is_err());
        // JSON: пустой ответ на none/24000 — тот же запрос, тоже без повтора.
        let req = base(CascadeKind::Json, "none", 24000, true);
        let (_, seen) = run(&req, vec![err(None, "пусто", true)]);
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn is_mandatory_reasoning_только_400() {
        let e = |status, msg: &str| LlmError { error: msg.into(), status, empty: false, networky: false, throttled: false, quota: false, usage: None };
        assert!(is_mandatory_reasoning(&e(Some(400), MANDATORY)));
        assert!(!is_mandatory_reasoning(&e(Some(500), MANDATORY)));
        assert!(!is_mandatory_reasoning(&e(Some(400), "LLM 400: bad request")));
    }
}
