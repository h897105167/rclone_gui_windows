//! 内置插件的英文文案（宿主侧翻译）。
//!
//! 插件包（`assets/plugins/<id>/{config.json,main.wasm,check.sha256}`）带完整性校验，
//! 不能为了出英文去改包里的内容，所以翻译放在宿主这一侧：插件交上来的中文原文按
//! 词典换成英文。原则是**只翻展示用字段**——落盘的表单值、下拉框写回的选项仍然是
//! 插件认识的中文原值，否则插件就读不懂自己保存的设置。
//!
//! 词典在 `assets/plugin_i18n.json`，按插件 id 分表，每表三类条目：
//!
//! - `exact`：整串 = 中文原文 → 英文；
//! - `patterns`：`[中文模板, 英文模板]`，用 `{}` 表示插件的运行时值（PID、端口、路径…），
//!   按模板的静态段做前后缀匹配后回填；
//! - `fragments`：出现在长文案内部的短词，整串没命中时兜底替换（长词优先）。
//!
//! 当前语言不是英文时所有接口直通（原串返回），中文模式下没有任何额外开销。

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;

use serde_json::Value;

/// 一条带占位符的模板：中文模板按 `{}` 切出的静态段 + 英文模板原文。
struct Pattern {
    /// 中文模板按 `{}` 切出的静态段，段数 = 占位符数 + 1（至少 2）
    parts: Vec<String>,
    /// 英文模板（含 `{}`，交给 [`rcgui_i18n::fill`] 回填）
    en: String,
}

impl Pattern {
    /// 把 `s` 按模板拆出各个占位符的值；不匹配返回 `None`。
    fn split<'a>(&self, s: &'a str) -> Option<Vec<&'a str>> {
        let n = self.parts.len();
        if n < 2 {
            return None;
        }
        let first = self.parts[0].as_str();
        let last = self.parts[n - 1].as_str();
        if !s.starts_with(first) || !s.ends_with(last) {
            return None;
        }
        if first.len() + last.len() > s.len() {
            return None;
        }
        let mut cur = &s[first.len()..s.len() - last.len()];
        let mut out = Vec::with_capacity(n - 1);
        for sep in &self.parts[1..n - 1] {
            let pos = cur.find(sep.as_str())?;
            out.push(&cur[..pos]);
            cur = &cur[pos + sep.len()..];
        }
        out.push(cur);
        Some(out)
    }
}

/// 一个插件的词表。
struct Table {
    exact: HashMap<String, String>,
    patterns: Vec<Pattern>,
    /// 英文 → 中文，供下拉框把展示值还原成插件认识的原值
    back: HashMap<String, String>,
    /// (中文片段, 英文片段)，按中文长度降序
    fragments: Vec<(String, String)>,
}

/// 编译期嵌进来的词典原文。
const DICT: &str = include_str!("../assets/plugin_i18n.json");

static TABLES: OnceLock<HashMap<String, Table>> = OnceLock::new();

fn tables() -> &'static HashMap<String, Table> {
    TABLES.get_or_init(|| parse(DICT).unwrap_or_default())
}

