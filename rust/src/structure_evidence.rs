//! Порт lib/structure-evidence.mjs: evidence receipts (sha256-пиннинг +
//! валидация) и каноническая JSON-сериализация, ПОБАЙТОВО совпадающая с JS
//! canonicalJson из lib/util.mjs — от неё зависят sha256-хеши receipt'ов.
//!
//! Алгоритм каноникализации JS (lib/util.mjs) и его воспроизведение:
//! - объекты: ключи через `Object.keys().sort()` — сортировка V8 идёт по
//!   UTF-16 code units (НЕ codepoints, НЕ байты): astral-символы (≥ U+10000)
//!   сортируются как surrogate pair 0xD800..0xDBFF и потому УПОРЯДОЧИВАЮТСЯ
//!   ПЕРЕД U+E000..U+FFFF. Здесь: `encode_utf16()` + лексикографическое
//!   сравнение Vec<u16>;
//! - строки: `JSON.stringify` (ES2019 well-formed) — `\"` `\\` `\b` `\f` `\n`
//!   `\r` `\t`, прочие c < 0x20 → `\u00xx` (нижний регистр), U+2028/U+2029
//!   выводятся БЕЗ экранирования, не-ASCII — без изменений; lone surrogates
//!   JS экранирует как `\udXXX` — в Rust-строках их нет (serde_json отклоняет
//!   на парсинге, см. отклонения в findings/json_repair);
//! - числа: `JSON.stringify` через ECMA Number::toString — shortest round-trip
//!   цифры с тай-брейком ECMA («ближайшее к точному значению, при равенстве —
//!   чётное»; точный выбор через mini-bignum, т.к. std `{:e}` в тай-кейсах
//!   даёт иную последнюю цифру, чем V8), десятичная запись для
//!   1e-6 ≤ |x| < 1e21, экспоненциальная иначе (`1e+21`, `1e-7`; знак '+' и
//!   ненулевой мантиссы-суффикса правила ECMA), -0 → `0`, NaN/±Infinity →
//!   `null`. Числа прогоняются через f64 (как JS JSON.parse), поэтому
//!   arbitrary_precision-лексема `5.0` → `5`, `1e999` → `null`;
//! - sha256(value): строка — хеш UTF-8 байтов строки, иначе хеш canonicalJson.
//!
//! Отклонения от JS (как у прочих Rust-модулей): lone surrogates на входе
//! непредставимы (serde_json их отклоняет); `Date.parse` в валидаторе —
//! строгое подмножество ISO-8601 с V8-леньностью (день/час 24 rollover,
//! 't'/'z', ±HHMM), RFC-2822 и прочие V8-форматы НЕ принимаются; ошибки
//! не-итерируемых значений — текст как у V8 ("x is not iterable"), но
//! глубокие TypeError-сообщения (.map на не-массиве) аппроксимированы.

use std::cmp::Ordering;
use std::collections::HashSet;

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_string, js_to_number, js_trim, js_truthy};
use crate::structure_scip::{normalize_repo_path, sha256_hex, stable_id};

pub const EVIDENCE_RECEIPT_VERSION: i64 = 1;
pub const TRUST_TIERS: [&str; 3] = ["signed_ci", "trusted_local", "untrusted"];
pub const EVIDENCE_CEILINGS: [&str; 7] = [
    "static_inventory",
    "aggregate_lcov",
    "per_test_execution",
    "assertion_bound",
    "mutation_bound",
    "property_bound",
    "contract_verified",
];

/// Значение по ключу объекта (None для не-объектов и отсутствующего ключа).
pub fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object().and_then(|o| o.get(key))
}

/// JS Map/Set-ключ для значения из JSON: скаляры — по значению (числа через
/// f64: 1e2 и 100 совпадают, как SameValueZero), объекты/массивы — по
/// идентичности (равные, но разные объекты в JS не совпадают).
pub fn value_key(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("bool:{b}"),
        Value::Number(n) => n
            .as_f64()
            .map(|x| format!("num:{x}"))
            .unwrap_or_else(|| format!("num:{n}")),
        Value::String(s) => format!("str:{s}"),
        other => format!("obj:{}", serde_json::to_string(other).unwrap_or_default()),
    }
}

/// `[...new Set(xs)]` для значений из JSON: скаляры дедуплицируются по значению,
/// объекты — никогда (идентичность). Порядок первого появления сохраняется.
pub fn unique_values(xs: &[Value]) -> Vec<Value> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (i, x) in xs.iter().enumerate() {
        let key = match x {
            Value::Object(_) | Value::Array(_) => format!("obj#{i}"),
            scalar => value_key(scalar),
        };
        if seen.insert(key) {
            out.push(x.clone());
        }
    }
    out
}

