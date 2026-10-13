//! Порт `lib/cost.mjs` (RFC 20, задача T15): учёт токенов и оценка стоимости
//! LLM-вызовов за прогон.
//!
//! Переиспользуется, а не дублируется:
//!
//! * [`format_cost_sticky_line`] — реэкспорт из `crate::report`, куда строка
//!   sticky была портирована раньше (её единственный потребитель — `buildSummary`).
//!
//! Отличия от JS, осознанные:
//!
//! * [`CostTracker`] потокобезопасен (`Mutex`): в JS движок однопоточный и
//!   `costTracker.record` зовётся из `await`-цепочки, а в Rust вызовы LLM идут
//!   из пула потоков (`crate::concurrency`), и трекер шарится между ними.
//!   Блокировки берутся игнорируя poisoning — состояние из чисел всегда согласовано.
//! * `loadPriceMap(env = process.env)` принимает окружение явно (глобалов нет,
//!   решение 3 RFC 20).
//! * JS-строка `totals.prompt_tokens += usage.prompt_tokens || 0` при нечисловом
//!   truthy-поле usage склеила бы аккумулятор в строку; здесь такое поле
//!   коэрсится `Number()`. Достижимо только в обход `normalizeUsage`
//!   (`crate::llm`), который на этом пути гарантирует числа.
//!
//! Паритет держим по: порядку ключей снимка (`prompt_tokens, completion_tokens,
//! total_tokens, calls, cost_usd, cost_known, byModel`), ключу разреза
//! `${callType || '?'}/${model || '?'}` и порядку его первого появления,
//! цепочке ставок `priceMap[model] || priceMap['*'] || priceMap.default`.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::findings::{js_to_number, js_truthy};

/// `formatCostStickyLine` живёт в `crate::report` (первый потребитель — sticky).
pub use crate::report::format_cost_sticky_line;

/// JS Number→JSON: целые печатаются без `.0` (как `JSON.stringify` в V8).
fn num_json(x: f64) -> Value {
    if x == x.trunc() && x.abs() < 9.0e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

/// Блокировка, игнорирующая poisoning (как в `crate::concurrency`).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// JS-доступ по ключу: у объекта — поле, у массива — числовой индекс
/// (`priceMap['*']` на массиве даёт undefined, `priceMap['0']` — элемент).
fn prop<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(o) => o.get(key),
        Value::Array(a) => key.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    }
}

/// `parsePriceMap` + `loadPriceMap`: JSON-карта
/// `{"model": {"in": $/1M prompt, "out": $/1M completion}}` из `REVIEW_PRICE_JSON`.
///
/// `null`, если переменной нет, она пустая, JSON битый или это не объект
/// (массив в JS — тоже `typeof 'object'`, поэтому проходит).
pub fn load_price_map(env: &HashMap<String, String>) -> Option<Value> {
    let raw = env.get("REVIEW_PRICE_JSON")?;
    if raw.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v) if v.is_object() || v.is_array() => Some(v),
        _ => None,
    }
}

/// Оценка $ по usage и таблице цен; `None`, если цены неизвестны.
///
/// Ставки ищутся по цепочке `priceMap[model] || priceMap['*'] || priceMap.default`;
/// если ни `in`, ни `out` не конечны — цены считаются неизвестными. Одна конечная
/// ставка достаточна: вторая половина суммы просто нулевая (как в JS).
pub fn estimate_cost(usage: Option<&Value>, model: &str, price_map: Option<&Value>) -> Option<f64> {
    let usage = usage.filter(|u| js_truthy(u))?;
    let price_map = price_map.filter(|p| js_truthy(p))?;
    if model.is_empty() {
        return None;
    }
    let rates = prop(price_map, model)
        .filter(|v| js_truthy(v))
        .or_else(|| prop(price_map, "*").filter(|v| js_truthy(v)))
        .or_else(|| prop(price_map, "default").filter(|v| js_truthy(v)))?;
    // typeof rates === 'object': объекты и массивы (null отсеян truthiness выше).
    if !rates.is_object() && !rates.is_array() {
        return None;
    }
    let rate = |key: &str| prop(rates, key).and_then(js_to_number).filter(|x| x.is_finite());
    let in_rate = rate("in");
    let out_rate = rate("out");
    if in_rate.is_none() && out_rate.is_none() {
        return None;
    }
    let field = |key: &str| match prop(usage, key).and_then(js_to_number) {
        Some(x) if x != 0.0 && !x.is_nan() => x, // Number(x) || 0
        _ => 0.0,
    };
    let prompt = field("prompt_tokens");
    let completion = field("completion_tokens");
    let dollars = in_rate.map_or(0.0, |r| prompt / 1_000_000.0 * r)
        + out_rate.map_or(0.0, |r| completion / 1_000_000.0 * r);
    if dollars.is_finite() {
        Some(dollars)
    } else {
        None
    }
}

