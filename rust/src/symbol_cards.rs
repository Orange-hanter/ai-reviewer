//! Карточки символов (RFC 14, шаг 2) — порт `lib/symbol-cards.mjs` (RFC 16, T38).
//!
//! Карточка — одна строка объявления того, что вызывают изменённые строки, с местом в
//! коде: доказывает, что символ существует и какова его сигнатура, и ничего не говорит о
//! теле. Источник места — граф Graphify ([`GraphIndex`], он же `BlastRadius::graph_index`),
//! источник текста — файл рабочего дерева через инъекцию `read_lines` ([`LineReader`] в
//! проде, словарь в тестах).
//!
//! Весь вход здесь пишет автор PR (дифф, окна, файлы, вывод графа), поэтому:
//! - ни одной регулярки с возвратом: всё, что в JS было регуляркой с lookbehind или
//!   обратной ссылкой, разобрано линейным кодом, повторяющим её семантику (включая
//!   откаты там, где они меняют ответ); `regex` (линейный движок) — только для
//!   [`is_absence_claim`];
//! - лексеры ([`mask_non_code`], [`scan_declaration`]) идут по UTF-16 юнитам, как JS:
//!   длины и пределы (`MAX_DECL_CHARS`, срез 24 юнитов перед `/`) совпадают с эталоном;
//! - [`LineReader`] сверяет путь ПОСЛЕ разыменования symlink: файл вне корня не читается.
//!
//! Что не совпадает с JS байт-в-байт (только на не-BMP символах): срез оператора импорта
//! до 2000 юнитов не режет суррогатную пару пополам, а квантификатор `{0,40}` в
//! [`is_absence_claim`] считает символы, а не юниты.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::{IndexMap, IndexSet};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::findings::{js_collapse_whitespace, js_is_whitespace, js_trim};
use crate::graphify_client::{bare_label, js_string, js_string_truthy, GraphIndex};

/// v1 — семейство JS/TS: для него верны правила «где кончается объявление» (`{`, `=>`, `;`).
pub const SYMBOL_CARD_EXTS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];

const KEYWORDS: &[&str] = &[
    "if", "for", "while", "switch", "catch", "return", "typeof", "function", "await", "async", "new",
    "super", "this", "import", "export", "require", "class", "throw", "delete", "void", "in", "of",
    "yield", "do", "else", "try", "case", "default", "with", "let", "const", "var", "instanceof",
];

/// Глобалы языка и тестовых раннеров: у них нет объявления в репозитории.
const GLOBALS: &[&str] = &[
    "console", "Promise", "JSON", "Object", "Array", "String", "Number", "Boolean", "Math", "Date",
    "Error", "TypeError", "RangeError", "RegExp", "Map", "Set", "WeakMap", "WeakSet", "Symbol",
    "BigInt", "Intl", "Reflect", "Proxy", "parseInt", "parseFloat", "isNaN", "isFinite",
    "setTimeout", "clearTimeout", "setInterval", "clearInterval", "setImmediate", "queueMicrotask",
    "structuredClone", "fetch", "URL", "URLSearchParams", "TextEncoder", "TextDecoder",
    "AbortController", "Buffer", "process", "globalThis", "encodeURIComponent",
    "decodeURIComponent", "describe", "it", "test", "expect", "assert", "beforeEach", "afterEach",
    "before", "after", "vi", "jest",
];

/// Длиннее — минифицированный или сгенерированный код: вызовы из него не собираем.
const MAX_CALL_LINE_CHARS: usize = 2000;
/// Оператор импорта длиннее — не разбираем.
const MAX_IMPORT_CHARS: usize = 2000;
/// Сколько строк собирать в многострочный `import {\n a,\n b\n} from 'x'`.
const MAX_IMPORT_LINES: usize = 40;
const MAX_DECL_CHARS: usize = 160;
const MAX_HEADER_LINES: usize = 3;
/// Предел строк, которые лексер просматривает от начала объявления.
pub const MAX_SCAN_LINES: usize = 2000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_OVERLOADS: usize = 8;
const LOCAL_SCAN_MAX_LINE: usize = 2000;

const MODULE_EXTS: [&str; 8] = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

// ── Примитивы символьных классов JS (не-unicode режим: `\w` — ASCII) ──

