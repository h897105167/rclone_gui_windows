# 用 PrintWindow 抓窗口 + 按窗口坐标点击，不依赖屏幕上的窗口位置。
#
# 为什么不用 [shot.ps1](shot.ps1) 那套 GetWindowRect + CopyFromScreen：
# 本机（175% 缩放）下 GetWindowRect 有时会返回高度 65535 这种编组坏值，
# CopyFromScreen 又会把别的窗口内容拍进来（曾拍到本会话的 DSH 窗口）。
# PrintWindow 直接让窗口把自己画到内存 DC 上，位置无关、可后台。
#
# 用法：
#   .\scripts\winshot.ps1 -Out a.png
#   .\scripts\winshot.ps1 -Out b.png -Click "50,622"
#   .\scripts\winshot.ps1 -Out c.png -Click "50,622" -Wait 1200
#   .\scripts\winshot.ps1 -Out d.png -Wheel "700,600,-3"   # 在 (700,600) 向下滚 3 格
param(
    [string]$Out = "shot.png",
    [string]$Click = "",
    [string]$Wheel = "",
    [int]$Wait = 900,
    [string]$ProcName = "rclone-gui",
    [int]$ProcId = 0,
    [int]$Width = 1400,
    [int]$Height = 900
)

Add-Type -TypeDefinition @"
using System;
using System.Drawing;
using System.Runtime.InteropServices;
public class WinCap {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint f);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint data, IntPtr extra);

    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }

    // 只在需要点击时移动光标：把窗口内的客户区坐标换算成屏幕坐标。
    public static void ClickClient(IntPtr h, int x, int y) {
        POINT p; p.X = x; p.Y = y;
        ClientToScreen(h, ref p);
        SetCursorPos(p.X, p.Y);
        System.Threading.Thread.Sleep(120);
        mouse_event(0x0002, 0, 0, 0, IntPtr.Zero);   // left down
        mouse_event(0x0004, 0, 0, 0, IntPtr.Zero);   // left up
    }

    // 滚轮：data 是 uint（mouse_event 的参数类型），负数要写成补码。
    // 3 格 = 3*120 = 360；向上滚传正数。
    public static void WheelClient(IntPtr h, int x, int y, int notches) {
        POINT p; p.X = x; p.Y = y;
        ClientToScreen(h, ref p);
        SetCursorPos(p.X, p.Y);
        System.Threading.Thread.Sleep(120);
        uint delta = (uint)(notches * 120);
        mouse_event(0x0800, 0, 0, delta, IntPtr.Zero);  // MOUSEEVENTF_WHEEL
    }

    public static bool Save(IntPtr h, string path) {
        RECT r; GetWindowRect(h, out r);
        int w = r.Right - r.Left, ht = r.Bottom - r.Top;
        if (w <= 0 || ht <= 0 || ht > 5000 || w > 5000) return false;
        Bitmap bmp = new Bitmap(w, ht);
        using (Graphics g = Graphics.FromImage(bmp)) {
            IntPtr hdc = g.GetHdc();
            bool ok = PrintWindow(h, hdc, 2);          // PW_RENDERFULLCONTENT
            g.ReleaseHdc(hdc);
            if (!ok) {
                hdc = g.GetHdc();
                PrintWindow(h, hdc, 0);
                g.ReleaseHdc(hdc);
            }
        }
        bmp.Save(path, System.Drawing.Imaging.ImageFormat.Png);
        bmp.Dispose();
        return true;
    }
}
"@ -ReferencedAssemblies System.Drawing

[WinCap]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null

$target = if ($ProcId -gt 0) {
    Get-Process -Id $ProcId -ErrorAction SilentlyContinue
} else {
    Get-Process $ProcName -ErrorAction SilentlyContinue | Select-Object -First 1
}
if (-not $target) { throw "process not found: $ProcName (id=$ProcId)" }
$h = $target.MainWindowHandle
if ($h -eq [IntPtr]::Zero) { throw "no main window for $ProcName" }

if ($Width -gt 0 -and $Height -gt 0) {
    [WinCap]::SetWindowPos($h, [IntPtr]::Zero, 40, 20, $Width, $Height, 0x0004) | Out-Null
}
[WinCap]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 500

if ($Click -ne "") {
    $parts = $Click.Split(",")
    [WinCap]::ClickClient($h, [int]$parts[0], [int]$parts[1])
    Start-Sleep -Milliseconds $Wait
}

if ($Wheel -ne "") {
    $parts = $Wheel.Split(",")
    [WinCap]::WheelClient($h, [int]$parts[0], [int]$parts[1], [int]$parts[2])
    Start-Sleep -Milliseconds $Wait
}

$ok = [WinCap]::Save($h, $Out)
if (-not $ok) { throw "capture failed (window rect out of range)" }
Write-Host "saved $Out"
