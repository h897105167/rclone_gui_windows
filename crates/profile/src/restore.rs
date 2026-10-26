//! 恢复脚本生成（PowerShell）。
//!
//! 硬性约束（来自重构指南 §5 与产品规则）：
//! - 脚本**只连接**已经在运行、且启用了 RC 的 rclone；连不上就 `exit 1`；
//! - **绝不**启动/停止/重启 rclone（不出现 `rclone rcd`、`Start-Process`、`Stop-Process`、`core/quit`）；
//! - **不含** RC 密码：凭据只从参数或环境变量读取；
//! - serve 密码也不内置：由 `-ServePasses` 在运行时提供，缺失则跳过并告警；
//! - 参数用 PowerShell 单引号字面量转义（`'` → `''`），JSON 由 `ConvertTo-Json` 生成，
//!   不做字符串拼接，避免空格/引号/`&`/`%` 被解释。

use crate::ProfileDoc;

/// 生成脚本正文。`endpoint_hint` 只用于注释与参数默认值，凭据一律不写入。
pub fn generate(doc: &ProfileDoc, endpoint_hint: &str) -> String {
    let name = if doc.meta.name.is_empty() {
        rcgui_i18n::t("默认档案").to_string()
    } else {
        doc.meta.name.clone()
    };
    let mut s = String::new();
    s.push_str(rcgui_i18n::t("# RCGUI 恢复脚本（由 RCGUI 自动生成，请勿手工修改）\n"));
    s.push_str(&rcgui_i18n::f("# 档案：{}\n", &[&(name)]));
    s.push_str(&rcgui_i18n::f("# 生成时间：{}\n", &[&(crate::now_iso())]));
    s.push_str("#\n");
    s.push_str(rcgui_i18n::t("# 本脚本只连接**已经在运行**且启用了 RC 的 rclone：\n"));
    s.push_str(rcgui_i18n::t("#   * 不会启动、停止或重启 rclone；\n"));
    s.push_str(rcgui_i18n::t("#   * 不会创建开机启动项或计划任务；\n"));
    s.push_str(rcgui_i18n::t("#   * 不内置 RC 密码（用 -RcUser / -RcPass 或环境变量 RCGUI_RC_USER / RCGUI_RC_PASS）；\n"));
    s.push_str(rcgui_i18n::t("#   * serve 密码不内置（用 -ServePasses 传入，缺失则跳过该项并告警）。\n"));
    s.push_str("#\n");
    s.push_str(rcgui_i18n::t("# 用法示例：\n"));
    s.push_str(&format!(
        "#   .\\restore.ps1 -Endpoint {} -RcUser <user>\n",
        ps_quote(endpoint_hint)
    ));
    s.push_str("\n[CmdletBinding()]\nparam(\n");
    s.push_str(&format!(
        "    [Parameter(Mandatory = $true)][string]$Endpoint = {},\n",
        ps_quote(endpoint_hint)
    ));
    s.push_str("    [string]$RcUser = $env:RCGUI_RC_USER,\n");
    s.push_str("    [string]$RcPass = $env:RCGUI_RC_PASS,\n");
    s.push_str("    [int]$HealthTimeoutSec = 30,\n");
    s.push_str("    [int]$HealthIntervalMs = 500,\n");
    s.push_str("    [hashtable]$ServePasses = @{}\n");
    s.push_str(")\n\n");
    s.push_str(HEADER_FUNCS);

    // ---- 挂载（保持保存顺序）----
    s.push_str(rcgui_i18n::t("\n# ---------------- 挂载（按保存顺序） ----------------\n"));
    let mut any_mount = false;
    for m in &doc.mounts {
        if !m.enabled {
            s.push_str(&rcgui_i18n::f("# 跳过（已停用）：{} -> {}\n", &[&(ps_comment(&compose_fs(&m.remote, &m.sub_path))), &(ps_comment(&m.point))]));
            continue;
        }
        any_mount = true;
        s.push_str(&rcgui_i18n::f("# 挂载 {}\n$p = @{{ fs = {}; mountPoint = {}; vfs_cache_mode = {}; read_only = ${} }}\nInvoke-Rc -Method 'mount/mount' -Params $p\n", &[&(ps_comment(&compose_fs(&m.remote, &m.sub_path))), &(ps_quote(&compose_fs(&m.remote, &m.sub_path))), &(ps_quote(&m.point)), &(ps_quote(if m.cache.is_empty() { "writes" } else { &m.cache })), &(if m.ro { "true" } else { "false" })]));
    }
    if !any_mount {
        s.push_str(rcgui_i18n::t("# （本档案没有启用的挂载）\n"));
    }

    // ---- 服务 ----
    s.push_str(rcgui_i18n::t("\n# ---------------- 服务（按保存顺序） ----------------\n"));
    let mut any_serve = false;
    for sv in &doc.serves {
        if !sv.enabled {
            s.push_str(&rcgui_i18n::f("# 跳过（已停用）：{} {}\n", &[&(ps_comment(&sv.stype)), &(ps_comment(&sv.addr))]));
            continue;
        }
        any_serve = true;
        let key = format!("{}@{}", sv.stype, sv.addr);
        s.push_str(&rcgui_i18n::f("# 启动 {} 服务于 {}（密码从 -ServePasses 取，缺失则跳过）\n$key = {}\nif ($ServePasses.ContainsKey($key)) {{\n    $p = @{{ type = {}; fs = {}; addr = {}; user = {}; pass = $ServePasses[$key] }}\n}} else {{\n    Write-Warning (\"缺少 serve 密码，已跳过：\" + $key)\n    $p = $null\n}}\nif ($null -ne $p) {{ Invoke-Rc -Method 'serve/start' -Params $p }}\n", &[&(ps_comment(&sv.stype)), &(ps_comment(&sv.addr)), &(ps_quote(&key)), &(ps_quote(&sv.stype)), &(ps_quote(&compose_fs(&sv.remote, &sv.sub_path))), &(ps_quote(&sv.addr)), &(ps_quote(&sv.user))]));
    }
    if !any_serve {
        s.push_str(rcgui_i18n::t("# （本档案没有启用的服务）\n"));
    }

    s.push_str(rcgui_i18n::t("\nWrite-Host 'RCGUI 恢复完成（未启动/停止任何 rclone 进程）'\n"));
    s
}

