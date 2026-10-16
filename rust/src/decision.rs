//! Decision-модели (RFC 21): вопросы, таблица семейств FP, построители входа.
//!
//! Вопросы с закрытым набором вариантов для режима `decide` бинаря `llm`:
//!   Q1 `fp_family`   — семейство FP находки (docs/false-positives.md) или `real`;
//!   Q2 `chunk_route` — нужно ли чанку жюри специалистов (`jury` | `single`).
//!
//! Модуль rust-owned: JS-копии нет (RFC 20 + решение владельца уходить от JS). JS-обёртка
//! lib/decision.mjs зовёт бинарь `decision` и держит только оркестрацию shadow, которую
//! вызывает pipeline.mjs. Метрики офлайн-замера — в [`crate::decision_eval`].

use serde_json::{json, Map, Value};

/// Вход моделей ~32K токенов; символов с запасом под инструкции и варианты.
pub const MAX_STATE_CHARS: usize = 48_000;
/// ±строк диффа вокруг строки находки во входе Q1.
pub const FP_WINDOW_MARGIN: i64 = 15;

/// (значение, описание). Порядок Q1 — порядок семейств в docs/false-positives.md, `real`
/// последним. Описания английские — как системные промпты Rust (RFC 20).
pub const FP_CHOICES: &[(&str, &str)] = &[
    ("f1_in_diff", "Claims a file or test is missing from the PR, but it is present in the diff."),
    ("f2_convergence", "Re-asserts an earlier finding that the current code already addressed."),
    ("f3_neighbor_commit", "Ignores a fix made in a neighbouring commit of the same PR."),
    ("f4_hypothetical", "Describes code, behaviour or an attack path that does not exist in the shown code."),
    ("f5_outdated", "Relies on outdated knowledge of a language, library or tool instead of the code."),
    ("f6_unseen", "Asserts a problem about code the reviewer could not see (outside its part of the PR)."),
    ("f7_absent_path", "Claims a file or module does not exist although it exists in the repository."),
    ("f8_retry_leak", "Claims a resource or timer leaks inside a retry loop although it is released each attempt."),
    ("other_fp", "A false positive of another kind, e.g. an intentional, documented design decision."),
    ("real", "A real defect that a careful reviewer would want fixed."),
];

const FP_INSTRUCTIONS: &str = "A code-review bot produced the finding below for the shown diff fragment. \
Decide whether it is a real defect or a false positive, and if false, of which kind.";

/// Q2: `jury` первым — исходный порядок совпадает с безопасным дефолтом.
pub const CHUNK_CHOICES: &[(&str, &str)] = &[
    ("jury", "Executable logic, configuration that runs, or security-relevant code with non-trivial behaviour."),
    ("single", "Documentation, data, fixtures, snapshots or a trivial change with no executable behaviour."),
];

const CHUNK_INSTRUCTIONS: &str = "A part of a pull request is shown below (file paths and the diff). Decide whether \
reviewing it needs a panel of specialist reviewers or a single general reviewer is enough.";

