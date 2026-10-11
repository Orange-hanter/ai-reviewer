//! Golden-харнесс пайплайна (RFC 16, решение 7; задача T09 — фикстуры, T25/T26 — сверка).
//!
//! Эталон поведения `pipeline.mjs` снят генератором `rust/parity/pipeline-golden-generate.mjs`
//! ДО удаления JS-пайплайна и лежит в `rust/parity/golden-fixtures/`. Node в рантайме теста
//! не нужен: тест читает готовые файлы.
//!
//! Здесь есть две части:
//!   1. `golden_fixtures_load` — загрузчик фикстур, гоняется всегда. Он же держит контракт
//!      каталога: если формат уедет, тест упадёт раньше, чем появится `pipeline::run`.
//!   2. `pipeline_run_matches_golden` — сама сверка (T26); с T25 идёт в обычном `cargo test`.
//!
//! Формат фикстур описан в `rust/parity/README-golden.md`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Структуры фикстуры
// ---------------------------------------------------------------------------

/// `golden-fixtures/index.json` — реестр кейсов и глобальные константы прогона.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct GoldenIndex {
    pub generator: String,
    /// Замороженные часы генератора: всё, что зовёт `Date.now()`/`new Date()`, видит их.
    pub fixed_now: String,
    pub head_sha: String,
    /// Плейсхолдер, которым в `env.json` подменён временный каталог экспорта.
    pub export_dir_placeholder: String,
    pub cases: Vec<GoldenIndexEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct GoldenIndexEntry {
    pub id: String,
    pub description: String,
    pub exit_code: i32,
}

/// `env.json` — вход конфигурации: env + разобранный CLI + зафиксированные часы.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct GoldenEnv {
    pub description: String,
    /// Ровно тот объект, что уходил в `loadReviewConfig(env, cli)`.
    pub env: BTreeMap<String, String>,
    pub cli: GoldenCli,
    pub fixed_now: String,
    /// true — golden снят с чистой JS-ветки (`AI_REVIEWER_*_FORCE_JS=1`).
    pub force_js: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct GoldenCli {
    pub dry_run: bool,
    pub diff_file: Option<String>,
    pub repo: String,
    pub pr_number: u64,
}

