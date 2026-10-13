//! Пиктограммы вместо платформенных эмодзи в PR-комментариях (`REVIEW_ICON_BASE_URL`) —
//! порт `lib/icons.mjs`.
//!
//! Подмена делается на границе публикации, а не в рендере: одна функция над готовым
//! Markdown, которая трогает только видимый текст:
//!   - HTML-комментарии пропускаются — в них маркеры и машинный футер `<ns>-data:{json}`,
//!     вставка `<img src="…">` внутрь JSON сломала бы carry-over находок;
//!   - код (fenced-блоки, включая ```suggestion, и инлайн-код) пропускается — там текст
//!     должен остаться буквальным.
//!
//! Эмодзи остаётся в `alt`. Логи, dry-run и Telegram по-прежнему печатают эмодзи:
//! обёртка [`WithIcons`] ставится только на github-клиент пайплайна.
//!
//! Регулярки JS (`PROTECTED_RE` с обратной ссылкой `\1`, `^`/`$` в режиме `m`) повторены
//! ручным сканером: крейта regex в зависимостях нет, а обратных ссылок он и не умеет.
//! Границы строк — как у JS-`m`: `\n`, `\r`, U+2028, U+2029.

use anyhow::Result;
use serde_json::Value;

use crate::github::review_summary_body;
use crate::traits::{
    DeleteStaleOpts, GhCallOpts, GithubApi, LoadedDiff, PostReviewParams, PostReviewResult, StickyData,
    ThreadRef,
};

/// Эмодзи → имя файла в наборе (`.github/icons/` — источник правды), `ICON_FILES`.
pub const ICON_FILES: [(&str, &str); 11] = [
    ("🤖", "bot"),
    ("⛔", "gate-blocked"),
    ("✅", "gate-open"),
    ("🔴", "sev-critical"),
    ("🟠", "sev-major"),
    ("🟡", "sev-minor"),
    ("🔵", "sev-nit"),
    ("🔄", "delta"),
    ("🔎", "verifier"),
    ("⚖\u{FE0F}", "jury"),
    ("🔧", "prescan"),
];

/// Высота в заголовке Markdown и в обычной строке — как в `.github/icons/README.md`.
const HEIGHT_HEADING: u32 = 20;
const HEIGHT_TEXT: u32 = 16;

/// Терминатор строки для `^`/`$`/`.` в JS.
fn is_lt(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `\s` JS (WhiteSpace + LineTerminator ECMAScript).
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}'
            | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `String(x).trim()` JS: срезает `\s` с обоих концов.
fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// Файл набора для эмодзи: `(alt, file)`; `⚖` без FE0F — тот же знак, что `⚖️`.
fn icon_file(c: char) -> Option<(&'static str, &'static str)> {
    ICON_FILES.iter().find(|(e, _)| e.starts_with(c)).copied()
}

/// `/^\s*(?:>\s*)*#{1,6}\s/` на одной строке.
fn is_heading(line: &str) -> bool {
    let mut it = line.chars().peekable();
    while it.peek().is_some_and(|c| is_js_space(*c) || *c == '>') {
        it.next();
    }
    let mut hashes = 0;
    while it.peek() == Some(&'#') {
        it.next();
        hashes += 1;
    }
    (1..=6).contains(&hashes) && it.next().is_some_and(is_js_space)
}

/// Замена эмодзи в одной строке (строка без терминаторов).
fn iconize_line(line: &str, base: &str, out: &mut String) {
    if !line.chars().any(|c| icon_file(c).is_some()) {
        out.push_str(line);
        return;
    }
    let height = if is_heading(line) { HEIGHT_HEADING } else { HEIGHT_TEXT };
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match icon_file(c) {
            Some((alt, file)) => {
                if c == '⚖' && chars.peek() == Some(&'\u{FE0F}') {
                    chars.next();
                }
                out.push_str(&format!(r#"<img src="{base}/{file}.svg" alt="{alt}" height="{height}">"#));
            }
            None => out.push(c),
        }
    }
}

/// `iconizeText`: `text.replace(/^.*$/gm, …)` — построчно, терминаторы как есть.
fn iconize_text(text: &str, base: &str, out: &mut String) {
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if is_lt(c) {
            iconize_line(&text[start..i], base, out);
            out.push(c);
            start = i + c.len_utf8();
        }
    }
    iconize_line(&text[start..], base, out);
}

