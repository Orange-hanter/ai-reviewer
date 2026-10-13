//! Порт `lib/absent-path.mjs` (RFC 20 T34): FP-семейство 7 — «файла/модуля нет» о пути,
//! который есть в репозитории, но не в PR.
//!
//! Модель видит импорт или ссылку на файл, которого нет ни в диффе, ни в контексте чанка,
//! и выводит из этого «не существует». Здесь утверждение об отсутствии сверяется с деревом
//! репозитория детерминированно, после LLM: путь нашёлся в дереве → находка снимается как
//! известный FP (`findings::filter_false_positive_findings`, только LLM-находки).
//!
//! Снимается только утверждение «нет файла/модуля». «Символа `X` нет в `a.ts`» сюда не
//! попадает: путь в нём существует по условию, отсутствует символ.
//!
//! **Регексы без lookaround.** JS-эталон держит границы слова lookbehind/lookahead
//! (`(?<![\p{L}\p{N}_])`), crate `regex` их не умеет, а backtracking-движок (`fancy-regex`)
//! на тексте находки — вход от модели, т.е. от автора PR — открывал бы ReDoS. Все места,
//! где JS проверяет совпадение через `.test()`, переписаны на поглощающие эквиваленты:
//! для существования совпадения `(?<!X)` ≡ `(?:^|[^X])`, хвостовой `(?=Y)` ≡ `Y`. Внутренние
//! границы ADJACENT-регексов избыточны (рядом по построению стоят не-словесные символы
//! или начало среза) и опущены. В `PATH_TOKEN` позиция совпадения важна, поэтому хвостовой
//! `(?![\w/])` поглощается вне захвата `tok`, а следующий поиск идёт с конца `tok` —
//! как `lastIndex` у `matchAll`.
//!
//! `\s` JS (с флагом `u`) — не то же, что `\s` crate `regex` (U+0085 есть только в Rust,
//! U+FEFF — только в JS), поэтому пробельный класс выписан явно. `\w` без флага `u` — ASCII.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use crate::findings::{js_to_string, js_trim, js_truthy};

/// Содержимое JS-класса `\s` (флаг `u`): WhiteSpace + LineTerminator.
const WS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
/// Символы слова в границах эталона: `[\p{L}\p{N}_]`.
const WORD: &str = r"\p{L}\p{N}_";

/// Подставляет `S` → JS-`\s`, `LB` → поглощающий lookbehind, `RB` → поглощающий lookahead.
fn pat(src: &str) -> String {
    src.replace("{S}", &format!("[{WS}]"))
        .replace("{LB}", &format!("(?:^|[^{WORD}])"))
        .replace("{RB}", &format!("(?:$|[^{WORD}])"))
}

fn re(src: &str) -> Regex {
    Regex::new(&pat(src)).expect("absent_path: регекс эталона")
}

/// `ABSENT_PHRASE` без хвостовой `R`: утверждение об отсутствии самого файла.
const ABSENT_PHRASE: &str = concat!(
    r"(?:не{S}+существу\p{L}*|не{S}+найден\p{L}*|отсутству\p{L}*{S}+в{S}+(?:репозитори|проект|кодов|дерев)\p{L}*",
    r"|нет{S}+в{S}+(?:репозитори|проект|кодов|дерев)\p{L}*|не{S}+резолвит\p{L}*|не{S}+разреша\p{L}*",
    r"|does{S}+not{S}+exist|doesn['’]t{S}+exist|is{S}+(?:missing|absent)|not{S}+found|cannot{S}+be{S}+(?:found|resolved)",
    r"|can['’]t{S}+be{S}+(?:found|resolved)|could{S}+not{S}+be{S}+(?:found|resolved)",
    // Только с существительным «файл/модуль/путь»: «вызывает несуществующую функцию» — про символ.
    r"|несуществующ\p{L}*{S}+(?:файл|модул|пут)\p{L}*|non-?existent{S}+(?:file|module|path))",
);

