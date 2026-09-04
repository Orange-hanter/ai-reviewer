//! Порт `lib/fp-patterns.mjs` (RFC 04, задача T07): загрузка курируемых
//! FP-паттернов из JSONL и матчинг текста находки по ним.
//!
//! Паттерны — **данные потребителя**: их пишет человек в диалекте JS RegExp,
//! и движок их только компилирует. Поэтому здесь (и только здесь, плюс
//! `layout.sensitive` профиля в `actionability.rs`) используется `fancy-regex`
//! с backtracking: 2 из 51 паттерна стартового набора
//! (`e2e-in-diff`, `file-absent-in-diff`) используют lookahead `(?=…)`,
//! которого нет в crate `regex`. Код самого движка остаётся на `regex`.
//!
//! # Отличия семантики fancy-regex от JS RegExp без флагов `m`/`u`
//!
//! Паттерны компилируются как `new RegExp(src, 'i')` → `(?i)` + `src`.
//! Флаги `m`/`s`/`u` в JS не выставлены, в Rust — тоже (по умолчанию выключены),
//! но дефолты классов различаются:
//!
//! - `\s`: в JS без `u` это фиксированный класс, включающий **U+FEFF** (BOM) и
//!   НЕ включающий U+0085 (NEL); в Rust `\s` — Unicode `White_Space`, где
//!   наоборот: U+0085 входит, U+FEFF нет. Расхождение возможно только на тексте
//!   с BOM/NEL внутри находки — в курируемом наборе таких нет.
//! - `\b`: в JS без `u` граница слова считается по ASCII `[A-Za-z0-9_]`, поэтому
//!   между кириллической и латинской буквой граница ЕСТЬ; в Rust `\b` — юникодный,
//!   и там границы НЕТ. Затрагивает только 2 паттерна с `\b` (оба — латинские
//!   слова `file`/`no`/`missing`); практическая разница — «файлfile» и подобные
//!   склейки алфавитов, которых в реальных находках не встречается.
//! - `.`: в JS без `s` не совпадает с `\n`, `\r`, `U+2028`, `U+2029`; в Rust —
//!   только с `\n`. То есть Rust-`.` шире на `\r`/LS/PS. Тексты находок
//!   (`title` + `body`) приходят из JSON модели, `\r` в них не нормализуется —
//!   теоретически паттерн `[^.]{0,25}` может перепрыгнуть через CRLF там, где JS
//!   бы остановился. Считаем допустимым: FP-паттерн срабатывает шире, а не уже.
//! - `$`/`^`: без `m` обе реализации якорят на границы всего входа (в отличие от
//!   Perl, crate `regex` не делает исключения для завершающего `\n`) — совпадает.
//! - `(?i)` в Rust юникодный (`К` ≡ `к`), в JS без `u` — тоже (`i` работает по
//!   Unicode Simple Case Folding) — совпадает.
//!
//! Корпусное сравнение JS ↔ Rust на всех находках `fixtures/*/findings.json`
//! расхождений не даёт (см. отчёт задачи T07).

use std::path::Path;
use std::sync::OnceLock;

use fancy_regex::Regex;
use serde_json::Value;

/// Стартовый набор паттернов пакета, вшитый в бинарь (RFC 04, решение 5):
/// рядом с одиночным исполняемым файлом дерева пакета нет.
pub const DEFAULT_FP_PATTERNS: &str = include_str!("../../feedback/fp-patterns.jsonl");

/// Лимит длины исходника регекса (в кодовых единицах UTF-16 — как `String#length` в JS).
pub const MAX_FP_REGEX_LENGTH: usize = 512;

/// Скомпилированный FP-паттерн (зеркало `FpPattern` из `lib/fp-patterns.mjs`).
#[derive(Debug)]
pub struct FpPattern {
    pub id: String,
    /// Человеческое описание из JSONL. В JS оно теряется на парсинге; здесь
    /// сохраняем — по нему удобно объяснять, почему находка снята.
    pub description: String,
    pub regex: Regex,
    pub requires_path: Option<Regex>,
    pub match_changed_basename: bool,
}

/// Эвристика вложенных квантификаторов из JS: `/\(\?[^)]*[+*][^)]*\)[+*]/`.
fn nested_quantifier_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\(\?[^)]*[+*][^)]*\)[+*]").unwrap())
}

/// `isSafeRegexSource`: пустой/слишком длинный источник и очевидный
/// catastrophic-backtracking-паттерн отклоняются.
///
/// Длина считается в UTF-16 (`String#length` в JS), иначе кириллический паттерн
/// длиной 400 символов «весил» бы 800 и отсекался бы на другом пороге.
pub fn is_safe_regex_source(source: &str) -> bool {
    if source.is_empty() {
        return false;
    }
    if source.encode_utf16().count() > MAX_FP_REGEX_LENGTH {
        return false;
    }
    !nested_quantifier_re().is_match(source)
}

