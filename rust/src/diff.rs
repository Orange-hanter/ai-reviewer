//! Порт lib/diff.mjs: парсер unified diff, аннотация строк, чанкинг,
//! извлечение символов для blast-radius.
//!
//! Паритет с JS-кодом сохранён построчно, включая:
//! - UTF-16-семантику измерения длин (`String.length` в JS = code units) — см. `u16_len`;
//! - git C-quoting unescape с октальными escape-последовательностями;
//! - порядок вставки в Map (serde_json с preserve_order → IndexMap);
//! - условное поле `unknownPaths` в чанках (в JS — spread только при true).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use indexmap::IndexMap;
use serde_json::Value;

// ---------------------------------------------------------------------------
// UTF-16-совместимые измерения длины/срезы
// ---------------------------------------------------------------------------

/// Длина строки в UTF-16 code units — эквивалент JS `String.length`.
pub fn u16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Обрезать строку так, чтобы `u16_len` результата был ≤ `max`.
/// JS `slice(0, n)` может разрезать суррогатную пару (lone surrogate → U+FFFD);
/// здесь символ берётся целиком — отклонение только для не-BMP символов.
pub fn u16_truncate(s: &str, max: usize) -> String {
    u16_slice(s, 0, max)
}

/// Срез по окну UTF-16 code units [start, end).
pub fn u16_slice(s: &str, start: usize, end: usize) -> String {
    let mut idx = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let l = c.len_utf16();
        let next = idx + l;
        if next <= start {
            idx = next;
            continue;
        }
        if idx >= end {
            break;
        }
        out.push(c);
        idx = next;
    }
    out
}

// ---------------------------------------------------------------------------
// git C-quoting unescape + парсинг заголовков путей
// ---------------------------------------------------------------------------