/// `^` в режиме `m`: начало текста или позиция после терминатора.
fn at_line_start(s: &str, i: usize) -> bool {
    i == 0 || s[..i].chars().next_back().is_some_and(is_lt)
}

/// `$` в режиме `m`: конец текста или позиция перед терминатором.
fn at_line_end(s: &str, i: usize) -> bool {
    s[i..].chars().next().is_none_or(is_lt)
}

/// `^ {0,3}<fence>[ \t]*$` начиная с позиции `p` (уже начало строки).
fn closing_fence_at(s: &str, p: usize, fence: &str) -> Option<usize> {
    let rest = &s[p..];
    let spaces = rest.len() - rest.trim_start_matches(' ').len();
    if spaces > 3 {
        return None;
    }
    let after = rest[spaces..].strip_prefix(fence)?;
    let tail = after.trim_start_matches([' ', '\t']);
    let end = s.len() - tail.len();
    at_line_end(s, end).then_some(end)
}

/// Альтернатива 2 `PROTECTED_RE`: fenced-блок, начинающийся ровно в `i`.
/// `` `{3,} `` жадный с возвратом: сперва самая длинная ограда, потом короче (остаток
/// уходит в `[^\n]*` строки-открытия) — как перебор движка JS.
fn fenced_at(s: &str, i: usize) -> Option<usize> {
    if !at_line_start(s, i) {
        return None;
    }
    let rest = &s[i..];
    let spaces = rest.len() - rest.trim_start_matches(' ').len();
    if spaces > 3 {
        return None;
    }
    let after_spaces = &rest[spaces..];
    let fc = after_spaces.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = after_spaces.len() - after_spaces.trim_start_matches(fc).len();
    if run < 3 {
        return None;
    }
    let fence_start = i + spaces;
    // `[^\n]*\n` — до первого `\n` (не любого терминатора).
    let body_start = s[fence_start..].find('\n')? + fence_start + 1;
    for n in (3..=run).rev() {
        let fence = &s[fence_start..fence_start + n];
        // `[\s\S]*?^` — первая позиция начала строки, где закрывается ограда.
        let mut p = body_start;
        loop {
            if at_line_start(s, p) {
                if let Some(end) = closing_fence_at(s, p, fence) {
                    return Some(end);
                }
            }
            match s[p..].chars().next() {
                Some(c) => p += c.len_utf8(),
                None => break,
            }
        }
    }
    None
}

/// `PROTECTED_RE` в позиции `i`: конец защищённого участка или `None`.
fn protected_at(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    // `<!--[\s\S]*?-->`
    if let Some(body) = rest.strip_prefix("<!--") {
        if let Some(k) = body.find("-->") {
            return Some(i + 4 + k + 3);
        }
    }
    if let Some(end) = fenced_at(s, i) {
        return Some(end);
    }
    // `` `[^`\n]+` ``
    if let Some(inner) = rest.strip_prefix('`') {
        let len = inner.find(['`', '\n']).filter(|k| *k > 0)?;
        if inner[len..].starts_with('`') {
            return Some(i + 1 + len + 1);
        }
    }
    None
}