/// Компиляция исходника как `new RegExp(src, 'i')`.
fn compile_ci(source: &str) -> Option<Regex> {
    Regex::new(&format!("(?i){source}")).ok()
}

/// `parseFpPatternsText`: JSONL, по строке на паттерн; пустые строки и `#`-комментарии
/// пропускаются, невалидные строки (битый JSON, нет `id`/`regex`, небезопасный или
/// некомпилируемый регекс) — молча тоже (курируемый файл правится руками, один
/// сломанный паттерн не должен ронять прогон).
pub fn parse_fp_patterns_text(text: &str) -> Vec<FpPattern> {
    let mut patterns = Vec::new();
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        // JS: !row?.id || !row?.regex — falsy отсекается (пустая строка, 0, null).
        let id = row.get("id").and_then(Value::as_str).unwrap_or("");
        let src = row.get("regex").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() || src.is_empty() || !is_safe_regex_source(src) {
            continue;
        }
        let req = row.get("requiresPath").and_then(Value::as_str).unwrap_or("");
        if !req.is_empty() && !is_safe_regex_source(req) {
            continue;
        }
        let Some(regex) = compile_ci(src) else { continue };
        let requires_path = if req.is_empty() {
            None
        } else {
            match compile_ci(req) {
                Some(r) => Some(r),
                None => continue,
            }
        };
        patterns.push(FpPattern {
            id: id.to_string(),
            description: row
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            regex,
            requires_path,
            match_changed_basename: row
                .get("matchChangedBasename")
                .map(crate::findings::js_truthy)
                .unwrap_or(false),
        });
    }
    patterns
}

/// `loadFpPatternsSync`: читаем указанный файл; если его нет или он не дал ни
/// одного паттерна — откатываемся на вшитый стартовый набор пакета (в JS роль
/// фолбэка играет `DEFAULT_FP_PATTERNS_PATH` на диске — тот же самый файл).
pub fn load_fp_patterns(path: &Path) -> Vec<FpPattern> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let parsed = parse_fp_patterns_text(&text);
        if !parsed.is_empty() {
            return parsed;
        }
    }
    parse_fp_patterns_text(DEFAULT_FP_PATTERNS)
}

/// Вшитый стартовый набор, разобранный один раз за процесс.
///
/// Это не кеш загрузки (у `load_fp_patterns` кеша нет и `resetFpPatternsCache`
/// здесь не нужен): содержимое зафиксировано на компиляции и измениться не может.
pub fn default_fp_patterns() -> &'static [FpPattern] {
    static PATTERNS: OnceLock<Vec<FpPattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| parse_fp_patterns_text(DEFAULT_FP_PATTERNS))
}

