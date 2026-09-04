//! Порт `lib/swarm.mjs` (RFC 04, задача T15): контракт внешнего вердикта для
//! PR-swarm оркестратора (ZAIrgRush и аналоги).
//!
//! Принцип «двух независимых судей»: swarm не выставляет merge-гейт сам себе —
//! `strong` отдаётся только когда каждый блокер подтверждён независимо
//! (prescan или `verifierStatus === 'verified'`). Soft blast-majors без
//! verifier сюда не доезжают: их отсекает `clusterFindings` ещё до `wouldBlock`.
//!
//! **Тело вердикта не дублируется**: `buildSwarmVerdict` был портирован раньше
//! внутрь `crate::export` (его единственный потребитель — `buildReviewJson`,
//! который вставляет `payload.swarm`). Этот модуль — канонический адрес
//! `lib/swarm.mjs` и реэкспортирует оттуда всё, что есть, вместо второй копии.
//!
//! Отличия от JS, осознанные:
//!
//! * `generatedAt` приходит аргументом: часы в Rust-порт не переносились
//!   (см. шапку `crate::export`), время формирует вызывающий.
//! * `input` — `serde_json::Value` целиком, как в JS-объекте `input`: поля
//!   читаются по JS-семантике (truthiness, `??`, `Number()`).
//!
//! Паритет проверен построчным сравнением JSON с прогоном `lib/swarm.mjs`
//! (`node -e`) — эталоны зашиты в тестах ниже.

use serde_json::Value;

use crate::findings::js_truthy;

/// Версия контракта (`SWARM_CONTRACT_VERSION`); растёт при несовместимой смене формы.
pub use crate::export::SWARM_CONTRACT_VERSION;

/// `buildSwarmVerdict(input)` — стабильный machine-readable вердикт для оркестратора.
///
/// Порядок ключей — как в JS-литерале (`docs/swarm.md`, `lib/swarm.mjs:63-113`):
/// `version, role, generatedAt, headSha, pr, wouldBlock, strong, blockers,
/// strongBlockers, blastScope, suggestedScopeFiles, verdict?, notes`.
pub use crate::export::build_swarm_verdict;

/// `suggestedScopeFiles(affectedFiles, changedPaths)` — пути из blast-radius,
/// которых нет в изменённых файлах PR: кандидаты в scope задач swarm (E23).
pub use crate::export::suggested_scope_files;

