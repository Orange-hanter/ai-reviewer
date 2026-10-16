//! Вызов decision-модели (RFC 21): закрытый набор вариантов → вероятность каждого.
//!
//! Эндпоинт — `POST <base>/decisions` OpenRouter (не chat/completions), схема из пробы
//! `docs/probe-decisions-2026-10-16.md`:
//!   {model, state, questions: {<name>: {type: "choice", instructions, criteria: {value: описание}}}}
//!   → {model, answers: {<name>: {choice, probabilities: {value: p}, confidence}}, usage}
//!
//! Один вопрос на вызов, перестановки вариантов — отдельными вызовами: у Decision-1 два
//! вопроса с разным порядком в ОДНОМ вызове дают побитово одинаковый ответ, то есть
//! смещение порядка так не измерить. Перестановки — циклические сдвиги на `k·⌊n/K⌋`:
//! детерминированы и без ГПСЧ. Ответ сопоставляется по значению варианта, не по индексу.
//!
//! Модуль rust-owned (RFC 20): JS-копии и parity нет. Семафоры, бюджет вызовов на прогон и
//! circuit breaker на квоте — в JS-обёртке (lib/decision.mjs): бинарь — процесс на вызов.

use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::{backoff_ms, is_quota_exhausted, is_transient, retry_after_of, truncate_chars};

#[derive(Debug, Clone, Deserialize)]
pub struct DecideChoice {
    pub value: String,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DecideQuestion {
    pub name: String,
    pub instructions: String,
    pub choices: Vec<DecideChoice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DecideRequest {
    /// Полный URL эндпоинта (`<base>/decisions`) — собирает обёртка.
    pub url: String,
    pub key: String,
    pub model: String,
    pub state: String,
    pub question: DecideQuestion,
    #[serde(default = "one")]
    pub permutations: u32,
    #[serde(rename = "httpRetries", default)]
    pub http_retries: u32,
    #[serde(rename = "timeoutMs")]
    pub timeout_ms: u64,
    /// Потолок длины state в символах (контекст моделей ~32K токенов); 0/нет — без обрезки.
    #[serde(rename = "maxStateChars", default)]
    pub max_state_chars: usize,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize)]
pub struct DecideOutcome {
    pub answer: Value,
    pub truncated: bool,
    /// {prompt_tokens, completion_tokens, total_tokens, cost} — сумма по перестановкам.
    pub usage: Value,
    #[serde(rename = "latencyMs")]
    pub latency_ms: u64,
    /// HTTP-вызовов, включая ретраи, — для бюджета обёртки.
    pub calls: u32,
    #[serde(rename = "servedModel")]
    pub served_model: Option<String>,
    pub throttled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecideError {
    pub error: String,
    pub status: Option<u16>,
    pub quota: bool,
    pub throttled: bool,
    pub networky: bool,
    pub calls: u32,
    /// usage уже дошедших перестановок — их выставили в счёт.
    pub usage: Value,
}

/// Порядки вариантов: K циклических сдвигов на `k·⌊n/K⌋`, K ≤ n. Первый — исходный порядок.
pub fn rotations(n: usize, k: u32) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![vec![]];
    }
    let k = (k.max(1) as usize).min(n);
    let step = n / k;
    (0..k)
        .map(|i| {
            let shift = i * step;
            (0..n).map(|j| (j + shift) % n).collect()
        })
        .collect()
}

/// Тело запроса для одного порядка вариантов.
pub fn build_body(model: &str, state: &str, q: &DecideQuestion, order: &[usize]) -> Value {
    let mut criteria = Map::new();
    for &i in order {
        let c = &q.choices[i];
        criteria.insert(c.value.clone(), Value::String(c.description.clone()));
    }
    let mut questions = Map::new();
    questions.insert(
        q.name.clone(),
        json!({ "type": "choice", "instructions": q.instructions, "criteria": criteria }),
    );
    json!({ "model": model, "state": state, "questions": questions })
}

/// Разобранный ответ одного вызова: вероятности в ИСХОДНОМ порядке вариантов.
#[derive(Debug, Clone, PartialEq)]
pub struct PermAnswer {
    pub probs: Vec<f64>,
    pub confidence: Option<f64>,
    pub refusal: Option<String>,
}

