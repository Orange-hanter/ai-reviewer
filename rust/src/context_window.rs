//! Порт lib/context-window.mjs: окна контекста вокруг строк-кандидатов (verifier) и
//! вокруг изменённых строк (advisors и жюри, RFC 07).
//!
//! Оба читателя пользуются одной функцией окон (`window_file_entry`), но центрируются на
//! разном: verifier знает строку находки, advisors — только то, какие строки трогает PR.
//! Вокруг каждой строки берётся ±margin строк, пересекающиеся и соприкасающиеся окна
//! сливаются, пропуски заменяются маркером `… (строки N–M пропущены)` — в голове и в
//! хвосте тоже: без него реальные номера строк невосстановимы.
//!
//! Записи-маркеры добавленных файлов (`added_file_marker` в diff.rs — заголовок без тела в
//! кавычках) проходят насквозь: резать там нечего.
//!
//! Числа — f64 с JS-семантикой: margin и номера строк приходят из конфига и из ответа
//! модели как JS-числа (дробные, NaN, Infinity), и поведение на них обязано совпадать с
//! эталоном — `f64::max`/`min` NaN глотают, `Math.max`/`Math.min` его распространяют.
//! Длины — в UTF-16 code units (`String.length`).

use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value};

use crate::diff::u16_len;
use crate::findings::js_to_number;
use crate::structure_evidence::js_number_to_string;

/// `Math.max(a, b)`: NaN распространяется.
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

/// `Math.min(a, b)`: NaN распространяется.
fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

/// Индекс `Array.prototype.slice`: ToIntegerOrInfinity (усечение, NaN → 0), отрицательный —
/// от конца, затем зажим в [0, len].
fn js_slice_index(x: f64, len: usize) -> usize {
    let t = if x.is_nan() { 0.0 } else { x.trunc() };
    let len_f = len as f64;
    let v = if t < 0.0 { js_max(len_f + t, 0.0) } else { js_min(t, len_f) };
    v as usize
}

/// Заголовок + тело записи contentMap; None — запись не в формате «### path ```body```».
/// Эквивалент `/^(### [^\n]*)\n```\n([\s\S]*)\n```$/` (без флага m: `$` — конец строки).
fn split_entry(entry: &str) -> Option<(&str, &str)> {
    let nl = entry.find('\n')?;
    let head = &entry[..nl];
    if !head.starts_with("### ") {
        return None;
    }
    let after = entry[nl + 1..].strip_prefix("```\n")?;
    let body = after.strip_suffix("\n```")?;
    Some((head, body))
}

/// Слить пересекающиеся и соприкасающиеся интервалы (вход не обязан быть отсортирован).
/// `s <= last.1 + 1` — соприкасающиеся тоже сливаем: маркер «пропущено 0 строк» был бы враньём.
fn merge_ranges(ranges: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let mut sorted = ranges;
    // Компаратор JS `a[0] - b[0]`; NaN здесь не бывает (отфильтрован раньше), sort стабилен.
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (s, e) in sorted {
        match out.last_mut() {
            Some(last) if s <= last.1 + 1.0 => last.1 = js_max(last.1, e),
            _ => out.push((s, e)),
        }
    }
    out
}

fn skip_marker(from: f64, to: f64) -> String {
    format!(
        "… (строки {}–{} пропущены)",
        js_number_to_string(from),
        js_number_to_string(to)
    )
}