/// Итерируемое значение (`xs || []` в for..of/spread): массив — элементы,
/// строка — посимвольно (JS итерирует строки), falsy — пусто; прочее truthy —
/// TypeError, как в JS.
/// `_what` (имя поля из JS-выражения) в сообщение не входит — параметр оставлен,
/// чтобы call-site'ы сохраняли привязку к оригиналу.
pub fn iterable_values(v: Option<&Value>, _what: &str) -> Result<Vec<Value>> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()), // null/undefined || []
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(Value::String(s)) => Ok(s.chars().map(|c| json!(c.to_string())).collect()),
        Some(other) if js_truthy(other) => bail!("{}", v8_not_iterable(other)),
        _ => Ok(Vec::new()),
    }
}

/// Текст TypeError не-итерируемого значения как у V8 для for..of/spread:
/// "number 5 is not iterable (cannot read property Symbol(Symbol.iterator))",
/// для null/undefined — "cannot read properties of null (reading ...)".
pub fn v8_not_iterable(v: &Value) -> String {
    const SUFFIX: &str = "(cannot read property Symbol(Symbol.iterator))";
    match v {
        Value::Null => "null is not iterable (cannot read properties of null (reading 'Symbol(Symbol.iterator)'))".into(),
        Value::Bool(b) => format!("boolean {b} is not iterable {SUFFIX}"),
        Value::Number(n) => format!(
            "number {} is not iterable {SUFFIX}",
            js_number_to_string(n.as_f64().unwrap_or(f64::NAN))
        ),
        Value::String(_) | Value::Array(_) => format!("{} is not iterable {SUFFIX}", js_to_string(v)),
        Value::Object(_) => format!("object is not iterable {SUFFIX}"),
    }
}

/// Побайтовое (по UTF-16 code units) сравнение как у V8 для строк:
/// astral-символы сравниваются как surrogate pairs и потому идут перед
/// U+E000..U+FFFF (0xD800 < 0xE000).
pub fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Дефолтный `Array.prototype.sort()` по String(x): конвертация элементов
/// через String, сравнение по UTF-16 code units, стабильно.
pub fn js_default_sort(values: &mut [Value]) {
    values.sort_by(|a, b| utf16_cmp(&js_to_string(a), &js_to_string(b)));
}

/// JS `String(value || '').trim()` — falsy → пустая строка.
fn js_string_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Каноническая JSON-сериализация (побайтовый паритет с lib/util.mjs)
// ---------------------------------------------------------------------------

/// Экранирование строки как у `JSON.stringify` (см. шапку модуля).
pub fn js_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // ES2019 (well-formed JSON.stringify): U+2028/U+2029 — БЕЗ экранирования
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Минимальный неотрицательный bignum (little-endian u32-limbs) для точного
/// выбора shortest-цифр по ECMA: Rust `{:e}` даёт минимальную длину, но в тай-
/// кейсах (два k-значных кандидата на равном расстоянии) последняя цифра
/// отличается от V8 — ECMA требует «ближайшее к точному значению, при
/// равенстве — чётное». Сравнения/дистанции считаются точно.
#[derive(Clone)]
struct BigU(Vec<u32>);

impl BigU {
    fn from_u64(mut v: u64) -> Self {
        let mut limbs = Vec::new();
        while v > 0 {
            limbs.push((v & 0xFFFF_FFFF) as u32);
            v >>= 32;
        }
        BigU(limbs)
    }
    fn trim(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }
    fn shl_bits(&mut self, bits: usize) {
        let words = bits / 32;
        let rem = bits % 32;
        if rem > 0 {
            let mut carry = 0u64;
            for l in self.0.iter_mut() {
                let nv = ((*l as u64) << rem) | carry;
                *l = (nv & 0xFFFF_FFFF) as u32;
                carry = nv >> 32;
            }
            if carry > 0 {
                self.0.push(carry as u32);
            }
        }
        if words > 0 {
            let mut nl = vec![0u32; words];
            nl.extend_from_slice(&self.0);
            self.0 = nl;
        }
    }
    fn mul_small(&mut self, m: u32) {
        let mut carry = 0u64;
        for l in self.0.iter_mut() {
            let nv = (*l as u64) * (m as u64) + carry;
            *l = (nv & 0xFFFF_FFFF) as u32;
            carry = nv >> 32;
        }
        while carry > 0 {
            self.0.push((carry & 0xFFFF_FFFF) as u32);
            carry >>= 32;
        }
    }
    fn mul_pow5(&mut self, k: u32) {
        for _ in 0..k {
            self.mul_small(5);
        }
    }
    fn cmp_big(&self, o: &Self) -> Ordering {
        if self.0.len() != o.0.len() {
            return self.0.len().cmp(&o.0.len());
        }
        for i in (0..self.0.len()).rev() {
            if self.0[i] != o.0[i] {
                return self.0[i].cmp(&o.0[i]);
            }
        }
        Ordering::Equal
    }
    /// self -= o (требуется self >= o).
    fn sub(&mut self, o: &Self) {
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let ov = if i < o.0.len() { o.0[i] as i64 } else { 0 };
            let mut nv = self.0[i] as i64 - ov - borrow;
            if nv < 0 {
                nv += 1 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            self.0[i] = nv as u32;
        }
        self.trim();
    }
}