/// Достаёт ответ на вопрос `q.name`. Вероятности неизвестных вариантов игнорируются,
/// отсутствующих — 0; нет вероятностей, но есть `choice` — one-hot. Отказ модели — не
/// ошибка: вероятности нулевые, `refusal` несёт текст.
pub fn parse_answer(body: &Value, q: &DecideQuestion) -> Result<PermAnswer, String> {
    let ans = body
        .get("answers")
        .and_then(|a| a.get(&q.name))
        .ok_or_else(|| format!("в ответе нет answers.{}", q.name))?;
    let n = q.choices.len();
    if let Some(r) = ans.get("refusal").filter(|r| !r.is_null()) {
        let text = r.as_str().map(String::from).unwrap_or_else(|| r.to_string());
        return Ok(PermAnswer { probs: vec![0.0; n], confidence: None, refusal: Some(text) });
    }
    let confidence = ans.get("confidence").and_then(Value::as_f64);
    let mut probs = vec![0.0; n];
    if let Some(map) = ans.get("probabilities").and_then(Value::as_object) {
        for (i, c) in q.choices.iter().enumerate() {
            if let Some(p) = map.get(&c.value).and_then(Value::as_f64) {
                if p.is_finite() && p >= 0.0 {
                    probs[i] = p;
                }
            }
        }
    } else if let Some(choice) = ans.get("choice").and_then(Value::as_str) {
        let i = q
            .choices
            .iter()
            .position(|c| c.value == choice)
            .ok_or_else(|| format!("choice «{choice}» вне набора вариантов"))?;
        probs[i] = 1.0;
    } else {
        return Err(format!("answers.{}: нет ни probabilities, ни choice", q.name));
    }
    let sum: f64 = probs.iter().sum();
    if sum <= 0.0 {
        return Err(format!("answers.{}: все вероятности нулевые", q.name));
    }
    // Нормируем: у квантующих моделей (luna, шаг 0.01) сумма бывает 0.99–1.01.
    for p in &mut probs {
        *p /= sum;
    }
    Ok(PermAnswer { probs, confidence, refusal: None })
}

/// Полная вариация двух распределений.
pub fn total_variation(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f64>() / 2.0
}

fn argmax(p: &[f64]) -> Option<usize> {
    // Первый из равных — в исходном порядке вариантов.
    p.iter().enumerate().fold(None, |best: Option<(usize, f64)>, (i, &v)| match best {
        Some((_, bv)) if bv >= v => best,
        _ => Some((i, v)),
    })
    .map(|(i, _)| i)
}

/// Свод перестановок: среднее, argmax, flip (argmax различается), tvMax (наибольшая
/// попарная TV). Отказы в среднее не входят; все отказались — argmax null.
pub fn aggregate(q: &DecideQuestion, perms: &[PermAnswer]) -> Value {
    let n = q.choices.len();
    let answered: Vec<&PermAnswer> = perms.iter().filter(|p| p.refusal.is_none()).collect();
    let mut mean = vec![0.0; n];
    for p in &answered {
        for (m, v) in mean.iter_mut().zip(&p.probs) {
            *m += v / answered.len() as f64;
        }
    }
    let arg = if answered.is_empty() { None } else { argmax(&mean) };
    let per_arg: Vec<Option<usize>> = answered.iter().map(|p| argmax(&p.probs)).collect();
    let flip = per_arg.windows(2).any(|w| w[0] != w[1]);
    let mut tv_max: f64 = 0.0;
    for i in 0..answered.len() {
        for j in (i + 1)..answered.len() {
            tv_max = tv_max.max(total_variation(&answered[i].probs, &answered[j].probs));
        }
    }
    let to_map = |probs: &[f64]| {
        let mut m = Map::new();
        for (c, p) in q.choices.iter().zip(probs) {
            m.insert(c.value.clone(), json!(p));
        }
        Value::Object(m)
    };
    let confs: Vec<f64> = answered.iter().filter_map(|p| p.confidence).collect();
    let confidence = if confs.is_empty() { Value::Null } else { json!(confs.iter().sum::<f64>() / confs.len() as f64) };
    let refusal = perms.iter().find_map(|p| p.refusal.clone());
    json!({
        "name": q.name,
        "probabilities": to_map(&mean),
        "perPermutation": perms.iter().map(|p| to_map(&p.probs)).collect::<Vec<_>>(),
        "argmax": arg.map(|i| q.choices[i].value.clone()),
        "flip": flip,
        "tvMax": tv_max,
        "confidence": confidence,
        "refusal": refusal,
    })
}

#[derive(Default)]
struct UsageSum {
    input: f64,
    output: f64,
    cost: f64,
    has_cost: bool,
}