/// `inputs.json` — ответы всех подменённых зависимостей за прогон.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct GoldenInputs {
    pub load_diff: GoldenLoadDiff,
    pub prior_sticky: GoldenPriorSticky,
    /// Результат `prescan.run()` целиком (findings/scanned/byTool/promptSection/…).
    pub prescan: Value,
    /// Результат `graphify.buildBlastRadius()`: `{ blocks, metrics }`.
    pub blast_radius: Value,
    /// Ответ `engine.reviewAllModels()`: по элементу на модель (или `{ model, error }`).
    pub reviews: Vec<Value>,
    pub verifier: GoldenVerifier,
    pub github_results: GoldenGithubResults,
    pub telegram: GoldenTelegram,
    /// Дерево репозитория для FP-семейства 7 (`deps.repoFiles`, `lib/absent-path.mjs`):
    /// путь на элемент. `None` — семейство выключено (эталон не зависит от `git ls-files`).
    #[serde(default)]
    pub repo_files: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct GoldenLoadDiff {
    pub pr: GoldenPr,
    pub head_sha: String,
    pub draft: bool,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GoldenPr {
    pub number: u64,
    pub title: String,
    /// Ключ в JSON именно `html_url` — как его отдаёт GitHub API и читает pipeline.
    pub html_url: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GoldenPriorSticky {
    pub findings: Vec<Value>,
    pub runs: Vec<Value>,
    pub sha: String,
}

/// Режим фейкового `engine.verifyFindings`:
/// `none` — verifier выключен; `static` — отдать `verdicts` как есть;
/// `all-verified` — по вердикту `verified` на каждого кандидата (кап/overflow-ветка).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GoldenVerifier {
    pub mode: String,
    #[serde(default)]
    pub verdicts: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct GoldenGithubResults {
    /// Что вернул `postReview` — этот id пайплайн передаёт в `deleteStaleInline`.
    pub post_review_id: i64,
    pub delete_stale_inline: i64,
    pub upsert_sticky: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GoldenTelegram {
    pub enabled: bool,
}

/// `expected/` — эталонный выход прогона.
#[derive(Debug)]
#[allow(dead_code)]
pub struct GoldenExpected {
    pub exit_code: i32,
    pub calls: GoldenCalls,
    /// Тело последнего `upsertSticky`; `None` — sticky не обновлялся.
    pub sticky: Option<String>,
    /// `<toolName>.json` из `writeReviewExports` — байт в байт.
    pub export_json: Option<Value>,
    pub sarif_json: Option<Value>,
    pub swarm_json: Option<Value>,
}

/// `expected/calls.json` — порядок и ключевые аргументы вызовов фейков (без тел).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct GoldenCalls {
    /// Последовательность наблюдаемых эффектов: `post` / `delete` / `sticky` / `telegram`.
    pub order: Vec<String>,
    pub engine_runs: u32,
    pub verifier_runs: u32,
    pub load_diff: u32,
    pub read_prior_sticky_data: u32,
    pub prescan_runs: u32,
    pub blast_runs: u32,
    pub post_review: Vec<GoldenPostReviewCall>,
    pub delete_stale_inline: Vec<Value>,
    pub upsert_sticky: Vec<GoldenStickyCall>,
    pub telegram: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct GoldenPostReviewCall {
    pub head_sha: String,
    pub inline: Vec<GoldenInlineComment>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GoldenInlineComment {
    pub path: String,
    pub line: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GoldenStickyCall {
    /// Размер тела в байтах UTF-8; само тело — в `expected/sticky.md`.
    pub bytes: u64,
}

/// Один кейс целиком: вход (diff/env/inputs) + ожидаемый выход.
#[derive(Debug)]
#[allow(dead_code)]
pub struct GoldenCase {
    pub id: String,
    pub dir: PathBuf,
    pub diff: String,
    pub env: GoldenEnv,
    pub inputs: GoldenInputs,
    pub expected: GoldenExpected,
}

// ---------------------------------------------------------------------------
// Загрузчик
// ---------------------------------------------------------------------------

/// Каталог `rust/parity/golden-fixtures` относительно манифеста crate.
fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("parity/golden-fixtures")
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("не читается {}: {e}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    let raw = read_to_string(path);
    serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("не разбирается {}: {e}", path.display()))
}

fn read_json_opt(path: &Path) -> Option<Value> {
    if path.exists() {
        Some(read_json(path))
    } else {
        None
    }
}

pub fn load_index() -> GoldenIndex {
    read_json(&golden_root().join("index.json"))
}

pub fn load_case(id: &str) -> GoldenCase {
    let dir = golden_root().join(id);
    assert!(dir.is_dir(), "нет каталога кейса {}", dir.display());
    let expected_dir = dir.join("expected");

    let exit_code: i32 = read_to_string(&expected_dir.join("exit-code.txt"))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("exit-code.txt кейса {id}: {e}"));

    let sticky_path = expected_dir.join("sticky.md");
    let expected = GoldenExpected {
        exit_code,
        calls: read_json(&expected_dir.join("calls.json")),
        sticky: sticky_path.exists().then(|| read_to_string(&sticky_path)),
        export_json: read_json_opt(&expected_dir.join("export.json")),
        sarif_json: read_json_opt(&expected_dir.join("sarif.json")),
        swarm_json: read_json_opt(&expected_dir.join("swarm.json")),
    };

    GoldenCase {
        id: id.to_string(),
        diff: read_to_string(&dir.join("diff.patch")),
        env: read_json(&dir.join("env.json")),
        inputs: read_json(&dir.join("inputs.json")),
        expected,
        dir,
    }
}

pub fn load_all_cases() -> Vec<GoldenCase> {
    load_index()
        .cases
        .iter()
        .map(|entry| load_case(&entry.id))
        .collect()
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

/// Контракт каталога фикстур: он читается целиком и внутренне согласован.
/// Гоняется всегда — до появления `pipeline::run` это единственная защита формата.
#[test]
fn golden_fixtures_load() {
    let index = load_index();
    assert!(
        index.cases.len() >= 10,
        "ожидали ≥10 golden-кейсов, нашли {}",
        index.cases.len()
    );

    for case in load_all_cases() {
        let entry = index
            .cases
            .iter()
            .find(|e| e.id == case.id)
            .expect("кейс есть в index.json");
        assert_eq!(
            case.expected.exit_code, entry.exit_code,
            "{}: exit-code.txt и index.json разошлись",
            case.id
        );
        assert!(!case.diff.is_empty(), "{}: пустой diff.patch", case.id);
        assert_eq!(
            case.env.fixed_now, index.fixed_now,
            "{}: часы кейса не совпадают с index.json",
            case.id
        );
        assert!(
            case.env.force_js,
            "{}: golden снят не с JS-ветки — эталон недействителен",
            case.id
        );
        assert!(
            case.env.env.contains_key("REVIEW_PROFILE"),
            "{}: env без REVIEW_PROFILE — прогон невоспроизводим",
            case.id
        );
        assert_eq!(
            case.env.env.get("REVIEW_EXPORT_DIR").map(String::as_str),
            Some(index.export_dir_placeholder.as_str()),
            "{}: REVIEW_EXPORT_DIR должен быть плейсхолдером, а не реальным путём",
            case.id
        );
        // sticky и артефакт экспорта — два независимых читателя одного набора находок;
        // кейс без обоих не проверяет ничего.
        assert!(
            case.expected.sticky.is_some() || case.expected.export_json.is_some(),
            "{}: ни sticky.md, ни export.json — нечего сверять",
            case.id
        );
        // Число upsertSticky в calls.json и наличие тела обязаны сходиться.
        assert_eq!(
            case.expected.calls.upsert_sticky.is_empty(),
            case.expected.sticky.is_none(),
            "{}: calls.json и sticky.md расходятся по факту вызова upsertSticky",
            case.id
        );
        if let (Some(body), Some(call)) =
            (&case.expected.sticky, case.expected.calls.upsert_sticky.last())
        {
            assert_eq!(
                body.len() as u64,
                call.bytes,
                "{}: размер sticky.md не равен зафиксированному в calls.json",
                case.id
            );
        }
    }
}

/// Прогон одного кейса через `ai_reviewer::pipeline` на фейках из `inputs.json`.
/// Возвращает список расхождений (пусто — кейс сошёлся).
fn run_case(case: &GoldenCase, fixed_now_ms: u64) -> Vec<String> {
    use ai_reviewer::engine::model_review_from_value;
    use ai_reviewer::pipeline::{create_review_pipeline, load_review_config, CliArgs, PipelineOptions, RepoFilesFn};
    use ai_reviewer::profile::{package_root, Profile};
    use ai_reviewer::testutil::FakeDeps;
    use ai_reviewer::traits::{BlastRadius, LoadedDiff, PrescanResult, PullRequestRef, StickyData};

    let root = package_root();
    let export_dir = std::env::temp_dir().join(format!(
        "golden-{}-{}-{}",
        case.id,
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&export_dir).unwrap();
    let env: std::collections::HashMap<String, String> = case
        .env
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("${EXPORT_DIR}", &export_dir.to_string_lossy())))
        .collect();
    let profile = Profile::load_file_at(&root.join(&env["REVIEW_PROFILE"]), &root).unwrap();
    let cli = CliArgs {
        dry_run: case.env.cli.dry_run,
        diff_file: case.env.cli.diff_file.clone(),
        head_sha: String::new(),
        repo: case.env.cli.repo.clone(),
        pr_number: case.env.cli.pr_number,
    };
    let cfg = load_review_config(&env, &cli, &profile);

    let mut fakes = FakeDeps::new();
    let ld = &case.inputs.load_diff;
    fakes.github.loaded_diff = Some(LoadedDiff {
        draft: ld.draft,
        pr: PullRequestRef { number: ld.pr.number, title: ld.pr.title.clone(), html_url: ld.pr.html_url.clone() },
        head_sha: ld.head_sha.clone(),
        diff: case.diff.clone(),
    });
    fakes.github.prior_sticky = StickyData {
        findings: case.inputs.prior_sticky.findings.clone(),
        runs: case.inputs.prior_sticky.runs.clone(),
        sha: case.inputs.prior_sticky.sha.clone(),
    };
    let gr = &case.inputs.github_results;
    fakes.github.post_review_id = u64::try_from(gr.post_review_id).ok();
    fakes.github.deleted_inline = gr.delete_stale_inline.max(0) as usize;
    fakes.github.upsert_result = gr.upsert_sticky.clone();
    // JS-фейк кладёт `skipped: 0` (число) — контракт трейта по реализации, а не по фейку.
    let mut prescan = case.inputs.prescan.clone();
    if prescan.get("skipped").is_some_and(|v| !v.is_string()) {
        prescan.as_object_mut().unwrap().remove("skipped");
    }
    fakes.prescan.result = serde_json::from_value::<PrescanResult>(prescan).unwrap_or_default();
    fakes.graphify.radius = serde_json::from_value::<BlastRadius>(case.inputs.blast_radius.clone()).unwrap_or_default();
    fakes.engine.reviews = case.inputs.reviews.iter().map(model_review_from_value).collect();
    fakes.engine.verdicts = match case.inputs.verifier.mode.as_str() {
        "static" => case.inputs.verifier.verdicts.clone().unwrap_or_default(),
        "all-verified" => (0..64).map(|i| serde_json::json!({ "candidateIndex": i, "decision": "verified" })).collect(),
        _ => Vec::new(),
    };
    let tree = case.inputs.repo_files.clone();
    let repo_files = move || -> anyhow::Result<Option<Vec<String>>> { Ok(tree.clone()) };
    let repo_files: &RepoFilesFn = &repo_files;
    let clock = move || fixed_now_ms;
    let deps = if case.inputs.telegram.enabled { fakes.deps() } else { fakes.deps_without_notifier() };
    let opts = PipelineOptions {
        repo_files: Some(repo_files),
        clock: Some(&clock),
        cwd: Some(root.clone()),
        ..PipelineOptions::default()
    };
    let mut diffs = Vec::new();
    match create_review_pipeline(&cfg, &profile, deps, opts).run_detailed() {
        Err(e) => diffs.push(format!("прогон упал: {e:#}")),
        Ok(out) => {
            if out.exit_code != case.expected.exit_code {
                diffs.push(format!("exit {} ≠ {}", out.exit_code, case.expected.exit_code));
            }
        }
    }
    let order: Vec<String> = fakes
        .calls
        .tags()
        .into_iter()
        .filter(|t| matches!(*t, "post" | "delete" | "sticky" | "telegram"))
        .map(str::to_string)
        .collect();
    if order != case.expected.calls.order {
        diffs.push(format!("order {order:?} ≠ {:?}", case.expected.calls.order));
    }
    let sticky = fakes.calls.sticky_bodies().last().cloned();
    if sticky != case.expected.sticky {
        if let Ok(dump) = std::env::var("GOLDEN_DUMP_DIR") {
            let d = Path::new(&dump).join(&case.id);
            let _ = fs::create_dir_all(&d);
            let _ = fs::write(d.join("sticky.md"), sticky.clone().unwrap_or_default());
        }
        diffs.push("sticky.md расходится".to_string());
    }
    let tool = &profile.tool_name;
    for (name, file, want) in [
        ("export.json", format!("{tool}.json"), &case.expected.export_json),
        ("sarif.json", format!("{tool}.sarif.json"), &case.expected.sarif_json),
        ("swarm.json", format!("{tool}-swarm.json"), &case.expected.swarm_json),
    ] {
        let got: Option<Value> =
            fs::read_to_string(export_dir.join(file)).ok().and_then(|t| serde_json::from_str(&t).ok());
        if &got != want {
            if let Ok(dump) = std::env::var("GOLDEN_DUMP_DIR") {
                let d = Path::new(&dump).join(&case.id);
                let _ = fs::create_dir_all(&d);
                let _ = fs::write(d.join(name), serde_json::to_string_pretty(&got).unwrap_or_default());
            }
            diffs.push(format!("{name} расходится"));
        }
    }
    let _ = fs::remove_dir_all(&export_dir);
    diffs
}

/// Сверка Rust-пайплайна с эталоном: все кейсы обязаны сходиться (T25).
/// Подробный вывод: `cargo test --test pipeline_golden -- --nocapture`
/// печатает, какие кейсы и по каким выходам расходятся.
#[test]
fn pipeline_run_matches_golden() {
    let index = load_index();
    let fixed_now_ms = 1_767_225_600_000u64; // 2026-01-01T00:00:00.000Z
    assert_eq!(index.fixed_now, "2026-01-01T00:00:00.000Z", "часы генератора сменились — поправь fixed_now_ms");
    let cases = load_all_cases();
    assert!(!cases.is_empty(), "golden-фикстуры не найдены");
    let mut failed = Vec::new();
    for case in &cases {
        let diffs = run_case(case, fixed_now_ms);
        if diffs.is_empty() {
            println!("ok   {}", case.id);
        } else {
            println!("FAIL {}: {}", case.id, diffs.join("; "));
            failed.push(case.id.clone());
        }
    }
    assert!(failed.is_empty(), "golden расходится: {}/{} кейсов — {failed:?}", failed.len(), cases.len());
}
