//! Загрузка LLM-free golden fixtures из `fixtures/<case>/`.
//!
//! Формат зеркалит lib/eval.mjs::loadFixtures: каждый подкаталог с
//! meta.json + diff.patch + findings.json — кейс; verifier.json опционален.
//! Неполные каталоги пропускаются (например fixtures/structure/ без meta.json).

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct Meta {
    pub id: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub expected: Value,
    #[serde(default)]
    pub gate: Value,
}

#[derive(Debug, Deserialize)]
pub struct FindingsPayload {
    #[serde(default)]
    pub findings: Vec<Value>,
}

#[derive(Debug, Deserialize)]
pub struct VerifierPayload {
    #[serde(default)]
    pub verdicts: Vec<Value>,
}

/// Один загруженный fixture-кейс (diff пока не парсится — Фаза 1).
#[derive(Debug)]
pub struct FixtureCase {
    pub id: String,
    pub dir: PathBuf,
    pub description: String,
    pub diff: String,
    pub findings: Vec<Value>,
    pub expected: Value,
    pub gate: Value,
    pub verifier_verdicts: Option<Vec<Value>>,
    /// `repo-files.txt` (путь на строку) — дерево репозитория для FP-семейства 7.
    pub repo_files: Option<Vec<String>>,
}

/// Загружает все валидные кейсы из `fixturesDir`, отсортированные по id.
/// Зеркалит lib/eval.mjs::loadFixtures: неполные каталоги пропускаются.
pub fn load_fixtures(fixtures_dir: &Path) -> std::io::Result<Vec<FixtureCase>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(fixtures_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(out),
    };
    for ent in entries.flatten() {
        if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let dir = ent.path();
        let Some(case) = load_case(&dir).ok().flatten() else {
            continue;
        };
        out.push(case);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn load_case(dir: &Path) -> anyhow::Result<Option<FixtureCase>> {
    let read_json = |name: &str| -> anyhow::Result<Value> {
        Ok(serde_json::from_str(&fs::read_to_string(dir.join(name))?)?)
    };

    // Любая ошибка чтения/парса — пропускаем каталог (как catch в JS).
    let (meta, diff, findings_payload) = match (
        read_json("meta.json"),
        fs::read_to_string(dir.join("diff.patch")),
        read_json("findings.json"),
    ) {
        (Ok(meta), Ok(diff), Ok(findings)) => (meta, diff, findings),
        _ => return Ok(None),
    };

    let meta: Meta = serde_json::from_value(meta)?;
    let findings_payload: FindingsPayload = serde_json::from_value(findings_payload)?;

    let verifier_verdicts = read_json("verifier.json")
        .ok()
        .and_then(|v| serde_json::from_value::<VerifierPayload>(v).ok())
        .map(|p| p.verdicts);

    // Дерево репозитория потребителя (по пути на строку) — для FP-семейства 7.
    let repo_files = fs::read_to_string(dir.join("repo-files.txt")).ok().map(|t| {
        t.split('\n')
            .map(crate::findings::js_trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    });

    let dir_name = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    Ok(Some(FixtureCase {
        id: meta.id.unwrap_or(dir_name),
        dir: dir.to_path_buf(),
        description: meta.description.unwrap_or_default(),
        diff,
        findings: findings_payload.findings,
        expected: meta.expected,
        gate: meta.gate,
        verifier_verdicts,
        repo_files,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Корень репо относительно этого манифеста (rust/ лежит в корне).
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("rust/ должен лежать в корне репо")
            .to_path_buf()
    }

    #[test]
    fn loads_real_fixtures_from_repo_root() {
        let cases = load_fixtures(&repo_root().join("fixtures")).unwrap();
        // Фикстуры = спека; сейчас валидных кейсов 16 (fixtures/structure без meta.json пропускается).
        assert!(!cases.is_empty(), "не загрузился ни один fixture-кейс");
        // id отсортированы
        let mut ids: Vec<&str> = cases.iter().map(|c| c.id.as_str()).collect();
        let sorted = {
            let mut s = ids.clone();
            s.sort();
            s
        };
        assert_eq!(ids, sorted);
        // Каждый кейс несёт diff и findings (формат совпадает с lib/eval.mjs)
        for c in &cases {
            assert!(!c.diff.is_empty(), "{}: пустой diff.patch", c.id);
            assert!(!c.findings.is_empty(), "{}: пустые findings", c.id);
        }
        ids.dedup();
        assert_eq!(ids.len(), cases.len(), "дубликаты id");
    }

    #[test]
    fn skips_incomplete_dirs() {
        let cases = load_fixtures(&repo_root().join("fixtures")).unwrap();
        assert!(
            cases.iter().all(|c| c.id != "structure"),
            "каталог без meta.json не должен загружаться"
        );
    }

    #[test]
    fn missing_dir_yields_empty_list() {
        let cases = load_fixtures(Path::new("/nonexistent/fixtures")).unwrap();
        assert!(cases.is_empty());
    }

    #[test]
    fn parses_expected_contract_fields() {
        let cases = load_fixtures(&repo_root().join("fixtures")).unwrap();
        let gate = cases
            .iter()
            .find(|c| c.id == "gate-critical-high-conf")
            .expect("fixture gate-critical-high-conf");
        assert_eq!(gate.expected["shouldBlock"], serde_json::json!(true));
        let verifier = cases
            .iter()
            .find(|c| c.id == "verifier-keeps-block")
            .expect("fixture verifier-keeps-block");
        assert!(verifier.verifier_verdicts.is_some());
    }
}