/// `\w` JS.
fn is_w(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `[\w$]`.
fn is_wd(b: u8) -> bool {
    is_w(b) || b == b'$'
}

/// `[A-Za-z_$]` — первый символ идентификатора.
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

/// LineTerminator JS: то, что не матчит `.` и после чего матчит `^` в режиме `m`.
fn is_lt(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.slice(0, n)` по UTF-16 юнитам; суррогатная пара на границе не режется.
fn utf16_prefix(s: &str, n: usize) -> &str {
    let mut units = 0usize;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &s[..i];
        }
    }
    s
}

/// Позиция после пробелов JS (`\s*`) начиная с байта `p`.
fn skip_ws(s: &str, mut p: usize) -> usize {
    while let Some(c) = s.get(p..).and_then(|r| r.chars().next()) {
        if !js_is_whitespace(c) {
            break;
        }
        p += c.len_utf8();
    }
    p
}

/// Позиция после `[ \t]*`.
fn skip_sp_tab(b: &[u8], mut p: usize) -> usize {
    while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
        p += 1;
    }
    p
}

/// Конец идентификатора `[A-Za-z_$][\w$]*`, начинающегося в `p`.
fn ident_end(b: &[u8], p: usize) -> Option<usize> {
    if p >= b.len() || !is_ident_start(b[p]) {
        return None;
    }
    let mut e = p + 1;
    while e < b.len() && is_wd(b[e]) {
        e += 1;
    }
    Some(e)
}

/// `\b` в байтовой позиции `k` (вне строки — не-словесный символ).
fn word_boundary(b: &[u8], k: usize) -> bool {
    let prev = k > 0 && is_w(b[k - 1]);
    let next = k < b.len() && is_w(b[k]);
    prev != next
}

/// Есть ли в `s` с позиции `p` слово `w`, за которым идёт хотя бы один пробел JS;
/// возвращает позицию после пробелов (`w\s+`).
fn word_then_ws(s: &str, p: usize, w: &str) -> Option<usize> {
    if !s.get(p..)?.starts_with(w) {
        return None;
    }
    let q = skip_ws(s, p + w.len());
    (q > p + w.len()).then_some(q)
}

/// То же для `w[ \t]+`.
fn word_then_sp_tab(b: &[u8], p: usize, w: &str) -> Option<usize> {
    if !b.get(p..)?.starts_with(w.as_bytes()) {
        return None;
    }
    let q = skip_sp_tab(b, p + w.len());
    (q > p + w.len()).then_some(q)
}

/// Позиции после 0, 1, …, k модификаторов подряд (`(?:(?:m1|m2|…)SEP)*`): все варианты
/// жадного повтора, на которые может откатиться регулярка. `sp_tab` — разделитель
/// `[ \t]+`, иначе `\s+`.
fn modifier_positions(s: &str, start: usize, mods: &[&str], sp_tab: bool) -> Vec<usize> {
    let b = s.as_bytes();
    let mut out = vec![start];
    let mut p = start;
    loop {
        let next = mods.iter().find_map(|m| {
            if sp_tab {
                word_then_sp_tab(b, p, m)
            } else {
                word_then_ws(s, p, m)
            }
        });
        match next {
            Some(q) => {
                out.push(q);
                p = q;
            }
            None => return out,
        }
    }
}

/// `(?:^|[^\w$])NAME(?![\w$])` — слово `name` целиком (граница `[\w$]`).
fn word_occurs(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(off) = text.get(from..).and_then(|r| r.find(name)) {
        let i = from + off;
        let end = i + name.len();
        if (i == 0 || !is_wd(b[i - 1])) && (end >= b.len() || !is_wd(b[end])) {
            return true;
        }
        from = i + text[i..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Все байтовые позиции вхождений `needle` в `hay` (с перекрытием).
fn occurrences<'a>(hay: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let mut from = 0usize;
    std::iter::from_fn(move || {
        let off = hay.get(from..)?.find(needle)?;
        let i = from + off;
        from = i + hay[i..].chars().next().map_or(1, char::len_utf8);
        Some(i)
    })
}

/// `^(?:\/\/|\*|\/\*)` — строка-комментарий.
fn starts_comment_like(t: &str) -> bool {
    t.starts_with("//") || t.starts_with('*') || t.starts_with("/*")
}

/// Сравнение UTF-16 юнитов — дефолтный `Array.prototype.sort` JS.
fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Первичный ключ ICU root для ASCII: пробелы, пунктуация, символы, `$`, цифры, буквы.
fn icu_primary(c: char) -> (u32, u32) {
    const ORDER: &str = "\t\n\u{b}\u{c}\r _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789";
    if let Some(i) = ORDER.find(c) {
        return (0, i as u32);
    }
    if c.is_ascii_alphabetic() {
        return (1, c.to_ascii_lowercase() as u32);
    }
    let lower = c.to_lowercase().next().unwrap_or(c);
    (2, lower as u32)
}

/// Аппроксимация `String.prototype.localeCompare` (ICU root): первично — порядок
/// пунктуации и символов ICU, буквы без учёта регистра; третично — строчная раньше
/// заглавной. На идентификаторах и путях совпадает с ICU.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let pa: Vec<(u32, u32)> = a.chars().map(icu_primary).collect();
    let pb: Vec<(u32, u32)> = b.chars().map(icu_primary).collect();
    match pa.cmp(&pb) {
        Ordering::Equal => {}
        other => return other,
    }
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            let (xl, yl) = (x.is_lowercase(), y.is_lowercase());
            if xl && !yl {
                return Ordering::Less;
            }
            if !xl && yl {
                return Ordering::Greater;
            }
            return x.cmp(&y);
        }
    }
    Ordering::Equal
}

// ── Импорты ──

/// `new Name(`/`name(` метода через точку — по позиции `idx` (просмотр назад ограничен 64).
fn preceded_by_dot(b: &[u8], idx: usize) -> bool {
    let mut k = idx;
    let mut steps = 0;
    while k > 0 && steps < 64 {
        k -= 1;
        steps += 1;
        match b[k] {
            b'.' => return true,
            b' ' | b'\t' => {}
            _ => return false,
        }
    }
    false
}

/// Спецификатор пакета, а не файла репозитория: `node:fs`, `react`, `@scope/pkg` (не `@/` и не `~/`).
fn is_package_specifier(spec: &str) -> bool {
    !(spec.starts_with('.')
        || spec.starts_with('/')
        || spec.starts_with("@/")
        || spec.starts_with("~/")
        || spec.starts_with('#'))
}

/// Кавычка, 1..300 юнитов без `'"\n`, та же кавычка: `(['"])([^'"\n]{1,300})\1` в `p`.
/// Возвращает (спецификатор, позиция после закрывающей кавычки).
fn quoted_spec(s: &str, p: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    let q = *b.get(p)?;
    if q != b'\'' && q != b'"' {
        return None;
    }
    let mut e = p + 1;
    while e < b.len() && !matches!(b[e], b'\'' | b'"' | b'\n') {
        e += 1;
    }
    let content = &s[p + 1..e];
    let n = utf16_len(content);
    if n == 0 || n > 300 || b.get(e) != Some(&q) {
        return None;
    }
    Some((content.to_string(), e + 1))
}

/// `FROM_SPEC_RE` = `\bfrom\s*(['"])([^'"\n]{1,300})\1`: (индекс `from`, спецификатор).
fn from_spec(stmt: &str) -> Option<(usize, String)> {
    let b = stmt.as_bytes();
    for i in occurrences(stmt, "from") {
        if i > 0 && is_w(b[i - 1]) {
            continue;
        }
        if let Some((spec, _)) = quoted_spec(stmt, skip_ws(stmt, i + 4)) {
            return Some((i, spec));
        }
    }
    None
}

/// `REQUIRE_SPEC_RE` = `\brequire\(\s*(['"])([^'"\n]{1,300})\1\s*\)`.
fn require_spec(stmt: &str) -> Option<(usize, String)> {
    let b = stmt.as_bytes();
    for i in occurrences(stmt, "require(") {
        if i > 0 && is_w(b[i - 1]) {
            continue;
        }
        if let Some((spec, after)) = quoted_spec(stmt, skip_ws(stmt, i + 8)) {
            if b.get(skip_ws(stmt, after)) == Some(&b')') {
                return Some((i, spec));
            }
        }
    }
    None
}

/// `s.replace(/^\s*type\s+/, '')`.
fn strip_type_prefix(s: &str) -> &str {
    let p = skip_ws(s, 0);
    match word_then_ws(s, p, "type") {
        Some(q) => &s[q..],
        None => s,
    }
}

/// JS `s.slice(start, end)` по байтам с отрицательным `end` (`indexOf` → -1).
fn js_slice(s: &str, start: usize, end: Option<usize>) -> &str {
    let end = match end {
        Some(e) => e,
        None => s.char_indices().last().map_or(0, |(i, _)| i),
    };
    let start = start.min(s.len());
    if start >= end {
        return "";
    }
    &s[start..end.min(s.len())]
}

/// Последний кусок `t.split(/\s/)`.
fn last_ws_piece(t: &str) -> &str {
    match t.char_indices().rfind(|(_, c)| js_is_whitespace(*c)) {
        Some((i, c)) => &t[i + c.len_utf8()..],
        None => t,
    }
}

fn is_ident(s: &str) -> bool {
    ident_end(s.as_bytes(), 0) == Some(s.len())
}

/// Оператор импорта: спецификатор, часть с именами, это ли `require`.
struct ImportStmt {
    spec: String,
    clause: String,
    is_require: bool,
}

/// Операторы `import … from 'x'` и `const … = require('x')` (`importStatements`).
fn import_statements(lines: &[String]) -> Vec<ImportStmt> {
    let mut out = Vec::new();
    for i in 0..lines.len() {
        let first = lines[i].as_str();
        let trimmed = first.trim_start_matches(js_is_whitespace);
        let is_import = trimmed.starts_with("import ") || trimmed.starts_with("import{");
        let is_require = !is_import
            && ["const", "let", "var"].iter().any(|k| {
                trimmed.starts_with(k) && trimmed[k.len()..].chars().next().is_some_and(js_is_whitespace)
            })
            && first.contains("require(");
        if !is_import && !is_require {
            continue;
        }
        let mut stmt = utf16_prefix(trimmed, MAX_IMPORT_CHARS).to_string();
        // многострочный import: дособрать до `from '…'`
        let mut j = i + 1;
        while is_import && from_spec(&stmt).is_none() && j < lines.len().min(i + MAX_IMPORT_LINES) {
            let joined = format!("{stmt} {}", js_trim(&lines[j]));
            stmt = utf16_prefix(&joined, MAX_IMPORT_CHARS).to_string();
            j += 1;
        }
        let spec = if is_import { from_spec(&stmt) } else { require_spec(&stmt) };
        let Some((idx, spec)) = spec else { continue };
        let clause = if is_import {
            js_slice(&stmt, "import".len(), Some(idx)).to_string()
        } else {
            let ws = stmt.char_indices().find(|(_, c)| js_is_whitespace(*c)).map(|(i, c)| i + c.len_utf8());
            js_slice(&stmt, ws.unwrap_or(0), stmt.find('=')).to_string()
        };
        out.push(ImportStmt { spec, clause, is_require });
    }
    out
}

fn add_local_names(names: &mut IndexSet<String>, clause: &str) {
    let add = |names: &mut IndexSet<String>, n: &str| {
        let id = js_trim(n);
        if is_ident(id) {
            names.insert(id.to_string());
        }
    };
    let open = clause.find('{');
    let head = strip_type_prefix(match open {
        Some(o) => &clause[..o],
        None => clause,
    });
    for part in head.split(',') {
        let t = js_trim(part);
        if t.starts_with('*') {
            add(names, last_ws_piece(t));
        } else {
            add(names, t);
        }
    }
    if let Some(o) = open {
        if let Some(c) = clause[o + 1..].find('}').map(|c| o + 1 + c) {
            for part in clause[o + 1..c].split(',') {
                let replaced = part.replacen(':', " as ", 1);
                let last = replaced.split(" as ").last().unwrap_or("");
                add(names, last);
            }
        }
    }
}

/// Имена, импортированные из пакетов: их объявления в репозитории нет, а одноимённый узел
/// графа — чужой (`ok` из `node:assert` ≠ `ok()` из какого-то файла).
pub fn package_imported_names(lines: &[String]) -> IndexSet<String> {
    let mut names = IndexSet::new();
    for st in import_statements(lines) {
        if is_package_specifier(&st.spec) {
            add_local_names(&mut names, &st.clause);
        }
    }
    names
}

/// Привязка относительного импорта: спецификатор и имя в модуле (`*` — namespace,
/// `default` — импорт по умолчанию).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportBinding {
    pub spec: String,
    pub imported: String,
}

/// Привязки из ОТНОСИТЕЛЬНЫХ импортов файла: локальное имя → { spec, imported }.
pub fn relative_import_bindings(lines: &[String]) -> IndexMap<String, ImportBinding> {
    let mut out = IndexMap::new();
    for st in import_statements(lines) {
        if is_package_specifier(&st.spec) {
            continue;
        }
        let clause = st.clause.as_str();
        let open = clause.find('{');
        let head = strip_type_prefix(match open {
            Some(o) => &clause[..o],
            None => clause,
        });
        for part in head.split(',') {
            let t = js_trim(part);
            if t.is_empty() {
                continue;
            }
            if t.starts_with('*') {
                let ns = last_ws_piece(t);
                if is_ident(ns) {
                    out.insert(ns.to_string(), ImportBinding { spec: st.spec.clone(), imported: "*".into() });
                }
            } else if is_ident(t) {
                let imported = if st.is_require { "*" } else { "default" };
                out.insert(t.to_string(), ImportBinding { spec: st.spec.clone(), imported: imported.into() });
            }
        }
        if let Some(o) = open {
            if let Some(c) = clause[o + 1..].find('}').map(|c| o + 1 + c) {
                for part in clause[o + 1..c].split(',') {
                    let replaced = strip_type_prefix(part).replacen(':', " as ", 1);
                    let tokens: Vec<&str> = replaced.split(" as ").map(js_trim).collect();
                    let imported = tokens[0];
                    let local = tokens[tokens.len() - 1];
                    if is_ident(imported) && is_ident(local) {
                        out.insert(
                            local.to_string(),
                            ImportBinding { spec: st.spec.clone(), imported: imported.to_string() },
                        );
                    }
                }
            }
        }
    }
    out
}

/// Файл модуля по относительному спецификатору — среди файлов, которые знает граф: точный
/// путь, TS-соглашение `./x.js` → `x.ts`, перебор расширений, `index.*`.
pub fn resolve_import_path(caller_path: &str, spec: &str, has_file: &dyn Fn(&str) -> bool) -> Option<String> {
    let mut parts: Vec<&str> = caller_path.split('/').collect();
    parts.pop();
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    let base = parts.join("/");
    let stem = [".mjsx", ".cjsx", ".mjs", ".cjs", ".jsx", ".js"]
        .iter()
        .find_map(|ext| base.strip_suffix(ext))
        .unwrap_or(&base)
        .to_string();
    let mut candidates = vec![base.clone()];
    candidates.extend(MODULE_EXTS.iter().map(|ext| format!("{stem}{ext}")));
    candidates.extend(MODULE_EXTS.iter().map(|ext| format!("{base}/index{ext}")));
    candidates.into_iter().find(|c| has_file(c))
}

/// Вызов через пространство имён `ns.fn(` (ключ результата — `ns.fn`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceCall {
    pub ns: String,
    #[serde(rename = "fn")]
    pub fn_name: String,
    pub count: usize,
}

/// Вызовы `ns.fn(` — только для `ns` из `import * as ns from './x'`.
pub fn extract_namespace_calls(added_lines: &[String], namespaces: &HashSet<String>) -> IndexMap<String, NamespaceCall> {
    let mut out: IndexMap<String, NamespaceCall> = IndexMap::new();
    if namespaces.is_empty() {
        return out;
    }
    for raw in added_lines {
        let trimmed = js_trim(raw);
        if trimmed.is_empty() || utf16_len(trimmed) > MAX_CALL_LINE_CHARS || starts_comment_like(trimmed) {
            continue;
        }
        let line = strip_literals_and_comment(trimmed);
        let b = line.as_bytes();
        let mut seen = HashSet::new();
        let mut i = 0;
        while i < b.len() {
            // (?<![\w$.])(ident)\.(ident)[ \t]*\(
            let m = (|| {
                if i > 0 && (is_wd(b[i - 1]) || b[i - 1] == b'.') {
                    return None;
                }
                let e1 = ident_end(b, i)?;
                if b.get(e1) != Some(&b'.') {
                    return None;
                }
                let e2 = ident_end(b, e1 + 1)?;
                let q = skip_sp_tab(b, e2);
                (b.get(q) == Some(&b'(')).then_some((e1, e2, q + 1))
            })();
            let Some((e1, e2, end)) = m else {
                i += 1;
                continue;
            };
            let ns = &line[i..e1];
            let fname = &line[e1 + 1..e2];
            i = end;
            if !namespaces.contains(ns) {
                continue;
            }
            let key = format!("{ns}.{fname}");
            if !seen.insert(key.clone()) {
                continue;
            }
            match out.get_mut(&key) {
                Some(prev) => prev.count += 1,
                None => {
                    out.insert(key, NamespaceCall { ns: ns.to_string(), fn_name: fname.to_string(), count: 1 });
                }
            }
        }
    }
    out
}

// ── Вызовы в добавленных строках ──

pub fn is_symbol_card_path(path: &str) -> bool {
    let b = path.as_bytes();
    let mut k = b.len();
    while k > 0 && b[k - 1].is_ascii_alphanumeric() {
        k -= 1;
    }
    if k == b.len() || k == 0 || b[k - 1] != b'.' {
        return false;
    }
    let ext = path[k..].to_ascii_lowercase();
    SYMBOL_CARD_EXTS.contains(&ext.as_str())
}

/// Убрать строковые литералы (→ `""`), комментарий строки и `/* … */` в ней:
/// внутри них `name(` — не вызов. Порт трёх `replace` из `stripLiteralsAndComment`.
fn strip_literals_and_comment(line: &str) -> String {
    let cs: Vec<char> = line.chars().collect();
    // 1. (['"`])(?:\\.|(?!\1)[^\\])*\1 → ""
    let mut s1 = String::with_capacity(line.len());
    let mut i = 0;
    while i < cs.len() {
        let q = cs[i];
        if matches!(q, '\'' | '"' | '`') {
            let mut j = i + 1;
            let close = loop {
                if j >= cs.len() {
                    break None;
                }
                let d = cs[j];
                if d == '\\' {
                    if j + 1 < cs.len() && !is_lt(cs[j + 1]) {
                        j += 2;
                        continue;
                    }
                    break None;
                }
                if d == q {
                    break Some(j);
                }
                j += 1;
            };
            if let Some(j) = close {
                s1.push_str("\"\"");
                i = j + 1;
                continue;
            }
        }
        s1.push(q);
        i += 1;
    }
    // 2. //.*$ — первое `//`, после которого до конца нет перевода строки
    let mut s2 = s1.as_str();
    for k in occurrences(&s1, "//") {
        if !s1[k..].chars().any(is_lt) {
            s2 = &s1[..k];
            break;
        }
    }
    // 3. /\*.*?\*/ глобально — без перевода строки внутри
    let mut out = String::with_capacity(s2.len());
    let mut p = 0;
    while p < s2.len() {
        if s2[p..].starts_with("/*") {
            let mut j = p + 2;
            let mut close = None;
            while j < s2.len() {
                if s2[j..].starts_with("*/") {
                    close = Some(j);
                    break;
                }
                let c = s2[j..].chars().next().unwrap_or('\0');
                if is_lt(c) {
                    break;
                }
                j += c.len_utf8();
            }
            if let Some(j) = close {
                p = j + 2;
                continue;
            }
        }
        let c = s2[p..].chars().next().unwrap_or('\0');
        out.push(c);
        p += c.len_utf8();
    }
    out
}

const METHOD_MODS: &[&str] = &[
    "public", "private", "protected", "static", "async", "abstract", "readonly", "override", "get", "set",
    "export", "declare",
];

/// `METHOD_HEAD_RE`: (имя, позиция `(`) — модификаторы, имя, дженерики, `(`.
fn method_head(line: &str) -> Option<(&str, usize)> {
    let b = line.as_bytes();
    for &start in modifier_positions(line, 0, METHOD_MODS, true).iter().rev() {
        let Some(e) = ident_end(b, start) else { continue };
        let q = skip_sp_tab(b, e);
        if b.get(q) == Some(&b'(') {
            return Some((&line[start..e], q));
        }
        if b.get(q) == Some(&b'<') {
            // <[^<>()\n]{0,200}>
            let mut r = q + 1;
            let mut units = 0;
            while r < b.len() && !matches!(b[r], b'<' | b'>' | b'(' | b')' | b'\n') {
                let c = line[r..].chars().next().unwrap_or('\0');
                units += c.len_utf16();
                r += c.len_utf8();
            }
            if units <= 200 && b.get(r) == Some(&b'>') {
                let q2 = skip_sp_tab(b, r + 1);
                if b.get(q2) == Some(&b'(') {
                    return Some((&line[start..e], q2));
                }
            }
        }
    }
    None
}

/// Имя метода, который ОБЪЯВЛЯЕТ эта строка, — не вызов (аудит RFC 14, п. 1.1).
fn method_declared_name(line: &str) -> Option<&str> {
    let (name, open) = method_head(line)?;
    let b = line.as_bytes();
    let mut depth = 0i64;
    let mut close = None;
    for (i, &c) in b.iter().enumerate().skip(open) {
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                close = Some(i);
                break;
            }
        }
    }
    let rest = js_trim(&line[close? + 1..]);
    if rest == "{" {
        return Some(name);
    }
    if rest.starts_with(':') && !rest.contains('=') && !rest.contains('(') {
        return Some(name);
    }
    None
}

