//! Порт lib/prompt-layout.mjs (RFC 04, решение 5; RFC 20 T36): раскладка сообщений под
//! префиксный кэш провайдера.
//!
//! Было: роль в `system`, жюри дописывает мнения советников в НАЧАЛО последнего user —
//! вызовы одного чанка расходятся с первого токена. Здесь:
//! `[system: общая рамка] [user: контекст чанка] [user: инструкция роли]` — первые два
//! сообщения побайтово равны у всех ролей чанка.
//!
//! Собирается из уже готовых сообщений ([`crate::prompt`]), здесь только перестановка.
//! Функции возвращают `None`, если форма входа не та: вызывающий остаётся на прежней
//! раскладке — тихо собрать «почти правильный» промпт хуже, чем не экономить.

use serde_json::{json, Value};

use crate::findings::{js_is_whitespace, js_to_string, js_truthy};

/// Допустимые значения `REVIEW_PROMPT_LAYOUT` (`PROMPT_LAYOUTS`).
pub const PROMPT_LAYOUTS: [&str; 2] = ["legacy", "shared-prefix"];

/// Общая рамка. Намеренно короткая и без правил проекта: правила едут внутри инструкции роли.
pub const SHARED_SYSTEM: &str = "You take part in a strict code review of a pull request.
The next message contains the full context of this part of the PR: the changed-file map, the
deterministic pre-scan, the blast-radius, findings from the previous run, file contents and the
annotated diff. The message after it defines YOUR role and the exact output format for it.
Read the context first, then follow the role instructions literally.";

fn role_of(m: &Value) -> Option<&str> {
    m.get("role").and_then(Value::as_str)
}

/// `content ?? ''` последнего user-сообщения; `None` — user-сообщений нет.
fn last_user(messages: &[Value]) -> Option<Value> {
    messages.iter().rev().find(|m| role_of(m) == Some("user")).map(|m| match m.get("content") {
        None | Some(Value::Null) => json!(""),
        Some(c) => c.clone(),
    })
}

fn layout_of(context: Value, instruction: Value) -> Vec<Value> {
    vec![
        json!({ "role": "system", "content": SHARED_SYSTEM }),
        json!({ "role": "user", "content": context }),
        json!({ "role": "user", "content": instruction }),
    ]
}

/// Роль-советник (`sharedPrefixAdvisor`): вход `[system: правила роли, user: контекст]`.
/// Инструкция роли — первое system-сообщение (позиция не фиксируется), контекст —
/// последний user. `None`, если system пуст/отсутствует или user нет.
pub fn shared_prefix_advisor(role_messages: &[Value]) -> Option<Vec<Value>> {
    let system = role_messages
        .iter()
        .find(|m| role_of(m) == Some("system"))
        .and_then(|m| m.get("content"))
        .filter(|c| js_truthy(c))?
        .clone();
    let context = last_user(role_messages)?;
    Some(layout_of(context, system))
}

/// Жюри-агрегатор (`sharedPrefixAggregator`): к базовому user приписаны СПЕРЕДИ мнения
/// советников и схема ответа. Контекст — базовый user, инструкция — правила агрегатора
/// (system базовых сообщений) плюс приписанное, с обрезкой хвостовых пробелов.
///
/// `None`, если первое базовое сообщение не system с непустыми правилами, нет user-сообщений
/// или агрегатный user не оканчивается базовым (приписали не спереди — форма изменилась).
pub fn shared_prefix_aggregator(base_messages: &[Value], agg_messages: &[Value]) -> Option<Vec<Value>> {
    let first = base_messages.first().filter(|m| role_of(m) == Some("system"))?;
    let rules = first.get("content").filter(|c| js_truthy(c))?;
    let base_user = last_user(base_messages)?;
    let agg_user = last_user(agg_messages)?;
    // endsWith/slice определены у строк; не-строковый content — форма не та.
    let (base_s, agg_s) = (base_user.as_str()?, agg_user.as_str()?);
    let prepended = agg_s.strip_suffix(base_s)?;
    let instruction = format!("{}\n\n{prepended}", js_to_string(rules));
    let instruction = instruction.trim_end_matches(js_is_whitespace).to_string();
    Some(layout_of(base_user, Value::String(instruction)))
}