/// Разрез usage по паре «тип вызова / модель».
#[derive(Debug, Default, Clone, Copy)]
struct ModelTotals {
    calls: f64,
    prompt_tokens: f64,
    completion_tokens: f64,
    total_tokens: f64,
}

#[derive(Debug, Default)]
struct Totals {
    prompt_tokens: f64,
    completion_tokens: f64,
    total_tokens: f64,
    calls: f64,
    cost_usd: f64,
    cost_known: bool,
    by_model: IndexMap<String, ModelTotals>,
}

/// Аккумулятор usage за прогон (`createCostTracker`): LLM-клиент → pipeline → `buildSummary`.
///
/// Потокобезопасен: `record` можно звать из нескольких воркеров пула
/// (в JS это последовательные `await`-вызовы одного цикла событий).
#[derive(Debug)]
pub struct CostTracker {
    price_map: Option<Value>,
    totals: Mutex<Totals>,
}

impl CostTracker {
    /// Трекер с таблицей цен (`None` — стоимость остаётся неизвестной).
    pub fn new(price_map: Option<Value>) -> Self {
        Self { price_map, totals: Mutex::new(Totals::default()) }
    }

    /// `record(usage, model, callType)`: usage без полей и `None` игнорируются
    /// (в JS — `if (!usage) return`).
    pub fn record(&self, usage: Option<&Value>, model: &str, call_type: &str) {
        let Some(usage) = usage.filter(|u| js_truthy(u)) else { return };
        let field = |key: &str| match prop(usage, key).and_then(js_to_number) {
            // JS: usage.prompt_tokens || 0 — falsy (0/NaN/undefined) даёт 0.
            Some(x) if x != 0.0 && !x.is_nan() => x,
            _ => 0.0,
        };
        let cost = estimate_cost(Some(usage), model, self.price_map.as_ref());

        let mut t = lock(&self.totals);
        t.prompt_tokens += field("prompt_tokens");
        t.completion_tokens += field("completion_tokens");
        t.total_tokens += field("total_tokens");
        t.calls += 1.0;
        if let Some(cost) = cost {
            t.cost_usd += cost;
            t.cost_known = true;
        }
        let key = format!(
            "{}/{}",
            if call_type.is_empty() { "?" } else { call_type },
            if model.is_empty() { "?" } else { model },
        );
        let entry = t.by_model.entry(key).or_default();
        entry.calls += 1.0;
        entry.prompt_tokens += field("prompt_tokens");
        entry.completion_tokens += field("completion_tokens");
        entry.total_tokens += field("total_tokens");
    }

    /// Снимок для `buildSummary`/артефакта: порядок ключей — как у JS-объекта.
    pub fn snapshot(&self) -> Value {
        let t = lock(&self.totals);
        let mut out = Map::new();
        out.insert("prompt_tokens".into(), num_json(t.prompt_tokens));
        out.insert("completion_tokens".into(), num_json(t.completion_tokens));
        out.insert("total_tokens".into(), num_json(t.total_tokens));
        out.insert("calls".into(), num_json(t.calls));
        out.insert("cost_usd".into(), num_json(t.cost_usd));
        out.insert("cost_known".into(), json!(t.cost_known));
        let mut by_model = Map::new();
        for (key, m) in &t.by_model {
            let mut e = Map::new();
            e.insert("calls".into(), num_json(m.calls));
            e.insert("prompt_tokens".into(), num_json(m.prompt_tokens));
            e.insert("completion_tokens".into(), num_json(m.completion_tokens));
            e.insert("total_tokens".into(), num_json(m.total_tokens));
            by_model.insert(key.clone(), Value::Object(e));
        }
        out.insert("byModel".into(), Value::Object(by_model));
        Value::Object(out)
    }
}

/// `Number(v) || 0` над полем JSON: отсутствие, NaN и нечисло — 0.
fn num_or_zero(v: Option<&Value>) -> f64 {
    match v.and_then(js_to_number) {
        Some(x) if !x.is_nan() => x,
        _ => 0.0,
    }
}