/// Заменяет известные эмодзи видимого текста на `<img>` с базой `base_url`
/// (`iconize`, `lib/icons.mjs`). Пустая база — текст без изменений.
pub fn iconize(markdown: &str, base_url: &str) -> String {
    let base = js_trim(base_url).trim_end_matches('/');
    if base.is_empty() || markdown.is_empty() {
        return markdown.to_string();
    }
    let mut out = String::with_capacity(markdown.len());
    let mut last = 0;
    let mut i = 0;
    while i < markdown.len() {
        if let Some(end) = protected_at(markdown, i) {
            iconize_text(&markdown[last..i], base, &mut out);
            out.push_str(&markdown[i..end]);
            last = end;
            i = end;
        } else {
            i += markdown[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    iconize_text(&markdown[last..], base, &mut out);
    out
}

/// `String(cfg.render?.iconBaseUrl || '')` — строка базы из конфига.
pub fn icon_base_from_cfg(cfg: &Value) -> String {
    match cfg.get("render").and_then(|r| r.get("iconBaseUrl")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => n.to_string(),
        Some(Value::Bool(true)) => "true".to_string(),
        _ => String::new(),
    }
}

/// Обёртка github-клиента (`withIcons`): пиктограммы в sticky, в шапке review-сводки и в
/// теле inline-комментариев. Остальные методы (auto-triage, чтение тредов, чистка) идут
/// как есть — в частности, ответы auto-triage опознаются по тексту маркера `🤖 auto-triage`.
pub struct WithIcons<'a> {
    inner: &'a dyn GithubApi,
    base: String,
}

impl<'a> WithIcons<'a> {
    /// `None` — база пустая (после trim): клиент остаётся без обёртки.
    pub fn wrap(inner: &'a dyn GithubApi, base_url: &str) -> Option<Self> {
        (!js_trim(base_url).is_empty()).then(|| WithIcons { inner, base: base_url.to_string() })
    }
}

impl GithubApi for WithIcons<'_> {
    fn gh(&self, path: &str, opts: &GhCallOpts) -> Result<Value> {
        self.inner.gh(path, opts)
    }
    fn load_diff(&self) -> Result<Option<LoadedDiff>> {
        self.inner.load_diff()
    }
    fn read_prior_sticky_data(&self) -> Result<StickyData> {
        self.inner.read_prior_sticky_data()
    }
    fn read_prior_findings(&self) -> Result<Vec<Value>> {
        self.inner.read_prior_findings()
    }
    fn upsert_sticky(&self, body: &str) -> Result<String> {
        self.inner.upsert_sticky(&iconize(body, &self.base))
    }
    fn delete_stale_inline(&self, opts: &DeleteStaleOpts) -> Result<usize> {
        self.inner.delete_stale_inline(opts)
    }
    fn post_review(&self, params: &PostReviewParams) -> Result<PostReviewResult> {
        let head = params.body.as_deref().unwrap_or_else(|| review_summary_body(params.inline.len()));
        let mut inline = params.inline.clone();
        for c in &mut inline {
            c.body = iconize(&c.body, &self.base);
        }
        self.inner.post_review(&PostReviewParams {
            inline,
            head_sha: params.head_sha.clone(),
            body: Some(iconize(head, &self.base)),
        })
    }
    fn list_inline_comments(&self) -> Result<Vec<Value>> {
        self.inner.list_inline_comments()
    }
    fn list_issue_labels(&self) -> Result<Vec<Value>> {
        self.inner.list_issue_labels()
    }
    fn read_sticky_review(&self) -> Result<Option<Value>> {
        self.inner.read_sticky_review()
    }
    fn list_pull_comment_reactions(&self, comment_id: u64) -> Result<Vec<Value>> {
        self.inner.list_pull_comment_reactions(comment_id)
    }
    fn list_unresolved_review_threads(&self) -> Result<Vec<ThreadRef>> {
        self.inner.list_unresolved_review_threads()
    }
    fn reply_review_thread(&self, thread_id: &str, body: &str) -> Result<()> {
        self.inner.reply_review_thread(thread_id, body)
    }
    fn resolve_review_thread(&self, thread_id: &str) -> Result<()> {
        self.inner.resolve_review_thread(thread_id)
    }
}

#[cfg(test)]
#[path = "icons/tests.rs"]
mod tests;
