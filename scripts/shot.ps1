param(
    [string]$Out = "shot.png",
    [int]$W = 0,
    [int]$H = 0,
    [int]$X = 40,
    [int]$Y = 20,
    [string]$Click = "",
    [string]$ProcName = "rclone-gui"
)

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class ShotWin {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint f);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint data, IntPtr extra);
    public struct RECT { public int Left, Top, Right, Bottom; }

    // PowerShell 传 [ref] 给 out RECT 时结构体会被错误编组（出现过高度 65535），
    // 因此统一走这个方法，返回 [left, top, width, height]。
    public static int[] Rect(IntPtr h) {
        RECT r;
        if (!GetWindowRect(h, out r)) return new int[] { 0, 0, 0, 0 };
        return new int[] { r.Left, r.Top, r.Right - r.Left, r.Bottom - r.Top };
    }
}
"@

# -4 = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2。
# 不做这一步的话，GetWindowRect 返回物理像素而 CopyFromScreen 按逻辑像素工作，
# 150% 缩放下截图会错位。
[ShotWin]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null

$target = Get-Process $ProcName -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $target) { throw "process not found: $ProcName" }
$h = $target.MainWindowHandle
if ($h -eq [IntPtr]::Zero) { throw "no main window for $ProcName" }

[ShotWin]::ShowWindow($h, 9) | Out-Null
if ($W -gt 0 -and $H -gt 0) {
    [ShotWin]::SetWindowPos($h, [IntPtr]::Zero, $X, $Y, $W, $H, 0x0004) | Out-Null
} else {
    [ShotWin]::SetWindowPos($h, [IntPtr]::Zero, $X, $Y, 0, 0, 0x0005) | Out-Null
}
[ShotWin]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 900

$r = [ShotWin]::Rect($h)
if ($Click -ne "") {
    $parts = $Click.Split(",")
    [ShotWin]::SetCursorPos($r[0] + [int]$parts[0], $r[1] + [int]$parts[1]) | Out-Null
    Start-Sleep -Milliseconds 150
    [ShotWin]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero) | Out-Null
    [ShotWin]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 1000
    $r = [ShotWin]::Rect($h)
}

Add-Type -AssemblyName System.Drawing
$cw = $r[2]
$ch = $r[3]
$bmp = New-Object System.Drawing.Bitmap $cw, $ch
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r[0], $r[1], 0, 0, (New-Object System.Drawing.Size $cw, $ch))
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()
Write-Host "saved $Out (${cw}x${ch} at $($r[0]),$($r[1]))"
