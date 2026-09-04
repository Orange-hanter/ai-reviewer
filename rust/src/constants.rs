//! Порт lib/constants.mjs — RFC 04.
//!
//! В JS этот модуль наполовину состоит из живых getter'ов поверх
//! process-wide профиля (`MARKERS`, `repoRules()`): модули импортируются
//! раньше, чем грузится профиль, поэтому снимок был бы неверен. В Rust
//! профиля-синглтона нет (решение 3 RFC 04), и всё «профильное» здесь —
//! функции, принимающие [`Profile`]/[`Markers`] аргументом.
//!
//! Константы, уже портированные в модули-потребители на прошлых фазах,
//! РЕЭКСПОРТИРУЮТСЯ, а не копируются: этот модуль — канонический адрес
//! lib/constants.mjs, но второй источник правды заводить нельзя.

use crate::profile::{Markers, Profile};

// Модели по умолчанию (lib/constants.mjs:4-19) — живут в config.rs, потому что
// нужны его мерджу env → профиль → дефолт ещё до появления этого модуля.
pub use crate::config::{
    DEFAULT_MOA_AGGREGATOR, DEFAULT_MOA_REFERENCES, DEFAULT_MOA_SPECIALIST_MODELS,
    DEFAULT_OLLAMA_MODELS_FALLBACK, DEFAULT_REVIEW_VERIFIER_MODEL,
};

/// `SEV` и `SEV[severity] || '⚪'` — определены в report.rs (первый потребитель).
pub use crate::report::{sev_label, SEV};

/// Маркер детерминированных blast-radius находок (`Finding.source` / model label).
pub use crate::actionability::BLAST_FINDING_SOURCE;

/// Расширения путей в Graphify blast-блоках (единый источник для парсера и
/// formatter). Алфавитные токены без regex-метасимволов; при добавлении
/// экранируйте в месте сборки regex.
pub const BLAST_PATH_EXTS: [&str; 11] =
    ["ts", "tsx", "js", "jsx", "mjs", "cjs", "vue", "svelte", "go", "py", "rs"];

/// Confidence рёбер Graphify → score для emitBlastRadiusFindings / top-N.
pub const BLAST_CONF_EXTRACTED: f64 = 0.9;
pub const BLAST_CONF_INFERRED: f64 = 0.75;
/// Unknown ≤ INFERRED (не «обгоняет» явную пониженную метку).
pub const BLAST_CONF_UNKNOWN: f64 = 0.7;
pub const BLAST_CONF_AMBIGUOUS: f64 = 0.55;
pub const BLAST_FINDINGS_MAX: usize = 8;

/// `VERDICT_RANK` — порядок вердиктов ревью (порядок элементов как в JS-объекте).
pub const VERDICT_RANK: &[(&str, u8)] =
    &[("changes_requested", 2), ("comment", 1), ("approve", 0)];

/// `VERDICT_RANK[v] ?? 0`: неизвестный вердикт — как approve.
pub fn verdict_rank(verdict: &str) -> u8 {
    VERDICT_RANK.iter().find(|(k, _)| *k == verdict).map(|(_, r)| *r).unwrap_or(0)
}

/// Правила репозитория для промпта: слои, канонические примитивы, чем проверяется
/// поведение. Приходят из профиля (`repoRules` / `repoRulesFile`); без профиля
/// блок пустой — движок ревьюит «просто код», не выдумывая архитектуру проекта.
///
/// В JS это `repoRules()` без аргументов поверх `getProfile()`; здесь профиль
/// передаётся явно.
pub fn repo_rules(profile: &Profile) -> &str {
    &profile.repo_rules
}

/// Наш комментарий — маркер в НАЧАЛЕ body (все наши посты начинаются с маркера).
/// `contains()` нельзя: процитированный маркер в чужом комментарии сделал бы его
/// «нашим» (перезапись sticky / удаление inline).
pub fn has_marker(body: &str, marker: &str) -> bool {
    body.starts_with(marker)
}

