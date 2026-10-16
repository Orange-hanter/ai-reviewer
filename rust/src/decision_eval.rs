//! Метрики и отчёт офлайн-замера decision-моделей (RFC 21, этап 0).
//!
//! Что меряем и почему так:
//!   - согласие с метками — AUC по p(real) (ранговая, без порога и калибровки) с бутстрап-ДИ:
//!     на десятках меток точечная оценка без интервала ничего не значит;
//!   - шум повтора и смещение порядка — РАЗДЕЛЬНО: у Decision-1 TV повтора ≈ 0.04 сравним с
//!     эффектом порядка ≈ 0.10 (проба 2026-10-16), без разделения шум сойдёт за смещение;
//!   - κ Коэна между моделями — независим ли второй сигнал (критерий K4);
//!   - калибровка (ECE, температура) — только разведка: на таком N порог не подбирается.
//!
//! Корпус и опрос моделей собирает тонкий JS (bin/pr-review-decision-eval.mjs): он зависит от
//! непортированного JS (FP-регексы с lookahead, gold-fetch, chunk-routing). Сюда приходят
//! строки «элемент × модель», отсюда уходят сводки и markdown.

use std::collections::{BTreeSet, HashMap};

use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Метрики (чистые функции)
// ---------------------------------------------------------------------------

/// Детерминированный ГПСЧ (mulberry32) — бутстрап воспроизводим между прогонами отчёта.
pub struct Mulberry32(u32);