/// Семейство каждого паттерна пакетного feedback/fp-patterns.jsonl. Формат jsonl не меняется:
/// у потребителя свои паттерны, их id сюда не попадут и получат None. Большая часть пакетных
/// паттернов — «намеренный дизайн» этого репозитория: в семейства 1–8 не ложатся → `other_fp`.
/// Тест держит покрытие: новый id в jsonl без записи здесь — красный.
pub const FP_FAMILY_BY_PATTERN_ID: &[(&str, &str)] = &[
    ("e2e-in-diff", "f1_in_diff"),
    ("file-absent-in-diff", "f1_in_diff"),
    ("gha-version-hallucination", "f5_outdated"),
    ("metrics-absent-observability", "f6_unseen"),
    ("softblast-typedef-cosmetic", "other_fp"),
    ("graphify-path-norm-speculation", "f4_hypothetical"),
    ("graphify-depth-shell-injection", "f4_hypothetical"),
    ("blast-source-literal-softblast", "other_fp"),
    ("parse-blast-dependents-export-exists", "f6_unseen"),
    ("costtracker-engine-deps-intentional", "other_fp"),
    ("graphify-symbol-dash-flag-fp", "f4_hypothetical"),
    ("softblast-module-boundary-intentional", "other_fp"),
    ("redact-vs-obs-duplicate-ok", "other_fp"),
    ("fp-findings-cluster-intentional", "other_fp"),
    ("crosschunk-filterFindingsFixedInDiff", "f6_unseen"),
    ("crosschunk-emitBlast-dropAmbiguous", "f6_unseen"),
    ("test-whitebox-graphify-import", "other_fp"),
    ("test-fp-import-channels-ok", "other_fp"),
    ("test-fixture-secrets-ok", "other_fp"),
    ("redact-query-ampersand-edge", "other_fp"),
    ("escape-md-ticks-intentional", "other_fp"),
    ("metrics-bymodel-object-contract", "other_fp"),
    ("plan-todo-completeness", "other_fp"),
    ("mcp-oversized-eof-tail", "f4_hypothetical"),
    ("structure-cli-base-source-required", "other_fp"),
    ("trust-tier-default-documented", "other_fp"),
    ("index-reexport-primitives-ok", "other_fp"),
    ("trust-tier-validated-in-receipt", "f6_unseen"),
    ("structure-namespace-parallel-ok", "other_fp"),
    ("hotspot-confidence-conservative", "other_fp"),
    ("property-evidence-vs-bound", "other_fp"),
    ("context-truncated-conservative", "other_fp"),
    ("drift-unique-threshold-strict-by-design", "other_fp"),
    ("drift-path-overlap-heuristic", "other_fp"),
    ("export-env-read-intentional", "other_fp"),
    ("crossboundary-deepest-prefix-ok", "other_fp"),
    ("lcov-match-edge-case-heuristic", "other_fp"),
    ("lcov-kind-list-coupling-ok", "other_fp"),
    ("mcp-pin-args-both-allowed", "other_fp"),
    ("scip-truncation-drops-edges-by-design", "other_fp"),
    ("scip-definition-choice-deterministic", "other_fp"),
    ("test-body-boundary-heuristic", "other_fp"),
    ("short-contract-labels-heuristic", "other_fp"),
    ("structure-primitives-single-source", "other_fp"),
    ("structure-symlink-guard-landed", "f3_neighbor_commit"),
    ("graph-sha-prefix-compare-ok", "other_fp"),
    ("schema-path-pattern-duplication-ok", "other_fp"),
    ("schema-no-cross-field-check-ok", "other_fp"),
    ("clamp-test-asserts-behaviour", "other_fp"),
    ("mcp-test-watchdog-ok", "other_fp"),
    ("mutation-line-zero-unreachable", "f4_hypothetical"),
];

fn question_json(name: &str, instructions: &str, choices: &[(&str, &str)]) -> Value {
    json!({
        "name": name,
        "instructions": instructions,
        "choices": choices.iter().map(|(v, d)| json!({"value": v, "description": d})).collect::<Vec<_>>(),
    })
}

/// Оба вопроса в форме входа `decide` бинаря llm.
pub fn questions() -> Value {
    json!({
        "fp": question_json("fp_family", FP_INSTRUCTIONS, FP_CHOICES),
        "chunk": question_json("chunk_route", CHUNK_INSTRUCTIONS, CHUNK_CHOICES),
    })
}

/// Семейство по id паттерна; неизвестный id → None.
pub fn fp_family_of_pattern(id: &str) -> Option<&'static str> {
    FP_FAMILY_BY_PATTERN_ID.iter().find(|(k, _)| *k == id).map(|(_, v)| *v)
}