const HEADER_FUNCS: &str = r#"# 只做 RC HTTP 调用；不接触进程、不写注册表、不建计划任务
$ErrorActionPreference = 'Stop'
$Headers = @{}
if ($RcUser) {
    $pair = '{0}:{1}' -f $RcUser, $RcPass
    $Headers['Authorization'] = 'Basic ' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($pair))
}

function Invoke-Rc {
    param([string]$Method, [hashtable]$Params = @{})
    $uri = $Endpoint.TrimEnd('/') + '/' + $Method
    $invoke = @{
        Uri         = $uri
        Method      = 'Post'
        Headers     = $Headers
        ContentType = 'application/json'
        Body        = ($Params | ConvertTo-Json -Depth 8 -Compress)
        TimeoutSec  = 600
    }
    return Invoke-RestMethod @invoke
}

# ---- 健康检查：等 RC 可用；不可用就明确退出，绝不启动 rclone ----
$deadline = (Get-Date).AddSeconds($HealthTimeoutSec)
$healthy = $false
while ((Get-Date) -lt $deadline) {
    try {
        $null = Invoke-Rc -Method 'core/version'
        $healthy = $true
        break
    } catch {
        Start-Sleep -Milliseconds $HealthIntervalMs
    }
}
if (-not $healthy) {
    [Console]::Error.WriteLine("RC endpoint '" + $Endpoint + "' 不可达或未启用 RC。")
    [Console]::Error.WriteLine('请先自行启动 rclone 并带 --rc（RCGUI 不会替你启动 rclone）。')
    exit 1
}
Write-Host ('已连接 rclone：' + $Endpoint)
"#;