/// Длина общего префикса в UTF-16 единицах (`sharedPrefixChars`) — то, что провайдер сможет
/// отдать из кэша. 0, если сообщений меньше трёх (раскладка не shared-prefix).
pub fn shared_prefix_chars(messages: &[Value]) -> usize {
    if messages.len() < 3 {
        return 0;
    }
    let len = |m: &Value| match m.get("content") {
        None | Some(Value::Null) => 0,
        Some(c) => js_to_string(c).encode_utf16().count(),
    };
    len(&messages[0]) + len(&messages[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(role: &str, content: &str) -> Value {
        json!({ "role": role, "content": content })
    }

    #[test]
    fn advisor_moves_role_to_last_message() {
        let out = shared_prefix_advisor(&[m("system", "RULES"), m("user", "CTX")]).unwrap();
        assert_eq!(out, vec![m("system", SHARED_SYSTEM), m("user", "CTX"), m("user", "RULES")]);
        // system последним — тоже годится; контекст — ПОСЛЕДНИЙ user.
        let out = shared_prefix_advisor(&[m("user", "old"), m("user", "CTX"), m("system", "RULES")]).unwrap();
        assert_eq!(out[1], m("user", "CTX"));
        assert_eq!(out[2], m("user", "RULES"));
    }

    #[test]
    fn advisor_prefix_is_shared_between_roles() {
        let a = shared_prefix_advisor(&[m("system", "security"), m("user", "CTX")]).unwrap();
        let b = shared_prefix_advisor(&[m("system", "correctness"), m("user", "CTX")]).unwrap();
        assert_eq!(a[..2], b[..2]);
        assert_ne!(a[2], b[2]);
    }

    #[test]
    fn advisor_wrong_shape_is_none() {
        assert!(shared_prefix_advisor(&[]).is_none());
        assert!(shared_prefix_advisor(&[m("user", "CTX")]).is_none());
        assert!(shared_prefix_advisor(&[m("system", ""), m("user", "CTX")]).is_none());
        assert!(shared_prefix_advisor(&[m("system", "R")]).is_none());
        // user без content → контекст '' (content ?? '').
        let out = shared_prefix_advisor(&[m("system", "R"), json!({ "role": "user" })]).unwrap();
        assert_eq!(out[1], m("user", ""));
    }

    #[test]
    fn aggregator_splits_prepended_part() {
        let base = [m("system", "AGG RULES"), m("user", "CTX")];
        let agg = [m("system", "AGG RULES"), m("user", "### Specialist: s\nS\nSCHEMA\nCTX")];
        let out = shared_prefix_aggregator(&base, &agg).unwrap();
        assert_eq!(
            out,
            vec![
                m("system", SHARED_SYSTEM),
                m("user", "CTX"),
                m("user", "AGG RULES\n\n### Specialist: s\nS\nSCHEMA"),
            ]
        );
        // Ничего не приписано — инструкция без висячих переводов строки.
        let out = shared_prefix_aggregator(&base, &base).unwrap();
        assert_eq!(out[2], m("user", "AGG RULES"));
    }

    #[test]
    fn aggregator_wrong_shape_is_none() {
        let base = [m("system", "R"), m("user", "CTX")];
        // Приписано в конец, а не спереди.
        assert!(shared_prefix_aggregator(&base, &[m("user", "CTX + tail")]).is_none());
        // Первое сообщение базы не system.
        assert!(shared_prefix_aggregator(&[m("user", "CTX"), m("system", "R")], &base).is_none());
        assert!(shared_prefix_aggregator(&[m("system", ""), m("user", "CTX")], &base).is_none());
        assert!(shared_prefix_aggregator(&base, &[m("system", "R")]).is_none());
        assert!(shared_prefix_aggregator(&[], &base).is_none());
    }

    #[test]
    fn prefix_chars_counts_utf16() {
        assert_eq!(shared_prefix_chars(&[m("system", "ab"), m("user", "cd")]), 0);
        let msgs = [m("system", "😀a"), m("user", "яz"), m("user", "long instruction")];
        assert_eq!(shared_prefix_chars(&msgs), 3 + 2);
        let msgs = [json!({ "role": "system" }), m("user", "x"), m("user", "y")];
        assert_eq!(shared_prefix_chars(&msgs), 1);
    }

    #[test]
    fn layouts_constant() {
        assert_eq!(PROMPT_LAYOUTS, ["legacy", "shared-prefix"]);
    }
}