/// Unescape git C-quoting внутри quoted-пути: \\ \" \n \t и октальные \ooo (байты UTF-8).
pub fn unescape_git_path(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        match b.get(i + 1) {
            Some(b'\\') | Some(b'"') => {
                out.push(b[i + 1]);
                i += 2;
            }
            Some(b'n') => {
                out.push(10);
                i += 2;
            }
            Some(b't') => {
                out.push(9);
                i += 2;
            }
            _ => {
                // JS смотрит фиксированное окно из 3 code units и требует, чтобы ВСЁ оно
                // было октальными цифрами; иначе backslash остаётся литеральным.
                let end = (i + 4).min(b.len());
                let cand = &b[i + 1..end];
                if !cand.is_empty() && cand.iter().all(|c| (b'0'..=b'7').contains(c)) {
                    let val = u32::from_str_radix(std::str::from_utf8(cand).unwrap(), 8).unwrap();
                    out.push(val as u8); // как Buffer: & 0xFF
                    i += 1 + cand.len();
                } else {
                    out.push(b'\\');
                    i += 1;
                }
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

struct PathRes {
    quoted: std::sync::OnceLock<regex::Regex>,
    plain: std::sync::OnceLock<regex::Regex>,
    quoted_src: &'static str,
    plain_src: &'static str,
}

impl PathRes {
    fn quoted(&self) -> &regex::Regex {
        self.quoted.get_or_init(|| regex::Regex::new(self.quoted_src).unwrap())
    }
    fn plain(&self) -> &regex::Regex {
        self.plain.get_or_init(|| regex::Regex::new(self.plain_src).unwrap())
    }
    fn parse(&self, line: &str) -> Option<String> {
        let l = strip_cr(line);
        if let Some(caps) = self.quoted().captures(l) {
            return Some(unescape_git_path(caps.get(1)?.as_str()));
        }
        self.plain()
            .captures(l)
            .map(|caps| caps.get(1).unwrap().as_str().to_string())
    }
}

static NEW_PATH: PathRes = PathRes {
    quoted: std::sync::OnceLock::new(),
    plain: std::sync::OnceLock::new(),
    // `\.` в JS не матчит \n и \r — воспроизводим классом
    quoted_src: r#"^\+\+\+ "b/((?:[^"\\]|\\[^\n\r])*)"$"#,
    plain_src: r#"^\+\+\+ b/(.+)$"#,
};

static GIT_PATH: PathRes = PathRes {
    quoted: std::sync::OnceLock::new(),
    plain: std::sync::OnceLock::new(),
    quoted_src: r#" "b/((?:[^"\\]|\\[^\n\r])*)"$"#,
    plain_src: r#" b/(.+)$"#,
};

/// Путь из строки `+++ b/path` (quoted-форма `+++ "b/path with spaces"` — с unescape).
pub fn parse_diff_new_path(line: &str) -> Option<String> {
    NEW_PATH.parse(line)
}

/// Путь (b-side) из строки `diff --git a/x b/y` (quoted-форма — с unescape).
pub fn parse_diff_git_path(line: &str) -> Option<String> {
    GIT_PATH.parse(line)
}

// ---------------------------------------------------------------------------
// parseDiff
// ---------------------------------------------------------------------------

/// Запись файла из parse_diff (`files.get(path)`). Формат — контракт с
/// lib/findings.mjs (fingerprint/filterStalePriorFindings): text = номер строки
/// RIGHT-стороны → её текст.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFileRecord {
    /// Номера строк RIGHT-стороны, на которые можно вешать inline. BTreeSet —
    /// порядок возрастания совпадает с порядком вставки в JS Set.
    pub valid_lines: BTreeSet<u64>,
    /// Тексты изменённых строк (`+`/`-`) для blast-radius и prescan.
    pub changed: Vec<String>,
    /// Номер строки RIGHT-стороны → её текст.
    pub text: BTreeMap<u64, String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseDiffResult {
    pub files: serde_json::Map<String, Value>,
    pub annotated: String,
    pub changed_text: String,
}

fn take_digits(s: &str) -> Option<(u64, &str)> {
    let n = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    Some((s[..n].parse().ok()?, &s[n..]))
}

/// `^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@` — вернуть стартовую строку RIGHT-стороны.
fn parse_hunk_start(line: &str) -> Option<u64> {
    let mut rest = line.strip_prefix("@@ -")?;
    let (_, r) = take_digits(rest)?;
    rest = r;
    if let Some(r) = rest.strip_prefix(',') {
        let (_, r) = take_digits(r)?;
        rest = r;
    }
    rest = rest.strip_prefix(" +")?;
    let (start, r) = take_digits(rest)?;
    rest = r;
    if let Some(r) = rest.strip_prefix(',') {
        let (_, r) = take_digits(r)?;
        rest = r;
    }
    rest.strip_prefix(" @@")?;
    Some(start)
}

/// Парсит unified diff: карта файлов, аннотированный дифф (номера строк RIGHT-стороны)
/// и склейка изменённых строк.
pub fn parse_diff(diff: &str) -> ParseDiffResult {
    let mut files: indexmap::IndexMap<String, DiffFileRecord> = indexmap::IndexMap::new();
    let mut annotated_parts: Vec<String> = Vec::new();
    let mut changed_lines: Vec<String> = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut git_path: Option<String> = None;
    let mut deleted_header = false;
    let mut in_hunk = false;
    let mut new_line: u64 = 0;

    for raw in diff.split('\n') {
        if raw.starts_with("diff --git") {
            cur_path = None;
            git_path = parse_diff_git_path(raw);
            deleted_header = false;
            in_hunk = false;
            annotated_parts.push(raw.to_string());
            continue;
        }
        if !in_hunk && raw.starts_with("deleted file mode ") {
            deleted_header = true;
        }
        // Заголовки `---`/`+++` — только до первого хунка секции; внутри хунка это
        // строки содержимого `-- …`/`++ …` (RFC 17).
        if !in_hunk && raw.starts_with("+++ ") {
            // Удалённый файл (`+++ /dev/null`) — путь из `diff --git` (RFC 17). Только в
            // заголовке секции с `deleted file mode`: внутри хунка это добавленная строка.
            cur_path = parse_diff_new_path(raw).or_else(|| {
                if deleted_header
                    && !in_hunk
                    && raw.strip_suffix('\r').unwrap_or(raw) == "+++ /dev/null"
                {
                    git_path.clone()
                } else {
                    None
                }
            });
            if let Some(p) = &cur_path {
                files.entry(p.clone()).or_default();
            }
            annotated_parts.push(raw.to_string());
            continue;
        }
        if !in_hunk && raw.starts_with("--- ") {
            annotated_parts.push(raw.to_string());
            continue;
        }
        if let Some(n) = parse_hunk_start(raw) {
            new_line = n;
            in_hunk = true;
            annotated_parts.push(raw.to_string());
            continue;
        }
        if raw.starts_with('\\') {
            annotated_parts.push(raw.to_string());
            continue;
        }
        let Some(path) = cur_path.clone() else {
            annotated_parts.push(raw.to_string());
            continue;
        };
        let rec = files.get_mut(&path).expect("cur_path из files");
        if raw.starts_with('+') && (in_hunk || !raw.starts_with("+++")) {
            rec.valid_lines.insert(new_line);
            rec.changed.push(raw[1..].to_string());
            rec.text.insert(new_line, raw[1..].to_string());
            changed_lines.push(raw[1..].to_string());
            annotated_parts.push(format!("{:>5}: {}", new_line, raw));
            new_line += 1;
        } else if raw.starts_with('-') && (in_hunk || !raw.starts_with("---")) {
            rec.changed.push(raw[1..].to_string());
            changed_lines.push(raw[1..].to_string());
            annotated_parts.push(format!("     : {}", raw));
        } else {
            rec.valid_lines.insert(new_line);
            let t = if raw.starts_with(' ') { &raw[1..] } else { raw };
            rec.text.insert(new_line, t.to_string());
            annotated_parts.push(format!("{:>5}: {}", new_line, raw));
            new_line += 1;
        }
    }

    let files_value = files
        .into_iter()
        .map(|(k, v)| (k, serde_json::to_value(v).expect("serialize DiffFileRecord")))
        .collect();
    ParseDiffResult {
        files: files_value,
        annotated: annotated_parts.join("\n"),
        changed_text: changed_lines.join("\n"),
    }
}

// ---------------------------------------------------------------------------
// extractChangedSymbols / collectBlastSymbols
// ---------------------------------------------------------------------------

fn symbol_patterns() -> &'static [regex::Regex] {
    static PATTERNS: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        // (?-u:\b) — ASCII word boundary как в JS без unicode-флага.
        // \w в JS = [A-Za-z0-9_] — расписан явно.
        [
            r"(?-u:\b)export\s+(?:default\s+)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            r"(?-u:\b)export\s+(?:abstract\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            r"(?-u:\b)export\s+(?:interface|type|enum)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
            r"(?-u:\b)export\s+const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:<[^>]*>\s*)?\(",
            r"(?-u:\b)export\s+const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(?:async\s*)?function",
            r"(?-u:\b)export\s*\{([^}]+)\}",
            // Python: публичное определение верхнего уровня (колонка 0). Якорь `(?:^|\n)`
            // вместо `(?m)` — у JS-флага `m` границы строк шире (`\r`, U+2028).
            r"(?:^|\n)(?:async[ \t]+)?def[ \t]+([A-Za-z][A-Za-z0-9_]*)[ \t]*\(",
            r"(?:^|\n)class[ \t]+([A-Za-z][A-Za-z0-9_]*)[ \t]*[(:]",
        ]
        .iter()
        .map(|src| regex::Regex::new(src).unwrap())
        .collect()
    })
}

/// `token.replace(/^type\s+/, '')` из JS.
fn strip_type_prefix(token: &str) -> &str {
    if let Some(rest) = token.strip_prefix("type") {
        let trimmed = rest.trim_start();
        if trimmed.len() < rest.len() {
            return trimmed;
        }
    }
    token
}

fn add_symbol(out: &mut Vec<String>, seen: &mut HashSet<String>, raw: &str) {
    let name = raw.trim();
    if !name.is_empty() && u16_len(name) >= 3 && seen.insert(name.to_string()) {
        out.push(name.to_string());
    }
}

/// Имена экспортов из текста (дифф или целый файл), до max_symbols.
/// Порядок — по паттернам, внутри паттерна по позиции совпадения; дедуп с сохранением
/// первого вхождения (как JS Set).
pub fn extract_changed_symbols(text: &str, max_symbols: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    static RE_AS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_as = RE_AS.get_or_init(|| regex::Regex::new(r"(?i)\s+as\s+").unwrap());
    for re in symbol_patterns() {
        for m in re.captures_iter(text) {
            let Some(g) = m.get(1) else { continue };
            let g = g.as_str();
            if g.contains(',') || g.contains(" as ") {
                for part in g.split(',') {
                    let token = part.trim();
                    if token.is_empty() || token == "type" || token == "typeof" {
                        continue;
                    }
                    // `bar as baz` → для blast-radius нужен исходный binding `bar`
                    let name = re_as.split(strip_type_prefix(token)).next().unwrap_or("").trim();
                    add_symbol(&mut out, &mut seen, name);
                }
            } else {
                add_symbol(&mut out, &mut seen, g);
            }
        }
    }
    out.truncate(max_symbols);
    out
}

const CODE_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".mts", ".cts", ".mjs", ".cjs", ".js", ".jsx", ".py", ".pyi",
];

