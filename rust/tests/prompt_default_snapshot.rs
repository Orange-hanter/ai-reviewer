//! Контракт «дефолтный гейт не меняет промпт»: при конфиге гейта по умолчанию
//! (`critical,major` / 0.8), явном или отсутствующем, и при мусорном `gate` бинарь
//! `prompt` отдаёт побайтово то же, что до переноса порога и уровней блока в `PromptGate`.
//!
//! Снимок `fixtures/prompt-default/cases.json` снят с бинаря ДО изменения: пары
//! {name, input, output}, output — сырой stdout. Обновлять снимок можно только вместе с
//! осознанной правкой текста промпта, а не чтобы «позеленить» тест.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const CASES: &str = include_str!("fixtures/prompt-default/cases.json");

fn run(input: &Value) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_prompt"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("запуск бинаря prompt");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "prompt упал: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn cases() -> Vec<Value> {
    serde_json::from_str::<Vec<Value>>(CASES).expect("снимок — JSON-массив")
}

fn with_gate(input: &Value, gate: Value) -> Value {
    let mut v = input.clone();
    v["gate"] = gate;
    v
}

#[test]
fn prompt_default_gate_is_byte_identical() {
    let cases = cases();
    assert!(cases.len() >= 22, "снимок неполный: {}", cases.len());
    let gates = [
        None,
        Some(json!({"blockOn": ["critical", "major"], "confidenceMin": 0.8})),
        // мусор и ненормализованный порядок сводятся к дефолту
        Some(json!({"blockOn": ["major", "critical", "bogus"], "confidenceMin": "x"})),
    ];
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let expected = case["output"].as_str().unwrap();
        for gate in &gates {
            let input = match gate {
                None => case["input"].clone(),
                Some(g) => with_gate(&case["input"], g.clone()),
            };
            assert_eq!(run(&input), expected, "{name}: промпт разошёлся со снимком при gate={gate:?}");
        }
    }
}

#[test]
fn prompt_non_default_gate_changes_the_jury_prompt() {
    // Обратная сторона снимка: gate действительно доходит до текста через бинарь.
    let case = cases().into_iter().find(|c| c["name"] == "empty/agg-full").unwrap();
    let out = run(&with_gate(&case["input"], json!({"blockOn": ["critical"], "confidenceMin": 0.7})));
    let msgs: Value = serde_json::from_str(&out).unwrap();
    let system = msgs["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Only critical findings at 0.7 or above block the merge"));
    assert!(system.contains("- 0.7–0.85: a well-known defect pattern"));
    assert!(!system.contains("critical/major"));
}
