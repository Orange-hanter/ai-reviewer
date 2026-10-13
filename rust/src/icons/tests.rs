//! Порт `test/icons.test.mjs` + граничные случаи регулярок JS.

use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;
use crate::traits::InlineComment;

const BASE: &str = "https://icons.example/v1/icons";

fn img(name: &str, alt: &str, h: u32) -> String {
    format!(r#"<img src="{BASE}/{name}.svg" alt="{alt}" height="{h}">"#)
}

#[test]
fn iconize_пустая_база_без_изменений() {
    let md = "## 🤖 Авто код-ревью\n\n> ⛔ **Merge заблокирован**";
    assert_eq!(iconize(md, ""), md);
    assert_eq!(iconize(md, "  /// "), md, "после trim и среза слэшей база пустая");
}

#[test]
fn iconize_заголовок_крупнее_строки_хвостовой_слэш_срезается() {
    let md = "## 🤖 Авто код-ревью — 🔴 нужны правки\n\n> ⛔ **Merge заблокирован:** 2.";
    assert_eq!(
        iconize(md, &format!("{BASE}/")),
        format!(
            "## {} Авто код-ревью — {} нужны правки\n\n> {} **Merge заблокирован:** 2.",
            img("bot", "🤖", 20),
            img("sev-critical", "🔴", 20),
            img("gate-blocked", "⛔", 16)
        )
    );
    // Заголовок внутри цитаты — тоже 20; семь решёток — не заголовок.
    assert_eq!(iconize("> ### ✅", BASE), format!("> ### {}", img("gate-open", "✅", 20)));
    assert_eq!(iconize("####### ✅", BASE), format!("####### {}", img("gate-open", "✅", 16)));
}

#[test]
fn iconize_весы_с_селектором_и_без_один_знак() {
    assert_eq!(
        iconize("⚖\u{FE0F} a ⚖ b", BASE),
        format!("{} a {} b", img("jury", "⚖\u{FE0F}", 16), img("jury", "⚖\u{FE0F}", 16))
    );
}

#[test]
fn iconize_html_комментарии_не_трогаются() {
    let footer = r#"<!-- ai-review-data:{"findings":[{"title":"🔴 IDOR ✅"}]} -->"#;
    let md = format!("<!-- ai-review -->\n## 🤖 Ревью\n\n{footer}");
    let out = iconize(&md, BASE);
    assert_eq!(out, format!("<!-- ai-review -->\n## {} Ревью\n\n{footer}", img("bot", "🤖", 20)));
}

#[test]
fn iconize_fenced_и_инлайн_код_буквальны() {
    let md = "🟡 **minor: x** в `a ✅ b`\n\n```suggestion\nlog(\"✅ ok\")\n```\n🔵 после";
    assert_eq!(
        iconize(md, BASE),
        format!(
            "{} **minor: x** в `a ✅ b`\n\n```suggestion\nlog(\"✅ ok\")\n```\n{} после",
            img("sev-minor", "🟡", 16),
            img("sev-nit", "🔵", 16)
        )
    );
}

#[test]
fn iconize_граничные_ограды_как_в_js() {
    // Эталоны — вывод lib/icons.mjs на тех же входах.
    // Открытие ```` и закрытие ``` — JS откатывает `{3,} до трёх: блок защищён.
    assert_eq!(iconize("````\n✅\n```\n✅", BASE), format!("````\n✅\n```\n{}", img("gate-open", "✅", 16)));
    // Закрытие длиннее открытия — не закрытие; незакрытый fence — текст.
    assert_eq!(
        iconize("```\n✅\n````\n✅", BASE),
        format!("```\n{}\n````\n{}", img("gate-open", "✅", 16), img("gate-open", "✅", 16))
    );
    // ~~~ и отступ до трёх пробелов; четыре пробела — уже не ограда.
    assert_eq!(iconize("   ~~~\n🔴\n   ~~~ \n", BASE), "   ~~~\n🔴\n   ~~~ \n");
    assert_eq!(
        iconize("    ```\n🔴\n```", BASE),
        format!("    ```\n{}\n```", img("sev-critical", "🔴", 16))
    );
    // Инлайн-код не переходит строку; пустой `` не код.
    assert_eq!(
        iconize("`a\n🔴` ``🔵", BASE),
        format!("`a\n{}` ``{}", img("sev-critical", "🔴", 16), img("sev-nit", "🔵", 16))
    );
    // Заголовок считается от начала участка между защищёнными: после `x` — « ## ».
    assert_eq!(iconize("`x` ## 🔴", BASE), format!("`x` ## {}", img("sev-critical", "🔴", 20)));
}

#[test]
fn iconize_эмодзи_вне_набора_не_меняются() {
    assert_eq!(iconize("⚠️ 💬 ⚪ 📈", BASE), "⚠️ 💬 ⚪ 📈");
}

#[test]
fn icon_files_каждому_знаку_есть_svg() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/icons");
    for (_, name) in ICON_FILES {
        assert!(dir.join(format!("{name}.svg")).exists(), "{name}");
    }
}