/// Имена, которые вызывают добавленные строки, со счётом строк-вызывателей.
/// `new Name(` и `name(`; вызов через точку (`obj.name(`) не берётся.
pub fn extract_used_identifiers(added_lines: &[String]) -> IndexMap<String, usize> {
    let mut counts: IndexMap<String, usize> = IndexMap::new();
    for raw in added_lines {
        let trimmed = js_trim(raw);
        if trimmed.is_empty() || utf16_len(trimmed) > MAX_CALL_LINE_CHARS || starts_comment_like(trimmed) {
            continue;
        }
        let line = strip_literals_and_comment(trimmed);
        let declared = method_declared_name(&line);
        let b = line.as_bytes();
        let mut seen = HashSet::new();
        let mut i = 0;
        while i < b.len() {
            // CALL_RE = \bnew\s+(ident) | (?<![\w$])(ident)\s*\(
            let mut m: Option<(usize, usize, bool, usize)> = None; // (start имени, end имени, m2, конец матча)
            if b[i..].starts_with(b"new") && (i == 0 || !is_w(b[i - 1])) {
                let q = skip_ws(&line, i + 3);
                if q > i + 3 {
                    if let Some(e) = ident_end(b, q) {
                        m = Some((q, e, false, e));
                    }
                }
            }
            if m.is_none() && (i == 0 || !is_wd(b[i - 1])) {
                if let Some(e) = ident_end(b, i) {
                    let q = skip_ws(&line, e);
                    if b.get(q) == Some(&b'(') {
                        m = Some((i, e, true, q + 1));
                    }
                }
            }
            let Some((ns, ne, is_m2, end)) = m else {
                i += 1;
                continue;
            };
            let idx = i;
            i = end;
            let name = &line[ns..ne];
            if name.len() < 2 || KEYWORDS.contains(&name) || GLOBALS.contains(&name) {
                continue;
            }
            if is_m2 && preceded_by_dot(b, idx) {
                continue;
            }
            if is_m2 && Some(name) == declared && line.find(name) == Some(idx) {
                continue;
            }
            if !seen.insert(name.to_string()) {
                continue;
            }
            *counts.entry(name.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

// ── Объявление имени в тексте (`declarationRegex(name).test(text)`) ──

const DECL_D_MODS: &[&str] = &["async", "static", "get", "set", "public", "private", "protected"];

/// Есть ли в тексте объявление `name`: функция, класс/interface/enum/namespace/type,
/// const/let/var, метод в начале строки, свойство-функция. Линейный разбор пяти
/// альтернатив `declarationRegex` (JS) с флагом `m`.
pub fn declaration_test(name: &str, text: &str) -> bool {
    if name.is_empty() || !text.contains(name) {
        return false;
    }
    let b = text.as_bytes();
    let name_at = |p: usize| text.get(p..).is_some_and(|r| r.starts_with(name));
    // A: function\*?\s+NAME\b
    for i in occurrences(text, "function") {
        let mut p = i + 8;
        if b.get(p) == Some(&b'*') {
            p += 1;
        }
        let q = skip_ws(text, p);
        if q > p && name_at(q) && word_boundary(b, q + name.len()) {
            return true;
        }
    }
    // B: (?:class|interface|enum|namespace|type)\s+NAME\b
    for kw in ["class", "interface", "enum", "namespace", "type"] {
        for i in occurrences(text, kw) {
            let p = i + kw.len();
            let q = skip_ws(text, p);
            if q > p && name_at(q) && word_boundary(b, q + name.len()) {
                return true;
            }
        }
    }
    // C: (?:const|let|var)\s+NAME\s*=
    for kw in ["const", "let", "var"] {
        for i in occurrences(text, kw) {
            let p = i + kw.len();
            let q = skip_ws(text, p);
            if q > p && name_at(q) && b.get(skip_ws(text, q + name.len())) == Some(&b'=') {
                return true;
            }
        }
    }
    // E: \bNAME\s*:\s*(?:async\s*)?(?:function\b|\()
    for i in occurrences(text, name) {
        if !word_boundary(b, i) {
            continue;
        }
        let p = skip_ws(text, i + name.len());
        if b.get(p) != Some(&b':') {
            continue;
        }
        let mut q = skip_ws(text, p + 1);
        if text[q..].starts_with("async") {
            q = skip_ws(text, q + 5);
        }
        if b.get(q) == Some(&b'(') || (text[q..].starts_with("function") && word_boundary(b, q + 8)) {
            return true;
        }
    }
    // D: ^[ \t]*(?:export[ \t]+)?(?:(?:mods)[ \t]+)*NAME[ \t]*\([^)\n]*\)[ \t]*(?::[^{\n]*)?\{
    let mut starts = vec![0usize];
    for (i, c) in text.char_indices() {
        if is_lt(c) {
            starts.push(i + c.len_utf8());
        }
    }
    let tail = |p: usize| -> bool {
        // NAME уже совпало в p
        let q = skip_sp_tab(b, p + name.len());
        if b.get(q) != Some(&b'(') {
            return false;
        }
        let mut r = q + 1;
        while r < b.len() && b[r] != b')' && b[r] != b'\n' {
            r += 1;
        }
        if b.get(r) != Some(&b')') {
            return false;
        }
        let mut s = skip_sp_tab(b, r + 1);
        if b.get(s) == Some(&b':') {
            s += 1;
            while s < b.len() && b[s] != b'{' && b[s] != b'\n' {
                s += 1;
            }
        }
        b.get(s) == Some(&b'{')
    };
    for st in starts {
        let p = skip_sp_tab(b, st);
        let mut bases = vec![p];
        if let Some(q) = word_then_sp_tab(b, p, "export") {
            bases.insert(0, q);
        }
        for base in bases {
            for pos in modifier_positions(text, base, DECL_D_MODS, true) {
                if name_at(pos) && tail(pos) {
                    return true;
                }
            }
        }
    }
    false
}

// ── Аннотированная секция диффа ──

/// Добавленная строка аннотированной секции: RIGHT-номер и текст.
#[derive(Debug, Clone, PartialEq)]
pub struct AddedEntry {
    pub line: f64,
    pub text: String,
}

/// `^\s*(\d+): \+(.*)$` без флага `m`: в тексте после `+` нет перевода строки.
fn parse_added_line(raw: &str) -> Option<AddedEntry> {
    let p = skip_ws(raw, 0);
    let b = raw.as_bytes();
    let mut e = p;
    while e < b.len() && b[e].is_ascii_digit() {
        e += 1;
    }
    if e == p || !raw[e..].starts_with(": +") {
        return None;
    }
    let text = &raw[e + 3..];
    if text.chars().any(is_lt) {
        return None;
    }
    Some(AddedEntry { line: raw[p..e].parse::<f64>().unwrap_or(f64::INFINITY), text: text.to_string() })
}

/// Добавленные строки с RIGHT-номером.
pub fn added_entries_of_section(section: &str) -> Vec<AddedEntry> {
    section.split('\n').filter_map(parse_added_line).collect()
}

/// Добавленные строки из аннотированной секции диффа файла.
pub fn added_lines_of_section(section: &str) -> Vec<String> {
    added_entries_of_section(section).into_iter().map(|e| e.text).collect()
}

// ── Лексер по UTF-16 ──

const fn u(c: char) -> u16 {
    c as u16
}

/// `\s` JS для одного юнита.
fn ws16(x: u16) -> bool {
    char::from_u32(x as u32).is_some_and(js_is_whitespace)
}

fn wd16(x: u16) -> bool {
    x < 128 && is_wd(x as u8)
}

fn in_set16(x: u16, set: &str) -> bool {
    x < 128 && set.as_bytes().contains(&(x as u8))
}

fn starts16(s: &[u16], p: usize, w: &str) -> bool {
    let wb = w.as_bytes();
    s.len() >= p + wb.len() && s[p..p + wb.len()].iter().zip(wb).all(|(a, b)| *a == *b as u16)
}

fn skip_ws16(s: &[u16], mut p: usize) -> usize {
    while p < s.len() && ws16(s[p]) {
        p += 1;
    }
    p
}

/// После этих слов `/` начинает литерал регулярки (`REGEX_AFTER_KEYWORD` на срезе 24 юнитов).
fn regex_after_keyword(line: &[u16], i: usize) -> bool {
    let s = &line[i.saturating_sub(24)..i];
    let mut end = s.len();
    while end > 0 && ws16(s[end - 1]) {
        end -= 1;
    }
    const KWS: [&str; 13] =
        ["return", "typeof", "case", "do", "else", "in", "of", "void", "yield", "throw", "delete", "instanceof", "await"];
    KWS.iter().any(|kw| {
        let n = kw.len();
        end >= n && starts16(s, end - n, kw) && (end == n || !wd16(s[end - n - 1]))
    })
}

/// Может ли `/` после этого символа начинать литерал регулярки (а не деление).
fn regex_can_start(prev: Option<u16>) -> bool {
    match prev {
        None => true,
        Some(p) => in_set16(p, "(,=:[!&|?{};+-*%<>~^"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LexState {
    Code,
    Sq,
    Dq,
    Line,
    Block,
    Regex,
    RegexClass,
}

#[derive(Clone, Copy)]
enum Ctx {
    Code(i64),
    Tpl,
}

/// Файл, где содержимое строк, шаблонов (кроме `${…}`), регулярок и комментариев заменено
/// пробелами, — построчно. Один проход, линейно; разделители сохраняются.
pub fn mask_non_code(lines: &[String]) -> Vec<String> {
    use LexState::*;
    let mut out = Vec::with_capacity(lines.len());
    let mut stack = vec![Ctx::Code(0)];
    let mut state = Code;
    let mut prev_sig: Option<u16> = None;
    for raw in lines {
        let line: Vec<u16> = raw.encode_utf16().collect();
        let mut masked: Vec<u16> = Vec::with_capacity(line.len());
        let sp = u(' ');
        let mut i = 0;
        while i < line.len() {
            let ch = line[i];
            let nx = line.get(i + 1).copied();
            let top_tpl = matches!(stack.last(), Some(Ctx::Tpl));
            if state == Block {
                if ch == u('*') && nx == Some(u('/')) {
                    state = Code;
                    masked.extend([sp, sp]);
                    i += 2;
                } else {
                    masked.push(sp);
                    i += 1;
                }
                continue;
            }
            if state == Sq || state == Dq {
                if ch == u('\\') {
                    masked.extend([sp, sp]);
                    i += 2;
                    continue;
                }
                if (state == Sq && ch == u('\'')) || (state == Dq && ch == u('"')) {
                    state = Code;
                    masked.push(ch);
                    prev_sig = Some(ch);
                    i += 1;
                    continue;
                }
                masked.push(sp);
                i += 1;
                continue;
            }
            if state == Regex || state == RegexClass {
                if ch == u('\\') {
                    masked.extend([sp, sp]);
                    i += 2;
                    continue;
                }
                if state == Regex && ch == u('[') {
                    state = RegexClass;
                } else if state == RegexClass && ch == u(']') {
                    state = Regex;
                } else if state == Regex && ch == u('/') {
                    state = Code;
                    masked.push(ch);
                    prev_sig = Some(u('/'));
                    i += 1;
                    continue;
                }
                masked.push(sp);
                i += 1;
                continue;
            }
            if top_tpl {
                if ch == u('\\') {
                    masked.extend([sp, sp]);
                    i += 2;
                    continue;
                }
                if ch == u('`') {
                    stack.pop();
                    masked.push(ch);
                    prev_sig = Some(u('`'));
                    i += 1;
                    continue;
                }
                if ch == u('$') && nx == Some(u('{')) {
                    stack.push(Ctx::Code(0));
                    masked.extend([u('$'), u('{')]);
                    i += 2;
                    continue;
                }
                masked.push(sp);
                i += 1;
                continue;
            }
            if ch == u('/') && nx == Some(u('/')) {
                masked.extend(std::iter::repeat_n(sp, line.len() - i));
                break;
            }
            if ch == u('/') && nx == Some(u('*')) {
                state = Block;
                masked.extend([sp, sp]);
                i += 2;
                continue;
            }
            if ch == u('\'') {
                state = Sq;
                masked.push(ch);
                i += 1;
                continue;
            }
            if ch == u('"') {
                state = Dq;
                masked.push(ch);
                i += 1;
                continue;
            }
            if ch == u('`') {
                stack.push(Ctx::Tpl);
                masked.push(ch);
                i += 1;
                continue;
            }
            if ch == u('/') && (regex_can_start(prev_sig) || regex_after_keyword(&line, i)) {
                state = Regex;
                masked.push(ch);
                i += 1;
                continue;
            }
            if stack.len() > 1 {
                if let Some(Ctx::Code(depth)) = stack.last_mut() {
                    if ch == u('{') {
                        *depth += 1;
                    } else if ch == u('}') {
                        if *depth == 0 {
                            stack.pop();
                            masked.push(ch);
                            i += 1;
                            continue;
                        }
                        *depth -= 1;
                    }
                }
            }
            masked.push(ch);
            if !ws16(ch) {
                prev_sig = Some(ch);
            }
            i += 1;
        }
        // строковый литерал и регулярка не переносятся через строку — сброс, как у парсера
        if matches!(state, Sq | Dq | Regex | RegexClass) {
            state = Code;
        }
        out.push(String::from_utf16_lossy(&masked));
    }
    out
}

/// Заголовок объявления (до тела) и строка конца (0-based) или `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Declaration {
    pub header: String,
    pub end_idx: Option<usize>,
}

fn tidy_header(header: &[u16]) -> String {
    let s = String::from_utf16_lossy(header);
    let s = js_trim(&js_collapse_whitespace(&s)).to_string();
    if utf16_len(&s) > MAX_DECL_CHARS {
        format!("{}…", utf16_prefix(&s, MAX_DECL_CHARS - 1))
    } else {
        s
    }
}

/// `^\s*(?:[\w$]+\s*:\s*)?(?:async\s*)?\(` — стрелка в свойстве объекта или async-стрелка.
fn header_is_arrow_params(h: &[u16]) -> bool {
    let p = skip_ws16(h, 0);
    let mut bases = Vec::with_capacity(2);
    let mut e = p;
    while e < h.len() && wd16(h[e]) {
        e += 1;
    }
    if e > p {
        let q = skip_ws16(h, e);
        if h.get(q) == Some(&u(':')) {
            bases.push(skip_ws16(h, q + 1));
        }
    }
    bases.push(p);
    bases.into_iter().any(|base| {
        (starts16(h, base, "async") && h.get(skip_ws16(h, base + 5)) == Some(&u('('))) || h.get(base) == Some(&u('('))
    })
}

/// `^\s*(?:async\s+)?[\w$]+\s*=>` — `x => …` без скобок.
fn header_is_bare_arrow(h: &[u16]) -> bool {
    let p = skip_ws16(h, 0);
    let try_at = |q: usize| {
        let mut e = q;
        while e < h.len() && wd16(h[e]) {
            e += 1;
        }
        e > q && {
            let r = skip_ws16(h, e);
            h.get(r) == Some(&u('=')) && h.get(r + 1) == Some(&u('>'))
        }
    };
    if starts16(h, p, "async") {
        let q = skip_ws16(h, p + 5);
        if q > p + 5 && try_at(q) {
            return true;
        }
    }
    try_at(p)
}

/// Объявление, начинающееся на строке `start_idx` (0-based): заголовок (до тела) и строка
/// конца — пара к открывающей `{` тела, `;` на нулевой глубине или конец выражения
/// стрелочной функции. Неоднозначность (незакрытый литерал, > `max_lines`) → `end_idx: None`.
pub fn scan_declaration(lines: &[String], start_idx: usize, max_lines: usize) -> Declaration {
    use LexState::*;
    if start_idx >= lines.len() {
        return Declaration { header: String::new(), end_idx: None };
    }
    let (mut paren, mut bracket, mut brace) = (0i64, 0i64, 0i64);
    let (mut angle, mut type_brace) = (0i64, 0i64);
    let mut seen_assign = false;
    let (mut body_open, mut arrow_expr, mut header_done) = (false, false, false);
    let mut header: Vec<u16> = Vec::new();
    let mut stack = vec![Ctx::Code(0)];
    let mut state = Code;
    let mut prev_sig: Option<u16> = None;
    let last = lines.len().min(start_idx.saturating_add(max_lines));
    let done = |h: &[u16], end: Option<usize>| Declaration { header: tidy_header(h), end_idx: end };

    for li in start_idx..last {
        let line: Vec<u16> = lines[li].encode_utf16().collect();
        if state == Line {
            state = Code;
        }
        let mut i = 0;
        while i < line.len() {
            let ch = line[i];
            let nx = line.get(i + 1).copied();
            let top_tpl = matches!(stack.last(), Some(Ctx::Tpl));
            if state == Block {
                if ch == u('*') && nx == Some(u('/')) {
                    state = Code;
                    i += 1;
                }
                i += 1;
                continue;
            }
            let comment_start =
                state == Code && !top_tpl && ch == u('/') && (nx == Some(u('/')) || nx == Some(u('*')));
            if !header_done && !comment_start {
                header.push(ch);
            }
            if state == Sq || state == Dq {
                if ch == u('\\') {
                    i += 2;
                    continue;
                }
                if (state == Sq && ch == u('\'')) || (state == Dq && ch == u('"')) {
                    state = Code;
                }
                i += 1;
                continue;
            }
            if state == Regex || state == RegexClass {
                if ch == u('\\') {
                    i += 2;
                    continue;
                }
                if state == Regex && ch == u('[') {
                    state = RegexClass;
                } else if state == RegexClass && ch == u(']') {
                    state = Regex;
                } else if state == Regex && ch == u('/') {
                    state = Code;
                    prev_sig = Some(u('/'));
                }
                i += 1;
                continue;
            }
            if top_tpl {
                if ch == u('\\') {
                    i += 2;
                    continue;
                }
                if ch == u('`') {
                    stack.pop();
                    prev_sig = Some(u('`'));
                    i += 1;
                    continue;
                }
                if ch == u('$') && nx == Some(u('{')) {
                    stack.push(Ctx::Code(0));
                    i += 1;
                }
                i += 1;
                continue;
            }
            // code
            if ch == u('/') && nx == Some(u('/')) {
                state = Line;
                break;
            }
            if ch == u('/') && nx == Some(u('*')) {
                state = Block;
                i += 2;
                continue;
            }
            if ch == u('\'') {
                state = Sq;
                i += 1;
                continue;
            }
            if ch == u('"') {
                state = Dq;
                i += 1;
                continue;
            }
            if ch == u('`') {
                stack.push(Ctx::Tpl);
                i += 1;
                continue;
            }
            if ch == u('/') && (regex_can_start(prev_sig) || regex_after_keyword(&line, i)) {
                state = Regex;
                i += 1;
                continue;
            }
            if stack.len() > 1 {
                // внутри `${…}` шаблона: своя глубина, закрывающая `}` возвращает в шаблон
                if let Some(Ctx::Code(depth)) = stack.last_mut() {
                    if ch == u('{') {
                        *depth += 1;
                    } else if ch == u('}') {
                        if *depth == 0 {
                            stack.pop();
                            i += 1;
                            continue;
                        }
                        *depth -= 1;
                    }
                }
                if !ws16(ch) {
                    prev_sig = Some(ch);
                }
                i += 1;
                continue;
            }
            let top0 = paren == 0 && bracket == 0 && brace == 0 && !body_open && !arrow_expr;
            if top0 && ch == u('<') && prev_sig.is_some_and(|p| wd16(p) || p == u('<')) {
                angle += 1;
                prev_sig = Some(ch);
                i += 1;
                continue;
            }
            if top0 && ch == u('>') && angle > 0 {
                angle -= 1;
                prev_sig = Some(ch);
                i += 1;
                continue;
            }
            if top0 && ch == u('{') && (angle > 0 || type_brace > 0 || prev_sig.is_some_and(|p| in_set16(p, ":|&"))) {
                type_brace += 1;
                prev_sig = Some(ch);
                i += 1;
                continue;
            }
            if top0 && ch == u('}') && type_brace > 0 {
                type_brace -= 1;
                prev_sig = Some(ch);
                i += 1;
                continue;
            }
            if top0
                && ch == u('=')
                && nx != Some(u('>'))
                && nx != Some(u('='))
                && !(i > 0 && in_set16(line[i - 1], "=!<>"))
                && angle == 0
                && type_brace == 0
            {
                seen_assign = true;
            }
            if ch == u('(') {
                paren += 1;
            } else if ch == u(')') {
                paren -= 1;
            } else if ch == u('[') {
                bracket += 1;
            } else if ch == u(']') {
                bracket -= 1;
            } else if ch == u('{') {
                if !body_open && !arrow_expr && paren == 0 && bracket == 0 && brace == 0 {
                    body_open = true;
                    if !header_done {
                        header.pop();
                        header_done = true;
                    }
                }
                brace += 1;
            } else if ch == u('}') {
                brace -= 1;
                if body_open && brace == 0 {
                    return done(&header, Some(li));
                }
                if brace < 0 {
                    return done(&header, None);
                }
            } else if ch == u(';')
                && paren == 0
                && bracket == 0
                && brace == 0
                && angle == 0
                && type_brace == 0
                && !body_open
            {
                if !header_done {
                    header.pop();
                }
                return done(&header, Some(li));
            } else if ch == u('=') && nx == Some(u('>')) && paren == 0 && bracket == 0 && brace == 0 && !body_open {
                if !header_done {
                    header.push(u('>'));
                }
                i += 1;
                // `=>` — стрелочная функция объявления только после присваивания или в
                // свойстве объекта; иначе это стрелка внутри аннотации типа.
                if !seen_assign && !header_is_arrow_params(&header) && !header_is_bare_arrow(&header) {
                    prev_sig = Some(u('>'));
                    i += 1;
                    continue;
                }
                // `=> {` — тело в скобках; иначе — выражение до `;` или конца строки
                let rest_start = skip_ws16(&line, i + 1);
                if rest_start < line.len() {
                    if line[rest_start] != u('{') {
                        arrow_expr = true;
                        header_done = true;
                    }
                } else {
                    let next = lines.get(li + 1).map(String::as_str).unwrap_or("");
                    if !next.trim_start_matches(js_is_whitespace).starts_with('{') {
                        arrow_expr = true;
                        header_done = true;
                    }
                }
                prev_sig = Some(u('>'));
                i += 1;
                continue;
            }
            if !ws16(ch) {
                prev_sig = Some(ch);
            }
            i += 1;
        }
        if matches!(state, Sq | Dq | Regex | RegexClass) {
            return done(&header, None); // незакрытый литерал на строке
        }
        if !header_done {
            header.push(u('\n'));
            if li - start_idx + 1 >= MAX_HEADER_LINES {
                header_done = true;
            }
        }
        if arrow_expr && paren == 0 && bracket == 0 && brace == 0 && stack.len() == 1 && state != Block {
            let trimmed = lines[li].trim_end_matches(js_is_whitespace);
            let continues = trimmed.chars().last().is_some_and(|c| ",([{=+-*/&|?:.".contains(c));
            if !continues {
                // выражение продолжается цепочкой или оператором на следующей строке
                let next = lines[li + 1..last].iter().find(|l| !js_trim(l).is_empty());
                let chained = next.is_some_and(|n| {
                    let t = n.trim_start_matches(js_is_whitespace);
                    t.starts_with('?')
                        || t.starts_with('.')
                        || t.starts_with(':')
                        || t.starts_with("&&")
                        || t.starts_with("||")
                        || t.starts_with(['+', '-', '*', '/', '%'])
                        || t.starts_with("=>")
                });
                if !chained {
                    return done(&header, Some(li));
                }
            }
        }
    }
    done(&header, None)
}

/// Конец объявления (0-based) или `None`.
pub fn find_declaration_end(lines: &[String], start_idx: usize, max_lines: usize) -> Option<usize> {
    scan_declaration(lines, start_idx, max_lines).end_idx
}

const SIG_MODS: &[&str] = &["public", "private", "protected", "static", "async", "abstract", "override"];

/// `^(?:export\s+)?(?:declare\s+)?(?:async\s+)?function\s+NAME\b|^(?:(?:mods)\s+)*NAME\s*[<(]`.
fn overload_signature(t: &str, name: &str) -> bool {
    let b = t.as_bytes();
    let mut p = 0;
    for w in ["export", "declare", "async"] {
        if let Some(q) = word_then_ws(t, p, w) {
            p = q;
        }
    }
    if let Some(q) = word_then_ws(t, p, "function") {
        if t[q..].starts_with(name) && word_boundary(b, q + name.len()) {
            return true;
        }
    }
    modifier_positions(t, 0, SIG_MODS, false).into_iter().any(|pos| {
        t[pos..].starts_with(name) && matches!(b.get(skip_ws(t, pos + name.len())), Some(b'<' | b'('))
    })
}

/// `^\s*(?:(?:public|private|protected)\s+)?constructor\s*\(`.
fn is_constructor_line(line: &str) -> bool {
    let p = skip_ws(line, 0);
    let mut bases = vec![p];
    for m in ["public", "private", "protected"] {
        if let Some(q) = word_then_ws(line, p, m) {
            bases.insert(0, q);
        }
    }
    bases.into_iter().any(|q| line[q..].starts_with("constructor") && line.as_bytes().get(skip_ws(line, q + 11)) == Some(&b'('))
}

/// `(?:^|\s)class\s`.
fn mentions_class(header: &str) -> bool {
    occurrences(header, "class").any(|i| {
        (i == 0 || header[..i].chars().last().is_some_and(js_is_whitespace))
            && header[i + 5..].chars().next().is_some_and(js_is_whitespace)
    })
}

/// Объявление для карточки: заголовок плюс перегрузки TS подряд и сигнатура
/// `constructor(…)` класса (аудит RFC 14, п. 1.6).
pub fn card_declaration(lines: &[String], start_idx: usize, name: &str) -> Declaration {
    let first = scan_declaration(lines, start_idx, MAX_SCAN_LINES);
    if first.header.is_empty() {
        return first;
    }
    let Declaration { mut header, mut end_idx } = first;
    let start_line = lines.get(start_idx).map(|l| js_trim(l)).unwrap_or("");
    if !name.is_empty() && end_idx == Some(start_idx) && start_line.ends_with(';') {
        let mut sigs = vec![header.clone()];
        let mut j = start_idx + 1;
        let mut k = 0;
        while k < MAX_OVERLOADS && j < lines.len() {
            k += 1;
            let t = js_trim(&lines[j]);
            if t.is_empty() || starts_comment_like(t) {
                j += 1;
                continue;
            }
            if !overload_signature(t, name) {
                break;
            }
            let r = scan_declaration(lines, j, MAX_SCAN_LINES);
            if r.header.is_empty() {
                break;
            }
            sigs.push(r.header.clone());
            end_idx = r.end_idx;
            // перегрузка — одна строка с `;`; реализация с телом закрывает серию
            if r.end_idx != Some(j) || !t.ends_with(';') {
                break;
            }
            j += 1;
        }
        let uniq: IndexSet<String> = sigs.into_iter().collect();
        header = uniq.into_iter().collect::<Vec<_>>().join("\n");
    }
    if mentions_class(&header) {
        if let Some(end) = end_idx {
            let mut k = start_idx + 1;
            while k <= end && k < lines.len() {
                if is_constructor_line(&lines[k]) {
                    let ctor = scan_declaration(lines, k, MAX_SCAN_LINES).header;
                    if !ctor.is_empty() {
                        header = format!("{header} {{ {ctor} }}");
                    }
                    break;
                }
                k += 1;
            }
        }
    }
    Declaration { header, end_idx }
}

// ── Граф и рабочее дерево ──

/// Лексическая нормализация абсолютного пути (`.`/`..`), как `path.resolve`.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Путь узла графа относительно корня; абсолютный путь вне корня → пусто.
fn normalize_node_path(p: &str, root: &Path) -> String {
    let s = p.replace('\\', "/");
    if s.is_empty() {
        return String::new();
    }
    let s = if s.starts_with('/') {
        let abs = lexical_normalize(Path::new(&s));
        let base = lexical_normalize(root);
        let rel = match abs.strip_prefix(&base) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => return String::new(),
        };
        if rel.starts_with("..") {
            return String::new();
        }
        rel
    } else {
        s
    };
    s.strip_prefix("./").map(str::to_string).unwrap_or(s)
}

/// `/^L?(\d+)/` по `source_location`; нет числа → 0.
fn node_line(node: &Value) -> usize {
    let loc = match node.get("source_location") {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_string(v),
    };
    let rest = loc.strip_prefix('L').unwrap_or(&loc);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return 0;
    }
    digits.parse::<f64>().map(|x| if x >= usize::MAX as f64 { usize::MAX } else { x as usize }).unwrap_or(0)
}

/// Строки файла рабочего дерева: инъекция для [`build_symbol_cards`].
pub type ReadLines<'a> = dyn Fn(&str) -> Option<Arc<Vec<String>>> + 'a;

/// Чтение файла рабочего дерева построчно, с кэшем и защитой от выхода за корень.
///
/// Путь приходит из графа, а граф и дерево в CI — версия PR: автор PR может положить
/// symlink `lib/x.mjs → ~/.ssh/…`. Поэтому сверка идёт ПОСЛЕ разыменования ссылок.
pub struct LineReader {
    base: PathBuf,
    cache: Mutex<HashMap<String, Option<Arc<Vec<String>>>>>,
}

impl LineReader {
    pub fn new(root: &Path) -> Self {
        let abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        let base = std::fs::canonicalize(&abs).unwrap_or_else(|_| lexical_normalize(&abs));
        Self { base, cache: Mutex::new(HashMap::new()) }
    }

    pub fn read(&self, path: &str) -> Option<Arc<Vec<String>>> {
        if let Some(hit) = self.cache.lock().ok().and_then(|c| c.get(path).cloned()) {
            return hit;
        }
        let lines = self.read_uncached(path);
        if let Ok(mut c) = self.cache.lock() {
            c.insert(path.to_string(), lines.clone());
        }
        lines
    }

    fn read_uncached(&self, path: &str) -> Option<Arc<Vec<String>>> {
        if path.contains('\0') {
            return None;
        }
        let real = std::fs::canonicalize(self.base.join(path)).ok()?;
        if real == self.base || !real.starts_with(&self.base) {
            return None;
        }
        if std::fs::metadata(&real).ok()?.len() > MAX_FILE_BYTES {
            return None;
        }
        let bytes = std::fs::read(&real).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        Some(Arc::new(text.split('\n').map(str::to_string).collect()))
    }
}

/// Тесты, моки и фикстуры: их `jest.fn()` — не объявление символа приложения.
fn is_test_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    for dir in ["__tests__/", "__mocks__/", "__fixtures__/", "test/", "tests/", "spec/", "fixture/", "fixtures/"] {
        for i in occurrences(&lower, dir) {
            if i == 0 || lower.as_bytes()[i - 1] == b'/' {
                return true;
            }
        }
    }
    // \.(?:spec|test|stories)\.[cm]?[jt]sx?$
    for kind in [".spec.", ".test.", ".stories."] {
        for i in occurrences(&lower, kind) {
            let mut ext = &lower[i + kind.len()..];
            if let Some(r) = ext.strip_prefix(['c', 'm']) {
                ext = r;
            }
            if matches!(ext, "js" | "ts" | "jsx" | "tsx") {
                return true;
            }
        }
    }
    false
}

/// Результат [`resolve_card_node`]: узел или причина пропуска.
#[derive(Debug, Clone, PartialEq)]
pub struct CardNode<'a> {
    pub node: Option<&'a Value>,
    pub reason: Option<&'static str>,
}

/// Узел графа для имени — только если он ЕДИНСТВЕННЫЙ среди всех способов назвать символ:
/// `name`, `name()` и «голая» метка, с учётом регистра (`Parser` ≠ `parser`).
pub fn resolve_card_node<'a>(index: Option<&'a GraphIndex>, name: &str) -> CardNode<'a> {
    let Some(index) = index.filter(|_| !name.is_empty()) else {
        return CardNode { node: None, reason: Some("unresolved") };
    };
    let lc = name.to_lowercase();
    let mut seen: IndexMap<String, &Value> = IndexMap::new();
    let lists = index
        .nodes_with_exact_label(&lc)
        .chain(index.nodes_with_exact_label(&format!("{lc}()")))
        .chain(index.nodes_with_bare_label(&lc));
    for n in lists {
        let has_id = !matches!(n.get("id"), None | Some(Value::Null));
        if has_id && bare_label(&js_string_truthy(n.get("label"))) == name {
            seen.insert(js_string(&n["id"]), n);
        }
    }
    match seen.len() {
        1 => CardNode { node: seen.values().next().copied(), reason: None },
        0 => CardNode { node: None, reason: Some("unresolved") },
        _ => CardNode { node: None, reason: Some("ambiguous") },
    }
}