impl Mulberry32 {
    pub fn new(seed: u32) -> Self {
        Mulberry32(seed)
    }
    pub fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6D2B_79F5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

/// Квантиль (линейная интерполяция); пусто → None.
pub fn quantile(values: &[f64], q: f64) -> Option<f64> {
    let mut xs: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pos = (xs.len() - 1) as f64 * q;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    Some(xs[lo] + (xs[hi] - xs[lo]) * (pos - lo as f64))
}

/// AUC (Манн–Уитни), ничьи по 0.5; нет одного из классов → None.
pub fn auc(scores: &[f64], labels: &[bool]) -> Option<f64> {
    let (mut pos, mut neg) = (Vec::new(), Vec::new());
    for (s, y) in scores.iter().zip(labels) {
        if s.is_finite() {
            if *y { pos.push(*s) } else { neg.push(*s) }
        }
    }
    if pos.is_empty() || neg.is_empty() {
        return None;
    }
    let mut sum = 0.0;
    for p in &pos {
        for n in &neg {
            sum += if p > n { 1.0 } else if p == n { 0.5 } else { 0.0 };
        }
    }
    Some(sum / (pos.len() * neg.len()) as f64)
}

/// Перцентильный бутстрап-ДИ (95%) AUC по парам (score, label).
pub fn auc_ci(pairs: &[(f64, bool)], b: usize, seed: u32) -> Option<(f64, f64)> {
    if pairs.is_empty() {
        return None;
    }
    let mut r = Mulberry32::new(seed);
    let mut vals = Vec::new();
    for _ in 0..b {
        let (mut s, mut y) = (Vec::with_capacity(pairs.len()), Vec::with_capacity(pairs.len()));
        for _ in 0..pairs.len() {
            let (ps, py) = pairs[(r.next_f64() * pairs.len() as f64) as usize];
            s.push(ps);
            y.push(py);
        }
        if let Some(v) = auc(&s, &y) {
            vals.push(v);
        }
    }
    if vals.len() < b / 2 {
        return None;
    }
    Some((quantile(&vals, 0.025)?, quantile(&vals, 0.975)?))
}

/// κ Коэна двух разметок; пары с None пропускаются.
pub fn cohen_kappa(a: &[Option<String>], b: &[Option<String>]) -> Option<f64> {
    let pairs: Vec<(&String, &String)> =
        a.iter().zip(b).filter_map(|(x, y)| Some((x.as_ref()?, y.as_ref()?))).collect();
    if pairs.is_empty() {
        return None;
    }
    let n = pairs.len() as f64;
    let po = pairs.iter().filter(|(x, y)| x == y).count() as f64 / n;
    let cats: BTreeSet<&String> = pairs.iter().flat_map(|(x, y)| [*x, *y]).collect();
    let pe: f64 = cats
        .iter()
        .map(|c| {
            let ca = pairs.iter().filter(|(x, _)| x == c).count() as f64 / n;
            let cb = pairs.iter().filter(|(_, y)| y == c).count() as f64 / n;
            ca * cb
        })
        .sum();
    Some(if (pe - 1.0).abs() < 1e-12 { if po == 1.0 { 1.0 } else { 0.0 } } else { (po - pe) / (1.0 - pe) })
}

fn ranks(xs: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..xs.len()).collect();
    idx.sort_by(|&i, &j| xs[i].partial_cmp(&xs[j]).unwrap());
    let mut r = vec![0.0; xs.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && xs[idx[j + 1]] == xs[idx[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for k in i..=j {
            r[idx[k]] = avg;
        }
        i = j + 1;
    }
    r
}

/// Спирмен (Пирсон над рангами); меньше трёх пар → None.
pub fn spearman(x: &[f64], y: &[f64]) -> Option<f64> {
    let pairs: Vec<(f64, f64)> = x.iter().zip(y).filter(|(a, b)| a.is_finite() && b.is_finite()).map(|(a, b)| (*a, *b)).collect();
    if pairs.len() < 3 {
        return None;
    }
    let rx = ranks(&pairs.iter().map(|p| p.0).collect::<Vec<_>>());
    let ry = ranks(&pairs.iter().map(|p| p.1).collect::<Vec<_>>());
    let n = rx.len() as f64;
    let (mx, my) = (rx.iter().sum::<f64>() / n, ry.iter().sum::<f64>() / n);
    let (mut num, mut dx, mut dy) = (0.0, 0.0, 0.0);
    for i in 0..rx.len() {
        num += (rx[i] - mx) * (ry[i] - my);
        dx += (rx[i] - mx).powi(2);
        dy += (ry[i] - my).powi(2);
    }
    if dx == 0.0 || dy == 0.0 { None } else { Some(num / (dx * dy).sqrt()) }
}

/// TV двух распределений-объектов {value: p}.
pub fn tv(a: &Map<String, Value>, b: &Map<String, Value>) -> f64 {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.iter()
        .map(|k| (a.get(*k).and_then(Value::as_f64).unwrap_or(0.0) - b.get(*k).and_then(Value::as_f64).unwrap_or(0.0)).abs())
        .sum::<f64>()
        / 2.0
}

pub fn argmax_of(p: &Map<String, Value>) -> Option<String> {
    let mut best: Option<(&String, f64)> = None;
    for (k, v) in p {
        let v = v.as_f64().unwrap_or(f64::NAN);
        if best.map_or(true, |(_, bv)| v > bv) {
            best = Some((k, v));
        }
    }
    best.map(|(k, _)| k.clone())
}

/// Шум повтора по элементу: сменился ли argmax и наибольшая попарная TV; меньше двух → None.
pub fn repeat_noise(dists: &[Map<String, Value>]) -> Option<(bool, f64)> {
    if dists.len() < 2 {
        return None;
    }
    let args: BTreeSet<Option<String>> = dists.iter().map(argmax_of).collect();
    let mut tv_max: f64 = 0.0;
    for i in 0..dists.len() {
        for j in (i + 1)..dists.len() {
            tv_max = tv_max.max(tv(&dists[i], &dists[j]));
        }
    }
    Some((args.len() > 1, tv_max))
}

/// ECE бинарных вероятностей по `bins` корзинам.
pub fn ece(probs: &[f64], labels: &[bool], bins: usize) -> Option<f64> {
    let items: Vec<(f64, bool)> = probs.iter().zip(labels).filter(|(p, _)| p.is_finite()).map(|(p, y)| (*p, *y)).collect();
    if items.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for b in 0..bins {
        let (lo, hi) = (b as f64 / bins as f64, (b + 1) as f64 / bins as f64);
        let in_bin: Vec<&(f64, bool)> = items.iter().filter(|(p, _)| (*p > lo || (b == 0 && *p >= lo)) && *p <= hi).collect();
        if in_bin.is_empty() {
            continue;
        }
        let conf = in_bin.iter().map(|(p, _)| p).sum::<f64>() / in_bin.len() as f64;
        let acc = in_bin.iter().filter(|(_, y)| *y).count() as f64 / in_bin.len() as f64;
        total += in_bin.len() as f64 / items.len() as f64 * (conf - acc).abs();
    }
    Some(total)
}

const EPS: f64 = 1e-6;

fn clamp_p(p: f64) -> f64 {
    p.clamp(EPS, 1.0 - EPS)
}

/// p' = σ(logit(p)/T).
pub fn apply_temperature(p: f64, t: f64) -> f64 {
    let p = clamp_p(p);
    1.0 / (1.0 + (-(p / (1.0 - p)).ln() / t).exp())
}

/// Температура по сетке, минимизирующая NLL.
pub fn fit_temperature(probs: &[f64], labels: &[bool]) -> f64 {
    let (mut best_t, mut best_nll) = (1.0, f64::INFINITY);
    let mut t = 0.25;
    while t <= 8.0001 {
        let nll: f64 = probs
            .iter()
            .zip(labels)
            .map(|(p, y)| {
                let q = clamp_p(apply_temperature(*p, t));
                if *y { -q.ln() } else { -(1.0 - q).ln() }
            })
            .sum();
        if nll < best_nll {
            best_t = t;
            best_nll = nll;
        }
        t *= 1.1;
    }
    best_t
}

/// ECE до и после температуры, подобранной leave-one-out. Меньше 4 элементов → None.
pub fn loo_temperature_ece(probs: &[f64], labels: &[bool]) -> Option<(f64, f64, f64)> {
    if probs.len() < 4 {
        return None;
    }
    let calibrated: Vec<f64> = (0..probs.len())
        .map(|i| {
            let (ps, ys): (Vec<f64>, Vec<bool>) =
                probs.iter().zip(labels).enumerate().filter(|(j, _)| *j != i).map(|(_, (p, y))| (*p, *y)).unzip();
            apply_temperature(probs[i], fit_temperature(&ps, &ys))
        })
        .collect();
    Some((ece(probs, labels, 10)?, ece(&calibrated, labels, 10)?, fit_temperature(probs, labels)))
}

// ---------------------------------------------------------------------------
// Сводка замера
// ---------------------------------------------------------------------------

/// Строка «элемент × модель» от JS-сборщика.
struct Row<'a> {
    kind: &'a str,
    label: Option<&'a str>,
    family: Option<&'a str>,
    regex_family: Option<&'a str>,
    deterministic: Option<&'a str>,
    perm: Option<&'a Map<String, Value>>,
    repeats: Vec<Map<String, Value>>,
    latency: Vec<f64>,
    cost: f64,
    errors: usize,
}

fn opt_str<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn row_of(v: &Value) -> Row<'_> {
    let probs = |d: &Value| d.as_object().cloned();
    Row {
        kind: opt_str(v, "kind").unwrap_or(""),
        label: opt_str(v, "label"),
        family: opt_str(v, "family"),
        regex_family: opt_str(v, "regexFamily"),
        deterministic: opt_str(v, "deterministic"),
        perm: v.get("perm").and_then(Value::as_object),
        repeats: v.get("repeats").and_then(Value::as_array).map(|a| a.iter().filter_map(probs).collect()).unwrap_or_default(),
        latency: v.get("latency").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default(),
        cost: v.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
        errors: v.get("errors").and_then(Value::as_array).map_or(0, Vec::len),
    }
}