/// Тело машинного футера sticky (сырой JSON-текст) или `None`.
///
/// Берём ПОСЛЕДНЕЕ вхождение маркера, а не первое: находка может процитировать
/// пример маркера в своём тексте — так и случилось на PR #531, где ревью привело
/// в теле замечания `<!-- ai-review-data:{"findings":[]} -->`. Разбор «до первого»
/// хватал этот пример, успешно парсил пустой объект, и отчёт выглядел чистым при
/// шести живых находках.
///
/// Закрытие ищем в порядке убывания надёжности: `-->` в самом конце тела (наш
/// футер пишется последним блоком) → последний `-->`, после которого хвост
/// парсится как JSON → просто последний. Последний вариант оставляет разбор
/// ремонту JSON у вызывающего.
///
/// Расхождение с JS: открывающий маркер (`<!--` + пробелы + `dataKey`) ищется
/// сканированием, а не regex'ом. Для JS `dataKey` — источник regex, так что
/// метасимвол в namespace там значил бы что-то своё (или бросил бы SyntaxError);
/// здесь ключ всегда литерал. Класс пробелов — `char::is_whitespace` (Unicode
/// White_Space): от JS `\s` отличается только на U+FEFF.
pub fn extract_sticky_footer_raw(body: &str, markers: &Markers) -> Option<String> {
    let start = last_footer_open(body, &markers.data_key)?;
    let rest = &body[start..];

    // `/-->\s*$/` — футер в самом конце тела.
    if let Some(i) = anchored_close(rest) {
        return Some(rest[..i].to_string());
    }

    // После футера что-то дописали (ручная правка комментария): перебираем
    // закрытия с конца и берём первое, дающее валидный JSON, — иначе обрезали бы
    // наш блок на чужой стрелке.
    let positions: Vec<usize> = rest.match_indices("-->").map(|(i, _)| i).collect();
    for &i in positions.iter().rev() {
        let candidate = &rest[..i];
        if serde_json::from_str::<serde_json::Value>(candidate.trim()).is_ok() {
            return Some(candidate.to_string());
        }
    }
    match positions.iter().max() {
        Some(&i) => Some(rest[..i].to_string()),
        None => Some(rest.to_string()),
    }
}

/// Позиция сразу ЗА последним `<!--\s*<data_key>` (аналог `m.index + m[0].length`
/// у глобального regex в JS). `None` — маркера в теле нет.
fn last_footer_open(body: &str, data_key: &str) -> Option<usize> {
    let mut found = None;
    let mut from = 0usize;
    while let Some(rel) = body[from..].find("<!--") {
        let open = from + rel;
        let after_open = open + "<!--".len();
        let ws = body[after_open..]
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(body.len() - after_open);
        let key_at = after_open + ws;
        if body[key_at..].starts_with(data_key) {
            found = Some(key_at + data_key.len());
            // lastIndex глобального regex встаёт за конец совпадения.
            from = key_at + data_key.len();
        } else {
            from = after_open;
        }
    }
    found
}