/// Первая деструктуризация `\b(?:const|let|var)\s*([{[][^=]*[}\]])\s*=` строки: группа.
fn first_destructuring(line: &str) -> Option<&str> {
    let b = line.as_bytes();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for kw in ["const", "let", "var"] {
        hits.extend(occurrences(line, kw).map(|i| (i, kw.len())));
    }
    hits.sort_unstable();
    for (i, n) in hits {
        if i > 0 && is_w(b[i - 1]) {
            continue;
        }
        let open = skip_ws(line, i + n);
        if !matches!(b.get(open), Some(b'{' | b'[')) {
            continue;
        }
        let Some(eq) = line[open + 1..].find('=').map(|e| open + 1 + e) else { continue };
        let mut k = eq;
        while k > open + 1 && line[..k].chars().last().is_some_and(js_is_whitespace) {
            k -= line[..k].chars().last().map_or(1, char::len_utf8);
        }
        // символ перед пробелами — закрывающая скобка, и она не сама открывающая
        if k > open + 1 && matches!(b[k - 1], b'}' | b']') {
            return Some(&line[open..k]);
        }
    }
    None
}

/// Группы `\(([^()]*)\)\s*(?::[^={]*)?(?:=>|\{)` (matchAll).
fn param_groups(line: &str) -> Vec<&str> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'(' {
            i += 1;
            continue;
        }
        let mut r = i + 1;
        while r < b.len() && b[r] != b'(' && b[r] != b')' {
            r += 1;
        }
        if b.get(r) != Some(&b')') {
            i += 1;
            continue;
        }
        let mut s = skip_ws(line, r + 1);
        if b.get(s) == Some(&b':') {
            s += 1;
            while s < b.len() && b[s] != b'=' && b[s] != b'{' {
                s += 1;
            }
        }
        let end = if b.get(s) == Some(&b'{') {
            Some(s + 1)
        } else if b.get(s) == Some(&b'=') && b.get(s + 1) == Some(&b'>') {
            Some(s + 2)
        } else {
            None
        };
        match end {
            Some(e) => {
                out.push(&line[i + 1..r]);
                i = e;
            }
            None => i += 1,
        }
    }
    out
}