fn perm_probs<'a>(r: &Row<'a>) -> Option<&'a Map<String, Value>> {
    r.perm.and_then(|p| p.get("probabilities")).and_then(Value::as_object)
}

fn perm_argmax(r: &Row<'_>) -> Option<String> {
    r.perm.and_then(|p| p.get("argmax")).and_then(Value::as_str).map(String::from)
}

fn p_real(r: &Row<'_>) -> Option<f64> {
    perm_probs(r).and_then(|p| p.get("real")).and_then(Value::as_f64)
}

fn rate(n: usize, d: usize) -> Value {
    if d == 0 { Value::Null } else { json!(n as f64 / d as f64) }
}

fn opt_num(x: Option<f64>) -> Value {
    x.map_or(Value::Null, |v| json!(v))
}

/// Сводка по одной модели. `params`: {repeats, timeoutMs}.
pub fn summarize(model: &str, rows_json: &[Value], params: &Value) -> Value {
    let rows: Vec<Row> = rows_json.iter().map(row_of).collect();
    let repeats = params.get("repeats").and_then(Value::as_u64).unwrap_or(0) as usize;
    let calls = rows.len() * (repeats + 1);
    let errs: usize = rows.iter().map(|r| r.errors).sum();
    let noise: Vec<(bool, f64)> = rows.iter().filter_map(|r| repeat_noise(&r.repeats)).collect();
    let with_perm: Vec<&Row> = rows.iter().filter(|r| r.perm.is_some()).collect();
    let order_tv: Vec<f64> = with_perm.iter().filter_map(|r| r.perm.unwrap().get("tvMax").and_then(Value::as_f64)).collect();
    let flips = with_perm.iter().filter(|r| r.perm.unwrap().get("flip").and_then(Value::as_bool).unwrap_or(false)).count();

    let fp: Vec<&Row> = with_perm.iter().copied().filter(|r| r.kind == "fp").collect();
    let labeled: Vec<&Row> = fp.iter().copied().filter(|r| r.label.is_some()).collect();
    let scores: Vec<f64> = labeled.iter().map(|r| p_real(r).unwrap_or(f64::NAN)).collect();
    let ys: Vec<bool> = labeled.iter().map(|r| r.label == Some("real")).collect();
    let pairs: Vec<(f64, bool)> = scores.iter().copied().zip(ys.iter().copied()).collect();
    let auc_v = auc(&scores, &ys);
    let ci = auc_ci(&pairs, 2000, 7);
    let fp_only: Vec<&&Row> = labeled.iter().filter(|r| r.label == Some("fp")).collect();
    let fp_recognized = fp_only.iter().filter(|r| p_real(r).unwrap_or(1.0) < 0.5).count();
    let fam: Vec<&Row> = fp.iter().copied().filter(|r| r.family.is_some()).collect();
    let fam_ok = fam.iter().filter(|r| perm_argmax(r).as_deref() == r.family).count();
    let regex_rows: Vec<&Row> = fp.iter().copied().filter(|r| r.regex_family.is_some()).collect();
    let regex_kappa = cohen_kappa(
        &regex_rows.iter().map(|r| r.regex_family.map(String::from)).collect::<Vec<_>>(),
        &regex_rows.iter().map(|r| perm_argmax(r)).collect::<Vec<_>>(),
    );
    let regex_fp = regex_rows.iter().filter(|r| perm_argmax(r).as_deref() != Some("real")).count();
    // Калибровка по одному классу бессмысленна: температура уходит в край сетки.
    let both = ys.iter().any(|y| *y) && ys.iter().any(|y| !*y);
    let cal = if both { loo_temperature_ece(&scores, &ys) } else { None };

    let ch: Vec<&Row> = with_perm.iter().copied().filter(|r| r.kind == "chunk").collect();
    let agree = ch.iter().filter(|r| perm_argmax(r).as_deref() == r.deterministic).count();
    let missed: Vec<&&Row> = ch.iter().filter(|r| r.label == Some("jury_required") && perm_argmax(r).as_deref() == Some("single")).collect();
    let k3 = missed.iter().filter(|r| perm_probs(r).and_then(|p| p.get("single")).and_then(Value::as_f64).unwrap_or(0.0) >= 0.7).count();
    let saved = ch.iter().filter(|r| r.deterministic == Some("jury") && perm_argmax(r).as_deref() == Some("single")).count();

    let lat: Vec<f64> = rows.iter().flat_map(|r| r.latency.iter().copied()).collect();
    json!({
        "model": model,
        "items": rows.len(),
        "calls": calls,
        "errorRate": rate(errs, calls),
        "noise": {
            "argmaxChangeRate": rate(noise.iter().filter(|n| n.0).count(), noise.len()),
            "tvMedian": opt_num(quantile(&noise.iter().map(|n| n.1).collect::<Vec<_>>(), 0.5)),
        },
        "order": { "flipRate": rate(flips, with_perm.len()), "tvMedian": opt_num(quantile(&order_tv, 0.5)) },
        "q1": {
            "labeled": labeled.len(),
            "positives": ys.iter().filter(|y| **y).count(),
            "auc": opt_num(auc_v),
            "aucCi": ci.map_or(Value::Null, |(lo, hi)| json!([lo, hi])),
            "fpRecognized": rate(fp_recognized, fp_only.len()),
            "familyLabeled": fam.len(),
            "familyAccuracy": rate(fam_ok, fam.len()),
            "regexRows": regex_rows.len(),
            "regexKappa": opt_num(regex_kappa),
            "regexFpRate": rate(regex_fp, regex_rows.len()),
            "calibration": cal.map_or(Value::Null, |(b, a, t)| json!({"before": b, "after": a, "tAll": t})),
        },
        "q2": {
            "chunks": ch.len(),
            "agreeWithDeterministic": rate(agree, ch.len()),
            "jurySaved": saved,
            "missedJuryRequired": missed.len(),
            "k3": k3,
        },
        "latency": { "p50": opt_num(quantile(&lat, 0.5)), "p95": opt_num(quantile(&lat, 0.95)) },
        "costUsd": rows.iter().map(|r| r.cost).sum::<f64>(),
    })
}

