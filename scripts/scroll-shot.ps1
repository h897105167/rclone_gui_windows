param(
    [string]$Out = "shot.png",
    [int]$WheelY = 600,
    [int]$Ticks = -3,
    [string]$ProcName = "rclone-gui"
)

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class ScrollWin {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint data, IntPtr extra);
    public struct RECT { public int Left, Top, Right, Bottom; }
    public static int[] Rect(IntPtr h) {
        RECT r;
        if (!GetWindowRect(h, out r)) return new int[] { 0, 0, 0, 0 };
        return new int[] { r.Left, r.Top, r.Right - r.Left, r.Bottom - r.Top };
    }
}
"@

[ScrollWin]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null

$target = Get-Process $ProcName -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $target) { throw "process not found: $ProcName" }
$h = $target.MainWindowHandle
if ($h -eq [IntPtr]::Zero) { throw "no main window for $ProcName" }

[ScrollWin]::ShowWindow($h, 9) | Out-Null
[ScrollWin]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 700

$r = [ScrollWin]::Rect($h)
[ScrollWin]::SetCursorPos($r[0] + 500, $r[1] + $WheelY) | Out-Null
Start-Sleep -Milliseconds 200
for ($i = 0; $i -lt [Math]::Abs($Ticks); $i++) {
    # 滚轮一格 = 120；负值（向下）作为 uint 传入要换算成 2^32 补数。
    $data = if ($Ticks -lt 0) { [uint32]4294967176 } else { [uint32]120 }
    [ScrollWin]::mouse_event(0x0800, 0, 0, $data, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 120
}
Start-Sleep -Milliseconds 900

Add-Type -AssemblyName System.Drawing
$cw = $r[2]
$ch = $r[3]
$bmp = New-Object System.Drawing.Bitmap $cw, $ch
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r[0], $r[1], 0, 0, (New-Object System.Drawing.Size $cw, $ch))
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()
Write-Host "saved $Out (${cw}x${ch})"
