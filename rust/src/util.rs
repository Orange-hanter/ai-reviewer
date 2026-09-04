//! Порт lib/util.mjs (RFC 04, задача T04) — канонический дом мелких утилит движка.
//!
//! Часть функций util.mjs была портирована раньше вместе с потребителем (env-хелперы
//! в config.rs, backoff/isTransient в llm.rs, publicErrorLabel в report.rs, канонические
//! примитивы в structure_evidence.rs / structure_scip.rs). Дублировать их здесь нельзя —
//! `sha256`/`stableId`/`canonicalJson` обязаны давать одну величину во всех точках
//! (см. CLAUDE.md, «Сходимость между прогонами»), поэтому util.rs их **реэкспортирует**.
//! Новыми здесь являются только те функции, у которых в Rust ещё не было потребителя:
//! `redact_secrets_for_log`, `resolve_contained_path`, `Logger` и env-хелперы над
//! `HashMap<String, String>` (в config.rs они живут над JSON-объектом `Map<String, Value>`,
//! потому что env туда приходит из JSON-контракта моста, а не из процесса).
//!
//! Не портированы намеренно:
//!   - `sleep` — в синхронном Rust не нужен (`std::thread::sleep` на месте вызова);
//!   - `mapLimit` и `createAdaptiveSemaphore` — задача T06, модуль `concurrency.rs`.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Result};
use regex::Regex;

// ---------------------------------------------------------------------------
// Реэкспорт уже портированных функций lib/util.mjs
// ---------------------------------------------------------------------------

/// `canonicalJson` — сортировка ключей по UTF-16 code units, JSON.stringify-экранирование.
pub use crate::structure_evidence::canonical_json;
/// `sha256(value)` для не-строк: хеш `canonicalJson(value)`.
pub use crate::structure_evidence::sha256_value;
/// `backoffMs` — Retry-After в секундах (cap 30с) либо экспонента 2^attempt (cap 15с).
pub use crate::llm::backoff_ms;
/// `isTransient` — 429 или любой 5xx.
pub use crate::llm::is_transient;
/// `parseModelList` — split/trim/dedupe по первому вхождению + вычитание `exclude`.
pub use crate::config::parse_model_list;
/// `publicErrorLabel` — категория ошибки для ПУБЛИЧНОГО sticky (без сырого текста).
pub use crate::report::public_error_label;
/// `normalizeRepoPath` — валидатор repo-relative пути (без абсолютных, `..` и NUL).
pub use crate::structure_scip::normalize_repo_path;
/// `sha256(string)` — хеш UTF-8 байтов строки.
pub use crate::structure_scip::sha256_hex;
/// `stableId(prefix, ...parts)` — `prefix:` + первые 24 hex от sha256(parts.join('\0')).
pub use crate::structure_scip::stable_id;

/// `sha256` над строкой — алиас под имя из lib/util.mjs.
#[inline]
pub fn sha256_str(value: &str) -> String {
    sha256_hex(value.as_bytes())
}

// ---------------------------------------------------------------------------
// JS-совместимые примитивы
// ---------------------------------------------------------------------------

/// Пробельные символы JS-регекса `\s` (WhiteSpace + LineTerminator из спецификации).
/// Отличается от `char::is_whitespace` в двух точках: U+0085 (NEL) для JS не пробел,
/// а U+FEFF (ZWNBSP) — пробел.
fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{000B}'
            | '\u{000C}'
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