/// m×2^e и s×10^t в общем целочисленном масштабе: (L, R) с L = m×2^e, R = s×10^t.
fn scaled_pair(m: u64, e: i32, s: u64, t: i32) -> (BigU, BigU) {
    let n2 = e.min(t);
    let mut a = BigU::from_u64(m);
    a.shl_bits((e - n2) as usize);
    let mut b = BigU::from_u64(s);
    b.shl_bits((t - n2) as usize);
    if t >= 0 {
        b.mul_pow5(t as u32);
    } else {
        a.mul_pow5((-t) as u32);
    }
    (a, b)
}

/// Exact-арифметика выбор shortest-цифр по ECMA Number::toString:
/// кандидаты ±2 вокруг цифр std `{:e}` (минимальная длина совпадает), из
/// round-trip кандидатов выбирается ближайший к точному значению x; при
/// равенстве дистанций — чётный (как в спецификации ECMA).
fn ecma_shortest_digits(ax: f64) -> (String, i32) {
    // Декомпозиция ax = m×2^e (нормализованная или субнормальная).
    let bits = ax.to_bits();
    let exp_bits = ((bits >> 52) & 0x7FF) as i64;
    let mantissa = bits & ((1u64 << 52) - 1);
    let (m, e) = if exp_bits == 0 {
        (mantissa, -1074i32)
    } else {
        (mantissa | (1u64 << 52), (exp_bits - 1075) as i32)
    };
    // std-shortest цифры: value ≈ s0×10^t
    let sci = format!("{:e}", ax);
    let (mant, exp) = sci.split_once('e').expect("{:e} всегда содержит e");
    let exp: i32 = exp.parse().expect("экспонента {:e} — целое");
    let s0: u64 = mant.chars().filter(|c| *c != '.').collect::<String>().parse().expect("цифры");
    let t = exp + 1 - sci_digits_len(&sci) as i32;

    let mut best: Option<(BigU, u64)> = None; // (дистанция, s)
    for s in s0.saturating_sub(2)..=s0 + 2 {
        if s == 0 {
            continue;
        }
        let ds = s.to_string();
        if ds.ends_with('0') {
            continue; // кандидат короче — противоречил бы минимальности k
        }
        // round-trip проверка: строка "s e t" — парсится в s×10^t
        let text = format!("{}e{}", ds, t);
        if text.parse::<f64>() != Ok(ax) {
            continue;
        }
        let (a, b) = scaled_pair(m, e, s, t);
        let dist = match a.cmp_big(&b) {
            Ordering::Equal => BigU::from_u64(0),
            Ordering::Greater => {
                let mut d = a;
                d.sub(&b);
                d
            }
            Ordering::Less => {
                let mut d = b;
                d.sub(&a);
                d
            }
        };
        best = match best {
            None => Some((dist, s)),
            Some((bd, bs)) => match dist.cmp_big(&bd) {
                Ordering::Less => Some((dist, s)),
                Ordering::Equal => {
                    // равная дистанция — ECMA выбирает чётное s
                    if s % 2 == 0 && bs % 2 == 1 {
                        Some((dist, s))
                    } else {
                        Some((bd, bs))
                    }
                }
                Ordering::Greater => Some((bd, bs)),
            },
        };
    }
    let (_, s) = best.expect("хотя бы s0 round-trip'ится");
    (s.to_string(), t)
}

/// Число значащих цифр в строке формата {:e} ("d[.ddd]e±X").
fn sci_digits_len(sci: &str) -> usize {
    sci.split_once('e').map(|(m, _)| m.chars().filter(|c| *c != '.').count()).unwrap_or(0)
}

/// JS `String(number)` / ECMA Number::toString для конечного x.
pub fn js_number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into(); // -0 → '0'
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let neg = x < 0.0;
    let (digits, t) = ecma_shortest_digits(x.abs());
    let k = digits.len() as i32;
    // ECMA: value = 0.digits × 10^point
    let point = t + k;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if point >= k && point <= 21 {
        out.push_str(&digits);
        out.push_str(&"0".repeat((point - k) as usize));
    } else if point > 0 && point < k {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    } else if point > -6 && point <= 0 {
        out.push_str("0.");
        out.push_str(&"0".repeat((-point) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        let e = point - 1;
        if e >= 0 {
            out.push('+');
            out.push_str(&e.to_string());
        } else {
            out.push('-');
            out.push_str(&(-e).to_string());
        }
    }
    out
}

/// Порт canonicalJson из lib/util.mjs: рекурсивная сериализация с сортировкой
/// ключей по UTF-16 code units. Ключ сортируется как строка, сериализуется
/// через JSON.stringify-экранирование.
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            // JSON.stringify: NaN/±Infinity → 'null'; прочее — ECMA Number::toString.
            Some(x) if x.is_finite() => js_number_to_string(x),
            _ => "null".into(),
        },
        Value::String(s) => js_json_string(s),
        Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_by(|a, b| utf16_cmp(a, b));
            let inner: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", js_json_string(k), canonical_json(&o[*k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