/// κ(argmax) и Спирмен p(real) для каждой пары моделей; строки выровнены по элементам.
pub fn cross_model(models: &[String], rows_by_model: &HashMap<String, Vec<Value>>) -> Vec<Value> {
    let mut out = Vec::new();
    for i in 0..models.len() {
        for j in (i + 1)..models.len() {
            let a: Vec<Row> = rows_by_model[&models[i]].iter().map(row_of).collect();
            let b: Vec<Row> = rows_by_model[&models[j]].iter().map(row_of).collect();
            let pairs: Vec<(&Row, &Row)> = a.iter().zip(&b).filter(|(x, y)| x.perm.is_some() && y.perm.is_some()).collect();
            let fp_pairs: Vec<&(&Row, &Row)> = pairs.iter().filter(|(x, _)| x.kind == "fp").collect();
            out.push(json!({
                "a": models[i], "b": models[j], "n": pairs.len(),
                "kappaArgmax": opt_num(cohen_kappa(
                    &pairs.iter().map(|(x, _)| perm_argmax(x)).collect::<Vec<_>>(),
                    &pairs.iter().map(|(_, y)| perm_argmax(y)).collect::<Vec<_>>(),
                )),
                "spearmanPReal": opt_num(spearman(
                    &fp_pairs.iter().map(|(x, _)| p_real(x).unwrap_or(f64::NAN)).collect::<Vec<_>>(),
                    &fp_pairs.iter().map(|(_, y)| p_real(y).unwrap_or(f64::NAN)).collect::<Vec<_>>(),
                )),
            }));
        }
    }
    out
}