/// Символы, которые JS-точка `.` (без флага `s`) не матчит.
fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Символ слова для ASCII-версии `\b` (JS-регексы без флага `u` считают словом
/// только `[A-Za-z0-9_]`).
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `Number(string)` из JS: trim по JS-пробелам, пустая строка → 0, `Infinity`
/// со знаком, литералы `0x`/`0o`/`0b`, иначе десятичный литерал или NaN.
/// Отдельно отвергаются `inf`/`infinity`/`nan`, которые парсит Rust, но не JS.
fn js_number(raw: &str) -> f64 {
    let t = raw.trim_matches(is_js_ws);
    if t.is_empty() {
        return 0.0;
    }
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    if body == "Infinity" {
        return if t.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if body.eq_ignore_ascii_case("inf")
        || body.eq_ignore_ascii_case("infinity")
        || body.eq_ignore_ascii_case("nan")
    {
        return f64::NAN;
    }
    // Радиксные литералы допускаются только без знака (как в JS).
    if t.len() > 2 && t.starts_with('0') {
        let radix = match t.as_bytes()[1] {
            b'x' | b'X' => Some(16),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        };
        if let Some(radix) = radix {
            return u128::from_str_radix(&t[2..], radix)
                .map(|n| n as f64)
                .unwrap_or(f64::NAN);
        }
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

// ---------------------------------------------------------------------------
// Env-хелперы
// ---------------------------------------------------------------------------

/// Снимок окружения процесса для `num_env`/`ratio_env`/`env_flag`: как и в JS,
/// env передаётся аргументом, а сам процесс читается ровно в одной точке.
pub fn os_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

/// `numEnv`: конечное число строго больше нуля, иначе дефолт.
pub fn num_env(env: &HashMap<String, String>, name: &str, def: f64) -> f64 {
    match env.get(name).map(|s| js_number(s)) {
        Some(n) if n.is_finite() && n > 0.0 => n,
        _ => def,
    }
}

/// `ratioEnv`: конечное число в диапазоне [0, 1] (доли/рейты), иначе дефолт.
pub fn ratio_env(env: &HashMap<String, String>, name: &str, def: f64) -> f64 {
    match env.get(name).map(|s| js_number(s)) {
        Some(n) if n.is_finite() && (0.0..=1.0).contains(&n) => n,
        _ => def,
    }
}

/// `envFlag`: `0/false/no/off` → false, `1/true/yes/on` → true, пусто и всё
/// остальное → `default_on`.
pub fn env_flag(env: &HashMap<String, String>, name: &str, default_on: bool) -> bool {
    let Some(raw) = env.get(name) else {
        return default_on;
    };
    let v = raw.trim_matches(is_js_ws).to_lowercase();
    match v.as_str() {
        "" => default_on,
        "0" | "false" | "no" | "off" => false,
        "1" | "true" | "yes" | "on" => true,
        _ => default_on,
    }
}

// ---------------------------------------------------------------------------
// createLogger
// ---------------------------------------------------------------------------

/// `createLogger(prefix)`: печать в stdout с постоянным префиксом.
#[derive(Debug, Clone)]
pub struct Logger {
    prefix: String,
}

impl Default for Logger {
    fn default() -> Self {
        Self::new("[pr-review]")
    }
}

impl Logger {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Готовая строка вывода — вынесена отдельно, чтобы её можно было проверить
    /// в тесте, не перехватывая stdout.
    pub fn line(&self, args: fmt::Arguments<'_>) -> String {
        format!("{} {}", self.prefix, args)
    }

    /// Аналог `console.log(prefix, ...args)`. Вызывается как
    /// `logger.log(format_args!("модель {model} упала"))`.
    pub fn log(&self, args: fmt::Arguments<'_>) {
        println!("{}", self.line(args));
    }
}

// ---------------------------------------------------------------------------
// resolveContainedPath
// ---------------------------------------------------------------------------

/// `path.resolve(base, target)` в POSIX-семантике: абсолютный `target` побеждает,
/// иначе склейка с `base`; `.`/`..` снимаются лексически, без обращения к ФС.
fn js_resolve(base: &Path, target: &str) -> PathBuf {
    let source = if target.starts_with('/') {
        target.to_string()
    } else {
        format!("{}/{}", base.to_string_lossy(), target)
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in source.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    PathBuf::from(format!("/{}", parts.join("/")))
}

/// `realpathOrAncestor`: realpath цели либо ближайшего существующего предка —
/// чтобы containment можно было проверить и для ещё не созданного файла.
fn realpath_or_ancestor(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&current) {
            return Ok(real);
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => bail!("path escapes allowed root: {}", path.display()),
        }
    }
}

/// Containment пути внутри root: realpath root'а и цели (или ближайшего существующего
/// предка — для записи в ещё не созданные каталоги), поэтому symlink внутри root,
/// указывающий наружу, проверку не обходит. `must_exist` — для чтения: цель обязана
/// существовать, realpath раскрывает symlink самой цели.
///
/// Возвращается лексически разрешённый путь (как в JS — `candidate`, а не realpath),
/// чтобы вызывающий работал с тем же именем, что передал.
///
/// Расхождение с JS: при `must_exist` и отсутствующей цели JS пробрасывает исходную
/// ошибку ENOENT — здесь возвращается та же `std::io::Error`, завёрнутая в `anyhow`,
/// так что вызывающий может достать её через `downcast_ref::<std::io::Error>()`.
pub fn resolve_contained_path(
    target: &str,
    root_dir: Option<&Path>,
    must_exist: bool,
) -> Result<PathBuf> {
    // `resolve(rootDir || process.cwd())`: относительный root — от рабочего каталога,
    // абсолютный побеждает базу (это уже учтено в `js_resolve`).
    let cwd = std::env::current_dir()?;
    let root_raw = match root_dir {
        Some(p) => p.to_path_buf(),
        None => cwd.clone(),
    };
    let root = std::fs::canonicalize(js_resolve(&cwd, &root_raw.to_string_lossy()))?;
    let candidate = js_resolve(&root, target);

    let real = if must_exist {
        match std::fs::canonicalize(&candidate) {
            Ok(real) => real,
            Err(error) => {
                // Цели нет: containment проверяем по ближайшему существующему предку,
                // и если он внутри root — пробрасываем исходную ошибку (ENOENT).
                let ancestor = realpath_or_ancestor(&candidate)?;
                if ancestor != root && !ancestor.starts_with(&root) {
                    bail!("path escapes allowed root: {target}");
                }
                return Err(error.into());
            }
        }
    } else {
        realpath_or_ancestor(&candidate)?
    };

    if real != root && !real.starts_with(&root) {
        bail!("path escapes allowed root: {target}");
    }
    Ok(candidate)
}

// ---------------------------------------------------------------------------
// redactSecretsForLog
// ---------------------------------------------------------------------------

/// Ключевые слова из регексов 2 и 3 в порядке альтернатив JS-регекса.
/// `[_-]?` между частями — опциональный разделитель (`api_key`, `api-key`, `apikey`).
const SECRET_KEYWORDS: [(&str, Option<&str>); 5] = [
    ("api", Some("key")),
    ("access", Some("token")),
    ("client", Some("secret")),
    ("secret", None),
    ("password", None),
];

fn match_literal_ci(chars: &[char], at: usize, lit: &str) -> Option<usize> {
    let mut p = at;
    for want in lit.chars() {
        let got = *chars.get(p)?;
        if got.to_ascii_lowercase() != want {
            return None;
        }
        p += 1;
    }
    Some(p)
}

/// Одна альтернатива `(api[_-]?key|access[_-]?token|client[_-]?secret|secret|password)`
/// на позиции `at` (с проверкой `\b` слева). Возвращает конец ключевого слова.
fn match_secret_keyword(chars: &[char], at: usize) -> Option<usize> {
    if at > 0 && is_word(chars[at - 1]) {
        return None;
    }
    for (head, tail) in SECRET_KEYWORDS {
        let Some(p) = match_literal_ci(chars, at, head) else {
            continue;
        };
        let Some(tail) = tail else { return Some(p) };
        // `[_-]?` жадный, но с откатом: `api__key` не матчится ни с разделителем, ни без.
        if matches!(chars.get(p), Some('_') | Some('-')) {
            if let Some(end) = match_literal_ci(chars, p + 1, tail) {
                return Some(end);
            }
        }
        if let Some(end) = match_literal_ci(chars, p, tail) {
            return Some(end);
        }
    }
    None
}

/// `\s*[:=]\s*` после ключевого слова. Возвращает (начало пробелов после разделителя,
/// конец пробелов после разделителя).
fn match_assign(chars: &[char], kw_end: usize) -> Option<(usize, usize)> {
    let mut p = kw_end;
    while p < chars.len() && is_js_ws(chars[p]) {
        p += 1;
    }
    if !matches!(chars.get(p), Some(':') | Some('=')) {
        return None;
    }
    p += 1;
    let ws_start = p;
    let mut ws_end = p;
    while ws_end < chars.len() && is_js_ws(chars[ws_end]) {
        ws_end += 1;
    }
    Some((ws_start, ws_end))
}

/// Тело `(?:\\.|(?!\2).)*\2` из JS-регекса: greedy-звезда с откатом, где `\\.`
/// пробуется раньше одиночного символа. DP справа налево воспроизводит порядок
/// перебора backtracking-движка за линейное время (рекурсии нет — текст лога
/// может быть большим). Возвращает позицию ЗА закрывающей кавычкой.
fn scan_quoted_value(chars: &[char], start: usize, quote: char) -> Option<usize> {
    let len = chars.len();
    // end[i] — результат звезды, начатой на позиции start + i.
    let mut end: Vec<Option<usize>> = vec![None; len - start + 1];
    for p in (start..=len).rev() {
        let i = p - start;
        let mut result = None;
        if p < len {
            let c = chars[p];
            // Альтернатива 1: `\\.` — обратный слэш и любой не-терминатор строки.
            if c == '\\' && p + 1 < len && !is_line_terminator(chars[p + 1]) && p + 2 <= len {
                result = end[i + 2];
            }
            // Альтернатива 2: `(?!\2).` — любой символ, кроме кавычки и терминатора.
            if result.is_none() && c != quote && !is_line_terminator(c) {
                result = end[i + 1];
            }
            // Выход из звезды: закрывающая кавычка.
            if result.is_none() && c == quote {
                result = Some(p + 1);
            }
        }
        end[i] = result;
    }
    end[0]
}

/// Регекс 2: `\b(keyword)\s*[:=]\s*(['"])(?:\\.|(?!\2).)*\2` → `$1=***`.
/// Отдельным проходом (как в JS): слитый с регексом 3 проход даёт другой результат
/// на строках вида `secret=a password="b, c" d`.
fn redact_quoted_pass(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let matched = match_secret_keyword(&chars, i)
            .and_then(|kw_end| match_assign(&chars, kw_end).map(|a| (kw_end, a)))
            .and_then(|(kw_end, (_, ws_end))| {
                let quote = *chars.get(ws_end)?;
                if quote != '\'' && quote != '"' {
                    return None;
                }
                let end = scan_quoted_value(&chars, ws_end + 1, quote)?;
                Some((kw_end, end))
            });
        match matched {
            Some((kw_end, end)) => {
                out.extend(&chars[i..kw_end]);
                out.push_str("=***");
                i = end;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Регекс 3: `\b(keyword)\s*[:=]\s*(?!\*\*\*)[^\n,;&]+` → `$1=***`.
fn redact_unquoted_pass(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let is_value_char = |c: char| !matches!(c, '\n' | ',' | ';' | '&');
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let matched = match_secret_keyword(&chars, i)
            .and_then(|kw_end| match_assign(&chars, kw_end).map(|a| (kw_end, a)))
            .and_then(|(kw_end, (ws_start, ws_end))| {
                // `\s*` жадный, но с откатом: на `password=  ***` lookahead `(?!\*\*\*)`
                // валит максимальный вариант, а вариант на символ короче проходит.
                for k in (ws_start..=ws_end).rev() {
                    let is_stars = chars.get(k) == Some(&'*')
                        && chars.get(k + 1) == Some(&'*')
                        && chars.get(k + 2) == Some(&'*');
                    if is_stars {
                        continue;
                    }
                    if !chars.get(k).copied().is_some_and(is_value_char) {
                        continue;
                    }
                    let mut end = k;
                    while end < chars.len() && is_value_char(chars[end]) {
                        end += 1;
                    }
                    return Some((kw_end, end));
                }
                None
            });
        match matched {
            Some((kw_end, end)) => {
                out.extend(&chars[i..kw_end]);
                out.push_str("=***");
                i = end;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

fn re_authorization() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(authorization:\s*(?:bearer|basic|digest)\s+)\S+")
            .expect("валидный регекс authorization")
    })
}

fn re_query_param() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)([?&](?:api_key|access_token|token|secret|key)=)[^&\s]+")
            .expect("валидный регекс query-параметра")
    })
}

fn re_token_forms() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `(?-u:\b)` — ASCII-граница слова, как у JS-регекса без флага `u`.
        Regex::new(concat!(
            r"(?-u:\b)(?:sk-[A-Za-z0-9_-]{8,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}",
            r"|xox[baprs]-[A-Za-z0-9-]{10,}|AIza[0-9A-Za-z_-]{20,}|AKIA[0-9A-Z]{16})(?-u:\b)"
        ))
        .expect("валидный регекс известных форм токенов")
    })
}