/// Порт sha256 из lib/util.mjs: строка — хеш её UTF-8 байтов, иначе хеш
/// canonicalJson(value).
pub fn sha256_value(v: &Value) -> String {
    match v {
        Value::String(s) => sha256_hex(s.as_bytes()),
        other => sha256_hex(canonical_json(other).as_bytes()),
    }
}

/// sha256 произвольной строки (utf8), как sha256('...') в JS.
pub fn sha256_str(s: &str) -> String {
    sha256_hex(s.as_bytes())
}

/// Порт stableId из lib/util.mjs (для бинарного op; JS-обёртка реэкспортирует
/// версию из util.mjs).
pub fn stable_id_op(prefix: &str, parts: &[Value]) -> String {
    let strings: Vec<String> = parts
        .iter()
        .map(|p| match p {
            Value::Null => String::new(), // String(null ?? '') — null → ''
            other => js_to_string(other),
        })
        .collect();
    stable_id(prefix, &strings)
}

// ---------------------------------------------------------------------------
// strings() и Date.parse
// ---------------------------------------------------------------------------

/// Порт strings() из lib/structure-evidence.mjs: String(v).trim(), filter
/// truthy, Set-дедуп (по строке), slice(0, max), дефолтный Array.sort.
pub fn strings_field(values: Option<&Value>, max: usize) -> Result<Vec<String>> {
    let items: Vec<Value> = match values {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        // `(values || []).map` на не-массиве — TypeError как в JS.
        Some(other) if js_truthy(other) => bail!("values.map is not a function"),
        _ => Vec::new(),
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let s = js_trim(&js_to_string(&item)).to_string();
        if s.is_empty() {
            continue; // filter(Boolean) после trim
        }
        if seen.insert(s.clone()) {
            out.push(s);
        }
        if out.len() >= max {
            break; // slice(0, max) до сортировки
        }
    }
    out.sort_by(|a, b| utf16_cmp(a, b));
    Ok(out)
}

