//! Порт lib/specialist-merge.mjs (RFC 04, решение 4; RFC 16 T36): слияние ролей в один
//! вызов, когда все они назначены на одну модель.
//!
//! Три специалиста на одной модели с одинаковым контекстом — одно мнение, оплаченное
//! трижды. Здесь решается, применимо ли слияние, и как разобрать слитый ответ обратно в те
//! же `references`, которые ждут жюри и refutation. Промпт слитого вызова —
//! [`crate::prompt::build_merged_specialist_system`]; заголовок секции —
//! [`crate::prompt::merged_role_heading`].
//!
//! Инвариант: сорванный формат секций НЕ раздаёт один текст всем ролям — три claims,
//! выведенные из одного абзаца, refutation приняла бы за подтверждённый консенсус.

use serde_json::{json, Value};

use crate::engine::SpecialistAssignment;
use crate::findings::js_trim;
use crate::prompt::merged_role_heading;

/// Применимо ли слияние (`shouldMergeSpecialists`). `always` — даже при разных моделях
/// (берётся первая: осознанный выбор оператора); `never` — никогда; иначе (`auto`) — только
/// когда модель у всех ролей одна. Ролей с непустыми role/model должно быть больше одной.
pub fn should_merge_specialists(assignments: &[SpecialistAssignment], mode: &str) -> bool {
    let list: Vec<&SpecialistAssignment> = valid(assignments).collect();
    if list.len() < 2 || mode == "never" {
        return false;
    }
    if mode == "always" {
        return true;
    }
    list.iter().all(|a| a.model == list[0].model)
}

fn valid(assignments: &[SpecialistAssignment]) -> impl Iterator<Item = &SpecialistAssignment> {
    assignments.iter().filter(|a| !a.role.is_empty() && !a.model.is_empty())
}

/// Результат разбора слитого ответа (`{ ok, sections }` в JS).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergedSplit {
    /// `false` — модель не выдержала формат; секций нет.
    pub ok: bool,
    /// Роль → текст секции, в порядке появления заголовков в ответе.
    pub sections: Vec<(String, String)>,
}

impl MergedSplit {
    pub fn get(&self, role: &str) -> Option<&str> {
        self.sections.iter().find(|(r, _)| r == role).map(|(_, t)| t.as_str())
    }
}

/// Терминаторы строки JS-регекса в режиме `m` (`^`/`$` срабатывают на них).
fn is_js_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Канонизация символа JS-регекса с флагом `i` без `u` (ES Canonicalize): toUpperCase,
/// если он даёт ровно одну кодовую единицу UTF-16 и не сводит не-ASCII к ASCII.
fn js_canon(c: char) -> char {
    if (c as u32) > 0xFFFF {
        return c;
    }
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(u), None) if (u as u32) <= 0xFFFF && !((c as u32) >= 128 && (u as u32) < 128) => u,
        _ => c,
    }
}