/// Связано ли имя в самом файле: объявление, параметр функции, деструктуризация, catch.
/// Тогда вызов идёт к локальному символу, и одноимённый узел из другого файла — чужой.
pub fn has_local_binding(lines: &[String], name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    for line in lines {
        if utf16_len(line) > LOCAL_SCAN_MAX_LINE || !line.contains(name) {
            continue;
        }
        if declaration_test(name, line) {
            return true;
        }
        if first_destructuring(line).is_some_and(|g| word_occurs(g, name)) {
            return true;
        }
        if param_groups(line).into_iter().any(|g| word_occurs(g, name)) {
            return true;
        }
        // (?:^|[^\w$.])NAME\s*=>
        let b = line.as_bytes();
        for i in occurrences(line, name) {
            if i > 0 && (is_wd(b[i - 1]) || b[i - 1] == b'.') {
                continue;
            }
            let q = skip_ws(line, i + name.len());
            if line[q..].starts_with("=>") {
                return true;
            }
        }
    }
    false
}

// ── Карточки ──

/// Карточка символа: место объявления и его текст.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolCard {
    pub name: String,
    pub path: String,
    pub line: usize,
    pub end_line: Option<usize>,
    pub declaration: String,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Вход [`build_symbol_cards`].
pub struct BuildOpts<'a> {
    /// `buildGraphIndex` (`BlastRadius::graph_index`); `None` → карточек нет.
    pub graph_index: Option<&'a GraphIndex>,
    /// Аннотированная секция диффа по пути.
    pub sections: &'a HashMap<String, String>,
    /// Текст контекста, который модель видит по пути.
    pub windows: &'a HashMap<String, String>,
    /// Строки файла рабочего дерева (`LineReader::read` в проде).
    pub read_lines: &'a ReadLines<'a>,
    /// `limits.symbolCardsBodyMaxLines`: короткое тело едет целиком; 0 — выключено.
    pub body_max_lines: f64,
    /// Корень для абсолютных путей графа.
    pub root: &'a Path,
    /// `profile.layout.appDir`: префикс для путей графа, построенного от него.
    pub app_dir: &'a str,
}

/// Результат [`build_symbol_cards`]: карточки по изменённому пути и метрики для лога.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolCards {
    pub by_path: IndexMap<String, Vec<SymbolCard>>,
    pub metrics: Value,
}

fn bump(reasons: &mut Map<String, Value>, key: &str) {
    let n = reasons.get(key).and_then(Value::as_u64).unwrap_or(0);
    reasons.insert(key.to_string(), json!(n + 1));
}

struct QueueItem {
    name: String,
    count: usize,
    binding: Option<ImportBinding>,
    via_namespace: bool,
}

/// Карточки по файлам PR (`buildSymbolCards`).
pub fn build_symbol_cards(paths: &[String], opts: &BuildOpts) -> SymbolCards {
    let mut by_path = IndexMap::new();
    let mut skip = Map::new();
    let (mut candidates, mut resolved, mut cards, mut import_resolved) = (0usize, 0usize, 0usize, 0usize);
    let Some(graph) = opts.graph_index else {
        return SymbolCards {
            by_path,
            metrics: json!({ "candidates": 0, "resolved": 0, "cards": 0, "skip_reasons": { "graph_unavailable": 1 } }),
        };
    };
    // Math.max(0, Number(x) || 0): NaN → 0, +∞ остаётся
    let body_max_lines = if opts.body_max_lines.is_nan() { 0.0 } else { opts.body_max_lines.max(0.0) };
    let root = std::path::absolute(opts.root).unwrap_or_else(|_| opts.root.to_path_buf());
    let root = root.as_path();
    let app_dir = {
        let mut s = opts.app_dir;
        if let Some(r) = s.strip_prefix("./") {
            s = r.trim_start_matches('/');
        }
        s.trim_end_matches('/').to_string()
    };
    let mut by_file: HashMap<String, Vec<&Value>> = HashMap::new();
    for n in graph.nodes_by_id() {
        let f = normalize_node_path(&js_string_truthy(n.get("source_file")), root);
        if !f.is_empty() {
            by_file.entry(f).or_default().push(n);
        }
    }
    let app_prefix = format!("{app_dir}/");
    let has_file = |f: &str| {
        by_file.contains_key(f)
            || (!app_dir.is_empty() && f.starts_with(&app_prefix) && by_file.contains_key(&f[app_prefix.len()..]))
    };
    let graph_path = |f: &str| -> String {
        if by_file.contains_key(f) {
            f.to_string()
        } else {
            f.get(app_dir.len() + 1..).unwrap_or("").to_string()
        }
    };
    let node_in_file = |file: &str, name: &str| -> Option<&Value> {
        let found: Vec<&Value> = by_file
            .get(file)
            .map(|v| v.iter().copied().filter(|n| bare_label(&js_string_truthy(n.get("label"))) == name).collect())
            .unwrap_or_default();
        (found.len() == 1).then(|| found[0])
    };

    for path in paths {
        if !is_symbol_card_path(path) {
            continue;
        }
        let section = opts.sections.get(path).map(String::as_str).unwrap_or("");
        let entries = added_entries_of_section(section);
        let caller_lines = (opts.read_lines)(path);
        // Добавленные строки — по маске полного файла, если файл на диске совпадает с диффом.
        let masked = caller_lines.as_ref().map(|l| mask_non_code(l));
        let mut by_text: Option<HashMap<&str, Option<&str>>> = None;
        let mut added = Vec::with_capacity(entries.len());
        for e in &entries {
            let (Some(cl), Some(mk)) = (caller_lines.as_deref(), masked.as_deref()) else {
                added.push(e.text.clone());
                continue;
            };
            let at = (e.line >= 1.0 && e.line.fract() == 0.0).then(|| e.line as usize - 1);
            if let Some(k) = at.filter(|&k| k < cl.len() && cl[k] == e.text) {
                added.push(mk[k].clone());
                continue;
            }
            let map = by_text.get_or_insert_with(|| {
                let mut m: HashMap<&str, Option<&str>> = HashMap::new();
                for (i, l) in cl.iter().enumerate() {
                    match m.get(l.as_str()).copied() {
                        None => {
                            m.insert(l, Some(mk[i].as_str()));
                        }
                        Some(prev) if prev != Some(mk[i].as_str()) => {
                            m.insert(l, None);
                        }
                        _ => {}
                    }
                }
                m
            });
            match map.get(e.text.as_str()) {
                Some(Some(m)) => added.push((*m).to_string()),
                _ => added.push(e.text.clone()),
            }
        }
        // Импорты читаются из полного файла: шапка с импортами обычно вне окна вокруг правок.
        let mut header_lines: Vec<String> = caller_lines.as_deref().cloned().unwrap_or_default();
        header_lines.extend(entries.iter().map(|e| e.text.clone()));
        let bindings = relative_import_bindings(&header_lines);
        let namespaces: HashSet<String> =
            bindings.iter().filter(|(_, b)| b.imported == "*").map(|(l, _)| l.clone()).collect();
        let used = extract_used_identifiers(&added);
        let ns_calls = extract_namespace_calls(&added, &namespaces);
        if used.is_empty() && ns_calls.is_empty() {
            continue;
        }
        let visible = format!("{section}\n{}", opts.windows.get(path).map(String::as_str).unwrap_or(""));
        let external = package_imported_names(&header_lines);
        let caller_is_test = is_test_source(path);
        let module_file = |spec: &str| resolve_import_path(path, spec, &has_file).map(|f| graph_path(&f));

        let mut queue: Vec<QueueItem> = Vec::new();
        for (name, count) in &used {
            let b = bindings.get(name).filter(|b| b.imported != "*").cloned();
            queue.push(QueueItem { name: name.clone(), count: *count, binding: b, via_namespace: false });
        }
        for c in ns_calls.values() {
            let spec = bindings.get(&c.ns).map(|b| b.spec.clone()).unwrap_or_default();
            queue.push(QueueItem {
                name: c.fn_name.clone(),
                count: c.count,
                binding: Some(ImportBinding { spec, imported: c.fn_name.clone() }),
                via_namespace: true,
            });
        }

        let mut list: Vec<SymbolCard> = Vec::new();
        for QueueItem { name, count, binding, via_namespace } in queue {
            candidates += 1;
            if !via_namespace && external.contains(&name) {
                bump(&mut skip, "package_import");
                continue;
            }
            if declaration_test(&name, &visible) {
                bump(&mut skip, "visible");
                continue;
            }
            let mut node: Option<&Value> = None;
            if let Some(b) = &binding {
                // Импорт называет модуль: ищем символ в НЁМ, а не уникальное имя по всему графу.
                let file = module_file(&b.spec);
                match &file {
                    Some(f) if b.imported != "default" => node = node_in_file(f, &b.imported),
                    _ if via_namespace => {
                        bump(&mut skip, if file.is_some() { "namespace_member_unresolved" } else { "import_unresolved" });
                        continue;
                    }
                    _ => {}
                }
            }
            let via_import = node.is_some();
            // имя в модуле, а не локальный псевдоним: `import { parse as px }` → `parse`
            let symbol = match &binding {
                Some(b) if !b.imported.is_empty() && b.imported != "default" => b.imported.clone(),
                _ => name.clone(),
            };
            let node = match node {
                Some(n) => n,
                None => {
                    let r = resolve_card_node(Some(graph), &symbol);
                    match r.node {
                        Some(n) => n,
                        None => {
                            bump(&mut skip, r.reason.unwrap_or("unresolved"));
                            continue;
                        }
                    }
                }
            };
            let mut file = normalize_node_path(&js_string_truthy(node.get("source_file")), root);
            let line = node_line(node);
            if file.is_empty() || line == 0 {
                bump(&mut skip, "no_location");
                continue;
            }
            if !is_symbol_card_path(&file) {
                bump(&mut skip, "non_js_target");
                continue;
            }
            if !caller_is_test && is_test_source(&file) {
                bump(&mut skip, "test_source");
                continue;
            }
            let mut lines = (opts.read_lines)(&file);
            // граф построен от layout.appDir (`graphify update app`): пути узлов без префикса
            if lines.is_none() && !app_dir.is_empty() && !file.starts_with(&app_prefix) {
                let prefixed = format!("{app_prefix}{file}");
                if let Some(alt) = (opts.read_lines)(&prefixed) {
                    lines = Some(alt);
                    file = prefixed;
                }
            }
            let Some(lines) = lines.filter(|l| line <= l.len()) else {
                bump(&mut skip, "unreadable");
                continue;
            };
            if binding.is_none()
                && file != *path
                && has_local_binding(caller_lines.as_deref().map(Vec::as_slice).unwrap_or(&[]), &name)
            {
                bump(&mut skip, "local_shadow");
                continue;
            }
            // Строка графа должна ОБЪЯВЛЯТЬ имя, а не просто содержать его (аудит, п. 0.2).
            if !declaration_test(&symbol, &lines[line - 1]) {
                bump(&mut skip, "stale_graph");
                continue;
            }
            let decl = card_declaration(&lines, line - 1, &symbol);
            if decl.header.is_empty() || !decl.header.contains(symbol.as_str()) {
                bump(&mut skip, "not_a_declaration");
                continue;
            }
            resolved += 1;
            let end_line = decl.end_idx.map(|e| e + 1);
            let body = match end_line {
                Some(end) if body_max_lines > 0.0 && ((end - line + 1) as f64) <= body_max_lines => {
                    Some(lines[line - 1..end].join("\n"))
                }
                _ => None,
            };
            list.push(SymbolCard { name: symbol, path: file, line, end_line, declaration: decl.header, count, body });
            cards += 1;
            if via_import {
                import_resolved += 1;
            }
        }
        if !list.is_empty() {
            list.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| locale_compare(&a.name, &b.name)));
            by_path.insert(path.clone(), list);
        }
    }
    SymbolCards {
        by_path,
        metrics: json!({
            "candidates": candidates, "resolved": resolved, "cards": cards,
            "via_import": import_resolved, "skip_reasons": Value::Object(skip),
        }),
    }
}