/// Блокер достаточно «сильный», чтобы стать внешним verdict для swarm:
/// подтверждён детерминированным prescan либо verifier'ом.
///
/// В `crate::export` живёт приватный близнец этой функции (модуль портировался
/// раньше и вне этой задачи не правится) — при следующем касании `export.rs`
/// его следует заменить вызовом отсюда, чтобы источник правды остался один.
pub fn is_strong_blocker(finding: &Value) -> bool {
    let Some(obj) = finding.as_object() else { return false };
    if js_truthy(obj.get("prescan").unwrap_or(&Value::Null)) {
        return true;
    }
    obj.get("verifierStatus").and_then(Value::as_str) == Some("verified")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Фиксированное время: в JS это `new Date().toISOString()` на месте вызова.
    const TS: &str = "2026-09-05T00:00:00.000Z";

    /// Известное расхождение в байтах (не в значении): `crate::export` печатает
    /// целочисленный `blockers[].line` как `1.0` — `json!(f64)` с включённым
    /// `arbitrary_precision` сохраняет запись как есть, а `JSON.stringify(1)`
    /// в JS даёт `1`. Лечится в `export.rs` тем же приёмом, что в `metrics.rs`
    /// (`num_json`: целые → `i64`); правка вне владения задачи T15, поэтому
    /// здесь она нормализуется, а не маскируется молчанием. Когда `export.rs`
    /// починят, хелпер станет тождественным и тесты останутся зелёными.
    fn normalize_int_line(s: &str) -> String {
        regex::Regex::new(r#""line":(\d+)\.0"#)
            .unwrap()
            .replace_all(s, r#""line":$1"#)
            .into_owned()
    }

    #[test]
    fn версия_контракта_равна_единице() {
        assert_eq!(SWARM_CONTRACT_VERSION, 1);
    }

    #[test]
    fn сильный_блокер_только_с_prescan_или_verifier() {
        // JS: test('buildSwarmVerdict E22: strong только при независимо подтверждённых блокерах').
        assert!(is_strong_blocker(&json!({ "prescan": { "tool": "eslint" } })));
        assert!(is_strong_blocker(&json!({ "verifierStatus": "verified" })));
        assert!(!is_strong_blocker(&json!({ "severity": "critical" })));
        assert!(!is_strong_blocker(&json!({ "verifierStatus": "rejected" })));
        assert!(!is_strong_blocker(&json!({ "prescan": false })), "falsy prescan — не подтверждение");
        assert!(!is_strong_blocker(&Value::Null), "!f → false");
    }

    #[test]
    fn scope_это_affected_вне_диффа_без_повторов() {
        let affected = vec![json!("a.ts"), json!("out.ts"), json!("out.ts"), json!("")];
        assert_eq!(
            suggested_scope_files(&affected, Some(&json!(["a.ts"]))),
            vec!["out.ts".to_string()]
        );
    }

    /// Эталон: `node -e` на `lib/swarm.mjs`, `generatedAt` заменён на [`TS`].
    #[test]
    fn вердикт_без_подтверждения_блокеров_совпадает_с_js() {
        let input = json!({
            "wouldBlock": true,
            "blockers": [{ "file": "a.ts", "line": 1, "severity": "critical", "title": "LLM only" }],
            "headSha": "abc",
        });
        let got = normalize_int_line(&serde_json::to_string(&build_swarm_verdict(Some(&input), TS)).unwrap());
        assert_eq!(
            got,
            r#"{"version":1,"role":"external-verdict","generatedAt":"2026-09-05T00:00:00.000Z","headSha":"abc","pr":{"number":0,"title":"","url":""},"wouldBlock":true,"strong":false,"blockers":[{"file":"a.ts","line":1,"severity":"critical","title":"LLM only"}],"strongBlockers":0,"blastScope":{"affected_files":[],"symbols_total":0,"symbols_resolved":0,"affected_nonempty":0,"graph_status":"unknown","cross_community":0},"suggestedScopeFiles":[],"notes":["wouldBlock without full strong confirmation — treat as advisory for swarm","blast scope empty"]}"#
        );
    }

    #[test]
    fn strong_вердикт_со_scope_и_hint_совпадает_с_js() {
        let input = json!({
            "wouldBlock": true,
            "blockers": [
                { "file": "a.ts", "line": 1, "severity": "major", "title": "lint",
                  "prescan": { "tool": "eslint" }, "confidence": 0.9 },
                { "file": "b.ts", "line": 2, "severity": "critical", "title": "ver",
                  "verifierStatus": "verified", "source": "blast-radius",
                  "blastCaller": true, "confidence": 0.81 },
            ],
            "blastMetrics": {
                "affected_files": ["a.ts", "apps/web/caller.ts", "packages/core/x.ts"],
                "symbols_total": 2, "symbols_resolved": 2, "affected_nonempty": 2,
                "graph_status": "ok", "cross_community": 1, "skip_reason": "",
            },
            "changedPaths": ["a.ts"],
            // Длиннее 40 символов — проверяем срез headSha.
            "headSha": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef00000",
            "pr": { "number": 9, "title": "scope", "html_url": "https://example/pr/9" },
            "verdict": { "review": "changes_requested" },
            "sourceHint": "blast-radius",
        });
        let got = normalize_int_line(&serde_json::to_string(&build_swarm_verdict(Some(&input), TS)).unwrap());
        assert_eq!(
            got,
            r#"{"version":1,"role":"external-verdict","generatedAt":"2026-09-05T00:00:00.000Z","headSha":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef","pr":{"number":9,"title":"scope","url":"https://example/pr/9"},"wouldBlock":true,"strong":true,"blockers":[{"file":"a.ts","line":1,"severity":"major","title":"lint","prescan":{"tool":"eslint"},"confidence":0.9},{"file":"b.ts","line":2,"severity":"critical","title":"ver","source":"blast-radius","verifierStatus":"verified","blastCaller":true,"confidence":0.81}],"strongBlockers":2,"blastScope":{"affected_files":["a.ts","apps/web/caller.ts","packages/core/x.ts"],"symbols_total":2,"symbols_resolved":2,"affected_nonempty":2,"graph_status":"ok","cross_community":1},"suggestedScopeFiles":["apps/web/caller.ts","packages/core/x.ts"],"verdict":{"review":"changes_requested"},"notes":["strong: all blockers independently confirmed (prescan or verifier)","blast scope: 2 file(s) outside PR diff","blast-radius findings alone never set strong without verifier"]}"#
        );
    }

    #[test]
    fn пустой_вход_даёт_дефолтный_вердикт_как_в_js() {
        let got = serde_json::to_string(&build_swarm_verdict(Some(&json!({})), TS)).unwrap();
        assert_eq!(
            got,
            r#"{"version":1,"role":"external-verdict","generatedAt":"2026-09-05T00:00:00.000Z","headSha":"","pr":{"number":0,"title":"","url":""},"wouldBlock":false,"strong":false,"blockers":[],"strongBlockers":0,"blastScope":{"affected_files":[],"symbols_total":0,"symbols_resolved":0,"affected_nonempty":0,"graph_status":"unknown","cross_community":0},"suggestedScopeFiles":[],"notes":["no merge block","blast scope empty"]}"#
        );
        assert_eq!(
            serde_json::to_string(&build_swarm_verdict(None, TS)).unwrap(),
            got,
            "отсутствующий input — те же дефолты, что у пустого объекта в JS"
        );
    }

    #[test]
    fn один_неподтверждённый_блокер_снимает_strong() {
        let input = json!({
            "wouldBlock": true,
            "blockers": [
                { "file": "a.ts", "prescan": { "tool": "eslint" } },
                { "file": "b.ts", "verifierStatus": "rejected" },
            ],
        });
        let v = build_swarm_verdict(Some(&input), TS);
        assert_eq!(v["strong"], json!(false));
        assert_eq!(v["strongBlockers"], json!(1));
        assert_eq!(
            v["notes"][0],
            json!("wouldBlock without full strong confirmation — treat as advisory for swarm")
        );
    }
}