fn is_code_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    CODE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// Символы для Graphify blast-radius: сначала из изменённых строк, затем добор
/// экспортами из полных текстов затронутых `.ts`/`.js`/`.py` файлов.
pub fn collect_blast_symbols(
    changed_text: &str,
    content_map: Option<&indexmap::IndexMap<String, String>>,
    max_symbols: usize,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push = |name: &str, out: &mut Vec<String>, seen: &mut HashSet<String>| {
        if u16_len(name) >= 3 && seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    };
    for name in extract_changed_symbols(changed_text, max_symbols) {
        push(&name, &mut out, &mut seen);
    }
    let Some(content_map) = content_map else {
        return out;
    };
    for (path, text) in content_map {
        if out.len() >= max_symbols {
            break;
        }
        if !is_code_path(path) {
            continue;
        }
        for name in extract_changed_symbols(text, max_symbols) {
            push(&name, &mut out, &mut seen);
            if out.len() >= max_symbols {
                return out;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// splitSegmentByHunks / buildChunks
// ---------------------------------------------------------------------------

/// Разрезать сегмент диффа по хункам с повтором заголовка файла; бюджеты — в UTF-16 code units.
pub fn split_segment_by_hunks(seg: &str, max_bytes: usize) -> Vec<String> {
    if u16_len(seg) <= max_bytes {
        return vec![seg.to_string()];
    }
    let lines: Vec<&str> = seg.split('\n').collect();
    let Some(first_hunk) = lines.iter().position(|l| l.starts_with("@@ ")) else {
        let mut out = Vec::new();
        let total = u16_len(seg);
        let mut off = 0;
        while off < total {
            out.push(u16_slice(seg, off, off + max_bytes));
            off += max_bytes;
        }
        return out;
    };
    let header = lines[..first_hunk].join("\n");
    let mut hunks: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for l in &lines[first_hunk..] {
        if l.starts_with("@@ ") && !cur.is_empty() {
            hunks.push(cur.join("\n"));
            cur = Vec::new();
        }
        cur.push(l);
    }
    if !cur.is_empty() {
        hunks.push(cur.join("\n"));
    }
    let mut pieces: Vec<String> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    let mut size = u16_len(&header);
    let flush = |buf: &mut Vec<String>, size: &mut usize, pieces: &mut Vec<String>| {
        if !buf.is_empty() {
            pieces.push(format!("{}\n{}", header, buf.join("\n")));
            buf.clear();
            *size = u16_len(&header);
        }
    };
    for h in &hunks {
        if !buf.is_empty() && size + u16_len(h) + 1 > max_bytes {
            flush(&mut buf, &mut size, &mut pieces);
        }
        size += u16_len(h) + 1;
        buf.push(h.clone());
    }
    flush(&mut buf, &mut size, &mut pieces);
    if pieces.is_empty() {
        vec![seg.to_string()]
    } else {
        pieces
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkLimits {
    pub max_diff_bytes: usize,
    pub max_context_bytes: usize,
    pub chunk_bytes: usize,
    pub max_chunks: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Chunk {
    pub annotated: String,
    pub file_contents: String,
    /// В чанке есть секция, у которой не удалось определить путь. Поле присутствует
    /// в JSON только при true — как spread `{...(curUnknown ? {unknownPaths: true} : {})}` в JS.
    #[serde(skip_serializing_if = "is_not_true")]
    pub unknown_paths: bool,
    pub paths: Vec<String>,
    pub partial_paths: Vec<String>,
    // Поля ниже `build_chunks` не заполняет: их дописывает пайплайн ПОСЛЕ нарезки
    // (`pipeline.mjs`: «windowedPaths дописываем ПОСЛЕ нарезки»), поэтому паритет нарезки
    // с JS они не трогают. `None`/пусто — ключа в JSON нет, как у неприсвоенного поля в JS.
    /// Пути чанка с контекстом, урезанным окнами (RFC 07). `None` — окна не резали
    /// ни одного файла PR, и поля нет вовсе (`chunk.windowedPaths === undefined`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windowed_paths: Option<Vec<String>>,
    /// Готовые строки карточек символов (RFC 14); читает `prompt::build_messages`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbol_cards: Vec<String>,
    /// Имена символов карточек — для `symbol_cards_outcome`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbol_card_names: Vec<String>,
    /// Размер карточек чанка (UTF-16) — метрика `plan.chunkPlan`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_cards_chars: Option<usize>,
    /// Сколько карточек отброшено лимитами — метрика `plan`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_cards_dropped: Option<usize>,
}

fn is_not_true(b: &bool) -> bool {
    !*b
}

fn first_line_starting_with<'a>(seg: &'a str, prefix: &str) -> Option<&'a str> {
    seg.split('\n').find(|l| l.starts_with(prefix))
}

/// Путь секции: из `+++ `, при неудаче — из `diff --git `, иначе ''.
fn path_of(seg: &str) -> String {
    if let Some(p) = first_line_starting_with(seg, "+++ ").and_then(parse_diff_new_path) {
        return p;
    }
    if let Some(p) = first_line_starting_with(seg, "diff --git ").and_then(parse_diff_git_path) {
        return p;
    }
    String::new()
}

/// Разрезать annotated-дифф на секции по файлам — как `split(/(?=^diff --git )/m)` + filter(trim).
fn split_file_segments(annotated: &str) -> Vec<String> {
    let mut points: Vec<usize> = Vec::new();
    let mut idx = 0usize;
    for line in annotated.split('\n') {
        if line.starts_with("diff --git ") {
            points.push(idx);
        }
        idx += line.len() + 1;
    }
    let mut segs: Vec<String> = Vec::new();
    let mut prev = 0usize;
    for p in points {
        if p > prev {
            segs.push(annotated[prev..p].to_string());
        }
        prev = p;
    }
    if prev < annotated.len() {
        segs.push(annotated[prev..].to_string());
    }
    segs.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// `collectAddedPaths` (`lib/diff.mjs`): пути, добавленные этим PR — секции
/// `diff --git` со строкой `new file mode`. Порядок — первого появления (JS `Set`).
///
/// Полный текст добавленного файла и есть его дифф, поэтому контекст такого файла
/// модели второй раз не прикладывают (RFC 04, `readChangedFiles` с `addedPaths`).
/// Путь — из `+++ `, иначе из `diff --git ` (пустой путь — falsy в JS, идём дальше).
pub fn collect_added_paths(diff: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in split_file_segments(diff) {
        if !seg.split('\n').any(|l| l.starts_with("new file mode ")) {
            continue;
        }
        let path = first_line_starting_with(&seg, "+++ ")
            .and_then(parse_diff_new_path)
            .filter(|p| !p.is_empty())
            .or_else(|| {
                first_line_starting_with(&seg, "diff --git ")
                    .and_then(parse_diff_git_path)
                    .filter(|p| !p.is_empty())
            });
        if let Some(p) = path {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// Собрать чанки для LLM: разрез аннотированного диффа по бюджету с картой видимости файлов.
#[allow(unused_assignments)] // финальный flush в конце цикла «зря» обнуляет состояние — как в JS
pub fn build_chunks(
    annotated: &str,
    content_map: &indexmap::IndexMap<String, String>,
    limits: &ChunkLimits,
) -> Vec<Chunk> {
    let file_segs = split_file_segments(annotated);
    let mut total = 0usize;
    for seg in &file_segs {
        total += u16_len(seg).min(limits.max_diff_bytes)
            + u16_len(content_map.get(&path_of(seg)).map(String::as_str).unwrap_or(""))
                .min(limits.max_context_bytes);
    }
    // Math.ceil(total / maxChunks) для неотрицательных целых == (total + maxChunks - 1) / maxChunks
    let eff_chunk = limits.chunk_bytes.max(total.div_ceil(limits.max_chunks));

    let mut items: Vec<(String, String, String, usize)> = Vec::new(); // (s, path, content, size)
    let mut truncated: HashSet<String> = HashSet::new();
    for seg in &file_segs {
        let path = path_of(seg);
        let diff = if u16_len(seg) > limits.max_diff_bytes {
            let d = format!(
                "{}\n… (файл обрезан в диффе)",
                u16_truncate(seg, limits.max_diff_bytes)
            );
            if !path.is_empty() {
                truncated.insert(path.clone());
            }
            d
        } else {
            seg.clone()
        };
        let mut content = content_map.get(&path).cloned().unwrap_or_default();
        if u16_len(&content) > limits.max_context_bytes {
            content = format!(
                "{}\n… (файл обрезан)",
                u16_truncate(&content, limits.max_context_bytes)
            );
            if !path.is_empty() {
                truncated.insert(path.clone());
            }
        }
        let pieces = split_segment_by_hunks(&diff, eff_chunk);
        for (piece_idx, s) in pieces.iter().enumerate() {
            // Полный контент файла — только в ПЕРВОМ куске файла
            let content = if piece_idx == 0 { content.clone() } else { String::new() };
            let size = u16_len(s) + u16_len(&content);
            items.push((s.clone(), path.clone(), content, size));
        }
    }

    // Путь → сколько его кусков всего (один путь может прийти несколькими секциями)
    let mut total_pieces: indexmap::IndexMap<String, usize> = indexmap::IndexMap::new();
    for (_, path, _, _) in &items {
        if !path.is_empty() {
            *total_pieces.entry(path.clone()).or_insert(0) += 1;
        }
    }

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut cur_segs: Vec<String> = Vec::new();
    let mut cur_content: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut cur_pieces: indexmap::IndexMap<String, usize> = indexmap::IndexMap::new();
    let mut cur_unknown = false;

    macro_rules! flush {
        () => {{
            if !cur_segs.is_empty() {
                let paths: Vec<String> = cur_pieces.keys().cloned().collect();
                let partial_paths: Vec<String> = paths
                    .iter()
                    .filter(|p| {
                        truncated.contains(*p)
                            || cur_pieces.get(*p).copied().unwrap_or(0)
                                < total_pieces.get(*p).copied().unwrap_or(1)
                    })
                    .cloned()
                    .collect();
                chunks.push(Chunk {
                    annotated: cur_segs.join("\n"),
                    file_contents: cur_content.join("\n\n"),
                    unknown_paths: cur_unknown,
                    paths,
                    partial_paths,
                    ..Chunk::default()
                });
                cur_segs = Vec::new();
                cur_content = Vec::new();
                cur_pieces = indexmap::IndexMap::new();
                cur_unknown = false;
                bytes = 0;
            }
        }};
    }

    for (s, path, content, size) in &items {
        if !cur_segs.is_empty() && bytes + size > eff_chunk {
            flush!();
        }
        cur_segs.push(s.clone());
        if !content.is_empty() {
            cur_content.push(content.clone());
        }
        if !path.is_empty() {
            *cur_pieces.entry(path.clone()).or_insert(0) += 1;
        } else {
            cur_unknown = true;
        }
        bytes += size;
    }
    flush!();

    if chunks.is_empty() {
        return vec![Chunk {
            annotated: annotated.to_string(),
            file_contents: String::new(),
            unknown_paths: false,
            paths: Vec::new(),
            partial_paths: Vec::new(),
            ..Chunk::default()
        }];
    }
    chunks
}

// ---------------------------------------------------------------------------
// readChangedFiles — чтение затронутых файлов для контекста модели (RFC 20 T35)
// ---------------------------------------------------------------------------

/// Пути, чьё содержимое модели не нужно: lock-файлы, бинарные и медиа-форматы.
/// Тот же список, что в `readChangedFiles` (lib/diff.mjs); регекс линейный (crate regex).
fn skip_context_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(^|/)package-lock\.json$|\.(lock|png|jpe?g|gif|svg|ico|pdf|min\.js|map|heic|heif|webp|avif|xlsx?|docx?|pptx?|zip|gz|tgz|bz2|7z|rar|mp[34]|mov|avi|woff2?|ttf|otf|eot|wasm|jar|pyc|so|dylib|dll|exe)$",
        )
        .expect("skip_context_re")
    })
}

/// `addedFileMarker`: запись контекста для добавленного файла — указатель на дифф вместо
/// второго экземпляра содержимого (RFC 04). Без тела в кавычках, поэтому окна
/// (`context_window`) её не режут.
pub fn added_file_marker(path: &str) -> String {
    format!("### {path}\n(новый файл — его полное содержимое целиком приведено в диффе ниже)")
}

/// Окно проверки на NUL — первые 64 КБ: детерминированная защита без чтения гигабайтов.
const NUL_PROBE_BYTES: usize = 65_536;

/// `readChangedFiles` относительно рабочего каталога процесса (как `process.cwd()` в JS).
/// Не удалось узнать cwd — пустая карта: контекст файлов не критичен (fail-open).
pub fn read_changed_files<I, S>(
    paths: I,
    max_file_lines: f64,
    log: &mut dyn FnMut(&str),
    added_paths: Option<&[String]>,
) -> IndexMap<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    match std::env::current_dir() {
        Ok(root) => read_changed_files_in(&root, paths, max_file_lines, log, added_paths),
        Err(_) => IndexMap::new(),
    }
}

/// `readChangedFiles` (lib/diff.mjs): затронутые файлы → записи контекста
/// `### path` + тело в тройных кавычках, до `max_file_lines` строк с хвостом
/// `… (показаны первые N из M строк)`. Порядок — порядок путей (ключи карты parseDiff).
///
/// Пропускаются молча: lock/бинарные расширения, нечитаемые файлы (нет, каталог, права).
/// С записью в лог: путь вне `root` и файл с NUL в первых 64 КБ.
/// `added_paths` — пути, добавленные PR (`collect_added_paths`): вместо содержимого
/// кладётся `added_file_marker`, файл не читается вовсе.
///
/// Пути — из диффа автора PR, т.е. недоверенные. Containment проверяется дважды:
/// лексически (`path.resolve`, как в JS) и по realpath — symlink внутри репо, указывающий
/// наружу (`ln -s /etc/passwd x.ts`), иначе отдал бы модели чужой файл.
pub fn read_changed_files_in<I, S>(
    root: &std::path::Path,
    paths: I,
    max_file_lines: f64,
    log: &mut dyn FnMut(&str),
    added_paths: Option<&[String]>,
) -> IndexMap<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut map = IndexMap::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let root = crate::util::js_resolve(&cwd, &root.to_string_lossy());
    // realpath корня один раз; не раскрылся — сравниваем с лексическим (чтение всё равно упадёт).
    let real_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    // `abs !== root && !abs.startsWith(root + sep)`: сам root «внутри» (чтение каталога упадёт молча).
    let inside = |p: &std::path::Path, base: &std::path::Path| p == base || p.starts_with(base);

    for path in paths {
        let path = path.as_ref();
        if skip_context_re().is_match(path) {
            continue;
        }
        if added_paths.is_some_and(|a| a.iter().any(|p| p == path)) {
            map.insert(path.to_string(), added_file_marker(path));
            continue;
        }
        let abs = crate::util::js_resolve(&root, path);
        if !inside(&abs, &root) {
            log(&format!("пропуск файла вне репо: {path}"));
            continue;
        }
        let Ok(real) = std::fs::canonicalize(&abs) else { continue };
        if !inside(&real, &real_root) {
            log(&format!("пропуск файла вне репо: {path}"));
            continue;
        }
        let Ok(buf) = std::fs::read(&real) else { continue };
        if buf[..buf.len().min(NUL_PROBE_BYTES)].contains(&0) {
            log(&format!("пропуск бинарного файла (NUL byte): {path}"));
            continue;
        }
        // Buffer#toString('utf8'): битые последовательности → U+FFFD, BOM сохраняется.
        let content = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = content.split('\n').collect();
        let end = js_slice_end(max_file_lines, lines.len());
        let mut body = lines[..end].join("\n");
        if (lines.len() as f64) > max_file_lines {
            body.push_str(&format!(
                "\n… (показаны первые {} из {} строк)",
                crate::structure_evidence::js_number_to_string(max_file_lines),
                lines.len()
            ));
        }
        map.insert(path.to_string(), format!("### {path}\n```\n{body}\n```"));
    }
    map
}

/// Конец `slice(0, end)` в JS: ToIntegerOrInfinity (NaN → 0), отрицательный — от конца.
fn js_slice_end(end: f64, len: usize) -> usize {
    let t = if end.is_nan() { 0.0 } else { end.trunc() };
    let len_f = len as f64;
    let v = if t < 0.0 { (len_f + t).max(0.0) } else { t.min(len_f) };
    v as usize
}

// ---------------------------------------------------------------------------
// Тесты — ключевые случаи из test/pr-review.test.mjs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = "diff --git a/foo.ts b/foo.ts\n--- a/foo.ts\n+++ b/foo.ts\n@@ -1,2 +1,3 @@\n const a = 1;\n-const b = 2;\n+const b = 3;\n+export function foo() { return a; }\n";

    #[test]
    fn parse_diff_valid_lines_changed_annotated() {
        let res = parse_diff(SAMPLE_DIFF);
        let rec = &res.files["foo.ts"];
        let valid: Vec<u64> = rec["validLines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        // ВАЖНО: завершающий "\n" даёт в JS пустую строку-элемент, она уходит в ветку
        // context → validLines = {1,2,3,4} (JS-тест проверяет только has() для 1..3).
        assert_eq!(valid, vec![1, 2, 3, 4]);
        assert_eq!(rec["changed"].as_array().unwrap().len(), 3);
        assert!(res.annotated.lines().any(|l| l.starts_with("    1: ")));
        assert!(res.changed_text.contains("export function foo"));
    }

    #[test]
    fn parse_diff_keeps_deleted_file() {
        let diff = "diff --git a/auth/guard.ts b/auth/guard.ts\ndeleted file mode 100644\n--- a/auth/guard.ts\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-export function guard() { return check(); }\n-export const x = 1;";
        let r = parse_diff(diff);
        let keys: Vec<&String> = r.files.keys().collect();
        assert_eq!(keys, vec!["auth/guard.ts"]);
        assert!(r.changed_text.contains("export function guard()"), "удалённые строки — в changedText для blast-radius");
        assert!(r.files["auth/guard.ts"]["validLines"].as_array().unwrap().is_empty());
    }

    #[test]
    fn parse_diff_hunk_line_does_not_spoof_deleted_header() {
        // добавленная строка `++ /dev/null` выглядит как `+++ /dev/null` — не заголовок
        let diff = "diff --git a/s.ts b/s.ts\n--- a/s.ts\n+++ b/s.ts\n@@ -1 +1,3 @@\n a\n+/*\n+++ /dev/null\n+import x from 'raw';";
        let r = parse_diff(diff);
        let keys: Vec<&String> = r.files.keys().collect();
        assert_eq!(keys, vec!["s.ts"]);
        assert!(r.changed_text.contains("import x from 'raw'"), "строки после подделки не теряются");
    }

    #[test]
    fn parse_diff_quoted_path_with_spaces() {
        let d = "diff --git \"a/Price copy/f.ts\" \"b/Price copy/f.ts\"\n--- \"a/Price copy/f.ts\"\n+++ \"b/Price copy/f.ts\"\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let res = parse_diff(d);
        let rec = &res.files["Price copy/f.ts"];
        // см. parse_diff_valid_lines_changed_annotated: trailing newline → +1 context-строка
        assert_eq!(
            rec["validLines"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn unescape_git_c_quoting() {
        assert_eq!(
            parse_diff_new_path("+++ \"b/Price/\\320\\237\\321\\200\\320\\260\\320\\271\\321\\201.xlsx\"").unwrap(),
            "Price/Прайс.xlsx"
        );
        assert_eq!(parse_diff_new_path("+++ \"b/Price copy/f.ts\"").unwrap(), "Price copy/f.ts");
        assert_eq!(
            parse_diff_git_path("diff --git \"a/Price/\\320\\237\\321\\200.xlsx\" \"b/Price/\\320\\237\\321\\200.xlsx\"").unwrap(),
            "Price/Пр.xlsx"
        );
        assert_eq!(parse_diff_new_path("+++ \"b/dir\\\\sub/f.ts\"").unwrap(), "dir\\sub/f.ts");
        assert_eq!(parse_diff_new_path("+++ b/plain.ts").unwrap(), "plain.ts");
        assert_eq!(unescape_git_path("no-escapes"), "no-escapes");
    }

    #[test]
    fn parse_diff_paths_strip_cr() {
        assert_eq!(parse_diff_new_path("+++ b/f.ts\r").unwrap(), "f.ts");
        assert_eq!(parse_diff_new_path("+++ \"b/Price copy/f.ts\"\r").unwrap(), "Price copy/f.ts");
        assert_eq!(parse_diff_git_path("diff --git a/f.ts b/f.ts\r").unwrap(), "f.ts");
    }

    #[test]
    fn extract_changed_symbols_exports_only() {
        let changed = "export function foo() {}\nexport const barBaz = async () => {}\nexport class Widget {}\nexport interface Shape {}\nconst localHelper = () => {}\nexport const x = 1;";
        let syms = extract_changed_symbols(changed, 25);
        assert!(syms.contains(&"foo".to_string()));
        assert!(syms.contains(&"barBaz".to_string()));
        assert!(syms.contains(&"Widget".to_string()));
        assert!(syms.contains(&"Shape".to_string()));
        assert!(!syms.contains(&"localHelper".to_string()));
        assert!(!syms.contains(&"x".to_string()));
    }

    #[test]
    fn extract_changed_symbols_export_clause() {
        let syms = extract_changed_symbols("export { makeDataStore, helper as aliasHelper };", 25);
        assert!(syms.contains(&"makeDataStore".to_string()));
        assert!(syms.contains(&"helper".to_string()));
        assert!(!syms.contains(&"aliasHelper".to_string()));
        for src in ["export { fooBar };", "export { fooBar } from './other.js';", "export {\n  fooBar,\n};"] {
            let syms = extract_changed_symbols(src, 25);
            assert!(syms.contains(&"fooBar".to_string()), "имя без пробелов для: {src}");
            assert!(syms.iter().all(|s| s.trim() == s.as_str()));
        }
    }

    #[test]
    fn collect_blast_symbols_picks_exports_from_full_file() {
        let changed = "+  return store.get(id);\n";
        let mut content_map = indexmap::IndexMap::new();
        content_map.insert(
            "app/server/store.ts".to_string(),
            "export function makeDataStore() { return {}; }\nexport const unused = 1;\n".to_string(),
        );
        let syms = collect_blast_symbols(changed, Some(&content_map), 25);
        assert!(syms.contains(&"makeDataStore".to_string()));
    }

    #[test]
    fn extract_changed_symbols_python_public_top_level() {
        let changed = "def row_id_for_slug(session, slug):\nasync def fetch_all(x):\nclass ProjectNotFoundError(LookupError):\nclass Plain:\ndef _private(x):\n    def method(self):\nclass _Hidden:\ndef ab(x):";
        let syms = extract_changed_symbols(changed, 25);
        assert_eq!(syms, vec!["row_id_for_slug", "fetch_all", "ProjectNotFoundError", "Plain"]);
    }

    #[test]
    fn collect_blast_symbols_picks_python_defs_from_full_file() {
        let changed = "    return session.scalar(stmt)\n";
        let mut content_map = indexmap::IndexMap::new();
        content_map.insert(
            "cod_doc/services/project_service.py".to_string(),
            "import x\n\n\ndef row_id_for_slug(session, slug):\n    return 1\n\n\ndef _helper():\n    pass\n".to_string(),
        );
        let syms = collect_blast_symbols(changed, Some(&content_map), 25);
        assert_eq!(syms, vec!["row_id_for_slug"]);
    }

    #[test]
    fn split_segment_by_hunks_small_and_large() {
        let small = "diff --git a/f b/f\n+++ b/f\n@@ -1 +1 @@\n+x";
        assert_eq!(split_segment_by_hunks(small, 10_000), vec![small]);

        let seg = [
            "diff --git a/f.ts b/f.ts", "--- a/f.ts", "+++ b/f.ts",
            "@@ -1,1 +1,1 @@", "+aaaaaaaaaaaaaaaaaaaa",
            "@@ -5,1 +5,1 @@", "+bbbbbbbbbbbbbbbbbbbb",
        ].join("\n");
        let header = "diff --git a/f.ts b/f.ts\n--- a/f.ts\n+++ b/f.ts";
        let pieces = split_segment_by_hunks(&seg, u16_len(header) + 25);
        assert_eq!(pieces.len(), 2);
        for p in &pieces {
            assert!(p.starts_with("diff --git a/f.ts"), "заголовок в каждой части");
        }
    }

    fn default_limits() -> ChunkLimits {
        ChunkLimits {
            max_diff_bytes: 200_000,
            max_context_bytes: 180_000,
            chunk_bytes: 40_000,
            max_chunks: 40,
        }
    }

    #[test]
    fn build_chunks_small_pr_one_chunk_with_content() {
        let res = parse_diff(SAMPLE_DIFF);
        let mut content_map = indexmap::IndexMap::new();
        content_map.insert("foo.ts".to_string(), "### foo.ts\n```\nconst a=1;\n```".to_string());
        let chunks = build_chunks(&res.annotated, &content_map, &default_limits());
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].file_contents.contains("### foo.ts"));
    }

    #[test]
    fn build_chunks_paths_partial_and_unknown() {
        let limits = default_limits();
        let two = "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n+const a = 1;\n"
            .to_string()
            + "diff --git a/b.ts b/b.ts\n--- a/b.ts\n+++ b/b.ts\n@@ -1 +1 @@\n+const b = 2;\n";
        let chunks = build_chunks(&two, &indexmap::IndexMap::new(), &limits);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].paths, vec!["a.ts".to_string(), "b.ts".to_string()]);
        assert!(chunks[0].partial_paths.is_empty());

        let hunks = ["diff --git a/big.ts b/big.ts", "--- a/big.ts", "+++ b/big.ts"].join("\n") + "\n"
            + &(0..6)
                .map(|i| format!("@@ -{} +{} @@\n+line{}\n", i + 1, i + 1, i))
                .collect::<String>();
        let split = build_chunks(
            &hunks,
            &indexmap::IndexMap::new(),
            &ChunkLimits { chunk_bytes: 120, ..limits },
        );
        assert!(split.len() > 1, "файл разрезан по хункам на несколько чанков");
        for c in &split {
            assert_eq!(c.paths, vec!["big.ts".to_string()]);
            assert_eq!(c.partial_paths, vec!["big.ts".to_string()]);
        }

        let long = "diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -1 +1 @@\n+".to_string()
            + &"y".repeat(400)
            + "\n";
        let cut = build_chunks(
            &long,
            &indexmap::IndexMap::new(),
            &ChunkLimits { max_diff_bytes: 120, ..limits },
        );
        assert_eq!(cut[0].partial_paths, vec!["x.ts".to_string()]);

        let odd = build_chunks("@@ -1 +1 @@\n+x\n", &indexmap::IndexMap::new(), &limits);
        assert_eq!(odd[0].paths, Vec::<String>::new());
        assert!(odd[0].unknown_paths);
    }

    #[test]
    fn build_chunks_content_only_in_first_piece() {
        let seg = [
            "diff --git a/f.ts b/f.ts", "--- a/f.ts", "+++ b/f.ts",
            "@@ -1,1 +1,1 @@", "+aaaaaaaaaaaaaaaaaaaa",
            "@@ -5,1 +5,1 @@", "+bbbbbbbbbbbbbbbbbbbb",
        ].join("\n");
        let mut content_map = indexmap::IndexMap::new();
        content_map.insert("f.ts".to_string(), "### f.ts\n```\nconst a=1;\n```".to_string());
        let chunks = build_chunks(
            &seg,
            &content_map,
            &ChunkLimits { chunk_bytes: 100, ..default_limits() },
        );
        assert_eq!(chunks.len(), 2, "два куска → два чанка");
        let with_content = chunks.iter().filter(|c| c.file_contents.contains("### f.ts")).count();
        assert_eq!(with_content, 1, "полный контент файла — ровно в одном чанке");
    }

    #[test]
    fn u16_len_counts_surrogate_pairs_as_two() {
        assert_eq!(u16_len("ab"), 2);
        assert_eq!(u16_len("😀"), 2);
        assert_eq!(u16_len("Прайс"), 5);
    }

    #[test]
    fn collect_added_paths_как_js() {
        // Эталон снят с node на lib/diff.mjs (collectAddedPaths, FORCE_JS):
        // ["new.ts","with space.md","empty.txt"] — изменённый файл не попадает,
        // quoted-путь раскрыт, пустой новый файл берётся из `diff --git`, повтор схлопнут.
        let diff = r#"diff --git a/old.ts b/old.ts
index 1..2 100644
--- a/old.ts
+++ b/old.ts
@@ -1 +1 @@
-a
+b
diff --git a/new.ts b/new.ts
new file mode 100644
index 0000000..3
--- /dev/null
+++ b/new.ts
@@ -0,0 +1 @@
+x
diff --git "a/with space.md" "b/with space.md"
new file mode 100644
--- /dev/null
+++ "b/with space.md"
@@ -0,0 +1 @@
+y
diff --git a/empty.txt b/empty.txt
new file mode 100644
index 0000000..e69de29
diff --git a/new.ts b/new.ts
new file mode 100644
+++ b/new.ts
"#;
        assert_eq!(collect_added_paths(diff), vec!["new.ts", "with space.md", "empty.txt"]);
        assert!(collect_added_paths("").is_empty());
    }

    // -----------------------------------------------------------------------
    // read_changed_files (RFC 20 T35)
    // -----------------------------------------------------------------------

    fn read_in(
        root: &std::path::Path,
        paths: &[&str],
        max: f64,
        added: Option<&[String]>,
    ) -> (IndexMap<String, String>, Vec<String>) {
        let mut logs = Vec::new();
        let map = read_changed_files_in(root, paths.iter(), max, &mut |m: &str| logs.push(m.to_string()), added);
        (map, logs)
    }

    #[test]
    fn read_changed_files_обычный_и_усечение() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.ts"), "l1\nl2\nl3\nl4\n").unwrap();
        let (map, logs) = read_in(tmp.path(), &["src/a.ts"], 100.0, None);
        assert_eq!(map["src/a.ts"], "### src/a.ts\n```\nl1\nl2\nl3\nl4\n\n```");
        assert!(logs.is_empty());
        // 5 элементов split('\n') (хвостовой '\n' даёт пустой) > 2 → хвост-маркер.
        let (map, _) = read_in(tmp.path(), &["src/a.ts"], 2.0, None);
        assert_eq!(map["src/a.ts"], "### src/a.ts\n```\nl1\nl2\n… (показаны первые 2 из 5 строк)\n```");
    }

    #[test]
    fn read_changed_files_добавленный_файл_маркером() {
        let tmp = tempfile::tempdir().unwrap();
        let body = format!("export const a = 1;\n{}", "// заполнение\n".repeat(200));
        std::fs::write(tmp.path().join("n.ts"), &body).unwrap();
        let (full, _) = read_in(tmp.path(), &["n.ts"], 100.0, None);
        assert!(full["n.ts"].contains("export const a = 1;"));
        let added = vec!["n.ts".to_string(), "missing.ts".to_string()];
        let (skipped, _) = read_in(tmp.path(), &["n.ts", "missing.ts"], 100.0, Some(&added));
        assert_eq!(skipped["n.ts"], added_file_marker("n.ts"));
        assert!(skipped["n.ts"].contains("новый файл"));
        assert!(skipped["n.ts"].len() < full["n.ts"].len());
        assert!(skipped.contains_key("missing.ts"), "добавленный файл не читается — маркер и без файла на диске");
        // Маркер не режется окнами.
        let w = crate::context_window::window_file_entry(&skipped["n.ts"], &[1.0], 1.0);
        assert_eq!(w, skipped["n.ts"]);
    }

    #[test]
    fn read_changed_files_пропуски() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bin.dat"), [0x89u8, 0x50, 0x4e, 0x47, 0x00, 0x0d]).unwrap();
        std::fs::write(tmp.path().join("yarn.lock"), "x").unwrap();
        std::fs::write(tmp.path().join("pic.PNG"), "x").unwrap();
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        let (map, logs) = read_in(
            tmp.path(),
            &["bin.dat", "yarn.lock", "pic.PNG", "package-lock.json", "dir", "nope.ts", ""],
            100.0,
            None,
        );
        assert!(map.is_empty(), "{map:?}");
        assert_eq!(logs, ["пропуск бинарного файла (NUL byte): bin.dat"]);
    }

    #[test]
    fn read_changed_files_путь_вне_репо() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(outer.path().join("secret.txt"), "TOKEN").unwrap();
        let (map, logs) = read_in(&root, &["../secret.txt", "/etc/hosts"], 100.0, None);
        assert!(map.is_empty());
        assert_eq!(logs, ["пропуск файла вне репо: ../secret.txt", "пропуск файла вне репо: /etc/hosts"]);
    }

    #[cfg(unix)]
    #[test]
    fn read_changed_files_symlink_наружу_не_читается() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(outer.path().join("secret.txt"), "TOKEN").unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.txt"), root.join("x.ts")).unwrap();
        std::fs::write(root.join("in.ts"), "ok").unwrap();
        std::os::unix::fs::symlink(root.join("in.ts"), root.join("link.ts")).unwrap();
        let (map, logs) = read_in(&root, &["x.ts", "link.ts"], 100.0, None);
        assert!(!map.contains_key("x.ts"));
        assert_eq!(map["link.ts"], "### link.ts\n```\nok\n```", "symlink внутри репо читается");
        assert_eq!(logs, ["пропуск файла вне репо: x.ts"]);
    }

    #[test]
    fn read_changed_files_невалидный_utf8_и_nan_лимит() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), [b'a', 0xff, b'b']).unwrap();
        let (map, _) = read_in(tmp.path(), &["a.txt"], 100.0, None);
        assert_eq!(map["a.txt"], "### a.txt\n```\na\u{fffd}b\n```");
        // slice(0, NaN) → пусто; 1 > NaN — false, хвоста нет.
        let (map, _) = read_in(tmp.path(), &["a.txt"], f64::NAN, None);
        assert_eq!(map["a.txt"], "### a.txt\n```\n\n```");
    }
}