impl UsageSum {
    fn add(&mut self, u: Option<&Value>) {
        let Some(u) = u else { return };
        self.input += u.get("input_tokens").and_then(Value::as_f64).unwrap_or(0.0);
        self.output += u.get("output_tokens").and_then(Value::as_f64).unwrap_or(0.0);
        if let Some(c) = u.get("cost").and_then(Value::as_f64) {
            self.cost += c;
            self.has_cost = true;
        }
    }
    fn to_json(&self) -> Value {
        json!({
            "prompt_tokens": self.input,
            "completion_tokens": self.output,
            "total_tokens": self.input + self.output,
            "cost": if self.has_cost { json!(self.cost) } else { Value::Null },
        })
    }
}

fn truncate_state(state: &str, max: usize) -> (String, bool) {
    if max == 0 || state.chars().count() <= max {
        return (state.to_string(), false);
    }
    (state.chars().take(max).collect(), true)
}

/// Весь вызов: K перестановок, ретраи transient/сеть в пределах бюджета `timeoutMs` на
/// перестановку, квота — без ретрая.
pub fn decide(req: &DecideRequest) -> Result<DecideOutcome, DecideError> {
    let timeout = Duration::from_millis(req.timeout_ms.max(1));
    let t_all = Instant::now();
    let mut calls: u32 = 0;
    let mut throttled = false;
    let mut usage = UsageSum::default();
    let fail = |error: String, status: Option<u16>, quota: bool, networky: bool, calls: u32, throttled: bool, usage: &UsageSum| DecideError {
        error,
        status,
        quota,
        throttled,
        networky,
        calls,
        usage: usage.to_json(),
    };
    if req.question.choices.len() < 2 {
        return Err(fail("нужно хотя бы два варианта".into(), None, false, false, 0, false, &usage));
    }
    let client = Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()
        .map_err(|e| fail(e.to_string(), None, false, true, 0, false, &usage))?;
    let (state, truncated) = truncate_state(&req.state, req.max_state_chars);
    let mut perms = Vec::new();
    let mut served_model = None;

    for order in rotations(req.question.choices.len(), req.permutations) {
        let payload = build_body(&req.model, &state, &req.question, &order).to_string();
        let t0 = Instant::now();
        let mut attempt: u32 = 0;
        let body: Value = loop {
            calls += 1;
            let res = client
                .post(&req.url)
                .header(AUTHORIZATION, format!("Bearer {}", req.key))
                .header(CONTENT_TYPE, "application/json")
                .body(payload.clone())
                .send();
            let res = match res {
                Ok(r) => r,
                Err(e) => {
                    if attempt < req.http_retries && t0.elapsed() < timeout {
                        std::thread::sleep(Duration::from_secs_f64(backoff_ms(attempt, None) / 1000.0));
                        attempt += 1;
                        continue;
                    }
                    return Err(fail(e.to_string(), None, false, true, calls, throttled, &usage));
                }
            };
            let status = res.status().as_u16();
            if !res.status().is_success() {
                let retry_after = retry_after_of(&res);
                let text = res.text().unwrap_or_default();
                if is_quota_exhausted(status, &text) {
                    let msg = format!("decision {status} (квота исчерпана): {}", truncate_chars(&text, 400));
                    return Err(fail(msg, Some(status), true, false, calls, throttled, &usage));
                }
                if status == 429 {
                    throttled = true;
                }
                if is_transient(status) && attempt < req.http_retries && t0.elapsed() < timeout {
                    std::thread::sleep(Duration::from_secs_f64(backoff_ms(attempt, retry_after.as_deref()) / 1000.0));
                    attempt += 1;
                    continue;
                }
                let msg = format!("decision {status}: {}", truncate_chars(&text, 400));
                return Err(fail(msg, Some(status), false, false, calls, throttled, &usage));
            }
            match res.text().map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) {
                Ok(v) => break v,
                Err(e) => {
                    if attempt < req.http_retries && t0.elapsed() < timeout {
                        attempt += 1;
                        continue;
                    }
                    return Err(fail(format!("decision: битый ответ: {e}"), Some(status), false, true, calls, throttled, &usage));
                }
            }
        };
        usage.add(body.get("usage"));
        if served_model.is_none() {
            served_model = body.get("model").and_then(Value::as_str).map(String::from);
        }
        let ans = parse_answer(&body, &req.question)
            .map_err(|e| fail(format!("decision: {e}"), None, false, false, calls, throttled, &usage))?;
        perms.push(ans);
    }

    Ok(DecideOutcome {
        answer: aggregate(&req.question, &perms),
        truncated,
        usage: usage.to_json(),
        latency_ms: t_all.elapsed().as_millis() as u64,
        calls,
        served_model,
        throttled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(values: &[&str]) -> DecideQuestion {
        DecideQuestion {
            name: "q".into(),
            instructions: "i".into(),
            choices: values.iter().map(|v| DecideChoice { value: (*v).into(), description: format!("d-{v}") }).collect(),
        }
    }

    #[test]
    fn rotations_циклические_и_не_больше_n() {
        assert_eq!(rotations(4, 2), vec![vec![0, 1, 2, 3], vec![2, 3, 0, 1]]);
        assert_eq!(rotations(3, 1), vec![vec![0, 1, 2]]);
        assert_eq!(rotations(2, 5).len(), 2, "K урезается до n");
        assert_eq!(rotations(3, 0), vec![vec![0, 1, 2]], "K=0 — как 1");
    }

    #[test]
    fn build_body_сохраняет_порядок_criteria() {
        let question = q(&["a", "b", "c"]);
        let body = build_body("m", "st", &question, &[2, 0, 1]);
        let keys: Vec<&String> = body["questions"]["q"]["criteria"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["c", "a", "b"]);
        assert_eq!(body["questions"]["q"]["type"], "choice");
        assert_eq!(body["state"], "st");
        assert_eq!(body["model"], "m");
    }

    #[test]
    fn parse_answer_по_значению_нормирует_и_игнорирует_чужие() {
        let question = q(&["a", "b"]);
        let body = json!({"answers": {"q": {"probabilities": {"b": 0.6, "a": 0.39, "zzz": 0.5}, "confidence": 0.7}}});
        let a = parse_answer(&body, &question).unwrap();
        assert!((a.probs[0] - 0.39 / 0.99).abs() < 1e-12);
        assert!((a.probs[1] - 0.6 / 0.99).abs() < 1e-12);
        assert_eq!(a.confidence, Some(0.7));
    }

    #[test]
    fn parse_answer_choice_без_вероятностей_и_отказ() {
        let question = q(&["a", "b"]);
        let one_hot = parse_answer(&json!({"answers": {"q": {"choice": "b"}}}), &question).unwrap();
        assert_eq!(one_hot.probs, vec![0.0, 1.0]);
        let refused = parse_answer(&json!({"answers": {"q": {"refusal": "нет"}}}), &question).unwrap();
        assert_eq!(refused.refusal.as_deref(), Some("нет"));
        assert!(parse_answer(&json!({"answers": {}}), &question).is_err());
        assert!(parse_answer(&json!({"answers": {"q": {"choice": "x"}}}), &question).is_err());
        assert!(parse_answer(&json!({"answers": {"q": {"probabilities": {"a": 0}}}}), &question).is_err());
    }

    #[test]
    fn aggregate_среднее_flip_tv() {
        let question = q(&["a", "b"]);
        let perms = vec![
            PermAnswer { probs: vec![0.7, 0.3], confidence: Some(0.6), refusal: None },
            PermAnswer { probs: vec![0.4, 0.6], confidence: Some(0.2), refusal: None },
        ];
        let out = aggregate(&question, &perms);
        assert_eq!(out["argmax"], "a", "среднее 0.55/0.45");
        assert_eq!(out["flip"], true);
        assert!((out["tvMax"].as_f64().unwrap() - 0.3).abs() < 1e-12);
        assert!((out["confidence"].as_f64().unwrap() - 0.4).abs() < 1e-12);
        assert_eq!(out["perPermutation"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn aggregate_отказы_вне_среднего() {
        let question = q(&["a", "b"]);
        let perms = vec![
            PermAnswer { probs: vec![0.0, 0.0], confidence: None, refusal: Some("r".into()) },
            PermAnswer { probs: vec![0.2, 0.8], confidence: None, refusal: None },
        ];
        let out = aggregate(&question, &perms);
        assert_eq!(out["argmax"], "b");
        assert_eq!(out["flip"], false);
        assert_eq!(out["refusal"], "r");
        assert_eq!(out["confidence"], Value::Null);
        let all_refused = aggregate(&question, &perms[..1]);
        assert_eq!(all_refused["argmax"], Value::Null);
    }

    #[test]
    fn truncate_state_по_символам() {
        assert_eq!(truncate_state("абвгд", 3), ("абв".to_string(), true));
        assert_eq!(truncate_state("абв", 0), ("абв".to_string(), false));
        assert_eq!(truncate_state("абв", 3), ("абв".to_string(), false));
    }
}
