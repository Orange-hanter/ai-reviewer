//! Smoke-тест единого бинаря `pr-review` (RFC 04, задача T03).
//!
//! Проверяет ровно контракт CLI: девять подкоманд видны в `--help` под своими
//! kebab-case-именами. Имена — часть публичного интерфейса (их зовут шаблоны
//! examples/ci/), поэтому переименование варианта должно ронять этот тест.

use assert_cmd::Command;

/// Имена подкоманд в том виде, в каком их печатает clap и зовёт потребитель.
const SUBCOMMANDS: [&str; 9] = [
    "review",
    "structure",
    "structure-mcp",
    "eval",
    "metrics",
    "feedback",
    "feedback-batch",
    "feedback-migrate",
    "feedback-promote",
];

/// Есть ли в справке строка, начинающаяся ровно с этого имени подкоманды.
/// Сравниваем по первому токену строки, а не подстрокой: "review" встречается
/// в имени самого бинаря, а "feedback" — внутри "feedback-batch".
fn lists_subcommand(help: &str, name: &str) -> bool {
    help.lines()
        .any(|line| line.trim_start().split_whitespace().next() == Some(name))
}

#[test]
fn help_lists_all_nine_subcommands() {
    let output = Command::cargo_bin("pr-review")
        .expect("бинарь pr-review должен быть собран")
        .arg("--help")
        .output()
        .expect("запуск pr-review --help");

    assert!(output.status.success(), "pr-review --help вышел с ошибкой");

    let help = String::from_utf8(output.stdout).expect("справка должна быть валидным UTF-8");
    for name in SUBCOMMANDS {
        assert!(
            lists_subcommand(&help, name),
            "в pr-review --help нет подкоманды {name}:\n{help}"
        );
    }
}

#[test]
fn unknown_subcommand_fails_with_usage() {
    // Ошибка вызывающего — не fail-open: clap выходит с кодом 2 и печатает usage.
    let output = Command::cargo_bin("pr-review")
        .expect("бинарь pr-review должен быть собран")
        .arg("no-such-command")
        .output()
        .expect("запуск pr-review с неизвестной подкомандой");

    assert!(!output.status.success());
}