/// `efficiencyMetrics(snapshot, outcome)`: токены на единицу результата.
///
/// Знаменатель — не число находок вообще (их легко нагенерировать), а то, что заняло
/// внимание человека: находки (`outcome.total`), строки в PR (`outcome.inline`) и
/// блокеры (`outcome.blockers`). Знаменатель 0 → поле отсутствует, а не Infinity:
/// «ничего не нашли» — отдельный случай, тренд по нему считать нельзя.
/// Токенов нет вовсе → `None` (JS `null`). `cost_usd` — только при `cost_known`.
/// `Math.round` для положительных — `floor(x + 0.5)`.
pub fn efficiency_metrics(snapshot: &Value, outcome: &Value) -> Option<Value> {
    let total = num_or_zero(snapshot.get("total_tokens"));
    if total == 0.0 {
        return None;
    }
    let per = |key: &str| {
        let d = num_or_zero(outcome.get(key));
        (d > 0.0).then(|| num_json((total / d + 0.5).floor()))
    };
    let mut out = Map::new();
    out.insert("tokens_total".into(), num_json(total));
    out.insert("prompt_tokens".into(), num_json(num_or_zero(snapshot.get("prompt_tokens"))));
    out.insert(
        "completion_tokens".into(),
        num_json(num_or_zero(snapshot.get("completion_tokens"))),
    );
    out.insert("llm_calls".into(), num_json(num_or_zero(snapshot.get("calls"))));
    for (key, field) in [
        ("tokens_per_finding", "total"),
        ("tokens_per_inline", "inline"),
        ("tokens_per_blocker", "blockers"),
    ] {
        if let Some(v) = per(field) {
            out.insert(key.into(), v);
        }
    }
    if snapshot.get("cost_known").is_some_and(js_truthy) {
        out.insert("cost_usd".into(), num_json(num_or_zero(snapshot.get("cost_usd"))));
    }
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: i64, completion: i64, total: i64) -> Value {
        json!({ "prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": total })
    }

    #[test]
    fn efficiency_metrics_знаменатель_ноль_даёт_пропуск_поля() {
        // Эталон снят с node -e на lib/cost.mjs (efficiencyMetrics).
        let snap = json!({
            "prompt_tokens": 900, "completion_tokens": 101, "total_tokens": 1001,
            "calls": 4, "cost_usd": 0.25, "cost_known": true,
        });
        assert_eq!(
            efficiency_metrics(&snap, &json!({ "total": 2, "inline": 0, "blockers": 3 })),
            Some(json!({
                "tokens_total": 1001, "prompt_tokens": 900, "completion_tokens": 101,
                "llm_calls": 4, "tokens_per_finding": 501, "tokens_per_blocker": 334,
                "cost_usd": 0.25,
            }))
        );
        let unknown = json!({ "total_tokens": 10, "cost_usd": 1.5, "cost_known": false });
        assert_eq!(
            efficiency_metrics(&unknown, &json!({})),
            Some(json!({ "tokens_total": 10, "prompt_tokens": 0, "completion_tokens": 0, "llm_calls": 0 })),
            "cost_usd только при cost_known; пустой outcome — без tokens_per_*"
        );
        assert_eq!(efficiency_metrics(&json!({ "total_tokens": 0 }), &json!({ "total": 1 })), None);
        assert_eq!(efficiency_metrics(&Value::Null, &json!({})), None);
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
    }

    #[test]
    fn price_map_из_env_только_объект_или_массив() {
        // Эталон снят с node -e на lib/cost.mjs.
        assert_eq!(
            load_price_map(&env(&[("REVIEW_PRICE_JSON", r#"{"m":{"in":1,"out":2}}"#)])),
            Some(json!({ "m": { "in": 1, "out": 2 } }))
        );
        assert_eq!(load_price_map(&env(&[("REVIEW_PRICE_JSON", "[]")])), Some(json!([])), "массив — объект по typeof JS");
        assert_eq!(load_price_map(&env(&[("REVIEW_PRICE_JSON", "5")])), None, "число — не объект");
        assert_eq!(load_price_map(&env(&[("REVIEW_PRICE_JSON", "{broken")])), None);
        assert_eq!(load_price_map(&env(&[("REVIEW_PRICE_JSON", "  ")])), None);
        assert_eq!(load_price_map(&env(&[])), None);
    }

    #[test]
    fn оценка_стоимости_по_цепочке_ставок() {
        let u = usage(1_000_000, 500_000, 1_500_000);
        assert_eq!(estimate_cost(Some(&u), "m1", Some(&json!({ "m1": { "in": 1, "out": 2 } }))), Some(2.0));
        assert_eq!(estimate_cost(Some(&u), "zzz", Some(&json!({ "*": { "in": 3, "out": 0 } }))), Some(3.0));
        assert_eq!(estimate_cost(Some(&u), "zzz", Some(&json!({ "default": { "in": 0, "out": 4 } }))), Some(2.0));
        assert_eq!(estimate_cost(Some(&u), "m1", Some(&json!({ "m1": { "in": 2 } }))), Some(2.0), "одной ставки достаточно");
    }

    #[test]
    fn оценка_стоимости_на_неизвестных_ценах() {
        let u = usage(1_000_000, 500_000, 1_500_000);
        assert_eq!(estimate_cost(Some(&u), "m1", None), None, "без карты цен");
        assert_eq!(estimate_cost(None, "m1", Some(&json!({ "m1": { "in": 1 } }))), None, "без usage");
        assert_eq!(estimate_cost(Some(&u), "", Some(&json!({ "m1": { "in": 1 } }))), None, "без модели");
        assert_eq!(estimate_cost(Some(&u), "m1", Some(&json!({ "m1": 5 }))), None, "ставки не объект");
        assert_eq!(estimate_cost(Some(&u), "m1", Some(&json!({ "zzz": { "in": 1 } }))), None, "модели нет в карте");
        // Number(null) === 0 — конечная ставка, поэтому не null, а 0 (как в JS).
        assert_eq!(estimate_cost(Some(&u), "m1", Some(&json!({ "m1": { "in": "x", "out": null } }))), Some(0.0));
        assert_eq!(estimate_cost(Some(&u), "m1", Some(&json!({ "m1": { "in": "x", "out": "y" } }))), None, "обе ставки NaN");
        assert_eq!(estimate_cost(Some(&json!({})), "m1", Some(&json!({ "m1": { "in": 1, "out": 2 } }))), Some(0.0), "usage без полей");
    }

    #[test]
    fn трекер_суммирует_токены_и_стоимость() {
        // JS: test('createCostTracker: aggregates tokens and cost from REVIEW_PRICE_JSON').
        let tracker = CostTracker::new(Some(json!({ "m1": { "in": 1, "out": 2 } })));
        tracker.record(Some(&usage(1_000_000, 500_000, 1_500_000)), "m1", "review_json");
        let snap = tracker.snapshot();
        assert_eq!(snap["prompt_tokens"], json!(1_000_000));
        assert_eq!(snap["total_tokens"], json!(1_500_000));
        assert_eq!(snap["cost_known"], json!(true));
        assert!((snap["cost_usd"].as_f64().unwrap() - 2.0).abs() < 0.001);
        assert!(format_cost_sticky_line(Some(&snap)).contains('💰'));
    }

    #[test]
    fn снимок_повторяет_json_из_js() {
        // Эталон: node -e на lib/cost.mjs с теми же тремя record().
        let tracker = CostTracker::new(Some(json!({ "m1": { "in": 1, "out": 2 } })));
        tracker.record(Some(&usage(1_000_000, 500_000, 1_500_000)), "m1", "review_json");
        tracker.record(Some(&usage(10, 0, 10)), "m2", "");
        tracker.record(None, "m2", "advisor");
        let snap = tracker.snapshot();
        assert_eq!(
            serde_json::to_string(&snap).unwrap(),
            r#"{"prompt_tokens":1000010,"completion_tokens":500000,"total_tokens":1500010,"calls":2,"cost_usd":2,"cost_known":true,"byModel":{"review_json/m1":{"calls":1,"prompt_tokens":1000000,"completion_tokens":500000,"total_tokens":1500000},"?/m2":{"calls":1,"prompt_tokens":10,"completion_tokens":0,"total_tokens":10}}}"#
        );
        assert_eq!(format_cost_sticky_line(Some(&snap)), "💰 ~$2.00 · 1.5M tokens", "строка sticky байт в байт");
    }

    #[test]
    fn без_карты_цен_стоимость_неизвестна() {
        let tracker = CostTracker::new(None);
        tracker.record(Some(&usage(100, 50, 150)), "m1", "advisor");
        let snap = tracker.snapshot();
        assert_eq!(snap["cost_known"], json!(false));
        assert_eq!(snap["cost_usd"], json!(0));
        assert_eq!(snap["calls"], json!(1));
        assert_eq!(format_cost_sticky_line(Some(&snap)), "💰 150 tokens", "без цен — только токены");
    }

    #[test]
    fn ключ_разреза_подставляет_вопрос_вместо_пустых() {
        let tracker = CostTracker::new(None);
        tracker.record(Some(&usage(1, 1, 2)), "", "");
        let snap = tracker.snapshot();
        assert!(snap["byModel"].get("?/?").is_some(), "{snap}");
    }

    #[test]
    fn запись_из_нескольких_потоков_не_теряет_вызовы() {
        // Причина Mutex: в едином бинаре LLM-вызовы идут из пула потоков.
        let tracker = CostTracker::new(Some(json!({ "*": { "in": 1, "out": 1 } })));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        tracker.record(Some(&usage(10, 5, 15)), "m1", "advisor");
                    }
                });
            }
        });
        let snap = tracker.snapshot();
        assert_eq!(snap["calls"], json!(400));
        assert_eq!(snap["prompt_tokens"], json!(4000));
        assert_eq!(snap["total_tokens"], json!(6000));
        assert_eq!(snap["byModel"]["advisor/m1"]["calls"], json!(400));
        assert!((snap["cost_usd"].as_f64().unwrap() - 400.0 * 15.0 / 1_000_000.0).abs() < 1e-9);
    }
}
