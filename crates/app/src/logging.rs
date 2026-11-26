//! 结构化日志：一行一个 JSON 对象，按大小轮转。
//!
//! 为什么不用现成的日志 crate：本程序的日志量很小（一轮同步几条），但有一条
//! 硬要求——**任何一行日志都不许出现 rclone 的 RC 密码、WebDAV 密码、插件
//! token**。日志是我们唯一会主动写到磁盘又给人看的文本，把"脱敏"放在写入口，
//! 比让每个调用点自己记得更可靠。
//!
//! 轮转按字节数：单个文件超过 `MAX_BYTES` 就滚到 `.1`、`.1` 滚到 `.2`，
//! 只保留 `KEEP` 个历史文件。不按天切分是因为用户可能一天开十次也可能
//! 一个月不开，按体积更接近"磁盘占用有上限"这个真实目标。

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Once;

/// 单个日志文件的上限：4 MiB 大约能装几万条事件，足够定位问题。
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// 保留的历史文件个数（不含当前文件）。
pub const KEEP: usize = 3;

/// 日志文件路径：`<data>/logs/rcgui.log`。与内核日志、插件日志同目录。
pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("rcgui.log")
}

/// 日志目录（`--diagnostics` 会报给用户）。
pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

/// 写一条事件。`target` 是模块名，便于过滤；`msg` 会先过 [`redact`]。
///
/// 写失败静默忽略：日志坏了不该让程序出错，这是日志与业务数据的区别。
pub fn event(level: &str, target: &str, msg: &str) {
    let data_dir = crate::config::data_dir();
    write_to(&log_path(&data_dir), level, target, msg);
}

/// 与 [`event`] 相同，但允许指定数据目录（测试与 CLI 用）。
pub fn write_to(path: &Path, level: &str, target: &str, msg: &str) {
    if let Some(dir) = path.parent() {
        if fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let line = line_for(level, target, msg);
    rotate_if_needed(path, line.len() as u64);
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = f.write_all(line.as_bytes());
}

/// 装上 panic 钩子：进程崩之前至少留下最后一行日志。
///
/// 日志文件路径在这里就定下来，钩子里不再调 `data_dir()`——那个函数可能触发权限探测
/// 甚至改目录权限，panic 现场不该再做这些。原来的钩子照样会被调用，所以控制台输出
/// 和调试器断点行为不变。重复调用只有第一次生效。
pub fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let path = log_path(&crate::config::data_dir());
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current()
                .name()
                .unwrap_or("<未命名线程>")
                .to_string();
            let at = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "位置未知".to_string());
            let msg = panic_message(info.payload());
            write_to(
                &path,
                "error",
                "panic",
                &format!("thread={thread} at={at} msg={msg}"),
            );
            default(info);
        }));
    });
}

/// panic 的载荷只有 `&str` / `String` 能读，别的类型（`panic_any`）读不了。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "非字符串 panic".to_string()
    }
}

/// 拼一行 JSON。字段固定为 `ts`/`level`/`target`/`msg`，方便用 `jq` 过滤。
fn line_for(level: &str, target: &str, msg: &str) -> String {
    let ts = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let obj = serde_json::json!({
        "ts": ts,
        "level": level,
        "target": target,
        "msg": redact(msg),
    });
    let mut s = serde_json::to_string(&obj).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

/// 当前文件加上这一行会不会超限；会就先滚动。
fn rotate_if_needed(path: &Path, incoming: u64) {
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size == 0 || size + incoming <= MAX_BYTES {
        return;
    }
    // 从最老的开始滚：.N 丢掉，.N-1 → .N，当前 → .1
    for i in (1..=KEEP).rev() {
        let from = numbered(path, i);
        if !from.exists() {
            continue;
        }
        if i == KEEP {
            let _ = fs::remove_file(&from);
        } else {
            let _ = fs::rename(&from, numbered(path, i + 1));
        }
    }
    let _ = fs::rename(path, numbered(path, 1));
}

fn numbered(path: &Path, n: usize) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

/// 会被脱敏的键名（小写比较，按前缀匹配到分隔符为止）。
const SECRET_KEYS: &[&str] = &[
    "pass",
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "access_key",
    "secret_key",
    "client_secret",
    "authorization",
    "auth",
    "credential",
];

/// 键名本身不含秘密值的键，但它们的值可能是凭据（用户名不是秘密，密码才是；
/// 这里只把 `user` 类键的名字保留、值原样，因为排查问题时需要知道是谁）。
const NOT_SECRET_KEYS: &[&str] = &["username", "user"];

/// 把一段文本里的凭据替换成 `***`。
///
/// 两条规则：
/// 1. URL 里的 `scheme://user:pass@host` → `scheme://***@host`，查询串整段 → `?***`。
/// 2. `键=值` / `键: 值` / `"键": "值"` 且键名命中 [`SECRET_KEYS`] → 值换成 `***`。
///
/// 宁可多遮一点也不要漏：日志一旦写出去就收不回来。
pub fn redact(text: &str) -> String {
    let masked = mask_query(&mask_userinfo(text));
    mask_key_values(&masked)
}

/// `scheme://user:pass@host/...` 里的 userinfo 部分换成 `***`。
fn mask_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        // 权限部分到下一个 / 空白 引号 或结尾
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'' || c == '>')
            .unwrap_or(tail.len());
        let (authority, after) = tail.split_at(end);
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// URL 的查询串整段换成 `?***`。
///
/// 为什么不靠键名：`?token=` 这类 [`SECRET_KEYS`] 认得出，但预签名 URL 的
/// `X-Amz-Signature`、`X-Sig`、`?a=1&b=<签名>` 认不出，漏一个就等于把钥匙写进日志。
/// 查询串里没有值得留档的东西（路径和主机名已经够定位了），所以整段遮掉。
fn mask_query(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    // 当前这个"词"里是否已经见过 `://`：是才把 `?` 当查询串开头。
    let mut at_url = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '?' && at_url {
            out.push_str("?***");
            i += 1;
            while i < chars.len() && !is_url_token_end(chars[i]) {
                i += 1;
            }
            at_url = false;
            continue;
        }
        if is_url_token_end(c) {
            at_url = false;
        } else if c == ':' && chars[i..].starts_with(&[':', '/', '/']) {
            at_url = true;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 一个 URL 到哪里算结束：空白、引号、括号之外的字符继续算 URL 的一部分。
fn is_url_token_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '>' | ')' | ']' | '}' | ',' | ';')
}

