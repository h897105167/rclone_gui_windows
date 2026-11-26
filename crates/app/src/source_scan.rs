//! 只在测试期编译：从真实源码里切一小段出来做断言的工具。
//!
//! 少数接线没法用行为测试守：`Ctx` 在单测里造不出来（要插件宿、KV、tokio 全套），
//! `crates/app/tests/` 又 import 不到 bin crate，于是这些测试退而读源码、断言调用点
//! 存在——被调用的函数各自有自己的行为测试，这里只管"有没有接上"。
//!
//! 切源码的代码必须与换行无关。开发机 `core.autocrlf=input` 检出成 LF，CI 上常见的是
//! CRLF；写死 `"\n}\n"` 这类终止符在 CRLF 下永远匹配不到，测试就死在"找不到结尾"上，
//! 而本地一路通过。这个模块存在的理由就是让那类断言只写一次、并且被两种行尾测过。

/// 从 `needle` 起切到函数收尾那一行为止，返回中间这段源码。
///
/// "收尾"指顶格的 `}`，允许 `\n` 与 `\r\n` 两种行尾，也允许它是文件最后一行（后面没有换行）。
/// 找不到 `needle`、或找不到收尾都返回 `None`——报错文案留给调用方，因为各自该说的话不同。
pub(crate) fn function_body<'a>(src: &'a str, needle: &str) -> Option<&'a str> {
    let start = src.find(needle)?;
    let rest = &src[start..];
    let mut from = 0;
    while let Some(hit) = rest[from..].find("\n}") {
        // `\n}` 之后的字符决定 `}` 是不是独占一行：`\n`（LF）、`\r\n`（CRLF）或文件结束。
        let after = &rest[from + hit + 2..];
        if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
            return Some(&rest[..from + hit]);
        }
        from += hit + 2;
    }
    None
}

/// 从 `from` 之后切到 `to` 之前，返回中间这段源码。
///
/// 后面还跟着别的函数、两头都有现成锚点（函数名）时用这个，比找收尾括号更稳：
/// 不依赖缩进，也不怕中途出现嵌套的大括号。
pub(crate) fn region<'a>(src: &'a str, from: &str, to: &str) -> Option<&'a str> {
    let start = src.find(from)? + from.len();
    let rest = &src[start..];
    let end = rest.find(to)?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两种行尾必须切出同一段内容；LF 与 CRLF 各测一遍，避免只有开发机能跑。
    #[test]
    fn function_body_is_cut_identically_for_lf_and_crlf() {
        let lf = "fn outer() {\n    fn apply_conn_view() {\n        a();\n        b();\n    }\n}\n";
        let crlf = lf.replace('\n', "\r\n");
        for (label, src) in [("LF", lf), ("CRLF", &crlf)] {
            let body = function_body(src, "fn apply_conn_view(")
                .unwrap_or_else(|| panic!("{label}: 应该切得出来"));
            assert!(body.contains("a();"), "{label}: 少了第一句");
            assert!(body.contains("b();"), "{label}: 少了第二句");
            assert!(!body.contains("outer"), "{label}: 不该跨到外层函数");
        }
    }

    /// 顶格 `}` 后面紧跟别的代码（同一缩进的兄弟函数）时，也要停在第一个独占行的 `}`。
    #[test]
    fn function_body_stops_at_the_first_standalone_brace() {
        let lf = "fn target() {\n    inner();\n}\nfn next() {\n    other();\n}\n";
        let crlf = lf.replace('\n', "\r\n");
        for (label, src) in [("LF", lf), ("CRLF", &crlf)] {
            let body =
                function_body(src, "fn target(").unwrap_or_else(|| panic!("{label}: 没切出来"));
            assert!(body.contains("inner();"), "{label}: 没拿到函数体");
            assert!(!body.contains("other();"), "{label}: 越到下一个函数了");
        }
    }

    /// 文件末尾最后一个函数没有尾随换行，也要算切到底。
    #[test]
    fn function_body_accepts_the_last_line_of_the_file() {
        for src in [
            "fn tail() {\n    done();\n}",
            "fn tail() {\r\n    done();\r\n}",
        ] {
            let body = function_body(src, "fn tail(").expect("文件末尾的函数也要切得出来");
            assert!(body.contains("done();"));
        }
    }

    /// 没有收尾括号时返回 `None`，让调用方自己决定报什么错。
    #[test]
    fn function_body_reports_a_missing_terminator() {
        assert!(function_body("fn x() {\n    y();\n", "fn x(").is_none());
        assert!(function_body("fn x() {\r\n    y();\r\n", "fn x(").is_none());
        assert!(function_body("", "fn x(").is_none());
    }

    /// `region` 同样必须与换行无关。
    #[test]
    fn region_is_cut_identically_for_lf_and_crlf() {
        let lf = "async fn first() {\n    one();\n}\n\nasync fn second() {\n    two();\n}\n";
        let crlf = lf.replace('\n', "\r\n");
        for (label, src) in [("LF", lf), ("CRLF", &crlf)] {
            let body = region(src, "async fn first(", "async fn second(")
                .unwrap_or_else(|| panic!("{label}: 应该切得出来"));
            assert!(body.contains("one();"), "{label}: 少了 first 的调用");
            assert!(!body.contains("two();"), "{label}: 把 second 也包进来了");
        }
    }
}