/// Критерии этапа 0 → 1 (RFC 21) по сводке модели.
pub fn verdict(s: &Value, timeout_ms: f64) -> Vec<(&'static str, bool)> {
    let f = |p: &str| s.pointer(p).and_then(Value::as_f64);
    let ci_lo = s.pointer("/q1/aucCi/0").and_then(Value::as_f64);
    vec![
        ("ошибки API < 5%", f("/errorRate").is_some_and(|x| x < 0.05)),
        ("смена argmax за повторы ≤ 10%", f("/noise/argmaxChangeRate").is_some_and(|x| x <= 0.1)),
        ("flip перестановок ≤ 15%", f("/order/flipRate").is_some_and(|x| x <= 0.15)),
        (
            "ДИ AUC > 0.5 или κ с регексом ≥ 0.6",
            ci_lo.is_some_and(|x| x > 0.5) || f("/q1/regexKappa").is_some_and(|x| x >= 0.6),
        ),
        ("p95 латентности < таймаута", f("/latency/p95").is_some_and(|x| x < timeout_ms)),
    ]
}

fn fmt(v: Option<f64>, d: usize) -> String {
    v.filter(|x| x.is_finite()).map_or("—".into(), |x| format!("{x:.d$}"))
}

/// Markdown-отчёт. `report`: {date, params:{repeats, permutations, timeoutMs}, corpus, models, cross}.
pub fn markdown(report: &Value) -> String {
    let models = report["models"].as_array().cloned().unwrap_or_default();
    let p = &report["params"];
    let c = &report["corpus"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let mut l = vec![
        format!("# Офлайн-замер decision-моделей — {}", report["date"].as_str().unwrap_or("")),
        String::new(),
        format!(
            "Корпус: {} находок (Q1, размечено {}), {} чанков (Q2, с меткой jury_required {}). Повторов {}, перестановок {}. Gold-находки: {}.",
            n(&c["fp"]), n(&c["fpLabeled"]), n(&c["chunk"]), n(&c["chunkLabeled"]), n(&p["repeats"]), n(&p["permutations"]),
            if c["goldFindings"].as_bool().unwrap_or(false) { "есть" } else { "нет" },
        ),
        String::new(),
        format!("| Метрика | {} |", models.iter().map(|s| format!("`{}`", s["model"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(" | ")),
        format!("|---|{}", "---|".repeat(models.len())),
    ];
    let g = |s: &Value, ptr: &str| s.pointer(ptr).and_then(Value::as_f64);
    let mut row = |name: &str, cell: &dyn Fn(&Value) -> String| {
        l.push(format!("| {name} | {} |", models.iter().map(cell).collect::<Vec<_>>().join(" | ")));
    };
    row("Ошибки API", &|s| fmt(g(s, "/errorRate"), 3));
    row("Шум: смена argmax за повторы", &|s| fmt(g(s, "/noise/argmaxChangeRate"), 3));
    row("Шум: медиана TV повторов", &|s| fmt(g(s, "/noise/tvMedian"), 3));
    row("Порядок: доля flip", &|s| fmt(g(s, "/order/flipRate"), 3));
    row("Порядок: медиана TV перестановок", &|s| fmt(g(s, "/order/tvMedian"), 3));
    row("Q1: AUC p(real), размечено (полож.)", &|s| {
        let ci = match (g(s, "/q1/aucCi/0"), g(s, "/q1/aucCi/1")) {
            (Some(a), Some(b)) => format!("[{a:.2}, {b:.2}]"),
            _ => "—".into(),
        };
        format!("{} {ci} n={} ({})", fmt(g(s, "/q1/auc"), 2), s["q1"]["labeled"], s["q1"]["positives"])
    });
    row("Q1: размеченные FP с p(real) < 0.5", &|s| fmt(g(s, "/q1/fpRecognized"), 2));
    row("Q1: точность семейства (справочно)", &|s| format!("{} n={}", fmt(g(s, "/q1/familyAccuracy"), 2), s["q1"]["familyLabeled"]));
    row("Q1: κ с регекс-семейством", &|s| format!("{} n={}", fmt(g(s, "/q1/regexKappa"), 2), s["q1"]["regexRows"]));
    row("Q1: регекс-FP, которые модель тоже считает FP", &|s| fmt(g(s, "/q1/regexFpRate"), 2));
    row("Q1: ECE до / после LOO-температуры (разведка)", &|s| match s["q1"]["calibration"].as_object() {
        Some(_) => format!(
            "{} / {} (T={})",
            fmt(g(s, "/q1/calibration/before"), 3), fmt(g(s, "/q1/calibration/after"), 3), fmt(g(s, "/q1/calibration/tAll"), 2)
        ),
        None => "—".into(),
    });
    row("Q2: согласие с chunkNeedsSpecialists", &|s| format!("{} n={}", fmt(g(s, "/q2/agreeWithDeterministic"), 2), s["q2"]["chunks"]));
    row("Q2: jury → single (потенц. экономия)", &|s| s["q2"]["jurySaved"].to_string());
    row("Q2: jury_required → single / при p≥0.7 (K3)", &|s| format!("{} / {}", s["q2"]["missedJuryRequired"], s["q2"]["k3"]));
    row("Латентность p50 / p95, мс", &|s| format!("{} / {}", fmt(g(s, "/latency/p50"), 0), fmt(g(s, "/latency/p95"), 0)));
    row("Цена замера, $", &|s| fmt(g(s, "/costUsd"), 4));
    l.push(String::new());
    for x in report["cross"].as_array().cloned().unwrap_or_default() {
        l.push(format!(
            "κ(argmax) `{}` × `{}`: {} (n={}); Спирмен p(real): {}",
            x["a"].as_str().unwrap_or(""), x["b"].as_str().unwrap_or(""),
            fmt(x["kappaArgmax"].as_f64(), 2), x["n"], fmt(x["spearmanPReal"].as_f64(), 2),
        ));
    }
    l.push(String::new());
    l.push("## Критерии этапа 0 → 1".into());
    l.push(String::new());
    let timeout = p["timeoutMs"].as_f64().unwrap_or(20_000.0);
    for s in &models {
        l.push(format!("**{}**", s["model"].as_str().unwrap_or("")));
        for (name, ok) in verdict(s, timeout) {
            l.push(format!("- {} {name}", if ok { "✅" } else { "❌" }));
        }
        l.push(String::new());
    }
    l.join("\n")
}

/// Весь отчёт: вход {date, params, corpus, models:[имя], rows:{модель: [строки]}}.
pub fn report(input: &Value) -> Value {
    let models: Vec<String> = input["models"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
    let rows_by_model: HashMap<String, Vec<Value>> = models
        .iter()
        .map(|m| (m.clone(), input["rows"][m].as_array().cloned().unwrap_or_default()))
        .collect();
    let summaries: Vec<Value> = models.iter().map(|m| summarize(m, &rows_by_model[m], &input["params"])).collect();
    let mut out = json!({
        "date": input["date"], "params": input["params"], "corpus": input["corpus"],
        "models": summaries, "cross": cross_model(&models, &rows_by_model),
    });
    let md = markdown(&out);
    out["markdown"] = json!(md);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} ≉ {b}");
    }

    fn dist(pairs: &[(&str, f64)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), json!(v))).collect()
    }

    #[test]
    fn quantile_и_rng() {
        near(quantile(&[3.0, 1.0, 2.0, 4.0], 0.5).unwrap(), 2.5);
        assert_eq!(quantile(&[], 0.5), None);
        let (mut a, mut b) = (Mulberry32::new(5), Mulberry32::new(5));
        assert_eq!([a.next_f64(), a.next_f64()], [b.next_f64(), b.next_f64()]);
        let x = Mulberry32::new(1).next_f64();
        assert!((0.0..1.0).contains(&x));
    }

    #[test]
    fn auc_края() {
        assert_eq!(auc(&[0.9, 0.8, 0.1, 0.2], &[true, true, false, false]), Some(1.0));
        assert_eq!(auc(&[0.1, 0.2, 0.9, 0.8], &[true, true, false, false]), Some(0.0));
        assert_eq!(auc(&[0.5, 0.5], &[true, false]), Some(0.5));
        assert_eq!(auc(&[0.5, 0.6], &[true, true]), None);
    }

    #[test]
    fn бутстрап_накрывает_05_на_шуме_и_нет_на_сигнале() {
        let noise: Vec<(f64, bool)> = (0..10).map(|i| (((i * 7919) % 10) as f64 / 10.0, i % 2 == 0)).collect();
        let (lo, hi) = auc_ci(&noise, 2000, 7).unwrap();
        assert!(lo < 0.5 && hi > 0.5);
        let signal: Vec<(f64, bool)> = (0..20).map(|i| (if i < 10 { 0.9 - i as f64 * 0.01 } else { 0.1 + i as f64 * 0.01 }, i < 10)).collect();
        assert!(auc_ci(&signal, 2000, 7).unwrap().0 > 0.5);
    }

    #[test]
    fn kappa_и_спирмен() {
        let s = |xs: &[&str]| xs.iter().map(|x| Some((*x).to_string())).collect::<Vec<_>>();
        assert_eq!(cohen_kappa(&s(&["a", "b", "a"]), &s(&["a", "b", "a"])), Some(1.0));
        near(cohen_kappa(&s(&["a", "a", "b", "b"]), &s(&["a", "b", "a", "b"])).unwrap(), 0.0);
        assert_eq!(cohen_kappa(&[None], &s(&["a"])), None);
        near(spearman(&[1.0, 2.0, 3.0, 4.0], &[10.0, 20.0, 30.0, 40.0]).unwrap(), 1.0);
        near(spearman(&[1.0, 2.0, 3.0, 4.0], &[4.0, 3.0, 2.0, 1.0]).unwrap(), -1.0);
        assert_eq!(spearman(&[1.0, 2.0], &[1.0, 2.0]), None);
    }

    #[test]
    fn tv_argmax_шум() {
        near(tv(&dist(&[("a", 0.7), ("b", 0.3)]), &dist(&[("a", 0.4), ("b", 0.6)])), 0.3);
        assert_eq!(argmax_of(&dist(&[("a", 0.2), ("b", 0.8)])).as_deref(), Some("b"));
        let (changed, t) = repeat_noise(&[dist(&[("a", 0.6), ("b", 0.4)]), dist(&[("a", 0.4), ("b", 0.6)])]).unwrap();
        assert!(changed);
        near(t, 0.2);
        assert_eq!(repeat_noise(&[dist(&[("a", 1.0)])]), None);
    }

    #[test]
    fn ece_и_температура() {
        near(ece(&[1.0, 0.0], &[true, false], 10).unwrap(), 0.0);
        near(ece(&[0.9, 0.9], &[true, false], 10).unwrap(), 0.4);
        near(apply_temperature(0.5, 3.0), 0.5);
        assert!(apply_temperature(0.9, 2.0) < 0.9);
        let probs = [0.99, 0.99, 0.01, 0.01, 0.99, 0.01];
        let labels = [true, false, false, true, true, false];
        assert!(fit_temperature(&probs, &labels) > 2.0, "переуверенность → T > 1");
        assert!(loo_temperature_ece(&probs, &labels).is_some());
        assert_eq!(loo_temperature_ece(&[0.5], &[true]), None);
    }

    fn fp_row(label: Option<&str>, p_real: f64, argmax: &str, regex: Option<&str>) -> Value {
        json!({
            "kind": "fp", "label": label, "family": label.map(|_| "f1_in_diff"), "regexFamily": regex,
            "perm": {"probabilities": {"real": p_real, "f1_in_diff": 1.0 - p_real}, "argmax": argmax, "flip": false, "tvMax": 0.01},
            "repeats": [{"real": p_real, "f1_in_diff": 1.0 - p_real}, {"real": p_real, "f1_in_diff": 1.0 - p_real}],
            "latency": [500.0], "cost": 0.001, "errors": [],
        })
    }

    #[test]
    fn summarize_и_отчёт() {
        let rows = vec![
            fp_row(Some("fp"), 0.1, "f1_in_diff", Some("f1_in_diff")),
            fp_row(Some("fp"), 0.2, "f1_in_diff", Some("f1_in_diff")),
            fp_row(Some("real"), 0.9, "real", None),
            json!({"kind": "chunk", "label": "jury_required", "deterministic": "jury",
                   "perm": {"probabilities": {"jury": 0.2, "single": 0.8}, "argmax": "single", "flip": true, "tvMax": 0.3},
                   "repeats": [], "latency": [400.0], "cost": 0.0, "errors": ["x"]}),
        ];
        let s = summarize("m", &rows, &json!({"repeats": 2}));
        assert_eq!(s["calls"], 12);
        near(s["errorRate"].as_f64().unwrap(), 1.0 / 12.0);
        assert_eq!(s["q1"]["auc"], json!(1.0));
        assert_eq!(s["q1"]["fpRecognized"], json!(1.0));
        assert_eq!(s["q1"]["regexKappa"], json!(1.0));
        assert_eq!(s["q2"]["missedJuryRequired"], 1);
        assert_eq!(s["q2"]["k3"], 1, "jury_required → single при p=0.8 — K3");
        near(s["order"]["flipRate"].as_f64().unwrap(), 0.25);

        let rep = report(&json!({
            "date": "2026-10-16", "params": {"repeats": 2, "permutations": 2, "timeoutMs": 20000},
            "corpus": {"fp": 3, "fpLabeled": 3, "chunk": 1, "chunkLabeled": 1, "goldFindings": false},
            "models": ["m", "n"], "rows": {"m": rows.clone(), "n": rows},
        }));
        assert_eq!(rep["cross"][0]["kappaArgmax"], json!(1.0));
        let md = rep["markdown"].as_str().unwrap();
        assert!(md.contains("| Ошибки API | 0.083 | 0.083 |"));
        assert!(md.contains("- ✅ ДИ AUC > 0.5 или κ с регексом ≥ 0.6"));
    }

    #[test]
    fn калибровка_по_одному_классу_не_считается() {
        let rows = vec![fp_row(Some("fp"), 0.1, "f1_in_diff", None); 5];
        let s = summarize("m", &rows, &json!({"repeats": 2}));
        assert_eq!(s["q1"]["calibration"], Value::Null);
        assert_eq!(s["q1"]["auc"], Value::Null);
    }
}