fn mask_key_values(text: &str) -> String {
    let lower = text.to_lowercase();
    let bytes: Vec<char> = text.chars().collect();
    let low: Vec<char> = lower.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        // 从 i 开始尝试匹配一个秘密键
        let mut matched: Option<(usize, usize, &'static str)> = None; // (键结束位置, 值开始位置, 键名)
        for key in SECRET_KEYS {
            if let Some((kend, vend)) = match_key(&low, &bytes, i, key) {
                if is_not_secret(&low, &bytes, i, key) {
                    continue;
                }
                matched = Some((kend, vend, key));
                break;
            }
        }
        match matched {
            Some((_kend, vend, key)) => {
                out.extend(bytes[i..vend].iter());
                let (mut masked_end, had_value) = value_end(&bytes, vend);
                if had_value {
                    // `Authorization: Bearer abc` 里 `Bearer` 只是方案名，
                    // 真正的凭据在它后面那个词上，只遮方案名等于没遮。
                    if let Some(end) = scheme_credential_end(key, &bytes, vend, masked_end) {
                        masked_end = end;
                    }
                    out.push_str("***");
                }
                i = masked_end;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    out
}

/// 键名允许有 `-`/`_` 前缀（如 `--rc-pass`），所以只做"左边不是字母"的宽松判断。
fn is_not_secret(low: &[char], raw: &[char], start: usize, key: &str) -> bool {
    // `username` 里含 `user`，避免把用户名当秘密；反之亦然。
    let keylen = key.chars().count();
    if !NOT_SECRET_KEYS.contains(&key) {
        return false;
    }
    let after = start + keylen;
    // 后面紧跟字母说明是更长的词（如 users），此时按"不是这个键"处理
    after < raw.len() && (low[after].is_ascii_alphabetic())
}

/// 尝试在 `start` 处匹配 `key`，成功返回 (键结束下标, 值起始下标)。
///
/// 键与值之间允许 `=`、`:`、空白、引号、冒号。
fn match_key(low: &[char], raw: &[char], start: usize, key: &str) -> Option<(usize, usize)> {
    let mut j = start;
    // 左边界：前一个字符不能是字母数字，否则 `compass` 里的 pass 会被误伤
    if j > 0 && (low[j - 1].is_alphanumeric()) {
        return None;
    }
    for kc in key.chars() {
        if j >= low.len() || low[j] != kc {
            return None;
        }
        j += 1;
    }
    let kend = j;
    // 键后允许引号/空白/等号/冒号
    while j < raw.len() && matches!(raw[j], '"' | '\'' | ' ' | '\t' | '=' | ':') {
        j += 1;
    }
    // 必须有分隔符，否则只是恰好包含这个词
    if j == kend || j >= raw.len() {
        return None;
    }
    // 值的起始引号要保留，值的结束在下个引号/逗号/空白/右括号
    Some((kend, j))
}

/// 从 `start` 找到值结尾返回 (结尾下标, 是否有值可遮)。
fn value_end(raw: &[char], start: usize) -> (usize, bool) {
    let mut j = start;
    // 值本身可能被引号包着：`"pass": "abc"` 的 `"` 已经跳过了，这里处理值前的引号
    let quoted = matches!(raw.get(j), Some('"') | Some('\''));
    let quote = if quoted {
        let q = raw[j];
        j += 1;
        Some(q)
    } else {
        None
    };
    let vstart = j;
    while j < raw.len() {
        let c = raw[j];
        if Some(c) == quote {
            j += 1;
            break;
        }
        if quote.is_none() && matches!(c, ' ' | '\t' | '"' | '\'' | ',' | '&' | '}' | ']' | '\n') {
            break;
        }
        j += 1;
    }
    (j, j > vstart)
}

/// `Authorization: Bearer <token>` 这种两段式凭据：方案名后面那个词也要遮。
///
/// 返回要一起遮掉的结尾下标；键不是认证头、或值不是已知方案名时返回 None。
fn scheme_credential_end(key: &str, raw: &[char], vstart: usize, vend: usize) -> Option<usize> {
    if !matches!(key, "authorization" | "auth") {
        return None;
    }
    let scheme: String = raw[vstart..vend]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    if !matches!(
        scheme.as_str(),
        "bearer" | "basic" | "token" | "digest" | "negotiate"
    ) {
        return None;
    }
    let mut j = vend;
    while j < raw.len() && matches!(raw[j], ' ' | '\t') {
        j += 1;
    }
    let (end, has) = value_end(raw, j);
    if has {
        Some(end)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_password_pairs_in_several_shapes() {
        for s in [
            "--rc-pass=hunter2",
            "pass: hunter2",
            r#"{"pass":"hunter2"}"#,
            "rc_pass='hunter2'",
            "PASSWORD = hunter2 rest",
        ] {
            let out = redact(s);
            assert!(!out.contains("hunter2"), "{s} -> {out}");
            assert!(out.contains("***"), "{s} -> {out}");
        }
    }

    #[test]
    fn keeps_ordinary_words_intact() {
        // compass / bypass 里含 pass，但不是键名
        let out = redact("compass=1 bypassed user=alice");
        assert!(out.contains("compass=1"), "{out}");
        assert!(out.contains("user=alice"), "用户名不是秘密：{out}");
    }

    #[test]
    fn masks_userinfo_but_keeps_host_and_path() {
        let out = redact("GET https://alice:s3cret@dav.example.com/rcgui.conf failed");
        assert!(!out.contains("s3cret"), "{out}");
        assert!(
            out.contains("https://***@dav.example.com/rcgui.conf"),
            "{out}"
        );
    }

    #[test]
    fn token_and_authorization_are_masked() {
        let out = redact("Authorization: Bearer abcdef token=xyz123 secret=zzz");
        assert!(!out.contains("abcdef"), "{out}");
        assert!(!out.contains("xyz123"), "{out}");
        assert!(!out.contains("zzz"), "{out}");
    }

    #[test]
    fn rotates_when_file_exceeds_limit() {
        let dir = std::env::temp_dir().join(format!("rcgui-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("rcgui.log");
        // 先写一个刚好超限的文件，再写一条，应该滚到 .1
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();
        write_to(&path, "info", "test", "hello");
        assert!(numbered(&path, 1).exists(), "旧文件应滚到 .1");
        let now = fs::metadata(&path).unwrap().len();
        assert!(now < MAX_BYTES, "新文件应该只含一条");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_is_single_json_object_without_newlines() {
        let line = line_for("warn", "sync", "多行\n消息");
        assert_eq!(line.matches('\n').count(), 1, "只允许结尾一个换行");
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["level"], "warn");
        assert_eq!(v["target"], "sync");
    }

    #[test]
    fn masks_url_query_entirely() {
        // 预签名 URL 的签名键名不叫 token，靠键名匹配会漏，只能整段遮。
        let out =
            redact("GET https://dav.example.com/rcgui.conf?X-Amz-Signature=abc123&x=1 failed");
        assert!(!out.contains("abc123"), "{out}");
        assert!(out.contains("?***"), "{out}");
        assert!(
            out.contains("dav.example.com/rcgui.conf"),
            "主机和路径要留着：{out}"
        );
        assert!(out.ends_with("failed"), "遮完要继续处理后面的文本：{out}");
    }

    #[test]
    fn question_mark_outside_a_url_is_left_alone() {
        assert_eq!(redact("真的要继续吗？"), "真的要继续吗？");
        assert!(redact("要删除吗? 不能撤销").contains("要删除吗?"));
    }

    #[test]
    fn panic_payload_reads_str_and_string() {
        assert_eq!(panic_message(&"炸了"), "炸了");
        assert_eq!(panic_message(&String::from("也炸了")), "也炸了");
        assert_eq!(panic_message(&42u8), "非字符串 panic");
    }
}