/// 解析词典。解析失败时返回空表（界面回落到中文），不让一条词典把程序带崩。
fn parse(raw: &str) -> Option<HashMap<String, Table>> {
    let root: Value = serde_json::from_str(raw).ok()?;
    let mut out = HashMap::new();
    for (id, entry) in root.as_object()? {
        let mut exact = HashMap::new();
        if let Some(map) = entry.get("exact").and_then(|v| v.as_object()) {
            for (zh, en) in map {
                if let Some(en) = en.as_str() {
                    exact.insert(zh.clone(), en.to_string());
                }
            }
        }
        // 反查表：同一句英文对应两个中文原值时保留先出现的，避免选项回填串味
        let mut back = HashMap::new();
        for (zh, en) in &exact {
            back.entry(en.clone()).or_insert_with(|| zh.clone());
        }
        let mut patterns = Vec::new();
        if let Some(list) = entry.get("patterns").and_then(|v| v.as_array()) {
            for pair in list {
                let (Some(zh), Some(en)) = (
                    pair.get(0).and_then(|v| v.as_str()),
                    pair.get(1).and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                patterns.push(Pattern {
                    parts: zh.split("{}").map(|s| s.to_string()).collect(),
                    en: en.to_string(),
                });
            }
        }
        let mut fragments: Vec<(String, String)> = Vec::new();
        if let Some(map) = entry.get("fragments").and_then(|v| v.as_object()) {
            for (zh, en) in map {
                if let Some(en) = en.as_str() {
                    fragments.push((zh.clone(), en.to_string()));
                }
            }
        }
        // 长词优先：短词先替换会把长词的上下文搅乱
        fragments.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        out.insert(
            id.clone(),
            Table {
                exact,
                patterns,
                back,
                fragments,
            },
        );
    }
    Some(out)
}

/// 把插件给出的一个展示字符串翻成当前语言。
///
/// 没命中任何条目就原样返回——插件新加的中文会保持中文，不会变成空串。
pub fn tr(id: &str, s: &str) -> String {
    if s.is_empty() || !rcgui_i18n::is_en() {
        return s.to_string();
    }
    let Some(tb) = tables().get(id) else {
        return s.to_string();
    };
    if let Some(en) = tb.exact.get(s) {
        return en.clone();
    }
    for p in &tb.patterns {
        if let Some(args) = p.split(s) {
            let refs: Vec<&dyn Display> = args.iter().map(|a| a as &dyn Display).collect();
            return rcgui_i18n::fill(&p.en, &refs);
        }
    }
    let mut out = s.to_string();
    for (zh, en) in &tb.fragments {
        if out.contains(zh.as_str()) {
            out = out.replace(zh.as_str(), en.as_str());
        }
    }
    out
}

/// 把界面上的展示值还原成插件认识的中文原值。
///
/// 只在英文模式生效：中文模式下界面上的值本来就是原值，不需要（也不该）动。
/// 用于下拉框——`select` 选中项会作为表单值写回插件，写英文插件就认不出来了。
pub fn tr_back(id: &str, s: &str) -> String {
    if s.is_empty() || !rcgui_i18n::is_en() {
        return s.to_string();
    }
    let Some(tb) = tables().get(id) else {
        return s.to_string();
    };
    tb.back.get(s).cloned().unwrap_or_else(|| s.to_string())
}

/// 这个字符算不算「上屏会露中文」的字符（只给测试用）。
///
/// 只看中文与全角标点：插件文案里出现的 `PID`、`rclone`、路径都是正常内容。
#[cfg(test)]
pub(crate) fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x3000..=0x303F   // 全角标点
            | 0x4E00..=0x9FFF // 汉字
            | 0xFF00..=0xFFEF // 全角符号
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 有几个测试会改全局语言，串行化执行，避免并行时互相踩。
    static LANG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn dict_parses_and_is_not_empty() {
        let t = parse(DICT).expect("plugin_i18n.json 必须能解析");
        for id in ["imagehost", "onerclone"] {
            let tb = t.get(id).unwrap_or_else(|| panic!("词典里缺少插件 {id}"));
            assert!(!tb.exact.is_empty(), "{id} 的 exact 表不能为空");
            assert!(!tb.back.is_empty(), "{id} 的反查表不能为空");
        }
    }

    /// 每条中文模板和英文模板的占位符数量必须对上，否则回填会错位。
    #[test]
    fn pattern_placeholders_match() {
        let t = parse(DICT).expect("plugin_i18n.json 必须能解析");
        for (id, tb) in &t {
            for p in &tb.patterns {
                assert!(
                    p.parts.len() >= 2,
                    "{id}: 模板缺少 {{}} 占位符：{}",
                    p.parts.join("|")
                );
                assert_eq!(
                    p.en.matches("{}").count(),
                    p.parts.len() - 1,
                    "{id}: 中英模板的 {{}} 数量不一致：{} → {}",
                    p.parts.join("{}"),
                    p.en
                );
            }
        }
    }

    #[test]
    fn tr_fills_pattern() {
        let t = parse(
            r#"{"x":{"exact":{"已停止":"Stopped"},
                 "patterns":[["已停止 rclone（PID {}）","rclone stopped (PID {})"]],
                 "fragments":{}}}"#,
        )
        .unwrap();
        let tb = &t["x"];
        let p = &tb.patterns[0];
        assert_eq!(p.split("已停止 rclone（PID 42）"), Some(vec!["42"]));
        assert_eq!(p.split("已停止"), None);
        // 反查表由 exact 反向生成
        assert_eq!(tb.back.get("Stopped").map(|s| s.as_str()), Some("已停止"));
    }

    #[test]
    fn zh_mode_passes_through() {
        let _g = LANG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        rcgui_i18n::set_lang(rcgui_i18n::Lang::Zh);
        assert_eq!(tr("imagehost", "上传"), "上传");
        assert_eq!(tr_back("imagehost", "Upload"), "Upload");
    }

    fn collect_strings(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) => {
                if has_cjk(s) {
                    out.push(s.clone());
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
            serde_json::Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
            _ => {}
        }
    }

    /// 随包插件的 `config.json` 里每条含中文的字符串，英文模式下都必须翻出来。
    ///
    /// 面板上的 label / hint / 选项 / 按钮文案全部来自 config.json，漏一条就会有一条
    /// 中文混在英文界面里，而且只有翻到那一页才看得见 —— 所以在这里一次查完。
    #[test]
    fn shipped_config_strings_all_translate() {
        let _g = LANG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        rcgui_i18n::set_lang(rcgui_i18n::Lang::En);

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/plugins");
        let mut missing: Vec<String> = Vec::new();
        let mut checked = 0usize;

        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
            .unwrap_or_else(|e| panic!("随包插件目录读不出来: {e}"))
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();

        for dir in dirs {
            let cfg_path = dir.join("config.json");
            let Ok(raw) = std::fs::read_to_string(&cfg_path) else {
                continue;
            };
            let Ok(cfg) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let id = cfg
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let mut strings = Vec::new();
            collect_strings(&cfg, &mut strings);
            for zh in strings {
                checked += 1;
                let en = tr(&id, &zh);
                if has_cjk(&en) {
                    missing.push(format!("{id}: {zh}"));
                }
            }
        }

        rcgui_i18n::set_lang(rcgui_i18n::Lang::Zh);
        assert!(checked > 0, "一个随包插件的 config.json 都没读到");
        assert!(
            missing.is_empty(),
            "以下插件文案没有英文（{} 条）:\n{}",
            missing.len(),
            missing.join("\n")
        );
    }
}