/// `ABSENT_BEFORE_SRC` без границ: утверждение до пути.
const ABSENT_BEFORE_CORE: &str = concat!(
    r"(?:(?:несуществующ|не{S}+существующ|отсутствующ)\p{L}*(?:{S}+(?:файл|модул|пут|импорт)\p{L}*)?",
    r"|cannot{S}+find{S}+module|can['’]t{S}+resolve|unable{S}+to{S}+resolve|module{S}+not{S}+found",
    r"|unresolved{S}+import(?:{S}+of)?|non-?existent(?:{S}+(?:file|module|path))?|no{S}+such{S}+(?:file|module)|missing{S}+(?:file|module))",
);

/// `ABSENT_AFTER`: утверждение где угодно в предложении (`.test`).
static ABSENT_AFTER: LazyLock<Regex> =
    LazyLock::new(|| re(&format!("(?i){{LB}}{ABSENT_PHRASE}{{RB}}")));

/// `ABSENT_AFTER_ADJACENT`: утверждение вплотную после пути. Между путём и утверждением —
/// только связки, после — лишь «в репозитории/в PR» и конец предложения.
static ABSENT_AFTER_ADJACENT: LazyLock<Regex> = LazyLock::new(|| {
    re(&concat!(
        r#"(?i)^[{WS}'"`):,]*(?:[—–-]{S}*)?(?:(?:указывает|ведёт|ссылается|points|refers){S}+(?:на|to){S}+(?:an?{S}+)?)?"#,
        "{PHRASE}",
        r"(?:{S}+(?:в|во|in|from){S}+(?:the{S}+|этом{S}+)?(?:репозитори|проект|кодов\p{L}*{S}+баз|дерев|pr|дифф|repo|repository|codebase|project|tree|workspace)\p{L}*)?",
        r"{S}*(?:$|[—–,:;(!?]|\.(?:{S}|$)|-{S})",
    )
    .replace("{PHRASE}", ABSENT_PHRASE)
    .replace("{WS}", WS))
});

/// `ABSENT_BEFORE`: утверждение до пути где угодно в предложении (`.test`).
static ABSENT_BEFORE: LazyLock<Regex> =
    LazyLock::new(|| re(&format!("(?i){{LB}}{ABSENT_BEFORE_CORE}{{RB}}")));

/// `ABSENT_BEFORE_ADJACENT`: утверждение вплотную перед путём (конец среза `before`).
static ABSENT_BEFORE_ADJACENT: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!("(?i){{LB}}{ABSENT_BEFORE_CORE}[{WS}:'\"`(]*$"))
});

/// `RUNTIME_CONDITION`: отсутствие как условие рантайма — верная находка о коде.
static RUNTIME_CONDITION: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i){LB}(?:если|когда|в{S}+случае|при{S}+отсутствии|if|when|unless|in{S}+case){RB}",
        r"|ENOENT|existsSync|exists{S}*\(|FileNotFound",
    ))
});

/// `sentences`: разрыв по `!?\n`, по точке с пробелом/концом, по `; `.
static SENTENCE_SPLIT: LazyLock<Regex> =
    LazyLock::new(|| re(r"[!?\n]+|\.(?:{S}+|$)|;{S}+"));

/// `PATH_TOKEN`: кандидат в путь. Хвостовой `(?![\w/])` третьей альтернативы поглощается
/// вне захвата `tok` — см. шапку модуля.
static PATH_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?P<a>[A-Za-z0-9_@~.\-/]*[A-Za-z0-9_@~\-]/[A-Za-z0-9_.\-/]*[A-Za-z0-9_]|\.{1,2}/[A-Za-z0-9_.\-/]+)",
        r"|(?P<b>[A-Za-z0-9_\-]+(?:\.[A-Za-z0-9_\-]+)*\.[A-Za-z][A-Za-z0-9]{0,5})(?:$|[^A-Za-z0-9_/])",
    ))
    .expect("absent_path: PATH_TOKEN")
});

static CLEAN_LINE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r":[0-9]+(?:[-–][0-9]+)?$").expect("absent_path: line suffix"));
static VERSION_LIKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]+(?:\.[0-9]+)*$").expect("absent_path: version"));
/// `/^(.*)\.([cm]?)js(x?)$/` — `.` JS не матчит терминаторы строк.
static ESM_JS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\n\r\x{2028}\x{2029}]*)\.([cm]?)js(x?)$").expect("absent_path: esm")
});
static JS_FAMILY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.(?:[cm]?[jt]sx?|vue|svelte)$").expect("absent_path: js family"));
static PY_FAMILY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.py$").expect("absent_path: py family"));