/// Строка карточки для промпта: место в коде, затем объявление или короткое тело целиком.
pub fn render_symbol_card(card: &SymbolCard) -> String {
    let range = match card.end_line {
        Some(end) if end != 0 && end != card.line => format!("L{}–L{}", card.line, end),
        _ => format!("L{}", card.line),
    };
    match card.body.as_deref().filter(|b| !b.is_empty()) {
        Some(body) => format!("// {}:{range} (full body)\n{body}", card.path),
        None => format!("// {}:{range}\n{}", card.path, card.declaration),
    }
}

/// Вход хэша RFC 05 по файлу: путь и текст объявления БЕЗ номеров строк — сдвиг
/// неизменённого объявления не инвалидирует файлы-вызыватели.
pub fn symbol_cards_hash_input(cards: &[SymbolCard]) -> String {
    let mut parts: Vec<String> =
        cards.iter().map(|c| format!("{}\n{}", c.path, c.body.as_deref().unwrap_or(&c.declaration))).collect();
    parts.sort_by(|a, b| utf16_cmp(a, b));
    parts.join("\n\n")
}

/// Карточки чанка: готовые строки, имена, сколько отброшено лимитами и сколько уже видны.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkCards {
    pub cards: Vec<String>,
    pub names: Vec<String>,
    pub dropped: usize,
    pub chars: usize,
    pub visible: usize,
}

/// Карточки чанка (`selectChunkCards`): объединение карточек его файлов без повторов,
/// приоритет — число вызывающих строк; лимит по числу (`max`) и по символам (`max_chars`).
pub fn select_chunk_cards(
    by_path: &IndexMap<String, Vec<SymbolCard>>,
    chunk_paths: &[String],
    max: f64,
    max_chars: f64,
    visible_text: &str,
) -> ChunkCards {
    let mut merged: IndexMap<String, SymbolCard> = IndexMap::new();
    for p in chunk_paths {
        for c in by_path.get(p).map(Vec::as_slice).unwrap_or(&[]) {
            let key = format!("{}:{}", c.path, c.line);
            match merged.get_mut(&key) {
                Some(prev) => prev.count += c.count,
                None => {
                    merged.insert(key, c.clone());
                }
            }
        }
    }
    let mut ordered: Vec<SymbolCard> = merged.into_values().collect();
    ordered.sort_by(|a, b| {
        b.count.cmp(&a.count).then_with(|| locale_compare(&a.path, &b.path)).then_with(|| a.line.cmp(&b.line))
    });
    let mut out = ChunkCards { cards: Vec::new(), names: Vec::new(), dropped: 0, chars: 0, visible: 0 };
    for c in ordered {
        // объявление уже видно в окне другого файла этого чанка — дубль (аудит RFC 14, п. 1.8)
        if !visible_text.is_empty() && declaration_test(&c.name, visible_text) {
            out.visible += 1;
            continue;
        }
        let text = render_symbol_card(&c);
        let len = utf16_len(&text);
        if out.cards.len() as f64 >= max || (out.chars + len) as f64 > max_chars {
            out.dropped += 1;
            continue;
        }
        out.cards.push(text);
        out.names.push(c.name);
        out.chars += len;
    }
    out
}

// ── Связь находок с карточками ──

/// `\s` JS как класс для `regex`.
const JS_WS_CLASS: &str = r"[\t\n\x0B\x0C\r \u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";

/// Английское слово в `i`-режиме JS без юникодной свёртки (`s` не ловит `ſ`).
fn ascii_ci(words: &str) -> String {
    words
        .split(' ')
        .map(|w| {
            w.chars()
                .map(|c| format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(&format!("{JS_WS_CLASS}+"))
}

fn absence_claim_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let ws = format!("{JS_WS_CLASS}+");
        let ru = ["определ", "существу", "найден", "экспортир", "импортир"]
            .iter()
            .map(|w| format!("(?i:не){ws}(?i:{w}[а-яё]*)"))
            .collect::<Vec<_>>();
        let en = ["is not defined", "does not exist", "is not exported", "is undefined"].map(ascii_ci);
        let word = format!("(?:{}|{})", ru.join("|"), en.join("|"));
        let ident = r"`[A-Za-z_$][0-9A-Za-z_$.]{0,80}`";
        let gap = r"[^.;`\n]{0,40}";
        Regex::new(&format!("{ident}{gap}{word}|{word}{gap}{ident}")).expect("ABSENCE_CLAIM_RE")
    })
}

/// `${v ?? ''}` для поля находки.
fn field_text(f: &Value, key: &str) -> String {
    match f.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_string(v),
    }
}

/// Утверждение об отсутствии вплотную к идентификатору в обратных кавычках.
pub fn is_absence_claim(finding: &Value) -> bool {
    absence_claim_re().is_match(&format!("{}\n{}", field_text(finding, "title"), field_text(finding, "body")))
}

fn mentions_any(finding: &Value, names: Option<&IndexSet<String>>) -> bool {
    let Some(names) = names.filter(|n| !n.is_empty()) else { return false };
    let text =
        format!("{}\n{}\n{}", field_text(finding, "title"), field_text(finding, "body"), field_text(finding, "quote"));
    names.iter().any(|n| word_occurs(&text, n))
}

/// Связь находок с карточками (`symbolCardsOutcome`): находка «упоминает карточку», если
/// её текст называет символ, чья карточка была в чанке её файла. `inline` — индексы
/// элементов `final_findings`, ушедших в строку (в JS — тождество объектов).
pub fn symbol_cards_outcome(
    chunks: &[Value],
    llm_findings: &[Value],
    final_findings: &[Value],
    inline: &[usize],
) -> Value {
    let mut names_by_file: HashMap<String, IndexSet<String>> = HashMap::new();
    let mut all_names: IndexSet<String> = IndexSet::new();
    let str_list = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array).map(|a| a.iter().map(js_string).collect()).unwrap_or_default()
    };
    for c in chunks {
        let names = str_list(c.get("symbolCardNames"));
        all_names.extend(names.iter().cloned());
        if names.is_empty() {
            continue;
        }
        for p in str_list(c.get("paths")) {
            names_by_file.entry(p).or_default().extend(names.iter().cloned());
        }
    }
    let mentions =
        |f: &Value| mentions_any(f, f.get("file").and_then(Value::as_str).and_then(|p| names_by_file.get(p)));
    let llm_mentioning = llm_findings.iter().filter(|f| mentions(f)).count();
    let inline_set: HashSet<usize> = inline.iter().copied().collect();
    let mut by_verifier = Map::new();
    let mut by_severity = Map::new();
    let (mut final_mentioning, mut final_inline) = (0usize, 0usize);
    for (i, f) in final_findings.iter().enumerate() {
        if !mentions(f) {
            continue;
        }
        final_mentioning += 1;
        if inline_set.contains(&i) {
            final_inline += 1;
        }
        let v = js_string_truthy(f.get("verifierStatus"));
        bump(&mut by_verifier, if v.is_empty() { "none" } else { &v });
        let s = js_string_truthy(f.get("severity"));
        bump(&mut by_severity, if s.is_empty() { "?" } else { &s });
    }
    let mut names: Vec<String> = all_names.into_iter().collect();
    names.sort_by(|a, b| utf16_cmp(a, b));
    names.truncate(50);
    json!({
        "names": names,
        "llm": {
            "findings": llm_findings.len(),
            "mentioning": llm_mentioning,
            "absenceClaims": llm_findings.iter().filter(|f| is_absence_claim(f)).count(),
        },
        "final": {
            "findings": final_findings.len(),
            "mentioning": final_mentioning,
            "inline": final_inline,
            "bySeverity": Value::Object(by_severity),
            "byVerifier": Value::Object(by_verifier),
        },
    })
}

#[cfg(test)]
mod tests {
    //! Порт кейсов `test/symbol-cards.test.mjs` (кроме проводки через пайплайн — T26).
    use super::*;
    use std::time::Instant;