/// `/-->\s*$/` — индекс `-->`, за которым до конца строки только пробелы.
fn anchored_close(rest: &str) -> Option<usize> {
    let trimmed = rest.trim_end();
    if trimmed.ends_with("-->") {
        Some(trimmed.len() - "-->".len())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn markers() -> Markers {
        Markers::default()
    }

    /// Реэкспорты из config.rs — не копии, но состав пула моделей меняется
    /// вместе с промптами, и молчаливый дрейф здесь стоит целого прогона eval.
    #[test]
    fn default_models_match_js() {
        assert_eq!(DEFAULT_MOA_REFERENCES, "kimi-k2.7-code,glm-5.2,deepseek-v4-flash");
        assert_eq!(
            DEFAULT_MOA_SPECIALIST_MODELS,
            "security:deepseek-v4-flash,correctness:kimi-k2.7-code,architecture:glm-5.2"
        );
        // Агрегатор и verifier намеренно ВНЕ пула advisors — два независимых судьи.
        assert_eq!(DEFAULT_MOA_AGGREGATOR, "minimax-m3");
        assert_eq!(DEFAULT_REVIEW_VERIFIER_MODEL, "deepseek-v4-pro");
        assert!(!DEFAULT_MOA_REFERENCES.contains(DEFAULT_MOA_AGGREGATOR));
        assert!(!DEFAULT_MOA_REFERENCES.contains(DEFAULT_REVIEW_VERIFIER_MODEL));
        assert!(!DEFAULT_MOA_SPECIALIST_MODELS.contains(DEFAULT_MOA_AGGREGATOR));
        assert_eq!(DEFAULT_OLLAMA_MODELS_FALLBACK, "deepseek-v4-flash");
    }

    #[test]
    fn sev_and_verdict_rank() {
        assert_eq!(sev_label("critical"), "🔴");
        assert_eq!(sev_label("nit"), "🔵");
        assert_eq!(sev_label("wat"), "⚪");
        assert_eq!(SEV.len(), 4);
        assert_eq!(verdict_rank("changes_requested"), 2);
        assert_eq!(verdict_rank("comment"), 1);
        assert_eq!(verdict_rank("approve"), 0);
        assert_eq!(verdict_rank("что-то новое"), 0, "неизвестный вердикт = approve");
    }

    #[test]
    fn blast_constants_match_js() {
        assert_eq!(BLAST_FINDING_SOURCE, "blast-radius");
        assert_eq!(BLAST_FINDINGS_MAX, 8);
        // Порядок confidence: extracted > inferred ≥ unknown > ambiguous.
        // Unknown не обгоняет явную пониженную метку inferred.
        let ladder = [
            ("ambiguous", BLAST_CONF_AMBIGUOUS),
            ("unknown", BLAST_CONF_UNKNOWN),
            ("inferred", BLAST_CONF_INFERRED),
            ("extracted", BLAST_CONF_EXTRACTED),
        ];
        for pair in ladder.windows(2) {
            let ((lo_name, lo), (hi_name, hi)) = (pair[0], pair[1]);
            assert!(lo <= hi, "{lo_name} ({lo}) обогнал {hi_name} ({hi})");
        }
        assert!(ladder[1].1 < ladder[2].1, "unknown строго ниже inferred");
        assert!(BLAST_PATH_EXTS.contains(&"svelte"));
        assert!(BLAST_PATH_EXTS.iter().all(|e| e.chars().all(|c| c.is_ascii_alphabetic())));
    }

    #[test]
    fn repo_rules_empty_without_profile() {
        let p = Profile::default_builtin(&crate::profile::package_root());
        assert_eq!(repo_rules(&p), "");
    }

    #[test]
    fn repo_rules_from_profile() {
        let raw = serde_json::json!({ "repoRules": "СЛОИ ПРОЕКТА" });
        let p = Profile::normalize(&raw, Path::new("/nowhere"), "", Path::new("/nowhere"));
        assert_eq!(repo_rules(&p), "СЛОИ ПРОЕКТА");
    }

    /// В JS `repoRules()` читает process-wide `getProfile()`, поэтому «правила
    /// двух профилей одновременно» там непредставимы. Здесь профиль — аргумент.
    #[test]
    fn repo_rules_of_two_profiles_coexist() {
        let a = Profile::normalize(
            &serde_json::json!({ "repoRules": "ПРАВИЛА A" }),
            Path::new("/nowhere"),
            "",
            Path::new("/nowhere"),
        );
        let b = Profile::normalize(
            &serde_json::json!({ "repoRules": "ПРАВИЛА B" }),
            Path::new("/nowhere"),
            "",
            Path::new("/nowhere"),
        );
        assert_eq!(repo_rules(&a), "ПРАВИЛА A");
        assert_eq!(repo_rules(&b), "ПРАВИЛА B");
    }

    #[test]
    fn has_marker_is_prefix_not_contains() {
        assert!(has_marker("<!-- ai-review -->\nтело", "<!-- ai-review -->"));
        // Процитированный маркер внутри чужого комментария не делает его нашим.
        assert!(!has_marker("смотри: <!-- ai-review -->", "<!-- ai-review -->"));
        assert!(!has_marker("", "<!-- ai-review -->"));
    }

    #[test]
    fn sticky_footer_anchored_at_end() {
        let body = "<!-- ai-review -->\nотчёт\n<!-- ai-review-data:{\"findings\":[1]} -->";
        let raw = extract_sticky_footer_raw(body, &markers()).unwrap();
        assert_eq!(raw.trim(), "{\"findings\":[1]}");
    }

    #[test]
    fn sticky_footer_takes_last_occurrence() {
        // Регрессия PR #531: находка процитировала пример маркера в своём теле.
        let body = concat!(
            "<!-- ai-review -->\n",
            "тело находки с примером `<!-- ai-review-data:{\"findings\":[]} -->`\n",
            "<!-- ai-review-data:{\"findings\":[1,2,3,4,5,6]} -->"
        );
        let raw = extract_sticky_footer_raw(body, &markers()).unwrap();
        assert_eq!(raw.trim(), "{\"findings\":[1,2,3,4,5,6]}");
    }

    #[test]
    fn sticky_footer_survives_text_appended_after_it() {
        let body = "<!-- ai-review-data:{\"a\":1} -->\nдописано вручную --> и ещё";
        let raw = extract_sticky_footer_raw(body, &markers()).unwrap();
        assert_eq!(raw.trim(), "{\"a\":1}");
    }

    #[test]
    fn sticky_footer_without_close_returns_tail() {
        let body = "<!-- ai-review-data:{\"a\":1}";
        let raw = extract_sticky_footer_raw(body, &markers()).unwrap();
        assert_eq!(raw, "{\"a\":1}", "разбор битого хвоста — забота json_repair");
    }

    #[test]
    fn sticky_footer_absent() {
        assert!(extract_sticky_footer_raw("просто комментарий", &markers()).is_none());
        assert!(extract_sticky_footer_raw("", &markers()).is_none());
    }

    /// Таблица снята прогоном `extractStickyFooterRaw` из lib/constants.mjs
    /// (`AI_REVIEWER_CONFIG_FORCE_JS=1 node`) на пограничных телах: пустой хвост,
    /// два футера подряд, чужие `-->`, кириллица (байтовые индексы), маркер без
    /// закрытия. Здесь она — замок паритета: расхождение видно как diff строки,
    /// а не как «сходимость sticky тихо поехала на живом PR».
    #[test]
    fn sticky_footer_matches_js_on_edge_cases() {
        let cases: &[(&str, Option<&str>)] = &[
            (r#"<!--ai-review-data:{"a":1}-->"#, Some(r#"{"a":1}"#)),
            ("<!--\t ai-review-data:{\"a\":1}   -->\n\n  ", Some(r#"{"a":1}   "#)),
            (r#"<!-- ai-review-data:{"a":1} --> хвост --> ещё"#, Some(r#"{"a":1} "#)),
            (
                r#"<!-- ai-review-data:{"a":1} --><!-- ai-review-data:{"b":2} -->"#,
                Some(r#"{"b":2} "#),
            ),
            (r#"текст <!-- --> <!-- ai-review-data:[] -->"#, Some("[] ")),
            ("<!--   -->", None),
            ("<!--", None),
            ("<!-- ai-review-data:", Some("")),
            ("<!-- ai-review-data:-->", Some("")),
            (
                r#"<!-- ai-review-data:{"a":1} -->{"junk"} -->"#,
                Some(r#"{"a":1} -->{"junk"} "#),
            ),
            (r#"<!-- ai-review-data:{"a":1} --> tail"#, Some(r#"{"a":1} "#)),
            (r#"<!-- ai-review-data:{"a":1}--> --> -->"#, Some(r#"{"a":1}--> --> "#)),
            (
                "a <!-- ai-review-data:x --> b <!-- ai-review-data:y --> c",
                Some("y "),
            ),
        ];
        for (body, expected) in cases {
            let got = extract_sticky_footer_raw(body, &markers());
            assert_eq!(got.as_deref(), *expected, "тело: {body:?}");
        }
    }

    #[test]
    fn sticky_footer_uses_profile_namespace() {
        let m = Markers::new("orakul-ai-review");
        let body = "<!--   orakul-ai-review-data:{\"a\":1} -->";
        assert_eq!(extract_sticky_footer_raw(body, &m).unwrap().trim(), "{\"a\":1}");
        // Чужой namespace в том же теле нашим футером не считается.
        assert!(extract_sticky_footer_raw(body, &markers()).is_none());
    }
}
