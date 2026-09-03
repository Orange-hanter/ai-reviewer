//! Харнес-скелет eval: читает fixtures и печатает список кейсов + сводку.
//!
//! Сравнение findings с expected — пока stub.
//! TODO(phase-1): после порта diff/findings (lib/diff.mjs, lib/findings.mjs)
//! реализовать runFixtureCase и computeEvalMetrics по аналогии с lib/eval.mjs.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use ai_reviewer::fixtures::{load_fixtures, FixtureCase};

#[derive(Parser)]
#[command(name = "eval", about = "Rust eval harness (Фаза 0: загрузка fixtures, сравнение — stub)")]
struct Cli {
    /// Корень репо ai-reviewer (где лежит fixtures/).
    /// По умолчанию — <CARGO_MANIFEST_DIR>/../..
    #[arg(default_value = None)]
    repo_root: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let repo_root = cli.repo_root.unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("rust/ должен лежать в корне репо")
            .to_path_buf()
    });

    let fixtures_dir = repo_root.join("fixtures");
    let cases = load_fixtures(&fixtures_dir)
        .with_context(|| format!("чтение fixtures из {}", fixtures_dir.display()))?;

    println!("=== rust eval harness (Фаза 0, сравнение — stub) ===");
    println!("repo root: {}", repo_root.display());
    println!("cases: {}\n", cases.len());

    for case in &cases {
        print_case(case);
    }

    let with_verifier = cases.iter().filter(|c| c.verifier_verdicts.is_some()).count();
    println!("---");
    println!("Итого кейсов: {}", cases.len());
    println!("  с verifier.json: {with_verifier}");
    println!("  findings всего: {}", cases.iter().map(|c| c.findings.len()).sum::<usize>());
    println!();
    println!("TODO(phase-1): реальное сравнение findings с expected появится после");
    println!("порта lib/diff.mjs и lib/findings.mjs в Rust-модули.");

    Ok(())
}

fn print_case(case: &FixtureCase) {
    let expected = &case.expected;
    let contract = [
        ("shouldBlock", &expected["shouldBlock"]),
        ("blockerCount", &expected["blockerCount"]),
        ("findingCount", &expected["findingCount"]),
    ]
    .into_iter()
    .filter_map(|(k, v)| (!v.is_null()).then(|| format!("{k}={v}")))
    .collect::<Vec<_>>()
    .join(", ");

    println!("· {}", case.id);
    if !case.description.is_empty() {
        println!("  {}", case.description);
    }
    println!(
        "  findings: {}, diff: {} bytes, expected: {{{}}}",
        case.findings.len(),
        case.diff.len(),
        if contract.is_empty() { "—".into() } else { contract },
    );
    if case.verifier_verdicts.is_some() {
        println!("  verifier: verdicts present");
    }
    println!("  status: STUB (сравнение не реализовано)");
    println!();
}