/// Валидность строки для JS `Date.parse` в валидаторе receipt'ов: строгое
/// подмножество ISO-8601 с V8-леньностью (день rollover — '2026-02-30'
/// принимается, час 24:00 принимается, разделитель T/t/space, зона Z/z или
/// ±HH:MM/±HHMM, 1-2 цифры месяца/дня). RFC-2822 и прочие V8-форматы не
/// принимаются (отклонение, producers всегда шлют ISO).
pub fn js_date_parse_ok(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0usize;
    let num = |b: &[u8], i: &mut usize, max: usize| -> Option<u32> {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() && *i - start < max {
            *i += 1;
        }
        if *i == start {
            return None;
        }
        std::str::from_utf8(&b[start..*i]).ok()?.parse().ok()
    };
    // год: [+-]? 4-6 цифр
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let year_digits = {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() && i - start < 6 {
            i += 1;
        }
        i - start
    };
    if !(4..=6).contains(&year_digits) || i >= b.len() || b[i] != b'-' {
        return false;
    }
    i += 1;
    let month = match num(b, &mut i, 2) {
        Some(m) => m,
        None => return false,
    };
    if !(1..=12).contains(&month) || i >= b.len() || b[i] != b'-' {
        return false;
    }
    i += 1;
    match num(b, &mut i, 2) {
        Some(d) if (1..=31).contains(&d) => {}
        _ => return false,
    }
    if i == b.len() {
        return true; // дата без времени
    }
    if !matches!(b[i], b'T' | b't' | b' ') {
        return false;
    }
    i += 1;
    let hour = match num(b, &mut i, 2) {
        Some(h) => h,
        None => return false,
    };
    if hour > 24 || i >= b.len() || b[i] != b':' {
        return false;
    }
    i += 1;
    let minute = match num(b, &mut i, 2) {
        Some(m) => m,
        None => return false,
    };
    if minute > 59 {
        return false;
    }
    if i < b.len() && b[i] == b':' {
        i += 1;
        match num(b, &mut i, 2) {
            Some(sec) if sec <= 59 => {}
            _ => return false,
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return false;
            }
        }
    }
    if i == b.len() {
        return true; // время без зоны
    }
    match b[i] {
        b'Z' | b'z' => i + 1 == b.len(),
        b'+' | b'-' => {
            i += 1;
            match num(b, &mut i, 2) {
                Some(oh) if oh <= 23 => {}
                _ => return false,
            }
            if i < b.len() && b[i] == b':' {
                i += 1;
            }
            match num(b, &mut i, 2) {
                Some(om) if om <= 59 => {}
                _ => return false,
            }
            i == b.len()
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Evidence receipts
// ---------------------------------------------------------------------------

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn is_head_sha(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Порт validateEvidenceReceipt: список ошибок (пустой — валидно). Массив
/// receipt отклоняется ('receipt must be an object'); не-итерируемый
/// subjectRefs — TypeError как в JS (bail).
pub fn validate_evidence_receipt(receipt: Option<&Value>) -> Result<Vec<String>> {
    let v = match receipt {
        Some(x) if !x.is_null() => x,
        _ => return Ok(vec!["receipt must be an object".into()]),
    };
    let obj = match v {
        Value::Object(o) => o,
        // typeof [] === 'object', но Array.isArray отсекает; строки/числа — тоже сюда.
        _ => return Ok(vec!["receipt must be an object".into()]),
    };
    let mut errors: Vec<String> = Vec::new();
    match get(v, "version").and_then(Value::as_f64) {
        Some(x) if x == 1.0 => {}
        _ => errors.push("version must be 1".into()),
    }
    for field in [
        "provider",
        "toolVersion",
        "capability",
        "headSha",
        "configHash",
        "scopeHash",
        "observedAt",
    ] {
        if js_trim(&js_string_or_empty(obj.get(field))).is_empty() {
            errors.push(format!("{field} is required"));
        }
    }
    if !is_head_sha(&js_string_or_empty(obj.get("headSha"))) {
        errors.push("headSha must be a git-like hex sha".into());
    }
    if !is_sha256_hex(&js_string_or_empty(obj.get("configHash"))) {
        errors.push("configHash must be sha256".into());
    }
    if !is_sha256_hex(&js_string_or_empty(obj.get("scopeHash"))) {
        errors.push("scopeHash must be sha256".into());
    }
    match obj.get("artifact") {
        // typeof [] === 'object': массив проходит сюда, uri/sha256 undefined → ошибки полей
        Some(a) if a.is_object() || a.is_array() => {
            if js_trim(&js_string_or_empty(get(a, "uri"))).is_empty() {
                errors.push("artifact.uri is required".into());
            }
            if !is_sha256_hex(&js_string_or_empty(get(a, "sha256"))) {
                errors.push("artifact.sha256 must be sha256".into());
            }
        }
        _ => errors.push("artifact is required".into()),
    }
    match obj.get("observation") {
        Some(o) if o.is_object() || o.is_array() => {
            if js_trim(&js_string_or_empty(get(o, "type"))).is_empty() {
                errors.push("observation.type is required".into());
            }
        }
        _ => errors.push("observation is required".into()),
    }
    match get(v, "trustTier").and_then(Value::as_str) {
        Some(s) if TRUST_TIERS.contains(&s) => {}
        _ => errors.push("invalid trustTier".into()),
    }
    match get(v, "evidenceCeiling").and_then(Value::as_str) {
        Some(s) if EVIDENCE_CEILINGS.contains(&s) => {}
        _ => errors.push("invalid evidenceCeiling".into()),
    }
    if obj.get("limitations").map(|l| !l.is_array()).unwrap_or(true) {
        errors.push("limitations must be an array".into());
    }
    if !js_date_parse_ok(&js_string_or_empty(obj.get("observedAt"))) {
        errors.push("observedAt must be ISO-8601".into());
    }
    for r in iterable_values(obj.get("subjectRefs").filter(|x| js_truthy(x)), "receipt.subjectRefs")? {
        if js_trim(&js_string_or_empty(Some(&r))).is_empty() {
            errors.push("subjectRefs cannot contain empty values".into());
        }
    }
    // receipt.observation?.path != null → normalizeRepoPath, ошибка — в список
    if let Some(o) = obj.get("observation").filter(|x| x.is_object() || x.is_array()) {
        if let Some(path) = get(o, "path").filter(|p| !p.is_null()) {
            let raw = if js_truthy(path) {
                js_to_string(path)
            } else {
                String::new()
            };
            if let Err(e) = normalize_repo_path(&raw) {
                errors.push(format!("{e:#}"));
            }
        }
    }
    Ok(errors)
}

/// `{...input.observation || {}}` — spread own enumerable: объект — копия,
/// массив — ключи-индексы, строка — посимвольно, прочее/falsy — пусто.
fn js_spread_object(v: Option<&Value>) -> Map<String, Value> {
    let mut out = Map::new();
    match v.filter(|x| js_truthy(x)) {
        Some(Value::Object(o)) => out.clone_from(o),
        Some(Value::Array(a)) => {
            for (i, item) in a.iter().enumerate() {
                out.insert(i.to_string(), item.clone());
            }
        }
        Some(Value::String(s)) => {
            for (i, c) in s.chars().enumerate() {
                out.insert(i.to_string(), json!(c.to_string()));
            }
        }
        _ => {}
    }
    out
}

/// Порт createEvidenceReceipt: нормализация + валидация; при ошибках —
/// throw с `invalid evidence receipt: ...` (в бинаре — exit 2).
/// `now` — ISO-замена `new Date().toISOString()` для дефолта observedAt
/// (fs/часы живут в JS-обёртке).
pub fn create_evidence_receipt(input: Option<&Value>, now: &str) -> Result<Value> {
    // JS: input.observation на null/undefined — TypeError; на прочих скалярах —
    // property access даёт undefined (пустые поля → ошибки валидации).
    if matches!(input, None | Some(Value::Null)) {
        bail!("Cannot read properties of null (reading 'observation')");
    }
    let input = input.filter(|x| js_truthy(x));
    let get_in = |key: &str| input.and_then(|i| get(i, key));

    let mut observation = js_spread_object(get_in("observation"));
    observation.shift_remove("covered"); // delete observation.covered
    if let Some(path) = observation.get("path").filter(|p| !p.is_null()) {
        // observation.path != null → normalizeRepoPath (ошибка пробрасывается)
        let raw = if js_truthy(path) {
            js_to_string(path)
        } else {
            String::new()
        };
        observation.insert("path".into(), json!(normalize_repo_path(&raw)?));
    }

    let mut receipt = Map::new();
    receipt.insert("version".into(), json!(EVIDENCE_RECEIPT_VERSION));
    receipt.insert(
        "provider".into(),
        json!(js_trim(&js_string_or_empty(get_in("provider")))),
    );
    receipt.insert(
        "toolVersion".into(),
        json!(js_trim(&js_string_or_empty(get_in("toolVersion")))),
    );
    receipt.insert(
        "capability".into(),
        json!(js_trim(&js_string_or_empty(get_in("capability")))),
    );
    receipt.insert(
        "headSha".into(),
        json!(js_trim(&js_string_or_empty(get_in("headSha")))),
    );
    receipt.insert(
        "testRunId".into(),
        match get_in("testRunId") {
            None | Some(Value::Null) => Value::Null,
            Some(v) => json!(js_to_string(v)),
        },
    );
    receipt.insert(
        "configHash".into(),
        json!(js_string_or_empty(get_in("configHash"))),
    );
    receipt.insert(
        "scopeHash".into(),
        json!(js_string_or_empty(get_in("scopeHash"))),
    );
    receipt.insert("subjectRefs".into(), json!(strings_field(get_in("subjectRefs"), 100)?));
    let artifact = get_in("artifact").filter(|a| a.is_object());
    receipt.insert(
        "artifact".into(),
        json!({
            "uri": js_trim(&js_string_or_empty(artifact.and_then(|a| get(a, "uri")))),
            "sha256": js_string_or_empty(artifact.and_then(|a| get(a, "sha256"))),
        }),
    );
    receipt.insert("observation".into(), Value::Object(observation));
    receipt.insert(
        "trustTier".into(),
        get_in("trustTier")
            .filter(|v| js_truthy(v))
            .cloned()
            .unwrap_or_else(|| json!("trusted_local")),
    );
    receipt.insert(
        "evidenceCeiling".into(),
        get_in("evidenceCeiling")
            .filter(|v| js_truthy(v))
            .cloned()
            .unwrap_or_else(|| json!("static_inventory")),
    );
    receipt.insert(
        "limitations".into(),
        json!(strings_field(get_in("limitations"), 50)?),
    );
    // complete: input.complete !== false (строгое — только literal false → false);
    // truncated: input.truncated === true
    receipt.insert(
        "complete".into(),
        json!(!matches!(get_in("complete"), Some(Value::Bool(false)))),
    );
    receipt.insert(
        "truncated".into(),
        json!(matches!(get_in("truncated"), Some(Value::Bool(true)))),
    );
    receipt.insert(
        "exitStatus".into(),
        match get_in("exitStatus") {
            Some(v @ Value::Number(n)) if n.as_f64().is_some_and(|x| x.is_finite() && x.fract() == 0.0) => {
                v.clone()
            }
            _ => Value::Null,
        },
    );
    receipt.insert(
        "observedAt".into(),
        match get_in("observedAt").filter(|v| js_truthy(v)) {
            Some(v) => v.clone(),
            None => json!(now),
        },
    );

    let errors = validate_evidence_receipt(Some(&Value::Object(receipt.clone())))?;
    if !errors.is_empty() {
        bail!("invalid evidence receipt: {}", errors.join("; "));
    }
    Ok(Value::Object(receipt))
}

/// Порт receiptRef: `receipt:` + sha256(receipt) (canonical для объектов).
pub fn receipt_ref(receipt: &Value) -> String {
    format!("receipt:{}", sha256_value(receipt))
}

/// js_to_number для thresholds-опций: null/undefined → default, иначе как JS.
pub fn opt_number(v: Option<&Value>, default: f64) -> f64 {
    match v {
        None | Some(Value::Null) => default,
        Some(x) => js_to_number(x).unwrap_or(f64::NAN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_by_utf16_units() {
        // astral-символ (surrogate pair 0xD834...) сортируется ПЕРЕД 'z' (0x7A)? Нет:
        // 0xD834 > 0x7A — значит ПОСЛЕ BMP-строк, но ПЕРЕД 0xE000+.
        let v = json!({"𝄞": 1, "": 2, "z": 3, "a": 4});
        // UTF-16: ''(пусто) < 'a'(0x61) < 'z'(0x7A) < '𝄞'(0xD834 0xDD1E) < '(0xE000)
        assert_eq!(canonical_json(&v), "{\"\":2,\"a\":4,\"z\":3,\"𝄞\":1}");
    }

    #[test]
    fn canonical_json_escapes_like_json_stringify() {
        // U+2028/U+2029 (ES2019) — без экранирования; контролы < 0x20 — \u00xx
        let v = json!({"k\n\"\\": "v\u{2028}\u{2029}\u{1f}\t"});
        assert_eq!(
            canonical_json(&v),
            "{\"k\\n\\\"\\\\\":\"v\u{2028}\u{2029}\\u001f\\t\"}"
        );
        // не-ASCII (BMP и astral) — без экранирования
        assert_eq!(canonical_json(&json!("héllo𝄞")), "\"héllo𝄞\"");
        assert_eq!(canonical_json(&json!("")), "\"\"");
        assert_eq!(canonical_json(&json!([])), "[]");
        assert_eq!(canonical_json(&json!({})), "{}");
    }

    #[test]
    fn canonical_json_numbers_match_ecma() {
        // Вектора сверены с node:
        // JSON.stringify через lib/util.mjs canonicalJson.
        let cases: Vec<(&str, &str)> = vec![
            ("1", "1"),
            ("1.0", "1"),          // лексема 5.0 → f64 → '5'
            ("-0", "0"),           // -0 → '0'
            ("1.5", "1.5"),
            ("0.1", "0.1"),
            ("0.30000000000000004", "0.30000000000000004"),
            ("1e21", "1e+21"),
            ("1e20", "100000000000000000000"),
            ("1e-7", "1e-7"),
            ("1e-6", "0.000001"),
            ("123456789123456789", "123456789123456780"),
            ("5e-324", "5e-324"),
            ("1.7976931348623157e308", "1.7976931348623157e+308"),
            ("2.5e-8", "2.5e-8"),
            ("1e999", "null"),     // Infinity → null
            ("-1e999", "null"),
            ("0.00000123", "0.00000123"),
            ("100", "100"),
            ("21.5", "21.5"),
            ("4.35e-2", "0.0435"),
            ("-1.5", "-1.5"),
        ];
        for (input, expected) in cases {
            let v: Value = serde_json::from_str(input).unwrap();
            assert_eq!(canonical_json(&v), expected, "input {input}");
        }
        assert_eq!(canonical_json(&json!({"b": [1, 1.5, null], "a": true})), "{\"a\":true,\"b\":[1,1.5,null]}");
    }

    #[test]
    fn sha256_matches_js() {
        assert_eq!(sha256_value(&json!("none")).len(), 64);
        // строка — прямой хеш utf8, объект — canonical
        assert_eq!(sha256_value(&json!("abc")), sha256_str("abc"));
        // 1 и 1.0 через f64 канонизируются в '1' — хеши совпадают
        assert_eq!(sha256_value(&json!({"a": 1})), sha256_value(&json!({"a": 1.0})));
        // порядок ключей каноникализации не влияет на хеш
        assert_eq!(sha256_value(&json!({"a": 1, "b": 2})), sha256_value(&json!({"b": 2, "a": 1})));
    }

    #[test]
    fn date_parse_subset() {
        for ok in [
            "2026-09-02T00:00:00.000Z",
            "2026-01-01",
            "2026-1-1",
            "2026-02-30", // V8 rollover
            "2026-01-01T24:00:00Z",
            "2026-01-01 12:00:00",
            "2026-01-01t12:00:00z",
            "2026-01-01T12:00:00+0300",
            "2026-01-01T12:00:00.123456789Z",
            "+002026-01-01T00:00:00Z",
            "2026-01-01T12:00",
        ] {
            assert!(js_date_parse_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "abc",
            "2026-13-01",
            "2026-00-01",
            "2026-01-00",
            "2026-01-01T25:00:00",
            "2026-01-01T12:60:00Z",
            "2026-01-01T12Z",
            "2026-01-01T12",
            "2026-01-01 12",
            "2026-01-01T12:00:00+03:60",
            "2026/01/01",
        ] {
            assert!(!js_date_parse_ok(bad), "{bad}");
        }
    }

    fn valid_input() -> Value {
        json!({
            "provider": "unit",
            "toolVersion": "1",
            "capability": "test",
            "headSha": "abcdef1234567890",
            "configHash": sha256_str("config"),
            "scopeHash": sha256_str("."),
            "artifact": { "uri": "memory:test", "sha256": sha256_str("artifact") },
            "observation": { "type": "sample", "value": true, "covered": true },
            "trustTier": "trusted_local",
            "evidenceCeiling": "static_inventory",
            "limitations": ["test only"],
            "observedAt": "2026-09-02T00:00:00.000Z",
        })
    }

    #[test]
    fn create_receipt_strips_covered_and_validates() {
        let receipt = create_evidence_receipt(Some(&valid_input()), "2026-09-03T00:00:00.000Z").unwrap();
        assert_eq!(get(&receipt, "version"), Some(&json!(1)));
        let observation = get(&receipt, "observation").unwrap();
        assert!(get(observation, "covered").is_none());
        assert_eq!(get(observation, "type"), Some(&json!("sample")));
        assert_eq!(validate_evidence_receipt(Some(&receipt)).unwrap(), Vec::<String>::new());
        // receiptRef стабилен
        assert_eq!(receipt_ref(&receipt), format!("receipt:{}", sha256_value(&receipt)));
    }

    #[test]
    fn create_receipt_applies_defaults() {
        let input = json!({
            "provider": "p", "toolVersion": "1", "capability": "c",
            "headSha": "abcdef1",
            "configHash": sha256_str("c"), "scopeHash": sha256_str("s"),
            "artifact": { "uri": "u", "sha256": sha256_str("a") },
            "observation": { "type": "t" },
            "observedAt": "2026-09-02T00:00:00.000Z",
        });
        let receipt = create_evidence_receipt(Some(&input), "NOW").unwrap();
        assert_eq!(get(&receipt, "trustTier"), Some(&json!("trusted_local")));
        assert_eq!(get(&receipt, "evidenceCeiling"), Some(&json!("static_inventory")));
        assert_eq!(get(&receipt, "complete"), Some(&json!(true)));
        assert_eq!(get(&receipt, "truncated"), Some(&json!(false)));
        assert_eq!(get(&receipt, "exitStatus"), Some(&Value::Null));
        assert_eq!(get(&receipt, "testRunId"), Some(&Value::Null));
        assert_eq!(get(&receipt, "subjectRefs"), Some(&json!([])));
        assert_eq!(get(&receipt, "limitations"), Some(&json!([])));
    }

    #[test]
    fn create_receipt_rejects_invalid() {
        let mut bad = valid_input();
        bad["headSha"] = json!("bad");
        let err = create_evidence_receipt(Some(&bad), "NOW").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.starts_with("invalid evidence receipt: "), "{msg}");
        assert!(msg.contains("headSha must be a git-like hex sha"), "{msg}");
    }

    #[test]
    fn create_receipt_normalizes_observation_path() {
        let mut input = valid_input();
        input["observation"] = json!({ "type": "file", "path": ".\\src\\x.ts" });
        let receipt = create_evidence_receipt(Some(&input), "NOW").unwrap();
        assert_eq!(
            get(get(&receipt, "observation").unwrap(), "path"),
            Some(&json!("src/x.ts"))
        );
    }

    #[test]
    fn validate_flags_field_errors() {
        let receipt = create_evidence_receipt(Some(&valid_input()), "NOW").unwrap();
        // artifact как массив — typeof [] === 'object': ошибки полей, а не 'artifact is required'
        let mut a = receipt.clone();
        a["artifact"] = json!([]);
        let errors = validate_evidence_receipt(Some(&a)).unwrap();
        assert!(errors.contains(&"artifact.uri is required".to_string()), "{errors:?}");
        assert!(errors.contains(&"artifact.sha256 must be sha256".to_string()), "{errors:?}");
        // limitations не-массив
        let mut l = receipt.clone();
        l["limitations"] = json!("nope");
        let errors = validate_evidence_receipt(Some(&l)).unwrap();
        assert!(errors.contains(&"limitations must be an array".to_string()), "{errors:?}");
        // невалидный observedAt
        let mut o = receipt.clone();
        o["observedAt"] = json!("not-a-date");
        let errors = validate_evidence_receipt(Some(&o)).unwrap();
        assert!(errors.contains(&"observedAt must be ISO-8601".to_string()), "{errors:?}");
        // не-объект receipt
        assert_eq!(
            validate_evidence_receipt(Some(&json!([1, 2]))).unwrap(),
            vec!["receipt must be an object".to_string()]
        );
    }

    #[test]
    fn strings_field_dedups_trims_slices_sorts() {
        let v = json!([" b ", "a", "b", "", "a", 10, "10"]);
        // String(10).trim() === '10' — дедуп с '10'
        assert_eq!(strings_field(Some(&v), 100).unwrap(), vec!["10", "a", "b"]);
        let many: Value = (0..150).map(|i| json!(format!("k{i:03}"))).collect();
        let out = strings_field(Some(&many), 100).unwrap();
        assert_eq!(out.len(), 100);
        assert!(out.contains(&"k000".to_string()));
        assert!(!out.contains(&"k100".to_string())); // slice до сортировки
    }
}