/// Окно аннотированной секции диффа (формат parseDiff: `    12: +код`, `     : -старое`)
/// вокруг строки `line`: строки с номером в [line−margin, line+margin] и удалённые строки
/// между ними. Заголовки секции (`diff --git`, `---`, `+++`) сохраняются. Нет строки — вся
/// секция (вход всё равно урежет бинарь llm).
pub fn diff_window(section: &str, line: Option<i64>, margin: i64) -> String {
    let ln = match line {
        Some(l) if l > 0 => l,
        _ => return section.to_string(),
    };
    let mut out = Vec::new();
    let mut last_no: i64 = 0;
    for raw in section.split('\n') {
        if raw.starts_with("diff --git") || raw.starts_with("--- ") || raw.starts_with("+++ ") {
            out.push(raw);
            continue;
        }
        let trimmed = raw.trim_start();
        let Some(colon) = trimmed.find(':') else { continue };
        let digits = &trimmed[..colon];
        if !digits.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let no = if digits.is_empty() { last_no } else { digits.parse::<i64>().unwrap_or(last_no) };
        if !digits.is_empty() {
            last_no = no;
        }
        if no >= ln - margin && no <= ln + margin {
            out.push(raw);
        }
    }
    out.join("\n")
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn line_of(v: &Value) -> Option<i64> {
    v.get("line").and_then(|l| l.as_i64().or_else(|| l.as_f64().map(|f| f as i64)))
}

/// Вход Q1: находка и окно диффа вокруг её строки. `section` — секция файла находки.
pub fn fp_state(finding: &Value, section: &str) -> String {
    let window = diff_window(section, line_of(finding), FP_WINDOW_MARGIN);
    let sev = match str_of(finding, "severity") { "" => "?", s => s };
    let file = match str_of(finding, "file") { "" => "?", s => s };
    let line = line_of(finding).map(|l| l.to_string()).unwrap_or_else(|| "?".into());
    format!(
        "Finding: [{sev}] {file}:{line}\nTitle: {}\nBody: {}\n\nDiff fragment (annotated with new line numbers):\n{}",
        str_of(finding, "title"),
        str_of(finding, "body"),
        if window.is_empty() { "(no diff fragment for this file)" } else { &window },
    )
}

/// Вход Q2: пути чанка с классами (`classes` — {path: class} от classifyFilePath) и его дифф.
pub fn chunk_state(chunk: &Value, classes: &Map<String, Value>) -> String {
    let paths: Vec<&str> = chunk
        .get("paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut s = String::from("Files:\n");
    if paths.is_empty() {
        s.push_str("- (unknown)\n");
    }
    for p in &paths {
        let class = classes.get(*p).and_then(Value::as_str).unwrap_or("other");
        s.push_str(&format!("- {p} ({class})\n"));
    }
    s.push_str("\nDiff:\n");
    s.push_str(str_of(chunk, "annotated"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn каждый_id_пакетного_jsonl_отнесён_к_семейству_q1() {
        let jsonl = include_str!("../../feedback/fp-patterns.jsonl");
        let values: Vec<&str> = FP_CHOICES.iter().map(|(v, _)| *v).collect();
        let mut missing = Vec::new();
        for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
            let id = serde_json::from_str::<Value>(line).unwrap()["id"].as_str().unwrap().to_string();
            if fp_family_of_pattern(&id).is_none() {
                missing.push(id);
            }
        }
        assert!(missing.is_empty(), "паттерны без семейства — допишите FP_FAMILY_BY_PATTERN_ID: {missing:?}");
        for (id, fam) in FP_FAMILY_BY_PATTERN_ID {
            assert!(values.contains(fam), "{id} → {fam} вне набора");
            assert_ne!(*fam, "real", "{id}: паттерн FP не может значить «настоящая»");
        }
        assert_eq!(fp_family_of_pattern("чужой-паттерн"), None);
    }

    #[test]
    fn вопросы_уникальны_и_jury_первым() {
        for choices in [FP_CHOICES, CHUNK_CHOICES] {
            let mut vals: Vec<&str> = choices.iter().map(|(v, _)| *v).collect();
            vals.sort_unstable();
            vals.dedup();
            assert_eq!(vals.len(), choices.len());
        }
        let q = questions();
        assert_eq!(q["chunk"]["choices"][0]["value"], "jury");
        assert_eq!(q["fp"]["name"], "fp_family");
        assert_eq!(q["fp"]["choices"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn diff_window_по_новым_номерам_с_удалёнными_внутри() {
        let mut lines = vec!["diff --git a/a.js b/a.js".to_string(), "--- a/a.js".into(), "+++ b/a.js".into(), "@@ -1,40 +1,40 @@".into()];
        lines.extend((1..=30).map(|i| format!("   {i}:  line{i}")));
        lines.push("     : -old".into());
        let section = lines.join("\n");
        let w = diff_window(&section, Some(25), 2);
        assert_eq!(
            w.split('\n').collect::<Vec<_>>(),
            ["diff --git a/a.js b/a.js", "--- a/a.js", "+++ b/a.js", "   23:  line23", "   24:  line24", "   25:  line25", "   26:  line26", "   27:  line27"]
        );
        assert!(diff_window(&section, Some(30), 0).ends_with("   30:  line30\n     : -old"));
        assert_eq!(diff_window(&section, None, 2), section, "нет строки — вся секция");
        assert_eq!(diff_window(&section, Some(0), 2), section);
    }

    #[test]
    fn построители_входа() {
        let f = json!({"file": "a.js", "line": 3, "severity": "major", "title": "T", "body": "B", "confidence": 0.9});
        let s = fp_state(&f, "");
        assert!(s.starts_with("Finding: [major] a.js:3\nTitle: T\nBody: B"));
        assert!(s.ends_with("(no diff fragment for this file)"));
        let f2 = json!({"file": "a.js", "line": 3, "severity": "major", "title": "T", "body": "B", "model": "x"});
        assert_eq!(fp_state(&f2, ""), s, "лишние поля не влияют");
        let mut classes = Map::new();
        classes.insert("a.js".into(), json!("logic"));
        let c = chunk_state(&json!({"paths": ["a.js", "b.md"], "annotated": "DIFF"}), &classes);
        assert!(c.contains("- a.js (logic)\n- b.md (other)\n"));
        assert!(c.ends_with("DIFF"));
    }
}