/// `windowFileEntry`: вырезать из записи contentMap окна ±margin вокруг строк `lines`.
/// Возвращает запись в том же формате (заголовок + тройные кавычки).
///
/// Запись возвращается БЕЗ изменений, когда: формат не распознан (маркер добавленного
/// файла), список строк пуст (нечего центрировать — отдаём файл целиком) или окна
/// покрывают файл целиком (резать нечего).
///
/// `lines` — уже `Number(x)` от входа; нечисловые/неположительные отбрасываются, как в JS.
pub fn window_file_entry(entry: &str, lines: &[f64], margin: f64) -> String {
    let Some((head, body)) = split_entry(entry) else {
        return entry.to_string();
    };
    let centers: Vec<f64> = lines.iter().copied().filter(|n| n.is_finite() && *n > 0.0).collect();
    if centers.is_empty() {
        return entry.to_string();
    }

    let body_lines: Vec<&str> = body.split('\n').collect();
    let total = body_lines.len() as f64;
    let windows = merge_ranges(
        centers
            .iter()
            .map(|n| (js_max(1.0, n - margin), js_min(total, n + margin)))
            .filter(|(s, e)| s <= e && *s <= total)
            .collect(),
    );
    if windows.is_empty() {
        return entry.to_string();
    }
    if windows.len() == 1 && windows[0].0 == 1.0 && windows[0].1 == total {
        return entry.to_string();
    }

    let mut out: Vec<String> = Vec::new();
    let mut prev = 0.0f64;
    for (s, e) in windows {
        if s > prev + 1.0 {
            out.push(skip_marker(prev + 1.0, s - 1.0));
        }
        let from = js_slice_index(s - 1.0, body_lines.len());
        let to = js_slice_index(e, body_lines.len());
        if from < to {
            out.extend(body_lines[from..to].iter().map(|l| l.to_string()));
        }
        prev = e;
    }
    if prev < total {
        out.push(skip_marker(prev + 1.0, total));
    }
    format!("{head}\n```\n{}\n```", out.join("\n"))
}

/// Результат `windowChunkFileContents`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkWindows {
    /// Карта контекста для чанков: оконные записи вместо полных.
    pub map: IndexMap<String, String>,
    /// Пути, чья запись реально урезана окнами (порядок — карты контекста).
    pub windowed: IndexSet<String>,
    /// Сумма длин записей до и после (UTF-16 code units).
    pub before: usize,
    pub after: usize,
}

/// `validLines` записи parseDiff как JS-числа; нет записи/поля — пусто.
fn valid_lines_of(files: Option<&Map<String, Value>>, path: &str) -> Vec<f64> {
    let Some(Value::Array(arr)) = files.and_then(|f| f.get(path)).and_then(|r| r.get("validLines"))
    else {
        return Vec::new();
    };
    arr.iter().map(|v| js_to_number(v).unwrap_or(f64::NAN)).collect()
}

/// `windowChunkFileContents`: контекст изменённых файлов для advisors и жюри (RFC 07) —
/// в каждом файле окна ±margin вокруг строк, которые PR трогает (`validLines` из
/// `parse_diff`), а не файл целиком.
///
/// Файл короче `min_chars` (по записи целиком, как и before/after) отдаётся целиком.
///
/// `margin <= 0` (и NaN) — это ВЫКЛЮЧЕНО: карта возвращается побайтово прежней, `windowed`
/// пуст, `after == before`. Это откат решения одной переменной (`REVIEW_CONTEXT_MARGIN=0`).
/// В JS возвращается та же Map; здесь — её копия с теми же записями в том же порядке.
pub fn window_chunk_file_contents(
    content_map: &IndexMap<String, String>,
    files: Option<&Map<String, Value>>,
    margin: f64,
    min_chars: f64,
) -> ChunkWindows {
    let before: usize = content_map.values().map(|e| u16_len(e)).sum();
    // `!(margin > 0)`: NaN тоже выключает окна.
    if margin.is_nan() || margin <= 0.0 {
        return ChunkWindows {
            map: content_map.clone(),
            windowed: IndexSet::new(),
            before,
            after: before,
        };
    }

    let mut map = IndexMap::new();
    let mut windowed = IndexSet::new();
    let mut after = 0usize;
    for (path, text) in content_map {
        let lines = if (u16_len(text) as f64) > min_chars {
            valid_lines_of(files, path)
        } else {
            Vec::new()
        };
        let out = if lines.is_empty() {
            text.clone()
        } else {
            window_file_entry(text, &lines, margin)
        };
        if &out != text {
            windowed.insert(path.clone());
        }
        after += u16_len(&out);
        map.insert(path.clone(), out);
    }
    ChunkWindows { map, windowed, before, after }
}

/// Результат `buildVerifierFileContents`.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifierContents {
    pub text: String,
    pub files: usize,
    pub before: usize,
    pub after: usize,
}

