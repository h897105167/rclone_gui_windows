//! RCGUI 的运行时文案翻译（Rust 侧）。
//!
//! 约定：源码里的界面文案直接写中文原文当 msgid，翻译发生在运行时：
//!
//! ```ignore
//! save.t(rcgui_i18n::t("已复制"));                                  // 无占位符
//! let m = rcgui_i18n::f("上传失败：{}", &[&err_msg]);                // 有占位符，按顺序填充
//! ```
//!
//! 调用一律写成全限定形式 `rcgui_i18n::t(...)` / `rcgui_i18n::f(...)`，不用 `use`
//! 引入短名：老代码里到处是局部变量 `t`（例如 `let t = text.trim();`），而
//! `impl fmt::Display for X { fn fmt(&self, f: &mut Formatter<'_>) }` 的形参又常叫 `f`，
//! 短名会被遮蔽。全限定写法不依赖任何 import，也永远不会被局部变量挡住。
//!
//! Slint 侧用 `@tr("同一句中文")`，两边共用同一批 msgid，因此 .po 与 `en.rs`
//! 的译文必须一致（同一字符串在两边出现时不许翻得不一样）。

mod en;

use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};

/// 界面语言（当前只需中文与英文）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// 简体中文（原始语言，msgid 即中文原文）
    Zh,
    /// 英文
    En,
}

/// 设置项里表示「跟随系统」的标签（空串按同样的意思处理）。
pub const TAG_SYSTEM: &str = "system";

impl Lang {
    /// 把设置里的语言标签解析成界面语言。
    ///
    /// 返回 `None` 表示「跟随系统」（`system`、空串或任何不认识的值），
    /// 由调用方去读系统 UI 语言——本 crate 不依赖 Windows API。
    pub fn from_tag(tag: &str) -> Option<Lang> {
        match tag.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh-hans" => Some(Lang::Zh),
            "en" | "en-us" | "en-gb" => Some(Lang::En),
            _ => None,
        }
    }

    /// 写回设置用的标签。
    pub fn tag(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// Slint 侧 `@tr` 用的语言标识。
    ///
    /// 原始语言是中文，所以中文对应空串（= 默认 bundle，取 msgid 原文），
    /// 英文对应打包进去的 `"en"` 目录。
    pub fn slint_language(self) -> &'static str {
        match self {
            Lang::Zh => "",
            Lang::En => "en",
        }
    }
}

static LANG: AtomicU8 = AtomicU8::new(0);

/// 设置当前界面语言；切换后新产生的文案立即使用新语言。
pub fn set_lang(lang: Lang) {
    LANG.store(
        match lang {
            Lang::Zh => 0,
            Lang::En => 1,
        },
        Ordering::Relaxed,
    );
}

/// 当前界面语言。
pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == 1 {
        Lang::En
    } else {
        Lang::Zh
    }
}

/// 当前是否为英文。
pub fn is_en() -> bool {
    lang() == Lang::En
}

/// 把一个中文 msgid 翻成当前语言。
pub fn t(msgid: &'static str) -> &'static str {
    if is_en() {
        en::lookup(msgid).unwrap_or(msgid)
    } else {
        msgid
    }
}

/// 翻译模板后用 `args` 依次填充 `{}`（对应 `format!` 的运行时版本）。
pub fn f(template: &'static str, args: &[&dyn Display]) -> String {
    fill(t(template), args)
}

/// 用 `args` 依次填充 `template` 里的 `{}`；`{{`/`}}` 按 Rust 习惯转义。
pub fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 24 * args.len());
    let mut args = args.iter();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '{' => {
                // 占位符内可能出现对齐/精度等说明，本工程只用到空的 `{}`；
                // 万一有别的写法就原样保留，避免静默吃掉内容。
                let mut spec = String::new();
                let mut closed = false;
                while let Some(&c2) = chars.peek() {
                    chars.next();
                    if c2 == '}' {
                        closed = true;
                        break;
                    }
                    spec.push(c2);
                }
                match (closed, args.next()) {
                    (true, Some(a)) if spec.is_empty() => out.push_str(&a.to_string()),
                    _ => {
                        out.push('{');
                        out.push_str(&spec);
                        if closed {
                            out.push('}');
                        }
                    }
                }
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_basic() {
        assert_eq!(fill("a {} b {}", &[&1, &"x"]), "a 1 b x");
        assert_eq!(fill("{}", &[&"only"]), "only");
        assert_eq!(fill("no placeholder", &[] as &[&dyn Display]), "no placeholder");
    }

    #[test]
    fn fill_escapes() {
        assert_eq!(fill("{{}}", &[] as &[&dyn Display]), "{}");
        assert_eq!(fill("{{ {} }}", &[&7]), "{ 7 }");
        // 参数不足时原样保留，方便发现漏传
        assert_eq!(fill("{} {}", &[&1]), "1 {}");
    }

    #[test]
    fn fill_multiline() {
        assert_eq!(fill("第一行\n第二行 {}", &[&2]), "第一行\n第二行 2");
    }

    #[test]
    fn zh_is_identity() {
        set_lang(Lang::Zh);
        assert_eq!(t("已复制"), "已复制");
        assert_eq!(f("任务 {} 个", &[&3]), "任务 3 个");
    }

    #[test]
    fn lang_from_tag() {
        assert_eq!(Lang::from_tag("zh"), Some(Lang::Zh));
        assert_eq!(Lang::from_tag(" zh-cn "), Some(Lang::Zh));
        assert_eq!(Lang::from_tag("EN-US"), Some(Lang::En));
        // 跟随系统 / 空串 / 脏值都不指定语言
        assert_eq!(Lang::from_tag(TAG_SYSTEM), None);
        assert_eq!(Lang::from_tag(""), None);
        assert_eq!(Lang::from_tag("de"), None);
        assert_eq!(Lang::Zh.tag(), "zh");
        assert_eq!(Lang::En.tag(), "en");
        assert_eq!(Lang::Zh.slint_language(), "");
        assert_eq!(Lang::En.slint_language(), "en");
    }

    /// 英文表必须按 msgid 的 UTF-8 字节序升序，否则二分查找会漏。
    #[test]
    fn en_table_sorted() {
        for w in en::EN.windows(2) {
            assert!(w[0].0 < w[1].0, "en.rs 未按 msgid 排序: {:?} >= {:?}", w[0].0, w[1].0);
        }
    }

    /// 译文里的 `{}` 数量必须与 msgid 一致，否则运行时会漏填。
    #[test]
    fn en_placeholders_match() {
        for (zh, e) in en::EN {
            let a = zh.matches("{}").count();
            let b = e.matches("{}").count();
            assert_eq!(a, b, "占位符数量不一致: {:?} -> {:?}", zh, e);
        }
    }
}