const MAX_GAP_BEFORE: usize = 40;

/// Расширения, которые подставляет резолвер языка файла находки. Чужие не перебираем:
/// `./data` из `.ts` не находит `data.json`.
const JS_EXTS: [&str; 10] = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".svelte"];
const PY_EXTS: [&str; 1] = [".py"];
const PY_INDEX: [&str; 1] = ["/__init__.py"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Js,
    Py,
}

fn family_of(file: &str) -> Option<Family> {
    if JS_FAMILY.is_match(file) {
        Some(Family::Js)
    } else if PY_FAMILY.is_match(file) {
        Some(Family::Py)
    } else {
        None
    }
}

/// Варианты пути, в которые резолвер языка может превратить спецификатор.
fn variants_of(p: &str, fam: Option<Family>) -> Vec<String> {
    let mut out = vec![p.to_string()];
    match fam {
        None => {}
        Some(Family::Js) => {
            // ESM-TypeScript: `./x.js` в исходнике резолвится в `./x.ts`.
            if let Some(m) = ESM_JS.captures(p) {
                out.push(format!("{}.{}ts{}", &m[1], &m[2], &m[3]));
                out.push(format!("{}.tsx", &m[1]));
            }
            for e in JS_EXTS {
                out.push(format!("{p}{e}"));
            }
            for e in JS_EXTS {
                out.push(format!("{p}/index{e}"));
            }
        }
        Some(Family::Py) => {
            for e in PY_EXTS {
                out.push(format!("{p}{e}"));
            }
            for i in PY_INDEX {
                out.push(format!("{p}{i}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// node:path posix — normalize / dirname / join (байт-в-байт с Node)
// ---------------------------------------------------------------------------

/// `normalizeString` из `node:path` для разделителя `/`. Работает по байтам: `/` и `.` —
/// ASCII, а длины сравниваются только с 2 вместе с проверкой «последние два — точки»,
/// поэтому байтовые и UTF-16 длины дают одинаковый ответ.
fn normalize_string(path: &[u8], allow_above_root: bool) -> Vec<u8> {
    let mut res: Vec<u8> = Vec::new();
    let mut last_segment_length: isize = 0;
    let mut last_slash: isize = -1;
    let mut dots: i32 = 0;
    let mut code: u8 = 0;
    let n = path.len() as isize;
    let mut i: isize = 0;
    while i <= n {
        if i < n {
            code = path[i as usize];
        } else if code == b'/' {
            break;
        } else {
            code = b'/';
        }
        if code == b'/' {
            if last_slash == i - 1 || dots == 1 {
                // NOOP
            } else if dots == 2 {
                let tail_dd = res.len() >= 2 && res[res.len() - 1] == b'.' && res[res.len() - 2] == b'.';
                if res.len() < 2 || last_segment_length != 2 || !tail_dd {
                    if res.len() > 2 {
                        match res.iter().rposition(|&b| b == b'/') {
                            None => {
                                res.clear();
                                last_segment_length = 0;
                            }
                            Some(idx) => {
                                res.truncate(idx);
                                let ls = res.iter().rposition(|&b| b == b'/').map_or(-1, |x| x as isize);
                                last_segment_length = res.len() as isize - 1 - ls;
                            }
                        }
                        last_slash = i;
                        dots = 0;
                        i += 1;
                        continue;
                    } else if !res.is_empty() {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i;
                        dots = 0;
                        i += 1;
                        continue;
                    }
                }
                if allow_above_root {
                    if res.is_empty() {
                        res.extend_from_slice(b"..");
                    } else {
                        res.extend_from_slice(b"/..");
                    }
                    last_segment_length = 2;
                }
            } else {
                let seg = &path[(last_slash + 1) as usize..i as usize];
                if !res.is_empty() {
                    res.push(b'/');
                }
                res.extend_from_slice(seg);
                last_segment_length = i - last_slash - 1;
            }
            last_slash = i;
            dots = 0;
        } else if code == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
        i += 1;
    }
    res
}

/// `path.posix.normalize`.
pub fn posix_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let is_absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let norm = normalize_string(path.as_bytes(), !is_absolute);
    // Срез идёт по границам `/` исходной строки — UTF-8 не рвётся.
    let mut out = String::from_utf8(norm).unwrap_or_default();
    if out.is_empty() {
        if is_absolute {
            return "/".to_string();
        }
        return if trailing { "./".to_string() } else { ".".to_string() };
    }
    if trailing {
        out.push('/');
    }
    if is_absolute {
        format!("/{out}")
    } else {
        out
    }
}

/// `path.posix.dirname`.
pub fn posix_dirname(path: &str) -> String {
    let b = path.as_bytes();
    if b.is_empty() {
        return ".".to_string();
    }
    let has_root = b[0] == b'/';
    let mut end: Option<usize> = None;
    let mut matched_slash = true;
    let mut i = b.len() - 1;
    while i >= 1 {
        if b[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
        i -= 1;
    }
    match end {
        None => (if has_root { "/" } else { "." }).to_string(),
        Some(1) if has_root => "//".to_string(),
        Some(e) => path[..e].to_string(),
    }
}

/// `path.posix.join(a, b)`: пустые аргументы пропускаются.
fn posix_join(a: &str, b: &str) -> String {
    let joined = match (a.is_empty(), b.is_empty()) {
        (true, true) => return ".".to_string(),
        (false, true) => a.to_string(),
        (true, false) => b.to_string(),
        (false, false) => format!("{a}/{b}"),
    };
    posix_normalize(&joined)
}

// ---------------------------------------------------------------------------
// Индекс дерева
// ---------------------------------------------------------------------------

/// Индекс дерева репозитория (`{ set, files }` в JS): `files` — уникальные нормализованные
/// пути в порядке первого появления, по ним идёт поиск по суффиксу.
#[derive(Debug, Clone, Default)]
pub struct RepoPathIndex {
    set: HashSet<String>,
    files: Vec<String>,
}

impl RepoPathIndex {
    /// Индекс из уже нормализованного списка (`repoIndex.files` JS-стороны, мост бинаря).
    pub fn from_files<I: IntoIterator<Item = String>>(files: I) -> Self {
        let mut idx = RepoPathIndex::default();
        for f in files {
            if idx.set.insert(f.clone()) {
                idx.files.push(f);
            }
        }
        idx
    }

    pub fn files(&self) -> &[String] {
        &self.files
    }

    pub fn contains(&self, p: &str) -> bool {
        self.set.contains(p)
    }
}

/// `norm` из `buildRepoPathIndex`: `posix.normalize` и срез ведущего `./`.
fn norm_repo_path(p: &str) -> String {
    let n = posix_normalize(p);
    match n.strip_prefix("./") {
        Some(rest) => rest.to_string(),
        None => n,
    }
}

/// `buildRepoPathIndex`. `None`, если дерева нет или оно явно чужое: меньше половины
/// изменённых путей PR в нём есть (локальный `--dry-run --diff` из другого каталога).
pub fn build_repo_path_index(repo_files: Option<&[String]>, changed_paths: &[String]) -> Option<RepoPathIndex> {
    let files = repo_files?;
    if files.is_empty() {
        return None;
    }
    let idx = RepoPathIndex::from_files(files.iter().map(|p| norm_repo_path(p)));
    let changed: Vec<String> = changed_paths.iter().filter(|p| !p.is_empty()).map(|p| norm_repo_path(p)).collect();
    if !changed.is_empty() {
        let present = changed.iter().filter(|p| idx.contains(p)).count();
        if present * 2 < changed.len() {
            return None;
        }
    }
    Some(idx)
}

fn exists_exact(index: &RepoPathIndex, p: &str, fam: Option<Family>) -> Option<String> {
    if p.is_empty() || p.starts_with("..") {
        return None;
    }
    variants_of(p, fam).into_iter().find(|v| index.contains(v))
}

/// Совпадение по концу пути принимается, только если оно единственное: при двух
/// кандидатах «файл есть» — догадка.
fn exists_by_unique_suffix(index: &RepoPathIndex, p: &str, fam: Option<Family>) -> Option<String> {
    let suffixes: Vec<String> = variants_of(p, fam).iter().map(|v| format!("/{v}")).collect();
    let mut hit: Option<&String> = None;
    for f in &index.files {
        if !suffixes.iter().any(|s| f.ends_with(s.as_str())) {
            continue;
        }
        if hit.is_some() {
            return None;
        }
        hit = Some(f);
    }
    hit.cloned()
}

fn clean_token(tok: &str) -> String {
    let t = tok
        .trim_start_matches(['\'', '"', '`', '('])
        .trim_end_matches(['\'', '"', '`', ')', ',', ':']);
    CLEAN_LINE_SUFFIX.replace(t, "").into_owned()
}

fn is_path_like(t: &str) -> bool {
    let len: usize = t.chars().map(char::len_utf16).sum();
    if t.is_empty() || len < 3 || len > 300 {
        return false;
    }
    if t.contains("://") {
        return false;
    }
    !VERSION_LIKE.is_match(t)
}

/// `resolveMentionedPath`: относительный путь — от каталога файла находки; алиас `@/`,
/// `~/` и путь с «/» — от корня или по единственному совпадению конца; голое имя — только
/// в каталоге находки. Расширения — только языка файла находки.
pub fn resolve_mentioned_path(token: &str, finding_file: Option<&str>, index: &RepoPathIndex) -> Option<String> {
    let t = clean_token(token);
    if !is_path_like(&t) {
        return None;
    }
    let file = finding_file.filter(|f| !f.is_empty());
    let fam = family_of(file.unwrap_or(""));
    let dir = file.map_or_else(|| ".".to_string(), posix_dirname);
    if t.starts_with("./") || t.starts_with("../") {
        return exists_exact(index, &posix_normalize(&posix_join(&dir, &t)), fam);
    }
    let unaliased = t.strip_prefix("@/").or_else(|| t.strip_prefix("~/")).unwrap_or(&t);
    let bare = posix_normalize(unaliased.trim_start_matches('/'));
    if bare.contains('/') {
        return exists_exact(index, &bare, fam).or_else(|| exists_by_unique_suffix(index, &bare, fam));
    }
    exists_exact(index, &posix_normalize(&posix_join(&dir, &bare)), fam)
}

/// `sentences`: части без пустых (по JS-trim).
fn sentences(text: &str) -> Vec<&str> {
    SENTENCE_SPLIT.split(text).filter(|s| !js_trim(s).is_empty()).collect()
}

/// `s.slice(Math.max(0, idx - 40), idx)` по UTF-16. Если граница режет суррогатную пару,
/// JS оставляет одинокий суррогат в начале среза; он не буква и не участвует в
/// совпадении, поэтому срез без него эквивалентен.
fn gap_before(s: &str, end: usize) -> &str {
    let mut units = 0usize;
    let mut start = end;
    for (i, c) in s[..end].char_indices().rev() {
        let n = c.len_utf16();
        if units + n > MAX_GAP_BEFORE {
            break;
        }
        units += n;
        start = i;
    }
    &s[start..end]
}

/// JS `${v || ''}`.
fn js_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    }
}

/// `findAbsentButExistingPath`: путь из дерева, об отсутствии которого заявляет находка.
/// Утверждение и путь должны стоять в одном предложении вплотную.
pub fn find_absent_but_existing_path(finding: &Value, index: Option<&RepoPathIndex>) -> Option<String> {
    let index = index?;
    let text = format!("{}. {}", js_or_empty(finding.get("title")), js_or_empty(finding.get("body")));
    let file = match finding.get("file") {
        Some(f) if js_truthy(f) => Some(js_to_string(f)),
        _ => None,
    };
    for s in sentences(&text) {
        if RUNTIME_CONDITION.is_match(s) {
            continue;
        }
        if !ABSENT_AFTER.is_match(s) && !ABSENT_BEFORE.is_match(s) {
            continue;
        }
        let mut pos = 0usize;
        while pos < s.len() {
            let Some(caps) = PATH_TOKEN.captures_at(s, pos) else { break };
            let Some(tok) = caps.name("a").or_else(|| caps.name("b")) else { break };
            // matchAll: следующий поиск — с конца токена (не с конца поглощённого хвоста).
            pos = tok.end();
            let after = &s[tok.end()..];
            let before = gap_before(s, tok.start());
            let claimed = ABSENT_AFTER_ADJACENT.is_match(after) || ABSENT_BEFORE_ADJACENT.is_match(before);
            if !claimed {
                continue;
            }
            if let Some(hit) = resolve_mentioned_path(tok.as_str(), file.as_deref(), index) {
                return Some(hit);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// listRepoFiles
// ---------------------------------------------------------------------------

const LS_FILES_TIMEOUT: Duration = Duration::from_secs(10);
const LS_FILES_MAX_BUFFER: usize = 64 * 1024 * 1024;

/// `git -C <cwd> ls-files -z` с таймаутом 10 с и лимитом вывода 64 МБ (как `execFile`).
/// `Err` — любой сбой: нет git, не репозиторий (ненулевой код), таймаут, переполнение.
fn git_ls_files(cwd: &Path) -> Result<String, String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["ls-files", "-z"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn git: {e}"))?;
    let Some(mut out) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("spawn git: нет stdout".into());
    };
    let reader = thread::spawn(move || -> Result<Vec<u8>, String> {
        let mut acc = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            match out.read(&mut buf) {
                Ok(0) => return Ok(acc),
                Ok(n) => {
                    if acc.len() + n > LS_FILES_MAX_BUFFER {
                        return Err("git ls-files: stdout maxBuffer exceeded".into());
                    }
                    acc.extend_from_slice(&buf[..n]);
                }
                Err(e) => return Err(format!("git ls-files: {e}")),
            }
        }
    });
    // Переполнение: читатель бросает трубу, git получает SIGPIPE и выходит сам.
    let deadline = Instant::now() + LS_FILES_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("git ls-files: timeout".into());
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait git: {e}"));
            }
        }
    };
    let stdout = reader.join().map_err(|_| "git ls-files: reader panic".to_string())??;
    if !status.success() {
        return Err(format!("git ls-files exit {:?}", status.code()));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// `listRepoFiles` с подменяемым исполнителем (тесты). Fail-open: ошибка → `None`.
pub fn list_repo_files_with<F>(cwd: &Path, exec: F) -> Option<Vec<String>>
where
    F: FnOnce(&Path) -> Result<String, String>,
{
    let stdout = exec(cwd).ok()?;
    let files: Vec<String> = stdout.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect();
    if files.is_empty() {
        None
    } else {
        Some(files)
    }
}

/// `listRepoFiles`: файлы рабочего дерева (`git ls-files`). Не git, нет git, таймаут →
/// `None`, и фильтр семейства 7 в этом прогоне выключен.
pub fn list_repo_files(cwd: &Path) -> Option<Vec<String>> {
    list_repo_files_with(cwd, git_ls_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const TREE: [&str; 4] = ["src/a.ts", "src/helpers.ts", "src/lib/db/index.ts", "config/app.json"];

    fn idx() -> RepoPathIndex {
        build_repo_path_index(Some(&s(&TREE)), &s(&["src/a.ts"])).unwrap()
    }

    fn claim_in(index: &RepoPathIndex, body: &str, file: &str) -> Option<String> {
        find_absent_but_existing_path(&json!({ "title": "t", "body": body, "file": file }), Some(index))
    }

    fn claim(body: &str) -> Option<String> {
        claim_in(&idx(), body, "src/a.ts")
    }

    #[test]
    fn утверждение_об_отсутствии_существующего_пути() {
        assert_eq!(claim("Импорт `./helpers` указывает на несуществующий файл").as_deref(), Some("src/helpers.ts"));
        assert_eq!(claim("Файл `src/helpers.ts` не существует в репозитории").as_deref(), Some("src/helpers.ts"));
        assert_eq!(claim("Cannot find module '@/lib/db'").as_deref(), Some("src/lib/db/index.ts"));
        assert_eq!(claim("импорт несуществующего модуля `./helpers`").as_deref(), Some("src/helpers.ts"));
        assert_eq!(claim("helpers.ts не найден — сборка упадёт").as_deref(), Some("src/helpers.ts"));
        assert_eq!(
            claim("The file `config/app.json` does not exist in the repo.").as_deref(),
            Some("config/app.json")
        );
    }

    #[test]
    fn путь_которого_нет_в_дереве_не_снимаем() {
        assert_eq!(claim("Файл `src/missing.ts` не существует"), None);
        assert_eq!(claim("Cannot find module './nope'"), None);
    }

    #[test]
    fn отсутствие_как_условие_рантайма() {
        assert_eq!(claim("Если `config/app.json` не существует, readFileSync бросит ENOENT"), None);
        assert_eq!(claim("When `config/app.json` does not exist the loader crashes"), None);
    }

    #[test]
    fn отсутствие_символа_в_существующем_файле() {
        assert_eq!(claim("`src/helpers.ts` вызывает несуществующую функцию foo"), None);
        assert_eq!(claim("Экспорт `parse` не найден в `src/helpers.ts`"), None);
        assert_eq!(claim("валидация отсутствует в `src/helpers.ts`"), None);
    }

    #[test]
    fn не_найден_о_содержимом_существующего_файла() {
        let mut tree = s(&TREE);
        tree.extend(s(&["src/auth/guard.ts", "config/csp.json"]));
        let t = build_repo_path_index(Some(&tree), &s(&["src/a.ts"])).unwrap();
        let c = |b: &str| claim_in(&t, b, "src/a.ts");
        assert_eq!(c("`src/auth/guard.ts` не найден в списке провайдеров — эндпоинт без авторизации"), None);
        assert_eq!(c("`config/csp.json` is missing the frame-ancestors directive"), None);
        assert_eq!(c("`config/csp.json` не существует как политика для /admin"), None);
    }

    #[test]
    fn разрешение_не_шире_настоящего_резолвера() {
        let mut tree = s(&TREE);
        tree.extend(s(&["src/data.json", "src/esm.ts", "packages/x/lib/cache/index.ts", "apps/y/lib/cache/index.ts"]));
        let t = build_repo_path_index(Some(&tree), &s(&["src/a.ts"])).unwrap();
        let c = |b: &str| claim_in(&t, b, "src/a.ts");
        assert_eq!(c("Cannot find module './data'"), None, ".json без расширения TS не резолвит");
        assert_eq!(c("Cannot find module '@/lib/cache'"), None, "два кандидата по суффиксу");
        assert_eq!(c("Cannot find module './esm.js'").as_deref(), Some("src/esm.ts"), "ESM-TS: .js → .ts");
    }

    #[test]
    fn голое_имя_только_в_каталоге_находки() {
        let i = idx();
        assert_eq!(claim_in(&i, "helpers.ts не найден", "other/x.ts"), None);
        assert_eq!(resolve_mentioned_path("helpers.ts", Some("src/a.ts"), &i).as_deref(), Some("src/helpers.ts"));
        assert_eq!(resolve_mentioned_path("../src/helpers", Some("src/a.ts"), &i).as_deref(), Some("src/helpers.ts"));
        assert_eq!(resolve_mentioned_path("https://x.io/a.ts", Some("src/a.ts"), &i), None);
    }

    #[test]
    fn чужое_или_пустое_дерево_индекса_нет() {
        assert!(build_repo_path_index(Some(&s(&["x/y.ts"])), &s(&["a.ts", "b.ts"])).is_none());
        assert!(build_repo_path_index(None, &s(&["a.ts"])).is_none());
        assert!(build_repo_path_index(Some(&[]), &[]).is_none());
        assert!(build_repo_path_index(Some(&s(&["src/a.ts"])), &s(&["./src/a.ts"])).is_some());
        assert_eq!(
            find_absent_but_existing_path(&json!({ "body": "Файл `src/helpers.ts` не существует" }), None),
            None
        );
    }

    #[test]
    fn list_repo_files_fail_open() {
        let cwd = Path::new(".");
        assert_eq!(list_repo_files_with(cwd, |_| Err("not a git repo".into())), None);
        assert_eq!(list_repo_files_with(cwd, |_| Ok("a.ts\0b/c.ts\0".into())), Some(s(&["a.ts", "b/c.ts"])));
        assert_eq!(list_repo_files_with(cwd, |_| Ok(String::new())), None);
        // Каталог вне git — fail-open, не паника.
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(list_repo_files(tmp.path()), None);
        // Сам репозиторий — непустой список с этим файлом.
        let files = list_repo_files(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        assert!(files.iter().any(|f| f == "Cargo.toml"), "git -C rust: пути от cwd");
    }

    #[test]
    fn posix_normalize_как_node() {
        for (inp, out) in [
            ("", "."),
            ("./", "./"),
            ("a/./b/../c", "a/c"),
            ("../a", "../a"),
            ("a/../../b", "../b"),
            ("/../a", "/a"),
            ("a//b/", "a/b/"),
            ("src/../..", ".."),
            ("..", ".."),
            ("./src/a.ts", "src/a.ts"),
            ("ж/../я", "я"),
        ] {
            assert_eq!(posix_normalize(inp), out, "{inp}");
        }
        for (inp, out) in [("src/a.ts", "src"), ("a.ts", "."), ("/a", "/"), ("//a", "//"), ("a/b/", "a"), ("", ".")] {
            assert_eq!(posix_dirname(inp), out, "{inp}");
        }
    }

    #[test]
    fn путь_не_выходит_за_корень() {
        let i = idx();
        assert_eq!(resolve_mentioned_path("../../../etc/passwd", Some("src/a.ts"), &i), None);
        assert_eq!(resolve_mentioned_path("/etc/passwd", Some("src/a.ts"), &i), None);
    }

    #[test]
    fn граница_слова_по_unicode() {
        // «xне существует» — не утверждение (буква перед «не»), _и_ «не существует» в
        // начале среза after — утверждение.
        assert_eq!(claim("Файл `src/helpers.ts`xне существует"), None);
        assert_eq!(claim("`src/helpers.ts` — не существует").as_deref(), Some("src/helpers.ts"));
        // Длинный текст без совпадений — линейное время (без backtracking).
        let long = format!("{} не существует", "a/".repeat(20_000));
        assert_eq!(claim(&long), None);
    }

    /// Фикстура `fp-absent-path-outside-diff`: те же шаги, что `runFixtureCase` до кластеризации
    /// (parseDiff → buildRepoPathIndex → filterFalsePositiveFindings c вшитыми паттернами).
    #[test]
    fn фикстура_fp_absent_path_outside_diff() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
        let cases = crate::fixtures::load_fixtures(&root).unwrap();
        let case = cases.iter().find(|c| c.id == "fp-absent-path-outside-diff").expect("фикстура");
        let changed: Vec<String> = crate::diff::parse_diff(&case.diff).files.keys().cloned().collect();
        let index = build_repo_path_index(case.repo_files.as_deref(), &changed).expect("дерево фикстуры своё");
        let out = crate::findings::filter_false_positive_findings(
            &case.findings,
            &changed,
            crate::fp_patterns::default_fp_patterns(),
            Some(&index),
        );
        let titles: Vec<&str> = out.kept.iter().map(|&i| case.findings[i]["title"].as_str().unwrap()).collect();
        assert_eq!(titles, vec!["Импорт audit-log без файла", "Нет обработки отсутствующего шаблона"]);
        assert_eq!(case.expected["findingCount"], json!(titles.len()));
        assert_eq!(
            out.logs,
            vec![
                "filtered false finding (путь есть в репозитории: app/server/services/pricing.ts): Импорт несуществующего модуля pricing",
                "filtered false finding (путь есть в репозитории: app/server/routes/templates/index.ts): templates не найден",
            ]
        );
        // Без индекса семейство 7 выключено.
        let off = crate::findings::filter_false_positive_findings(
            &case.findings,
            &changed,
            crate::fp_patterns::default_fp_patterns(),
            None,
        );
        assert_eq!(off.kept.len(), 4);
    }

    #[test]
    fn prescan_и_blast_находки_дерево_не_перебивает() {
        let i = idx();
        let findings = vec![
            json!({ "file": "src/a.ts", "line": 1, "severity": "critical", "title": "Модуль не найден", "body": "Cannot find module './helpers'", "model": "m1" }),
            json!({ "file": "src/a.ts", "line": 2, "severity": "major", "title": "IDOR", "body": "нет проверки userId", "model": "m1" }),
            json!({ "file": "src/a.ts", "line": 1, "severity": "major", "title": "import/no-unresolved", "body": "Unable to resolve path to module './helpers'", "model": "eslint", "prescan": true }),
            json!({ "file": "src/a.ts", "line": 1, "severity": "major", "title": "blast", "body": "Cannot find module './helpers'", "source": "blast-radius" }),
        ];
        let out = crate::findings::filter_false_positive_findings(&findings, &s(&["src/a.ts"]), &[], Some(&i));
        assert_eq!(out.kept, vec![1, 2, 3]);
        assert!(out.logs[0].contains("src/helpers.ts"));
    }
}