#[test]
fn icon_base_from_cfg_читает_render() {
    assert_eq!(icon_base_from_cfg(&json!({"render": {"iconBaseUrl": BASE}})), BASE);
    assert_eq!(icon_base_from_cfg(&json!({})), "");
}

/// Фейк, записывающий публикации.
#[derive(Default)]
struct Rec {
    calls: Mutex<Vec<Value>>,
}

impl GithubApi for Rec {
    fn gh(&self, _: &str, _: &GhCallOpts) -> Result<Value> {
        Ok(json!({}))
    }
    fn load_diff(&self) -> Result<Option<LoadedDiff>> {
        Ok(None)
    }
    fn read_prior_sticky_data(&self) -> Result<StickyData> {
        Ok(StickyData::default())
    }
    fn read_prior_findings(&self) -> Result<Vec<Value>> {
        Ok(vec![])
    }
    fn upsert_sticky(&self, body: &str) -> Result<String> {
        self.calls.lock().unwrap().push(json!(["sticky", body]));
        Ok("created".into())
    }
    fn delete_stale_inline(&self, _: &DeleteStaleOpts) -> Result<usize> {
        Ok(0)
    }
    fn post_review(&self, p: &PostReviewParams) -> Result<PostReviewResult> {
        self.calls.lock().unwrap().push(json!(["review", p]));
        Ok(PostReviewResult { review_id: Some(7) })
    }
    fn list_inline_comments(&self) -> Result<Vec<Value>> {
        Ok(vec![])
    }
    fn list_issue_labels(&self) -> Result<Vec<Value>> {
        Ok(vec![])
    }
    fn read_sticky_review(&self) -> Result<Option<Value>> {
        Ok(None)
    }
    fn list_pull_comment_reactions(&self, _: u64) -> Result<Vec<Value>> {
        Ok(vec![])
    }
    fn list_unresolved_review_threads(&self) -> Result<Vec<ThreadRef>> {
        Ok(vec![])
    }
    fn reply_review_thread(&self, _: &str, body: &str) -> Result<()> {
        self.calls.lock().unwrap().push(json!(["reply", body]));
        Ok(())
    }
    fn resolve_review_thread(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

#[test]
fn with_icons_sticky_шапка_и_inline_остальное_как_было() {
    let gh = Rec::default();
    assert!(WithIcons::wrap(&gh, "  ").is_none(), "пустая база — без обёртки");
    let wrapped = WithIcons::wrap(&gh, BASE).unwrap();
    wrapped.upsert_sticky("🔄 delta").unwrap();
    let res = wrapped
        .post_review(&PostReviewParams {
            inline: vec![InlineComment::new("a.js", 3, "🟠 **major: y**")],
            head_sha: "abc".into(),
            body: None,
        })
        .unwrap();
    assert_eq!(res.review_id, Some(7));
    wrapped.reply_review_thread("t", "<sub>🤖 auto-triage</sub>").unwrap();
    assert_eq!(
        *gh.calls.lock().unwrap(),
        vec![
            json!(["sticky", format!("{} delta", img("delta", "🔄", 16))]),
            json!(["review", {
                "inline": [{"path": "a.js", "line": 3, "side": "RIGHT",
                            "body": format!("{} **major: y**", img("sev-major", "🟠", 16))}],
                "headSha": "abc",
                "body": format!("{} Авто-ревью: инлайн-замечания ниже, общий вывод — в закреплённом комментарии.",
                                img("bot", "🤖", 16)),
            }]),
            json!(["reply", "<sub>🤖 auto-triage</sub>"]),
        ]
    );
}