/// `matchFpPatterns`: id первого сработавшего паттерна либо `None`.
///
/// Паттерн срабатывает, когда совпал сам регекс И (если задан) `requiresPath`
/// хотя бы по одному изменённому пути И (если поднят `matchChangedBasename`)
/// текст содержит basename хотя бы одного изменённого файла.
///
/// Ошибка исполнения fancy-regex (backtrack limit на патологическом входе)
/// трактуется как «не совпало»: FP-фильтр обязан быть fail-open — иначе
/// сбой на одном паттерне снял бы фильтрацию всей находки не в ту сторону.
pub fn match_fp_patterns<'a>(
    text: &str,
    changed_paths: &[String],
    patterns: &'a [FpPattern],
) -> Option<&'a str> {
    for p in patterns {
        if !p.regex.is_match(text).unwrap_or(false) {
            continue;
        }
        if let Some(req) = &p.requires_path {
            if !changed_paths.iter().any(|path| req.is_match(path).unwrap_or(false)) {
                continue;
            }
        }
        if p.match_changed_basename {
            let hit = changed_paths.iter().any(|path| {
                let b = path.rsplit('/').next().unwrap_or("");
                !b.is_empty() && text.contains(b)
            });
            if !hit {
                continue;
            }
        }
        return Some(&p.id);
    }
    None
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_set_compiles_fully() {
        // Число строк в JSONL (без пустых и комментариев) должно совпасть с числом
        // скомпилированных паттернов: молча выпавший паттерн = тихая потеря фильтра.
        let rows = DEFAULT_FP_PATTERNS
            .split('\n')
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with('#')
            })
            .count();
        let patterns = default_fp_patterns();
        assert_eq!(rows, 51, "ожидается 51 курируемый паттерн в feedback/fp-patterns.jsonl");
        assert_eq!(
            patterns.len(),
            rows,
            "часть паттернов не скомпилировалась fancy-regex"
        );
        // id уникальны
        let mut ids: Vec<&str> = patterns.iter().map(|p| p.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "дубликаты id в стартовом наборе");
        // описание у каждого есть — по нему объясняем снятие находки
        assert!(patterns.iter().all(|p| !p.description.is_empty()));
    }

    #[test]
    fn lookahead_patterns_supported() {
        // Ровно те 2, из-за которых FP-фильтр раньше жил только в JS.
        let p = default_fp_patterns();
        assert!(p.iter().any(|x| x.id == "e2e-in-diff"));
        assert!(p.iter().any(|x| x.id == "file-absent-in-diff"));
        assert!(Regex::new("(?i)(?=abc)abc").unwrap().is_match("abc").unwrap());
    }

    #[test]
    fn is_safe_regex_source_rejects() {
        assert!(!is_safe_regex_source(""));
        assert!(is_safe_regex_source("abc"));
        assert!(!is_safe_regex_source(&"a".repeat(MAX_FP_REGEX_LENGTH + 1)));
        assert!(is_safe_regex_source(&"a".repeat(MAX_FP_REGEX_LENGTH)));
        // кириллица считается по UTF-16, а не по байтам
        assert!(is_safe_regex_source(&"я".repeat(MAX_FP_REGEX_LENGTH)));
        assert!(!is_safe_regex_source(&"я".repeat(MAX_FP_REGEX_LENGTH + 1)));
        // вложенный квантификатор
        assert!(!is_safe_regex_source("(?:a+)+"));
        assert!(!is_safe_regex_source("(?:a*)*"));
        assert!(is_safe_regex_source("(?:ab)+"));
    }

    #[test]
    fn parse_skips_invalid_lines() {
        let text = concat!(
            "# комментарий\n",
            "\n",
            "не json\n",
            "{\"id\":\"no-regex\"}\n",
            "{\"regex\":\"x\"}\n",
            "{\"id\":\"unsafe\",\"regex\":\"(?:a+)+\"}\n",
            "{\"id\":\"bad-syntax\",\"regex\":\"(\"}\n",
            "{\"id\":\"bad-req\",\"regex\":\"ok\",\"requiresPath\":\"(?:a+)+\"}\n",
            "{\"id\":\"ok\",\"regex\":\"нет\\\\s+e2e\",\"requiresPath\":\"\\\\.spec\\\\.ts$\",\"matchChangedBasename\":true}\n",
        );
        let p = parse_fp_patterns_text(text);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].id, "ok");
        assert!(p[0].requires_path.is_some());
        assert!(p[0].match_changed_basename);
    }

    #[test]
    fn match_respects_requires_path_and_basename() {
        let p = parse_fp_patterns_text(concat!(
            "{\"id\":\"scoped\",\"regex\":\"ложь\",\"requiresPath\":\"^ops/\"}\n",
            "{\"id\":\"basename\",\"regex\":\"файл\",\"matchChangedBasename\":true}\n",
        ));
        assert_eq!(match_fp_patterns("ложь", &paths(&["ops/x.sh"]), &p), Some("scoped"));
        assert_eq!(match_fp_patterns("ложь", &paths(&["app/x.ts"]), &p), None);
        assert_eq!(
            match_fp_patterns("файл foo.ts не в диффе", &paths(&["a/foo.ts"]), &p),
            Some("basename")
        );
        assert_eq!(match_fp_patterns("файл не в диффе", &paths(&["a/foo.ts"]), &p), None);
        // регистронезависимость (флаг i)
        assert_eq!(match_fp_patterns("ЛОЖЬ", &paths(&["ops/x.sh"]), &p), Some("scoped"));
    }

    #[test]
    fn load_falls_back_to_embedded() {
        let missing = Path::new("/nonexistent/fp-patterns.jsonl");
        assert_eq!(load_fp_patterns(missing).len(), default_fp_patterns().len());
    }

    #[test]
    fn flag_semantics_matches_js() {
        // $ без m якорит на конец входа, а не перед завершающим \n (как в JS).
        assert!(!Regex::new("(?i)abc$").unwrap().is_match("abc\n").unwrap());
        // . без s не совпадает с \n (общее с JS).
        assert!(!Regex::new("(?i)a.b").unwrap().is_match("a\nb").unwrap());
        // Задокументированное расхождение: JS-`.` не берёт \r, Rust-`.` берёт.
        assert!(Regex::new("(?i)a.b").unwrap().is_match("a\rb").unwrap());
        // Задокументированное расхождение по \b: в JS (ASCII-класс слова) между
        // «й» и «f» граница есть, в Rust (юникодный \b) — нет.
        assert!(!Regex::new(r"(?i)\bfile\b").unwrap().is_match("файлfile").unwrap());
        assert!(Regex::new(r"(?i)\bfile\b").unwrap().is_match("a file b").unwrap());
        // Задокументированное расхождение по \s: JS включает U+FEFF и не включает
        // U+0085; в Rust ровно наоборот.
        assert!(!Regex::new(r"(?i)a\sb").unwrap().is_match("a\u{FEFF}b").unwrap());
        assert!(Regex::new(r"(?i)a\sb").unwrap().is_match("a\u{0085}b").unwrap());
        // `^` без m не совпадает после перевода строки — как в JS.
        assert!(!Regex::new("(?i)^abc").unwrap().is_match("x\nabc").unwrap());
    }
}