/// `buildVerifierFileContents`: контекст файлов для verifier — только файлы кандидатов,
/// и в них только окна ±margin вокруг строк кандидатов.
///
/// Файл, у которого ХОТЬ ОДИН кандидат без строки (`Number(line)` не конечно или ≤ 0),
/// отдаётся целиком: центрировать окно не на чем, а выкинуть область находки без строки —
/// лишить verifier'а возможности её подтвердить.
///
/// `candidates` — находки как JSON (`file`, `line`); кандидат без непустой строки `file`
/// пропускается (в JS нестроковый ключ всё равно не совпал бы ни с одним путём карты).
pub fn build_verifier_file_contents(
    content_map: &IndexMap<String, String>,
    candidates: &[Value],
    margin: f64,
) -> VerifierContents {
    // path → (строки, full)
    let mut by_file: IndexMap<&str, (Vec<f64>, bool)> = IndexMap::new();
    for c in candidates {
        let Some(path) = c.get("file").and_then(Value::as_str).filter(|p| !p.is_empty()) else {
            continue;
        };
        let rec = by_file.entry(path).or_insert_with(|| (Vec::new(), false));
        match c.get("line").and_then(js_to_number) {
            Some(n) if n.is_finite() && n > 0.0 => rec.0.push(n),
            _ => rec.1 = true,
        }
    }

    let mut original: Vec<&str> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for (path, entry) in content_map {
        let Some((lines, full)) = by_file.get(path.as_str()) else {
            continue;
        };
        original.push(entry);
        kept.push(if *full { entry.clone() } else { window_file_entry(entry, lines, margin) });
    }
    let text = kept.join("\n");
    // `before` — ровно так же, как собирался бы полный контекст (join('\n') по тем же записям).
    let before = u16_len(&original.join("\n"));
    let after = u16_len(&text);
    VerifierContents { text, files: kept.len(), before, after }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Запись contentMap в формате readChangedFiles: `### path` + тело в тройных кавычках.
    fn mk_entry(path: &str, n: usize) -> String {
        let body: Vec<String> = (1..=n).map(|i| format!("строка {i}")).collect();
        format!("### {path}\n```\n{}\n```", body.join("\n"))
    }

    /// Тело записи построчно (без заголовка и кавычек).
    fn entry_body(entry: &str) -> Vec<String> {
        let lines: Vec<&str> = entry.split('\n').collect();
        lines[2..lines.len() - 1].iter().map(|s| s.to_string()).collect()
    }

    fn markers(body: &[String]) -> Vec<String> {
        body.iter().filter(|l| l.starts_with("… (строки")).cloned().collect()
    }

    fn mk_files(spec: &[(&str, &[u64])]) -> Map<String, Value> {
        spec.iter()
            .map(|(p, lines)| (p.to_string(), json!({ "validLines": lines })))
            .collect()
    }

    fn cmap(entries: &[(&str, String)]) -> IndexMap<String, String> {
        entries.iter().map(|(p, e)| (p.to_string(), e.clone())).collect()
    }

    const ADDED_MARKER: &str =
        "### new.ts\n(новый файл — его полное содержимое целиком приведено в диффе ниже)";

    #[test]
    fn одна_строка_окно_и_маркеры() {
        let out = window_file_entry(&mk_entry("a.ts", 300), &[150.0], 10.0);
        let body = entry_body(&out);
        assert_eq!(body[0], "… (строки 1–139 пропущены)");
        assert_eq!(body[1], "строка 140");
        assert_eq!(body[body.len() - 2], "строка 160");
        assert_eq!(body[body.len() - 1], "… (строки 161–300 пропущены)");
        assert!(out.starts_with("### a.ts\n```\n") && out.ends_with("\n```"));
    }

    #[test]
    fn окна_сливаются_и_разносятся() {
        let merged = entry_body(&window_file_entry(&mk_entry("a.ts", 300), &[100.0, 110.0], 10.0));
        assert_eq!(markers(&merged).len(), 2, "только голова и хвост");
        assert_eq!(merged[1..3], ["строка 90".to_string(), "строка 91".to_string()]);

        let split = entry_body(&window_file_entry(&mk_entry("a.ts", 300), &[250.0, 50.0], 10.0));
        assert_eq!(
            markers(&split),
            [
                "… (строки 1–39 пропущены)",
                "… (строки 61–239 пропущены)",
                "… (строки 261–300 пропущены)",
            ]
        );
    }

    #[test]
    fn соприкасающиеся_окна_сливаются() {
        // [40,60] и [61,81] — без маркера «пропущено 0 строк» между ними.
        let body = entry_body(&window_file_entry(&mk_entry("a.ts", 300), &[50.0, 71.0], 10.0));
        assert_eq!(markers(&body), ["… (строки 1–39 пропущены)", "… (строки 82–300 пропущены)"]);
    }

    #[test]
    fn маркер_добавленного_и_целый_файл_без_изменений() {
        assert_eq!(window_file_entry(ADDED_MARKER, &[3.0], 80.0), ADDED_MARKER);
        let small = mk_entry("a.ts", 20);
        assert_eq!(window_file_entry(&small, &[10.0], 80.0), small, "окно покрывает файл");
        assert_eq!(window_file_entry(&small, &[], 80.0), small, "нет строк — целиком");
        assert_eq!(
            window_file_entry(&small, &[f64::NAN, -1.0, 0.0, f64::INFINITY], 80.0),
            small,
            "нечисловые и неположительные центры отброшены"
        );
    }

    #[test]
    fn строка_за_концом_файла_и_nan_margin() {
        let e = mk_entry("a.ts", 20);
        // n − margin > total — окно отфильтровано, резать нечего.
        assert_eq!(window_file_entry(&e, &[500.0], 10.0), e);
        // Math.max(1, NaN) = NaN → окно отфильтровано; f64::max вернул бы 1 и разрезал.
        assert_eq!(window_file_entry(&e, &[5.0], f64::NAN), e);
    }

    #[test]
    fn дробные_центры_как_в_js() {
        // Эталон: node -e "windowFileEntry(mkEntry('a.ts',30),[10.5],{margin:2})".
        let out = window_file_entry(&mk_entry("a.ts", 30), &[10.5], 2.0);
        let body = entry_body(&out);
        assert_eq!(body[0], "… (строки 1–7.5 пропущены)");
        assert_eq!(body[1], "строка 8");
        assert_eq!(body[body.len() - 2], "строка 12");
        assert_eq!(body[body.len() - 1], "… (строки 13.5–30 пропущены)");
    }

    #[test]
    fn чанки_окна_вокруг_изменённых_строк() {
        let map = cmap(&[("a.ts", mk_entry("a.ts", 300))]);
        let files = mk_files(&[("a.ts", &[50, 51, 250])]);
        let r = window_chunk_file_contents(&map, Some(&files), 10.0, 0.0);
        assert_eq!(
            markers(&entry_body(&r.map["a.ts"])),
            [
                "… (строки 1–39 пропущены)",
                "… (строки 62–239 пропущены)",
                "… (строки 261–300 пропущены)",
            ]
        );
        assert_eq!(r.windowed.iter().collect::<Vec<_>>(), ["a.ts"]);
        assert!(r.after < r.before);
    }

    #[test]
    fn чанки_файл_короче_min_chars_целиком() {
        let small = mk_entry("small.ts", 30);
        let big = mk_entry("big.ts", 300);
        let map = cmap(&[("small.ts", small.clone()), ("big.ts", big.clone())]);
        let files = mk_files(&[("small.ts", &[15]), ("big.ts", &[150])]);
        let r = window_chunk_file_contents(&map, Some(&files), 5.0, u16_len(&small) as f64);
        assert_eq!(r.map["small.ts"], small, "ровно на пороге — целиком (порог строгий)");
        assert_ne!(r.map["big.ts"], big);
        assert_eq!(r.windowed.iter().collect::<Vec<_>>(), ["big.ts"]);
    }

    #[test]
    fn чанки_margin_ноль_откат_на_целые_файлы() {
        let map = cmap(&[("a.ts", mk_entry("a.ts", 300)), ("b.ts", mk_entry("b.ts", 400))]);
        let files = mk_files(&[("a.ts", &[50, 250]), ("b.ts", &[10])]);
        for margin in [0.0, -5.0, f64::NAN] {
            let r = window_chunk_file_contents(&map, Some(&files), margin, 0.0);
            assert_eq!(r.map, map, "margin={margin}: карта побайтово прежняя");
            assert_eq!(
                r.map.keys().collect::<Vec<_>>(),
                map.keys().collect::<Vec<_>>(),
                "порядок тот же"
            );
            assert!(r.windowed.is_empty());
            assert_eq!(r.after, r.before);
        }
    }

    #[test]
    fn чанки_review_context_margin_0_из_конфига() {
        // Критерий приёмки T35: `REVIEW_CONTEXT_MARGIN=0` — откат на целые файлы.
        use crate::config::{load_review_config_parts, normalize_profile_facts};
        let profile = normalize_profile_facts(&json!({}), "/pkg", "(default)");
        let bits = json!({ "strict": false, "appDir": null, "paths": profile.get("paths") });
        let limits_of = |pairs: &[(&str, &str)]| {
            let env: Map<String, Value> =
                pairs.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
            load_review_config_parts(&env, &bits)["limits"].clone()
        };
        let map = cmap(&[("a.ts", mk_entry("a.ts", 3000))]);
        let files = mk_files(&[("a.ts", &[1500])]);
        let run = |limits: &Value| {
            window_chunk_file_contents(
                &map,
                Some(&files),
                limits["contextMargin"].as_f64().unwrap(),
                limits["contextWindowMinChars"].as_f64().unwrap(),
            )
        };

        let on = run(&limits_of(&[]));
        assert_eq!(on.windowed.len(), 1, "по умолчанию (±40) большой файл режется");
        let off = run(&limits_of(&[("REVIEW_CONTEXT_MARGIN", "0")]));
        assert_eq!(off.map, map, "REVIEW_CONTEXT_MARGIN=0 — файлы целиком");
        assert!(off.windowed.is_empty());
    }

    #[test]
    fn чанки_маркер_добавленного_и_файл_без_строк() {
        let untouched = mk_entry("u.ts", 300);
        let map = cmap(&[("new.ts", ADDED_MARKER.to_string()), ("u.ts", untouched.clone())]);
        let files = mk_files(&[("new.ts", &[1, 2, 3])]);
        let r = window_chunk_file_contents(&map, Some(&files), 10.0, 0.0);
        assert_eq!(r.map["new.ts"], ADDED_MARKER);
        assert_eq!(r.map["u.ts"], untouched);
        assert!(r.windowed.is_empty());
        // Без карты parseDiff центров нет вовсе.
        let r = window_chunk_file_contents(&map, None, 10.0, 0.0);
        assert!(r.windowed.is_empty());
    }

    #[test]
    fn verifier_только_файлы_кандидатов_и_окна() {
        let map = cmap(&[
            ("a.ts", mk_entry("a.ts", 300)),
            ("b.ts", mk_entry("b.ts", 300)),
            ("c.ts", mk_entry("c.ts", 300)),
        ]);
        let cands = vec![
            json!({ "file": "a.ts", "line": 150 }),
            json!({ "file": "b.ts", "line": "20" }),
            json!({ "file": "b.ts" }),
            json!({ "file": "", "line": 5 }),
            json!({ "line": 5 }),
        ];
        let r = build_verifier_file_contents(&map, &cands, 10.0);
        assert_eq!(r.files, 2, "c.ts без кандидатов не едет");
        let expected_a = window_file_entry(&map["a.ts"], &[150.0], 10.0);
        assert_eq!(r.text, format!("{expected_a}\n{}", map["b.ts"]), "b.ts: кандидат без строки — целиком");
        assert_eq!(r.before, u16_len(&format!("{}\n{}", map["a.ts"], map["b.ts"])));
        assert_eq!(r.after, u16_len(&r.text));
    }

    #[test]
    fn verifier_line_как_number_в_js() {
        let map = cmap(&[("a.ts", mk_entry("a.ts", 300))]);
        // Number(null) = 0 → не строка → файл целиком.
        let r = build_verifier_file_contents(&map, &[json!({ "file": "a.ts", "line": null })], 10.0);
        assert_eq!(r.text, map["a.ts"]);
        // Number([150]) = 150.
        let r = build_verifier_file_contents(&map, &[json!({ "file": "a.ts", "line": [150] })], 10.0);
        assert_eq!(r.text, window_file_entry(&map["a.ts"], &[150.0], 10.0));
        // Нет кандидатов — пусто.
        let r = build_verifier_file_contents(&map, &[], 10.0);
        assert_eq!((r.text.as_str(), r.files, r.before, r.after), ("", 0, 0, 0));
    }

    #[test]
    fn split_entry_граничные() {
        assert_eq!(split_entry("### a\n```\n\n```"), Some(("### a", "")));
        assert_eq!(split_entry("### a\n```\n```"), None, "7 символов — регекс не матчит");
        assert_eq!(split_entry("## a\n```\nx\n```"), None);
        assert_eq!(split_entry("### a\n```\nx\n```\n"), None, "хвост после кавычек");
        assert_eq!(split_entry("### a\n```\nx\n```\ny\n```"), Some(("### a", "x\n```\ny")), "жадное тело");
    }
}