/// Маскирует типичные секреты в тексте для логов / observability (не для sticky).
///
/// Порядок проходов тот же, что в lib/util.mjs: Authorization-заголовок → значения
/// в кавычках (могут содержать пробелы) → значения без кавычек до `,`/`;`/`&`/EOL →
/// query-параметры → известные формы токенов → усечение.
///
/// Расхождение с JS: усечение считается в символах (`char`), а не в UTF-16 code units.
/// На ASCII и BMP результат совпадает; на суррогатных парах Rust режет по символам и
/// не может разорвать символ пополам.
pub fn redact_secrets_for_log(text: &str, max_len: usize) -> String {
    let s = re_authorization().replace_all(text, "${1}***").into_owned();
    let s = redact_quoted_pass(&s);
    let s = redact_unquoted_pass(&s);
    let s = re_query_param().replace_all(&s, "${1}***").into_owned();
    let s = re_token_forms().replace_all(&s, "***").into_owned();

    if s.chars().count() > max_len {
        let kept: String = s.chars().take(max_len.saturating_sub(1)).collect();
        return format!("{kept}…");
    }
    s
}

/// Дефолтный `maxLen` из lib/util.mjs.
pub const REDACT_DEFAULT_MAX_LEN: usize = 200;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn redact(text: &str) -> String {
        redact_secrets_for_log(text, REDACT_DEFAULT_MAX_LEN)
    }

    // -- env-хелперы ------------------------------------------------------

    #[test]
    fn env_flag_семантика() {
        let e = env(&[
            ("A", "0"),
            ("B", "false"),
            ("C", "no"),
            ("D", "off"),
            ("E", "1"),
            ("F", "true"),
            ("G", "yes"),
            ("H", "on"),
            ("I", "junk"),
            ("J", ""),
            ("K", " OFF "),
        ]);
        for k in ["A", "B", "C", "D", "K"] {
            assert!(!env_flag(&e, k, true), "{k} должно выключать");
        }
        for k in ["E", "F", "G", "H"] {
            assert!(env_flag(&e, k, false), "{k} должно включать");
        }
        assert!(env_flag(&e, "I", true), "junk → default_on");
        assert!(!env_flag(&e, "I", false), "junk → default_off");
        assert!(env_flag(&e, "J", true), "пусто → default_on");
        assert!(env_flag(&e, "MISSING", true), "отсутствует → default_on");
    }

    #[test]
    fn num_env_только_положительные() {
        let e = env(&[
            ("A", "2.5"),
            ("B", "0"),
            ("C", "abc"),
            ("D", ""),
            ("E", "-3"),
            ("F", "1e3"),
        ]);
        assert_eq!(num_env(&e, "A", 3.0), 2.5);
        assert_eq!(num_env(&e, "B", 3.0), 3.0, "0 не > 0 → def");
        assert_eq!(num_env(&e, "C", 3.0), 3.0);
        assert_eq!(num_env(&e, "D", 3.0), 3.0, "Number('') === 0 → не > 0");
        assert_eq!(num_env(&e, "E", 3.0), 3.0);
        assert_eq!(num_env(&e, "F", 3.0), 1000.0);
        assert_eq!(num_env(&e, "MISSING", 3.0), 3.0);
    }

    #[test]
    fn ratio_env_диапазон() {
        let e = env(&[
            ("A", "0"),
            ("B", "1"),
            ("C", "0.5"),
            ("D", "1.5"),
            ("E", "-0.1"),
        ]);
        assert_eq!(ratio_env(&e, "A", 0.1), 0.0);
        assert_eq!(ratio_env(&e, "B", 0.1), 1.0);
        assert_eq!(ratio_env(&e, "C", 0.1), 0.5);
        assert_eq!(ratio_env(&e, "D", 0.1), 0.1);
        assert_eq!(ratio_env(&e, "E", 0.1), 0.1);
        assert_eq!(ratio_env(&e, "MISSING", 0.1), 0.1);
    }

    #[test]
    fn js_number_повторяет_семантику_number() {
        assert_eq!(js_number(""), 0.0, "Number('') === 0");
        assert_eq!(js_number("   "), 0.0);
        assert_eq!(js_number("0x10"), 16.0);
        assert_eq!(js_number("0b101"), 5.0);
        assert_eq!(js_number("0o17"), 15.0);
        assert_eq!(js_number(".5"), 0.5);
        assert_eq!(js_number("Infinity"), f64::INFINITY);
        assert_eq!(js_number("-Infinity"), f64::NEG_INFINITY);
        assert!(js_number("inf").is_nan(), "Number('inf') === NaN");
        assert!(js_number("nan").is_nan(), "Number('nan') === NaN");
        assert!(js_number("12abc").is_nan());
    }

    // -- реэкспорты (полные наборы кейсов живут в модулях-владельцах) ------

    #[test]
    fn parse_model_list_дедуп_и_исключения() {
        let got = parse_model_list("m1, m2,m1, ,m3", &["m2".to_string()]);
        assert_eq!(got, vec!["m1", "m3"]);
    }

    #[test]
    fn is_transient_и_backoff() {
        assert!(is_transient(429));
        assert!(is_transient(500));
        assert!(is_transient(599));
        assert!(!is_transient(404));
        assert!(!is_transient(200));
        assert_eq!(backoff_ms(0, None), 1000.0);
        assert_eq!(backoff_ms(3, None), 8000.0);
        assert_eq!(backoff_ms(10, None), 15_000.0, "cap 15с");
        assert_eq!(backoff_ms(0, Some("2")), 2000.0);
        assert_eq!(backoff_ms(0, Some("99")), 30_000.0, "cap 30с");
        assert_eq!(backoff_ms(0, Some("0")), 1000.0, "0 → не Retry-After");
    }

    #[test]
    fn канонические_примитивы() {
        assert_eq!(canonical_json(&json!({ "b": 1, "a": [2, null] })), r#"{"a":[2,null],"b":1}"#);
        // node -e "console.log(require('node:crypto').createHash('sha256').update('x').digest('hex'))"
        assert_eq!(
            sha256_str("x"),
            "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
        );
        assert_eq!(sha256_value(&json!("x")), sha256_str("x"));
        assert_eq!(
            stable_id("edge", &["a".to_string(), "b".to_string()]),
            format!("edge:{}", &sha256_str("a\0b")[..24])
        );
    }

    #[test]
    fn normalize_repo_path_отвергает_выходы() {
        // 1:1 из test/structure.test.mjs
        assert_eq!(normalize_repo_path("./src/x.ts").unwrap(), "src/x.ts");
        assert_eq!(normalize_repo_path(".").unwrap(), ".");
        assert!(normalize_repo_path("../secret").is_err());
        assert!(normalize_repo_path("/etc/passwd").is_err());
        assert!(normalize_repo_path("C:\\secret").is_err());
    }

    #[test]
    fn public_error_label_только_категория() {
        // 1:1 из test/pr-review.test.mjs («publicErrorLabel: только категория»)
        assert_eq!(
            public_error_label(Some(&json!(
                r#"Ollama → 500: {"error":"sk-live-secret-token-123"}"#
            ))),
            "HTTP 500"
        );
        assert_eq!(
            public_error_label(Some(&json!("Ollama → 429: rate limited"))),
            "HTTP 429 (rate limit)"
        );
        assert_eq!(
            public_error_label(Some(&json!({ "message": "This operation was aborted" }))),
            "таймаут"
        );
        assert_eq!(
            public_error_label(Some(&json!("fetch failed: socket hang up"))),
            "сеть"
        );
        assert_eq!(
            public_error_label(Some(&json!("invalid json: Unexpected token <"))),
            "невалидный ответ"
        );
        assert_eq!(public_error_label(Some(&json!("something weird"))), "ошибка вызова");
        assert_eq!(public_error_label(None), "ошибка вызова");
    }

    // -- Logger -----------------------------------------------------------

    #[test]
    fn logger_печатает_с_префиксом() {
        let l = Logger::default();
        assert_eq!(l.prefix(), "[pr-review]");
        assert_eq!(l.line(format_args!("готово: {}", 3)), "[pr-review] готово: 3");
        let custom = Logger::new("[gold]");
        assert_eq!(custom.line(format_args!("x")), "[gold] x");
    }

    // -- resolveContainedPath ---------------------------------------------

    #[test]
    fn resolve_contained_path_держит_цель_внутри_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize root");
        std::fs::write(root.join("a.txt"), b"x").expect("write");
        std::fs::create_dir(root.join("sub")).expect("mkdir");

        assert_eq!(
            resolve_contained_path("a.txt", Some(&root), true).unwrap(),
            root.join("a.txt")
        );
        assert_eq!(
            resolve_contained_path("./sub/../a.txt", Some(&root), true).unwrap(),
            root.join("a.txt")
        );
        // Ещё не созданный файл: containment по существующему предку.
        assert_eq!(
            resolve_contained_path("sub/new/out.json", Some(&root), false).unwrap(),
            root.join("sub/new/out.json")
        );
        // Сам root — тоже внутри root.
        assert_eq!(resolve_contained_path(".", Some(&root), true).unwrap(), root);
    }

    #[test]
    fn resolve_contained_path_отвергает_выход_наружу() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize root");
        std::fs::create_dir(root.join("sub")).expect("mkdir");

        for target in ["../outside.txt", "/etc/passwd", "sub/../../outside.txt"] {
            let err = resolve_contained_path(target, Some(&root), false)
                .expect_err(target)
                .to_string();
            assert!(err.contains("path escapes allowed root"), "{target}: {err}");
        }
    }

    #[test]
    fn resolve_contained_path_пробрасывает_enoent_изнутри_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize root");
        let err = resolve_contained_path("нет-такого.json", Some(&root), true)
            .expect_err("must_exist");
        let io = err
            .downcast_ref::<std::io::Error>()
            .expect("исходная io-ошибка сохранена");
        assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn js_resolve_снимает_точки_лексически() {
        // path.resolve: абсолютная цель побеждает базу, `.`/`..` снимаются без ФС.
        assert_eq!(js_resolve(Path::new("/a/b"), "c/d"), PathBuf::from("/a/b/c/d"));
        assert_eq!(js_resolve(Path::new("/a/b"), "../c"), PathBuf::from("/a/c"));
        assert_eq!(js_resolve(Path::new("/a/b"), "./c/./d"), PathBuf::from("/a/b/c/d"));
        assert_eq!(js_resolve(Path::new("/a/b"), "/etc/passwd"), PathBuf::from("/etc/passwd"));
        assert_eq!(js_resolve(Path::new("/a/b"), ""), PathBuf::from("/a/b"));
        // Выход выше корня упирается в `/` (как в POSIX-resolve).
        assert_eq!(js_resolve(Path::new("/a"), "../../.."), PathBuf::from("/"));
    }

    #[test]
    fn resolve_contained_path_нормализует_root() {
        // root приходит не-каноническим (`sub/..`) — как после `path.join` у вызывающего.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize root");
        std::fs::create_dir(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("a.txt"), b"x").expect("write");

        let noisy = root.join("sub").join("..");
        assert_eq!(
            resolve_contained_path("a.txt", Some(&noisy), true).unwrap(),
            root.join("a.txt")
        );
        assert!(resolve_contained_path("../x", Some(&noisy), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_contained_path_ловит_symlink_наружу() {
        let outer = tempfile::tempdir().expect("tempdir outer");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize root");
        let target = std::fs::canonicalize(outer.path()).expect("canonicalize outer");
        std::fs::write(target.join("secret.txt"), b"x").expect("write");
        std::os::unix::fs::symlink(&target, root.join("link")).expect("symlink");

        let err = resolve_contained_path("link/secret.txt", Some(&root), true)
            .expect_err("symlink наружу")
            .to_string();
        assert!(err.contains("path escapes allowed root"), "{err}");
    }

    // -- redactSecretsForLog ----------------------------------------------
    //
    // Эталоны получены прогоном самой lib/util.mjs через `node -e`, поэтому
    // сравнение идёт на точное равенство строк, а не на includes(), как в JS-тесте.

    #[test]
    fn redact_кейсы_из_js_теста() {
        // 1:1 из test/pr-review.test.mjs («redactSecretsForLog: quoted secret ...»)
        let q = redact("password=\"a b c\" and api_key=xyz rest");
        assert!(q.contains("password=***"), "{q}");
        assert!(q.contains("api_key=***"), "{q}");
        assert!(!q.contains("a b c"), "{q}");
        assert!(!q.contains("xyz"), "{q}");
        assert_eq!(q, "password=*** and api_key=***");

        let unquoted = redact("password=hello world; next");
        assert!(unquoted.contains("password=***"), "{unquoted}");
        assert!(!unquoted.contains("hello"), "{unquoted}");
        assert!(unquoted.contains("next"), "{unquoted}");
        assert_eq!(unquoted, "password=***; next");

        let basic = redact("Authorization: Basic YWRtaW46c2VjcmV0");
        assert!(basic.contains("Basic ***"), "{basic}");
        assert!(!basic.contains("YWRtaW4"), "{basic}");
        assert_eq!(basic, "Authorization: Basic ***");

        let forms = redact(
            "xoxb-1234567890-token AIzaSyAbCdEfGhIjKlMnOpQrStUvWxYz012345 AKIAIOSFODNN7EXAMPLE",
        );
        assert!(!forms.contains("xoxb-"), "{forms}");
        assert!(!forms.contains("AIza"), "{forms}");
        assert!(!forms.contains("AKIA"), "{forms}");
        assert_eq!(forms, "*** *** ***");

        // Голые «key=» / «token=» больше не матчим (был false positive).
        let bare = redact("map key=alpha token=beta");
        assert_eq!(bare, "map key=alpha token=beta");

        assert_eq!(redact(""), "", "String(undefined ?? '') === ''");
        assert_eq!(redact_secrets_for_log("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn redact_кавычки_и_экранирование() {
        // Экранированная кавычка внутри значения (`\\.` в JS-регексе).
        assert_eq!(redact(r#"password="a\"b" tail"#), "password=*** tail");
        assert_eq!(redact(r"password='it\'s ok' tail"), "password=*** tail");
        // Экранирован сам обратный слэш → значение закрывается на следующей кавычке.
        assert_eq!(redact(r#"password="a\\"b" tail"#), r#"password=***b" tail"#);
        assert_eq!(redact(r#"password="a\\""#), "password=***");
        // Незакрытая кавычка: проход 2 не матчит, добивает проход 3.
        assert_eq!(redact("password=\"abc"), "password=***");
        // `.` не матчит перевод строки — значение обрывается на нём.
        assert_eq!(
            redact("password=\"unterminated\nnext line"),
            "password=***\nnext line"
        );
    }

    #[test]
    fn redact_два_прохода_не_сливаются() {
        // Проход по кавычкам обязан идти отдельно: слитый проход дал бы
        // «secret=***, c\" d» — запятая внутри кавычек оборвала бы значение.
        assert_eq!(redact(r#"secret=a password="b, c" d"#), "secret=***");
        assert_eq!(redact("secret=one, password=two"), "secret=***, password=***");
        assert_eq!(redact(r#"password=v1, secret="q""#), "password=***, secret=***");
    }

    #[test]
    fn redact_идемпотентность_и_откат_пробелов() {
        assert_eq!(redact("password=***"), "password=***", "lookahead (?!\\*\\*\\*)");
        // `\s*` откатывается на символ, значение становится « *** x» → снова «***».
        assert_eq!(redact("password=  *** x"), "password=***");
    }

    #[test]
    fn redact_варианты_ключевых_слов() {
        assert_eq!(redact("client-secret: 'zzz' end"), "client-secret=*** end");
        assert_eq!(redact("API_KEY: SuperSecret123"), "API_KEY=***");
        assert_eq!(redact("access-token = tok, x=1"), "access-token=***, x=1");
        assert_eq!(redact("a=1 SECRET:\t\"q\" b"), "a=1 SECRET=*** b");
        assert_eq!(
            redact("client_secret=abc&api-key=def"),
            "client_secret=***&api-key=***"
        );
        // `\b` слева: «secretary» и «passwords» — не ключевые слова.
        assert_eq!(
            redact("secretary=5 and passwords ok"),
            "secretary=5 and passwords ok"
        );
    }

    #[test]
    fn redact_authorization_и_query() {
        assert_eq!(redact("Authorization: Bearer abc.def"), "Authorization: Bearer ***");
        assert_eq!(redact("authorization: digest xyz"), "authorization: digest ***");
        // `\s*` в заголовке переходит через перевод строки.
        assert_eq!(
            redact("Authorization:\n  bearer   tok123 rest"),
            "Authorization:\n  bearer   *** rest"
        );
        // Без «Authorization:» голый Bearer не трогаем.
        assert_eq!(redact("Bearer tok"), "Bearer tok");
        assert_eq!(redact("?secret=zzz&key=www"), "?secret=***&key=***");
        assert_eq!(
            redact("https://x/y?api_key=abc&token=def#frag"),
            "https://x/y?api_key=***&token=***"
        );
    }

    #[test]
    fn redact_известные_формы_токенов() {
        assert_eq!(redact("sk-live-secret-token-123"), "***");
        assert_eq!(redact("sk-abcdefgh"), "***");
        assert_eq!(redact("sk-abcdefg"), "sk-abcdefg", "меньше 8 символов");
        assert_eq!(redact("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"), "***");
        assert_eq!(redact("gho_ABCDEFGHIJKLMNOPQRSTUVWXYZ"), "***");
        assert_eq!(redact("xoxp-1234567890"), "***");
        // `\b` слева: без границы слова форма не матчится.
        assert_eq!(
            redact("prefixAKIAIOSFODNN7EXAMPLE"),
            "prefixAKIAIOSFODNN7EXAMPLE"
        );
    }

    #[test]
    fn redact_многострочность_и_усечение() {
        assert_eq!(
            redact("line1 password=p1\nline2 secret=s2"),
            "line1 password=***\nline2 secret=***"
        );
        assert_eq!(redact_secrets_for_log("abcde", 5), "abcde", "ровно maxLen");
        assert_eq!(redact_secrets_for_log("abcdef", 5), "abcd…");
        assert_eq!(redact_secrets_for_log("abcdefghij", 0), "…", "maxLen=0");
        // UTF-8: маскируем и режем по символам, не разрывая их пополам.
        assert_eq!(
            redact("привет password=тайна конец"),
            "привет password=***"
        );
        assert_eq!(
            redact_secrets_for_log("日本語 password=秘密 abc", 12),
            "日本語 passwor…"
        );
    }
}