/// PowerShell 单引号字面量：内部 `'` 需要写成 `''`。
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 注释里的自由文本：去掉换行，避免注释被截断成代码。
fn ps_comment(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

/// `remote` + `sub_path` → rclone fs 字符串。
fn compose_fs(remote: &str, sub_path: &str) -> String {
    let r = remote.trim().trim_end_matches(':');
    let p = sub_path.trim().trim_matches('/');
    if p.is_empty() {
        format!("{r}:")
    } else {
        format!("{r}:{p}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MountProfile, ProfileDoc, ServeProfile};

    fn doc() -> ProfileDoc {
        let mut d = ProfileDoc::default();
        d.meta.name = "工作".into();
        d.mounts.push(MountProfile {
            remote: "teldrive".into(),
            sub_path: "media".into(),
            point: "T:".into(),
            ro: true,
            cache: "writes".into(),
            enabled: true,
        });
        d.mounts.push(MountProfile {
            remote: "off".into(),
            point: "U:".into(),
            enabled: false,
            ..Default::default()
        });
        d.serves.push(ServeProfile {
            stype: "webdav".into(),
            remote: "teldrive".into(),
            sub_path: String::new(),
            addr: "127.0.0.1:8080".into(),
            user: "u".into(),
            pass: "SUPER_SECRET".into(),
            enabled: true,
        });
        d
    }

    #[test]
    fn script_never_starts_or_stops_rclone() {
        let s = generate(&doc(), "http://127.0.0.1:5572");
        for forbidden in [
            "rclone rcd",
            "Start-Process",
            "Stop-Process",
            "core/quit",
            "taskkill",
            "mount/unmount",
            "serve/stop",
            "schtasks",
            "Register-ScheduledTask",
            "HKCU:",
        ] {
            assert!(!s.contains(forbidden), "脚本不应包含 {forbidden}");
        }
    }

    #[test]
    fn script_has_health_check_and_clear_exit() {
        let s = generate(&doc(), "http://127.0.0.1:5572");
        assert!(s.contains("core/version"), "必须先做健康检查");
        assert!(s.contains("exit 1"), "连接失败必须明确退出");
        assert!(s.contains("不会替你启动 rclone"));
    }

    #[test]
    fn no_password_is_embedded() {
        let s = generate(&doc(), "http://127.0.0.1:5572");
        assert!(!s.contains("SUPER_SECRET"), "serve 密码不得写入脚本");
        assert!(s.contains("-ServePasses"), "serve 密码应改为运行时传入");
        // RC 密码同样只能来自参数/环境变量
        assert!(s.contains("$env:RCGUI_RC_PASS"));
    }

    #[test]
    fn enabled_mounts_in_order_and_disabled_skipped() {
        let s = generate(&doc(), "http://127.0.0.1:5572");
        let a = s.find("mount/mount").expect("应包含挂载调用");
        assert!(s.contains("'teldrive:media'"));
        assert!(s.contains("'T:'"));
        assert!(s.contains("read_only = $true"));
        assert!(!s.contains("'U:'"), "停用的挂载不应被写入");
        assert!(a > 0);
        assert!(s.contains("跳过（已停用）"));
    }

    #[test]
    fn quotes_are_escaped_not_concatenated() {
        let s = ps_quote("a'b c&%");
        assert_eq!(s, "'a''b c&%'");
        let mut d = ProfileDoc::default();
        d.mounts.push(MountProfile {
            remote: "r".into(),
            point: "T:".into(),
            ..Default::default()
        });
        d.serves.push(ServeProfile {
            stype: "http".into(),
            addr: "it's".into(),
            // ServeProfile 的 enabled 默认是 false（结构体 Default）；
            // 这里显式启用，才能验证带单引号的参数确实被转义。
            enabled: true,
            ..Default::default()
        });
        let body = generate(&d, "http://x/");
        assert!(body.contains("it''s"), "含单引号的参数必须转义而不是拼接");
    }

    #[test]
    fn endpoint_hint_is_escaped_into_param_default() {
        let s = generate(&ProfileDoc::default(), "http://127.0.0.1:5572");
        assert!(s.contains("[string]$Endpoint = 'http://127.0.0.1:5572'"));
    }

    #[test]
    fn enabled_serves_generate_calls_and_disabled_are_skipped() {
        let mut d = doc();
        d.serves.push(ServeProfile {
            stype: "sftp".into(),
            remote: "teldrive".into(),
            addr: "127.0.0.1:2222".into(),
            enabled: false,
            ..Default::default()
        });
        let s = generate(&d, "http://127.0.0.1:5572");
        assert!(s.contains("serve/start"), "启用的服务必须生成 serve/start 调用");
        assert!(s.contains("'webdav@127.0.0.1:8080'"));
        assert!(s.contains("# 跳过（已停用）：sftp 127.0.0.1:2222"), "停用服务应只留注释");
        assert!(!s.contains("'sftp@127.0.0.1:2222'"), "停用服务不应生成任何调用");
    }

    #[test]
    fn forbidden_list_targets_process_and_task_ops_not_restore_calls() {
        // 禁止的是进程/任务/停止类操作；mount/mount 与 serve/start 是恢复所必需的 RC 调用。
        let s = generate(&doc(), "http://127.0.0.1:5572");
        assert!(s.contains("mount/mount"), "恢复必须能挂载");
        assert!(s.contains("serve/start"), "恢复必须能起服务");
        for forbidden in [
            "rclone rcd",
            "Start-Process",
            "Stop-Process",
            "core/quit",
            "taskkill",
            "WScript.Shell",
            "schtasks",
            "Register-ScheduledTask",
        ] {
            assert!(!s.contains(forbidden), "脚本不应包含 {forbidden}");
        }
    }}