/// Первое вхождение строки вида `^[ \t]*<heading>[ \t]*$` (флаги `im`) — `(start, end)` в
/// байтах. Заголовок обязан стоять в начале строки: упоминание `### ROLE: security` внутри
/// цитаты кода не расколет чужую секцию.
fn find_heading_line(src: &str, heading: &str) -> Option<(usize, usize)> {
    let head: Vec<char> = heading.chars().map(js_canon).collect();
    let mut line_starts = vec![0usize];
    line_starts.extend(src.char_indices().filter(|(_, c)| is_js_line_terminator(*c)).map(|(i, c)| i + c.len_utf8()));
    for start in line_starts {
        let rest = &src[start..];
        let mut it = rest.char_indices().peekable();
        // [ \t]* — заголовок начинается с '#', поэтому жадный пропуск точен.
        while matches!(it.peek(), Some((_, ' ' | '\t'))) {
            it.next();
        }
        let mut matched = true;
        for &h in &head {
            match it.next() {
                Some((_, c)) if js_canon(c) == h => {}
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if !matched {
            continue;
        }
        while matches!(it.peek(), Some((_, ' ' | '\t'))) {
            it.next();
        }
        match it.peek() {
            None => return Some((start, src.len())),
            Some((i, c)) if is_js_line_terminator(*c) => return Some((start, start + i)),
            _ => {}
        }
    }
    None
}

/// Разбирает слитый ответ на секции ролей (`splitMergedSpecialistText`).
///
/// `ok: false`, если текст пуст, ролей нет или заголовок хотя бы одной роли не найден на
/// отдельной строке. Вызывающий обязан НЕ раздавать один текст всем ролям.
pub fn split_merged_specialist_text<S: AsRef<str>>(text: &str, roles: &[S]) -> MergedSplit {
    let list: Vec<&str> = roles.iter().map(AsRef::as_ref).filter(|r| !r.is_empty()).collect();
    if js_trim(text).is_empty() || list.is_empty() {
        return MergedSplit::default();
    }
    let mut marks: Vec<(&str, usize, usize)> = Vec::with_capacity(list.len());
    for role in &list {
        if let Some((start, end)) = find_heading_line(text, &merged_role_heading(role)) {
            marks.push((role, start, end));
        }
    }
    if marks.len() != list.len() {
        return MergedSplit::default();
    }
    // Array.prototype.sort стабилен — sort_by_key тоже.
    marks.sort_by_key(|m| m.1);
    let mut sections: Vec<(String, String)> = Vec::with_capacity(marks.len());
    for (i, (role, _, end)) in marks.iter().enumerate() {
        let to = marks.get(i + 1).map_or(text.len(), |m| m.1);
        // JS slice(end, to) при to < end (повтор роли) — пустая строка.
        let body = if to > *end { js_trim(&text[*end..to]).to_string() } else { String::new() };
        // Map.set: повтор роли перезаписывает значение, позиция ключа — первая.
        match sections.iter_mut().find(|(r, _)| r == role) {
            Some(slot) => slot.1 = body,
            None => sections.push((role.to_string(), body)),
        }
    }
    MergedSplit { ok: true, sections }
}

/// Слитый ответ → `references` в том же виде, что даёт опрос ролей по отдельности
/// (`mergedTextToReferences`): `{role, model, text, merged: true}` на каждую роль.
///
/// При сорванном формате — одна (первая) роль с полным текстом и `mergedFallback: true`,
/// плюс вызов `on_fallback`: жюри увидит одного советника вместо трёх, без выдуманного
/// консенсуса. Пустой список назначений даёт пустой результат (в JS — TypeError на
/// `list[0].role`; движок зовёт функцию только после `should_merge_specialists`).
pub fn merged_text_to_references(
    text: &str,
    assignments: &[SpecialistAssignment],
    on_fallback: Option<&mut dyn FnMut()>,
) -> Vec<Value> {
    let list: Vec<&SpecialistAssignment> = valid(assignments).collect();
    let roles: Vec<&str> = list.iter().map(|a| a.role.as_str()).collect();
    let split = split_merged_specialist_text(text, &roles);
    if !split.ok {
        if let Some(cb) = on_fallback {
            cb();
        }
        return list
            .first()
            .map(|first| vec![json!({ "role": first.role, "model": first.model, "text": text, "mergedFallback": true })])
            .unwrap_or_default();
    }
    list.iter()
        .map(|a| json!({ "role": a.role, "model": a.model, "text": split.get(&a.role).unwrap_or(""), "merged": true }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(role: &str, model: &str) -> SpecialistAssignment {
        SpecialistAssignment { role: role.into(), model: model.into() }
    }

    fn three(model_b: &str) -> Vec<SpecialistAssignment> {
        vec![a("security", "m1"), a("correctness", model_b), a("architecture", "m1")]
    }

    const ROLES: [&str; 3] = ["security", "correctness", "architecture"];

    #[test]
    fn should_merge_modes() {
        assert!(should_merge_specialists(&three("m1"), "auto"));
        assert!(!should_merge_specialists(&three("m2"), "auto"));
        assert!(should_merge_specialists(&three("m2"), "always"));
        assert!(!should_merge_specialists(&three("m1"), "never"));
        // Неизвестный режим ведёт себя как auto.
        assert!(should_merge_specialists(&three("m1"), "weird"));
        // Одна роль — сливать нечего.
        assert!(!should_merge_specialists(&[a("security", "m1")], "always"));
        // Роли с пустыми полями не считаются.
        assert!(!should_merge_specialists(&[a("security", "m1"), a("", "m1"), a("x", "")], "auto"));
        assert!(!should_merge_specialists(&[], "auto"));
    }

    #[test]
    fn split_well_formed_answer() {
        let text = "Вступление\n### ROLE: security\n- [major] `a.ts:1` — x\n\n### ROLE: correctness\nнет\n  ### role: ARCHITECTURE  \r\n- [minor] `b.ts:2` — y\n";
        let s = split_merged_specialist_text(text, &ROLES);
        assert!(s.ok);
        assert_eq!(s.get("security"), Some("- [major] `a.ts:1` — x"));
        assert_eq!(s.get("correctness"), Some("нет"));
        assert_eq!(s.get("architecture"), Some("- [minor] `b.ts:2` — y"));
    }

    #[test]
    fn split_orders_by_position_not_by_roles() {
        let text = "### ROLE: architecture\nA\n### ROLE: security\nS\n### ROLE: correctness\nC";
        let s = split_merged_specialist_text(text, &ROLES);
        assert!(s.ok);
        let order: Vec<&str> = s.sections.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(order, ["architecture", "security", "correctness"]);
        assert_eq!(s.get("correctness"), Some("C"));
    }

    #[test]
    fn heading_inside_line_does_not_count() {
        // Упоминание заголовка внутри цитаты — не начало секции.
        let text = "### ROLE: security\nquote: `### ROLE: correctness`\n### ROLE: architecture\nok";
        assert!(!split_merged_specialist_text(text, &ROLES).ok);
        // Хвост после заголовка на той же строке — тоже не заголовок.
        let text = "### ROLE: security x\n### ROLE: correctness\n### ROLE: architecture\n";
        assert!(!split_merged_specialist_text(text, &ROLES).ok);
    }

    #[test]
    fn line_terminators_like_js_multiline() {
        let text = "### ROLE: security\u{2028}S\u{2029}### ROLE: correctness\rC\r\n### ROLE: architecture";
        let s = split_merged_specialist_text(text, &ROLES);
        assert!(s.ok);
        assert_eq!(s.get("security"), Some("S"));
        assert_eq!(s.get("correctness"), Some("C"));
        assert_eq!(s.get("architecture"), Some(""));
    }

    #[test]
    fn case_folding_is_js_non_unicode() {
        // U+017F LONG S не сводится к `s` в JS-регексе без `u`.
        let text = "### ROLE: \u{17F}ecurity\n### ROLE: correctness\n### ROLE: architecture\n";
        assert!(!split_merged_specialist_text(text, &ROLES).ok);
    }

    #[test]
    fn split_rejects_empty_input() {
        assert!(!split_merged_specialist_text(" \n\u{FEFF}", &ROLES).ok);
        let none: [&str; 0] = [];
        assert!(!split_merged_specialist_text("### ROLE: security\nx", &none).ok);
    }

    #[test]
    fn duplicate_role_keeps_last_section() {
        let text = "### ROLE: security\nS\n### ROLE: correctness\nC";
        let s = split_merged_specialist_text(text, &["security", "security", "correctness"]);
        assert!(s.ok);
        assert_eq!(s.get("security"), Some("S"));
        assert_eq!(s.sections.len(), 2);
    }

    #[test]
    fn broken_format_does_not_hand_one_text_to_three_roles() {
        let text = "- [critical] `a.ts:1` — SQL-инъекция\n  quote: q";
        let mut fallbacks = 0;
        let mut cb = || fallbacks += 1;
        let refs = merged_text_to_references(text, &three("m1"), Some(&mut cb));
        assert_eq!(fallbacks, 1);
        assert_eq!(refs.len(), 1, "сорванный формат — один советник, не три");
        assert_eq!(refs[0], json!({ "role": "security", "model": "m1", "text": text, "mergedFallback": true }));
        // Частично выдержанный формат (2 из 3 заголовков) — тоже fallback.
        let partial = "### ROLE: security\nS\n### ROLE: correctness\nC";
        let refs = merged_text_to_references(partial, &three("m1"), None);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["mergedFallback"], json!(true));
    }

    #[test]
    fn well_formed_answer_becomes_references() {
        let text = "### ROLE: security\nS\n### ROLE: correctness\n\n### ROLE: architecture\nA";
        let mut called = false;
        let mut cb = || called = true;
        let refs = merged_text_to_references(text, &three("m1"), Some(&mut cb));
        assert!(!called);
        assert_eq!(
            refs,
            vec![
                json!({ "role": "security", "model": "m1", "text": "S", "merged": true }),
                json!({ "role": "correctness", "model": "m1", "text": "", "merged": true }),
                json!({ "role": "architecture", "model": "m1", "text": "A", "merged": true }),
            ]
        );
        assert!(merged_text_to_references(text, &[], None).is_empty());
    }
}