    fn ls(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn sorted_keys<V>(m: &IndexMap<String, V>) -> Vec<String> {
        let mut k: Vec<String> = m.keys().cloned().collect();
        k.sort();
        k
    }

    fn section(added: &[&str]) -> String {
        added.iter().enumerate().map(|(i, l)| format!("{:>5}: +{l}", i + 10)).collect::<Vec<_>>().join("\n")
    }

    fn graph(nodes: Value) -> GraphIndex {
        crate::graphify_client::build_graph_index(&json!({ "nodes": nodes, "links": [] }), None)
    }

    fn decl(header: &str, end: Option<usize>) -> Declaration {
        Declaration { header: header.to_string(), end_idx: end }
    }

    #[test]
    fn extract_used_identifiers_вызовы_и_new_без_методов_ключевых_слов_глобалов_и_литералов() {
        let used = extract_used_identifiers(&v(&[
            "const x = parseLimits(env.LIMITS, { strict: true });",
            "if (ok) return new ReviewEngine(cfg).run(items.map(fn));",
            "console.log(\"call fake(1) in string\"); // comment(2)",
            "const y = parseLimits(other) + JSON.stringify(z); assert . ok(y);",
            "  // строка-комментарий: helper()",
            "return compute(a);",
        ]));
        let mut got: Vec<(String, usize)> = used.into_iter().collect();
        got.sort();
        assert_eq!(
            got,
            vec![("ReviewEngine".into(), 1), ("compute".into(), 1), ("parseLimits".into(), 2)]
        );
    }

    #[test]
    fn package_imported_names_пакеты_но_не_файлы_и_алиасы() {
        let names = package_imported_names(&v(&[
            "import { ok, deepEqual as de } from 'node:assert';",
            "import express from 'express';",
            "import * as path from 'node:path';",
            "import { local } from './local.mjs';",
            "import { aliased } from '@/lib/x';",
            "import { scoped } from '@scope/pkg';",
            "const { readFile: rf, stat } = require('fs');",
        ]));
        let mut got: Vec<String> = names.into_iter().collect();
        got.sort();
        assert_eq!(got, v(&["de", "express", "ok", "path", "rf", "scoped", "stat"]));
    }

    #[test]
    fn разбор_текста_автора_pr_линеен() {
        let n = 100_000;
        let cases: Vec<(&str, Box<dyn Fn()>)> = vec![
            ("литерал из обратных слэшей", Box::new(|| {
                extract_used_identifiers(&[format!("\"{}x(1)", "\\\\".repeat(5000))]);
            })),
            ("вызов и пробелы", Box::new(|| {
                extract_used_identifiers(&[format!("x{}", " ".repeat(1990))]);
            })),
            ("точка и пробелы", Box::new(|| {
                extract_used_identifiers(&[format!(".{}a b c", " ".repeat(1990))]);
            })),
            ("пробелы по краям в обход предела длины", Box::new(move || {
                extract_used_identifiers(&[format!("{}{}{}", " ".repeat(n), "/*".repeat(900), " ".repeat(n))]);
            })),
            ("import и пробелы", Box::new(move || {
                package_imported_names(&[format!("import a{}x", " ".repeat(n))]);
            })),
            ("многострочный import без from", Box::new(move || {
                let mut lines = vec!["import {".to_string()];
                lines.extend(std::iter::repeat_n(" ".repeat(50), n));
                package_imported_names(&lines);
            })),
            ("объявление: пустые строки", Box::new(move || {
                declaration_test("foo", &"\n".repeat(n));
            })),
            ("объявление: длинные аргументы", Box::new(move || {
                declaration_test("foo", &format!("foo({}", "a".repeat(n)));
            })),
            ("маска и лексер на цепочке делений", Box::new(|| {
                let line = format!("a{}", "/a".repeat(50_000));
                mask_non_code(std::slice::from_ref(&line));
                scan_declaration(&[format!("const x = {line};")], 0, MAX_SCAN_LINES);
            })),
            ("объявление: тысячи модификаторов", Box::new(move || {
                declaration_test("static", &format!("{}static(", "static ".repeat(n / 10)));
            })),
        ];
        for (label, f) in cases {
            let t0 = Instant::now();
            f();
            // debug-сборка: предел с запасом, квадратичный разбор на 1e5 его не уложит
            assert!(t0.elapsed().as_millis() < 1500, "{label}: {:?}", t0.elapsed());
        }
    }

    #[test]
    fn line_reader_symlink_за_корень_не_читается() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("lib")).unwrap();
        std::fs::write(root.path().join("lib/ok.mjs"), "export function ok() {}\n").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "TOKEN=abc\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.path().join("lib/leak.mjs")).unwrap();
        let read = LineReader::new(root.path());
        assert_eq!(read.read("lib/ok.mjs").as_deref(), Some(&v(&["export function ok() {}", ""])));
        assert_eq!(read.read("lib/leak.mjs"), None, "symlink наружу");
        let name = outside.path().file_name().unwrap().to_string_lossy();
        assert_eq!(read.read(&format!("../{name}/secret.txt")), None, "выход через ..");
        assert_eq!(read.read(outside.path().join("secret.txt").to_str().unwrap()), None, "абсолютный путь");
        assert_eq!(read.read(""), None, "сам корень — не файл");
    }

    #[test]
    fn added_lines_of_section_только_плюс_строки() {
        let s = "    8:  keep();\n     : -  gone();\n    9: +  added();\n   10: +  more();";
        assert_eq!(added_lines_of_section(s), v(&["  added();", "  more();"]));
        assert!(added_lines_of_section("    9: +a\r").is_empty(), "`.*$` без флага m не проходит \\r");
    }

    #[test]
    fn declaration_test_объявления_и_не_объявления() {
        for d in [
            "function foo(a) {",
            "export async function foo() {",
            "class foo {",
            "const foo = (x) => x",
            "  async foo(a, b) {",
            "  foo: async (x) => x,",
            "let foo = function () {",
        ] {
            assert!(declaration_test("foo", d), "{d}");
        }
        assert!(!declaration_test("foo", "const r = foo(1);"));
        assert!(!declaration_test("foo", "foodle(1)"));
        for d in ["interface Foo {", "export type Foo = {", "enum Foo {", "namespace Foo {"] {
            assert!(declaration_test("Foo", d), "{d}");
        }
    }

    #[test]
    fn scan_declaration_заголовок_без_тела_и_строка_конца() {
        let src = v(&[
            "export function parseLimits(raw, { strict = false } = {}) {",
            "  const s = \"}\";",
            "  const t = `${raw}}`;",
            "  // } в комментарии",
            "  return /[}]/.test(s) ? t : raw;",
            "}",
        ]);
        assert_eq!(
            scan_declaration(&src, 0, MAX_SCAN_LINES),
            decl("export function parseLimits(raw, { strict = false } = {})", Some(5))
        );
    }

    #[test]
    fn scan_declaration_класс_метод_стрелки_цепочка() {
        let s = |t: &str| scan_declaration(&ls(t), 0, MAX_SCAN_LINES);
        assert_eq!(s("class A extends B {\n  m() {}\n}"), decl("class A extends B", Some(2)));
        assert_eq!(s("  async run(a, b) {\n    return a;\n  }"), decl("async run(a, b)", Some(2)));
        assert_eq!(s("const f = async (x) => {\n  return x;\n};"), decl("const f = async (x) =>", Some(2)));
        assert_eq!(s("const g = (x) => x + 1;\nconst h = 2;"), decl("const g = (x) =>", Some(0)));
        assert_eq!(
            s("const n = (m) => (Array.isArray(m) ? m : [])\n  .reduce((a, b) => a + b, 0);\nnext();"),
            decl("const n = (m) =>", Some(1))
        );
        assert_eq!(s("const LIMIT = 40;"), decl("const LIMIT = 40", Some(0)));
    }

    #[test]
    fn scan_declaration_незакрытый_литерал_и_max_lines() {
        assert_eq!(scan_declaration(&ls("function f() {\n  const s = \"oops\n}"), 0, MAX_SCAN_LINES).end_idx, None);
        let r = scan_declaration(&ls("function g() {\n  a();\n  b();\n"), 0, 2);
        assert_eq!(r, decl("function g()", None));
        assert_eq!(find_declaration_end(&v(&["x"]), 5, MAX_SCAN_LINES), None);
    }

    #[test]
    fn scan_declaration_длинный_заголовок_обрезается() {
        let src = ls(&format!("export function veryLong(\n  {},\n  {},\n  c,\n) {{\n}}", "a".repeat(100), "b".repeat(100)));
        let r = scan_declaration(&src, 0, MAX_SCAN_LINES);
        assert!(utf16_len(&r.header) <= 160);
        assert!(r.header.ends_with('…'));
        assert_eq!(r.end_idx, Some(5));
    }

    #[test]
    fn scan_declaration_ts_стрелка_и_литерал_типа_в_аннотации() {
        let s = |t: &str| scan_declaration(&ls(t), 0, MAX_SCAN_LINES);
        assert_eq!(
            s("export function make(n: number): (x: number) => number {\n  return (x) => x + n;\n}"),
            decl("export function make(n: number): (x: number) => number", Some(2))
        );
        assert_eq!(
            s("async function load(id: string): Promise<{ id: string; ok: boolean }> {\n  return { id, ok: true };\n}"),
            decl("async function load(id: string): Promise<{ id: string; ok: boolean }>", Some(2))
        );
        assert_eq!(s("function shape(): { a: number } {\n  return { a: 1 };\n}"), decl("function shape(): { a: number }", Some(2)));
        assert_eq!(
            s("const twice: (x: number) => number = (x) => x * 2;\nnext();"),
            decl("const twice: (x: number) => number = (x) =>", Some(0))
        );
        assert_eq!(
            s("class Box<T extends { id: string }> extends Base<T> {\n  get() {}\n}"),
            decl("class Box<T extends { id: string }> extends Base<T>", Some(2))
        );
    }

    #[test]
    fn card_declaration_перегрузки_и_конструктор() {
        let src = v(&[
            "export function pick(a: string): string;",
            "export function pick(a: number): number;",
            "export function pick(a: string | number) {",
            "  return a;",
            "}",
            "const after = 1;",
        ]);
        assert_eq!(
            card_declaration(&src, 0, "pick"),
            decl(
                "export function pick(a: string): string\nexport function pick(a: number): number\nexport function pick(a: string | number)",
                Some(4)
            )
        );
        let cls = v(&[
            "export class Client extends Base {",
            "  private readonly url: string;",
            "  constructor(url: string, opts: Opts = {}) {",
            "    super();",
            "  }",
            "}",
        ]);
        assert_eq!(
            card_declaration(&cls, 0, "Client"),
            decl("export class Client extends Base { constructor(url: string, opts: Opts = {}) }", Some(5))
        );
    }

    fn main_graph() -> GraphIndex {
        graph(json!([
            { "id": "p", "label": "parseLimits()", "source_file": "lib/config.mjs", "source_location": "L2" },
            { "id": "h1", "label": "helper()", "source_file": "lib/a.mjs", "source_location": "L1" },
            { "id": "h2", "label": "helper()", "source_file": "lib/b.mjs", "source_location": "L1" },
            { "id": "s", "label": "staleFn()", "source_file": "lib/stale.mjs", "source_location": "L1" },
            { "id": "py", "label": "pyThing()", "source_file": "tools/x.py", "source_location": "L1" },
            { "id": "own", "label": "localFar()", "source_file": "src/app.ts", "source_location": "L90" },
            { "id": "short", "label": "tiny()", "source_file": "lib/tiny.mjs", "source_location": "L1" },
            { "id": "okn", "label": "ok()", "source_file": "lib/ok.mjs", "source_location": "L1" },
        ]))
    }

    fn main_files() -> HashMap<String, Arc<Vec<String>>> {
        let mut app = vec!["import { ok } from 'node:assert';".to_string()];
        app.extend(std::iter::repeat_n("// filler".to_string(), 88));
        app.extend(v(&["function localFar(x: number): number {", "  return x;", "}"]));
        HashMap::from([
            ("lib/config.mjs".into(), Arc::new(ls("// header\nexport function parseLimits(raw, { strict = false } = {}) {\n  return raw;\n}"))),
            ("lib/stale.mjs".into(), Arc::new(ls("export function renamed() {}"))),
            ("lib/ok.mjs".into(), Arc::new(ls("export function ok(x) {}"))),
            ("lib/tiny.mjs".into(), Arc::new(ls("export function tiny(a) {\n  return a + 1;\n}"))),
            ("src/app.ts".into(), Arc::new(app)),
        ])
    }

    fn build(
        paths: &[&str],
        g: Option<&GraphIndex>,
        sections: &[(&str, String)],
        windows: &[(&str, &str)],
        files: &HashMap<String, Arc<Vec<String>>>,
        body_max_lines: f64,
        app_dir: &str,
    ) -> SymbolCards {
        let sections: HashMap<String, String> = sections.iter().map(|(k, s)| (k.to_string(), s.clone())).collect();
        let windows: HashMap<String, String> = windows.iter().map(|(k, s)| (k.to_string(), s.to_string())).collect();
        let read = |p: &str| files.get(p).cloned();
        build_symbol_cards(
            &v(paths),
            &BuildOpts {
                graph_index: g,
                sections: &sections,
                windows: &windows,
                read_lines: &read,
                body_max_lines,
                root: Path::new("/repo"),
                app_dir,
            },
        )
    }

    #[test]
    fn build_symbol_cards_каждый_фильтр_отсекает_свой_случай() {
        let g = main_graph();
        let r = build(
            &["src/app.ts", "README.md"],
            Some(&g),
            &[
                (
                    "src/app.ts",
                    section(&[
                        "const a = parseLimits(raw);",
                        "const b = helper();",
                        "const c = staleFn();",
                        "const d = pyThing();",
                        "const e = visibleFn();",
                        "const f = localFar(1);",
                        "const g = unknownFn();",
                        "ok(g);",
                    ]),
                ),
                ("README.md", section(&["parseLimits()"])),
            ],
            &[("src/app.ts", "function visibleFn() {\n  return 1;\n}")],
            &main_files(),
            0.0,
            "",
        );
        assert_eq!(r.by_path.keys().cloned().collect::<Vec<_>>(), v(&["src/app.ts"]), "не JS/TS-файл не разбирается");
        let cards = &r.by_path["src/app.ts"];
        let got: Vec<(String, String, usize, Option<usize>)> =
            cards.iter().map(|c| (c.name.clone(), c.path.clone(), c.line, c.end_line)).collect();
        assert_eq!(
            got,
            vec![
                ("localFar".into(), "src/app.ts".into(), 90, Some(92)),
                ("parseLimits".into(), "lib/config.mjs".into(), 2, Some(4)),
            ]
        );
        assert_eq!(cards[1].declaration, "export function parseLimits(raw, { strict = false } = {})");
        let mut reasons: Vec<(String, u64)> =
            r.metrics["skip_reasons"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap())).collect();
        reasons.sort();
        assert_eq!(
            reasons,
            vec![
                ("ambiguous".into(), 1),
                ("non_js_target".into(), 1),
                ("package_import".into(), 1),
                ("stale_graph".into(), 1),
                ("unresolved".into(), 1),
                ("visible".into(), 1),
            ]
        );
        assert_eq!(r.metrics["candidates"], 8);
        assert_eq!(r.metrics["cards"], 2);
    }

    #[test]
    fn build_symbol_cards_без_графа_причина_в_метриках() {
        let r = build(&["src/app.ts"], None, &[], &[], &main_files(), 0.0, "");
        assert!(r.by_path.is_empty());
        assert_eq!(r.metrics["skip_reasons"], json!({ "graph_unavailable": 1 }));
    }

    #[test]
    fn build_symbol_cards_короткое_тело_целиком() {
        let g = main_graph();
        let files = main_files();
        let sec = [("src/app.ts", section(&["tiny(1);", "parseLimits(x);"]))];
        let off = build(&["src/app.ts"], Some(&g), &sec, &[], &files, 0.0, "");
        assert!(off.by_path["src/app.ts"].iter().all(|c| c.body.is_none()));
        let on = build(&["src/app.ts"], Some(&g), &sec, &[], &files, 3.0, "");
        let tiny = on.by_path["src/app.ts"].iter().find(|c| c.name == "tiny").unwrap();
        assert_eq!(tiny.body.as_deref(), Some("export function tiny(a) {\n  return a + 1;\n}"));
        assert!(render_symbol_card(tiny).starts_with("// lib/tiny.mjs:L1–L3 (full body)\n"));
    }

    fn card(name: &str, path: &str, line: usize, count: usize) -> SymbolCard {
        SymbolCard {
            name: name.into(),
            path: path.into(),
            line,
            end_line: Some(line + 2),
            declaration: format!("function {name}()"),
            count,
            body: None,
        }
    }

    #[test]
    fn select_chunk_cards_объединение_приоритет_лимиты() {
        let by_path: IndexMap<String, Vec<SymbolCard>> = IndexMap::from([
            ("a.ts".to_string(), vec![card("x", "lib/x.mjs", 1, 1), card("y", "lib/y.mjs", 5, 3)]),
            ("b.ts".to_string(), vec![card("x", "lib/x.mjs", 1, 4)]),
        ]);
        let chunk = v(&["a.ts", "b.ts"]);
        let all = select_chunk_cards(&by_path, &chunk, 20.0, 3000.0, "");
        assert_eq!(all.cards, v(&["// lib/x.mjs:L1–L3\nfunction x()", "// lib/y.mjs:L5–L7\nfunction y()"]));
        assert_eq!(all.dropped, 0);
        let capped = select_chunk_cards(&by_path, &chunk, 1.0, 3000.0, "");
        assert_eq!(capped.cards, v(&["// lib/x.mjs:L1–L3\nfunction x()"]));
        assert_eq!(capped.dropped, 1);
        let by_chars = select_chunk_cards(&by_path, &chunk, 20.0, 35.0, "");
        assert_eq!(by_chars.cards.len(), 1);
        assert_eq!(by_chars.chars, utf16_len(&by_chars.cards[0]));
        // аудит 1.8: объявление видно в окне другого файла чанка — не дублируется
        let shared = IndexMap::from([("a.ts".to_string(), vec![card("shared", "lib/s.mjs", 1, 1)])]);
        let sel = select_chunk_cards(&shared, &chunk, 20.0, 3000.0, "### b.ts\nexport function shared() {\n}");
        assert!(sel.cards.is_empty());
        assert_eq!(sel.visible, 1);
    }

    #[test]
    fn hash_input_без_номеров_строк() {
        let mk = |line, decl: &str| vec![SymbolCard { declaration: decl.into(), end_line: Some(line + 10), ..card("x", "lib/x.mjs", line, 1) }];
        assert_eq!(symbol_cards_hash_input(&mk(10, "function x(a)")), symbol_cards_hash_input(&mk(42, "function x(a)")));
        assert_ne!(symbol_cards_hash_input(&mk(10, "function x(a)")), symbol_cards_hash_input(&mk(10, "function x(a, b)")));
    }

    fn audit_graph() -> GraphIndex {
        graph(json!([
            { "id": "call", "label": "run()", "source_file": "lib/uses-run.mjs", "source_location": "L1" },
            { "id": "prop", "label": "getId()", "source_file": "lib/widget.mjs", "source_location": "L1" },
            { "id": "mock", "label": "listTypes()", "source_file": "src/svc.spec.ts", "source_location": "L2" },
            { "id": "log", "label": "log()", "source_file": "lib/logger.mjs", "source_location": "L1" },
            { "id": "var", "label": "dup", "source_file": "lib/a.mjs", "source_location": "L1" },
            { "id": "fn", "label": "dup()", "source_file": "lib/b.mjs", "source_location": "L1" },
            { "id": "P", "label": "Parser", "source_file": "lib/parser.mjs", "source_location": "L1" },
            { "id": "p", "label": "parser()", "source_file": "lib/parse.mjs", "source_location": "L1" },
            { "id": "m", "label": ".method()", "source_file": "lib/m.mjs", "source_location": "L1" },
            { "id": "app", "label": "inApp()", "source_file": "server/x.ts", "source_location": "L1" },
        ]))
    }

    fn audit_files() -> HashMap<String, Arc<Vec<String>>> {
        HashMap::from([
            ("lib/uses-run.mjs".into(), Arc::new(ls("const r = run(a, b);"))),
            ("lib/widget.mjs".into(), Arc::new(ls("const widget = { getId: () => 1 };"))),
            ("src/svc.spec.ts".into(), Arc::new(ls("// spec\nconst listTypes = jest.fn();"))),
            ("lib/logger.mjs".into(), Arc::new(ls("export function log(msg) {}"))),
            ("src/caller.ts".into(), Arc::new(ls("function work(log) {\n  log(\"x\");\n}"))),
            ("app/server/x.ts".into(), Arc::new(ls("export function inApp(a: number) {\n  return a;\n}"))),
        ])
    }

    fn audit_cards(added: &[&str], path: &str, app_dir: &str) -> SymbolCards {
        let g = audit_graph();
        build(&[path], Some(&g), &[(path, section(added))], &[], &audit_files(), 0.0, app_dir)
    }

    #[test]
    fn аудит_0_2_строка_вызова_или_свойства_не_объявление() {
        let r = audit_cards(&["go(run(1));", "show(getId());"], "src/caller.ts", "");
        assert!(r.by_path.is_empty());
        assert_eq!(r.metrics["skip_reasons"]["stale_graph"], 1, "run: строка — вызов");
        assert_eq!(r.metrics["skip_reasons"]["not_a_declaration"], 1, "getId: заголовок без имени");
    }

    #[test]
    fn аудит_1_1_объявление_метода_не_вызов_mock_не_источник() {
        assert!(extract_used_identifiers(&v(&[
            "  listTypes(filter: Filter): Promise<Item[]>;",
            "  async run(a, b) {",
            "  private build<T>(x: T): void {",
        ]))
        .is_empty());
        assert_eq!(sorted_keys(&extract_used_identifiers(&v(&["run(a);", "build(x).then(done);"]))), v(&["build", "run"]));
        let r = audit_cards(&["const t = listTypes(f);"], "src/caller.ts", "");
        assert_eq!(r.metrics["skip_reasons"]["test_source"], 1);
        let from_test = audit_cards(&["const t = listTypes(f);"], "src/other.spec.ts", "");
        assert!(from_test.metrics["skip_reasons"].get("test_source").is_none(), "тест может опираться на тестовый хелпер");
    }

    #[test]
    fn аудит_1_2_параметр_и_деструктуризация_затеняют() {
        let r = audit_cards(&["  log(\"y\");"], "src/caller.ts", "");
        assert!(r.by_path.is_empty());
        assert_eq!(r.metrics["skip_reasons"]["local_shadow"], 1);
        assert!(has_local_binding(&v(&["const { log, warn: w } = deps;"]), "log"));
        assert!(has_local_binding(&v(&["items.forEach((item, log) => {"]), "log"));
        assert!(has_local_binding(&v(&["} catch (log) {"]), "log"));
        assert!(has_local_binding(&v(&["const f = log => log + 1;"]), "log"));
        assert!(!has_local_binding(&v(&["logger.info(dialog);", "import { log } from \"./logger.mjs\";"]), "log"), "импорт — не тень");
    }

    #[test]
    fn аудит_1_3_resolve_card_node_неоднозначность_и_регистр() {
        let g = audit_graph();
        assert_eq!(resolve_card_node(Some(&g), "dup"), CardNode { node: None, reason: Some("ambiguous") });
        assert_eq!(resolve_card_node(Some(&g), "Parser").node.map(|n| n["id"].clone()), Some(json!("P")));
        assert_eq!(resolve_card_node(Some(&g), "parser").node.map(|n| n["id"].clone()), Some(json!("p")));
        assert_eq!(resolve_card_node(Some(&g), "method"), CardNode { node: None, reason: Some("unresolved") });
    }

    #[test]
    fn аудит_1_11_граф_от_app_dir() {
        let r = audit_cards(&["const v = inApp(1);"], "app/client/y.ts", "app");
        let got: Vec<(String, usize, Option<usize>)> =
            r.by_path["app/client/y.ts"].iter().map(|c| (c.path.clone(), c.line, c.end_line)).collect();
        assert_eq!(got, vec![("app/server/x.ts".into(), 1, Some(3))]);
        let no_dir = audit_cards(&["const v = inApp(1);"], "app/client/y.ts", "");
        assert_eq!(no_dir.metrics["skip_reasons"]["unreadable"], 1);
    }

    #[test]
    fn аудит_1_5_is_absence_claim() {
        assert!(is_absence_claim(&json!({ "title": "Функция `parseLimits` не определена" })));
        assert!(is_absence_claim(&json!({ "body": "В модуле `x.ts` экспорт `foo` is not exported" })));
        assert!(!is_absence_claim(&json!({ "title": "`isCurrentUser` не защищён от null" })));
        assert!(!is_absence_claim(&json!({ "body": "Отсутствует обработка случая отсутствия OrganizationMapping" })));
        assert!(is_absence_claim(&json!({ "title": "`X` НЕ ОПРЕДЕЛЕНА" })), "регистр не важен");
    }

    #[test]
    fn аудит_1_5_outcome_связывает_находки_с_карточками() {
        let chunks = [json!({ "paths": ["a.ts"], "symbolCardNames": ["parseLimits"] }), json!({ "paths": ["b.ts"] })];
        let on_card = json!({ "file": "a.ts", "severity": "major", "title": "вызов `parseLimits` без strict", "verifierStatus": "verified" });
        let other_file = json!({ "file": "b.ts", "severity": "minor", "title": "parseLimits где-то ещё" });
        let substring = json!({ "file": "a.ts", "severity": "minor", "title": "parseLimitsX — другое имя" });
        let absent = json!({ "file": "a.ts", "severity": "minor", "title": "`helper` не определён" });
        let out = symbol_cards_outcome(
            &chunks,
            &[on_card.clone(), other_file.clone(), substring, absent],
            &[on_card, other_file],
            &[0],
        );
        assert_eq!(out["names"], json!(["parseLimits"]));
        assert_eq!(out["llm"], json!({ "findings": 4, "mentioning": 1, "absenceClaims": 1 }));
        assert_eq!(
            out["final"],
            json!({ "findings": 2, "mentioning": 1, "inline": 1, "bySeverity": { "major": 1 }, "byVerifier": { "verified": 1 } })
        );
    }

    #[test]
    fn аудит_1_12_relative_import_bindings() {
        let b = relative_import_bindings(&v(&[
            "import { parse, build as make } from './a.mjs';",
            "import def, { other } from '../b';",
            "import * as util from './util/index.js';",
            "import { ok } from 'node:assert';",
            "const helpers = require('./helpers');",
            "import {",
            "  multi,",
            "} from './c';",
        ]));
        let bind = |spec: &str, imported: &str| ImportBinding { spec: spec.into(), imported: imported.into() };
        let want: IndexMap<String, ImportBinding> = IndexMap::from([
            ("parse".to_string(), bind("./a.mjs", "parse")),
            ("make".to_string(), bind("./a.mjs", "build")),
            ("def".to_string(), bind("../b", "default")),
            ("other".to_string(), bind("../b", "other")),
            ("util".to_string(), bind("./util/index.js", "*")),
            ("helpers".to_string(), bind("./helpers", "*")),
            ("multi".to_string(), bind("./c", "multi")),
        ]);
        assert_eq!(b, want);
    }

    #[test]
    fn аудит_1_12_resolve_import_path() {
        let files: HashSet<&str> = ["src/a.ts", "src/lib/b.mjs", "src/util/index.ts", "shared/c.tsx"].into();
        let has = |f: &str| files.contains(f);
        assert_eq!(resolve_import_path("src/x.ts", "./a.js", &has).as_deref(), Some("src/a.ts"));
        assert_eq!(resolve_import_path("src/x.ts", "./lib/b.mjs", &has).as_deref(), Some("src/lib/b.mjs"));
        assert_eq!(resolve_import_path("src/x.ts", "./util", &has).as_deref(), Some("src/util/index.ts"));
        assert_eq!(resolve_import_path("src/x.ts", "../shared/c", &has).as_deref(), Some("shared/c.tsx"));
        assert_eq!(resolve_import_path("src/x.ts", "./missing", &has), None);
        assert_eq!(resolve_import_path("x.ts", "../../escape", &has), None);
    }

    #[test]
    fn аудит_1_12_extract_namespace_calls() {
        let calls = extract_namespace_calls(
            &v(&["const r = util.parse(x) + other.parse(y);", "util.parse(z); util.build();"]),
            &HashSet::from(["util".to_string()]),
        );
        let got: Vec<NamespaceCall> = calls.into_values().collect();
        assert_eq!(
            got,
            vec![
                NamespaceCall { ns: "util".into(), fn_name: "parse".into(), count: 2 },
                NamespaceCall { ns: "util".into(), fn_name: "build".into(), count: 1 },
            ]
        );
    }

    #[test]
    fn аудит_1_12_импорт_снимает_неоднозначность_и_открывает_ns_fn() {
        let g = graph(json!([
            { "id": "p1", "label": "parse()", "source_file": "src/a.ts", "source_location": "L1" },
            { "id": "p2", "label": "parse()", "source_file": "src/other.ts", "source_location": "L1" },
            { "id": "u1", "label": "build()", "source_file": "src/util/index.ts", "source_location": "L1" },
        ]));
        let files: HashMap<String, Arc<Vec<String>>> = HashMap::from([
            ("src/a.ts".into(), Arc::new(ls("export function parse(raw: string, strict = false) {\n  return raw;\n}"))),
            ("src/other.ts".into(), Arc::new(ls("export function parse(n: number) {\n  return n;\n}"))),
            ("src/util/index.ts".into(), Arc::new(ls("export function build(cfg: Cfg) {\n  return cfg;\n}"))),
            (
                "src/caller.ts".into(),
                Arc::new(ls("import { parse as px } from './a.js';\nimport * as util from './util';\n\nfunction go() {}")),
            ),
        ]);
        let r = build(
            &["src/caller.ts"],
            Some(&g),
            &[("src/caller.ts", section(&["const v = px(x);", "const c = util.build(cfg);"]))],
            &[],
            &files,
            0.0,
            "",
        );
        let mut got: Vec<(String, String, String)> =
            r.by_path["src/caller.ts"].iter().map(|c| (c.name.clone(), c.path.clone(), c.declaration.clone())).collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("build".into(), "src/util/index.ts".into(), "export function build(cfg: Cfg)".into()),
                ("parse".into(), "src/a.ts".into(), "export function parse(raw: string, strict = false)".into()),
            ]
        );
        assert_eq!(r.metrics["via_import"], 2);
        // без импорта то же имя неоднозначно по графу — карточки нет
        let empty: HashMap<String, Arc<Vec<String>>> = HashMap::from([("src/z.ts".into(), Arc::new(Vec::new()))]);
        let bare = build(&["src/z.ts"], Some(&g), &[("src/z.ts", section(&["parse(x);"]))], &[], &empty, 0.0, "");
        assert_eq!(bare.metrics["skip_reasons"]["ambiguous"], 1);
    }

    #[test]
    fn маска_литерал_и_комментарий_не_дают_вызовов() {
        let file = v(&[
            "const sql = `",
            "  -- Postgres materialises a pg_constraint row (contype 'n') for every",
            "  select ${quote(name)} from t",
            "`;",
            "/*",
            "  helper(1) в комментарии",
            "*/",
            "run(x);",
        ]);
        let masked = mask_non_code(&file);
        assert_eq!(masked.len(), file.len());
        assert!(!masked[1].contains("row"), "текст литерала замаскирован");
        assert!(masked[2].contains("quote(name)"), "код в ${{…}} сохранён");
        assert!(!masked[5].contains("helper"), "блочный комментарий замаскирован");
        assert_eq!(sorted_keys(&extract_used_identifiers(&masked)), v(&["quote", "run"]));
        let g = graph(json!([{ "id": "r", "label": "row()", "source_file": "lib/r.mjs", "source_location": "L1" }]));
        let files: HashMap<String, Arc<Vec<String>>> = HashMap::from([
            ("src/q.ts".into(), Arc::new(file.clone())),
            ("lib/r.mjs".into(), Arc::new(v(&["export function row(x) {}"]))),
        ]);
        let r = build(&["src/q.ts"], Some(&g), &[("src/q.ts", format!("    2: +{}", file[1]))], &[], &files, 0.0, "");
        assert_eq!(r.metrics["candidates"], 0);
    }

    #[test]
    fn абсолютный_путь_графа_вне_корня_отбрасывается() {
        let root = Path::new("/repo");
        assert_eq!(normalize_node_path("/repo/lib/x.mjs", root), "lib/x.mjs");
        assert_eq!(normalize_node_path("/repo/../etc/passwd", root), "");
        assert_eq!(normalize_node_path("/elsewhere/x.mjs", root), "");
        assert_eq!(normalize_node_path("./lib\\y.mjs", root), "lib/y.mjs");
    }

    #[test]
    fn locale_compare_как_icu_на_идентификаторах() {
        assert_eq!(locale_compare("a", "B"), Ordering::Less);
        assert_eq!(locale_compare("b", "A"), Ordering::Greater);
        assert_eq!(locale_compare("a", "A"), Ordering::Less, "строчная раньше заглавной");
        assert_eq!(locale_compare("_x", "$x"), Ordering::Less);
        assert_eq!(locale_compare("$x", "0x"), Ordering::Less);
        assert_eq!(locale_compare("9", "a"), Ordering::Less);
    }
}
