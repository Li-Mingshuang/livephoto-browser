# Capture only the app window content (no desktop, no other windows) for README shots.
#
# Why PrintWindow instead of a screen copy:
#   an earlier screen copy of "the rectangle where the window is" also photographed
#   whatever else happened to be on screen (a browser) and that got published by
#   mistake. PrintWindow + PW_RENDERFULLCONTENT asks the window to paint itself into
#   an in-memory bitmap, so overlapping windows are never included.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI when there is no BOM, so a Chinese
# comment can be decoded into mojibake whose trailing byte swallows the next line.
#
# Usage:
#   powershell -File tools\capture-window.ps1 -ProcessId 1234 -Out shot.png
#   powershell -File tools\capture-window.ps1 -Title "LivePhoto" -Out shot.png

param(
    [int]$ProcessId = 0,
    [string]$Title = "LivePhoto",
    [string]$Out = "shot.png",
    [int]$SettleMs = 1200
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

Add-Type -TypeDefinition @"
using System;
using System.Drawing;
using System.Runtime.InteropServices;
public class WinCap {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT r, int size);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  public const uint PW_RENDERFULLCONTENT = 0x2;

  public static RECT Frame(IntPtr h) {
    RECT r;
    if (DwmGetWindowAttribute(h, 9, out r, Marshal.SizeOf(typeof(RECT))) == 0) return r;
    GetWindowRect(h, out r);
    return r;
  }

  public static Bitmap Capture(IntPtr h) {
    RECT r = Frame(h);
    int w = Math.Max(1, r.R - r.L), ht = Math.Max(1, r.B - r.T);
    var bmp = new Bitmap(w, ht, System.Drawing.Imaging.PixelFormat.Format32bppArgb);
    using (var g = Graphics.FromImage(bmp)) {
      IntPtr hdc = g.GetHdc();
      PrintWindow(h, hdc, PW_RENDERFULLCONTENT);
      g.ReleaseHdc(hdc);
    }
    return bmp;
  }
}
"@ -ReferencedAssemblies System.Drawing

if ($ProcessId -gt 0) {
    $proc = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
} else {
    $proc = Get-Process | Where-Object { $_.MainWindowTitle -like "*$Title*" -and $_.MainWindowHandle -ne 0 } | Select-Object -First 1
}
if (-not $proc) { Write-Host "NO WINDOW (pid=$ProcessId title=$Title)"; exit 1 }

$h = $proc.MainWindowHandle
[WinCap]::ShowWindow($h, 9) | Out-Null       # SW_RESTORE
[WinCap]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds $SettleMs

$bmp = [WinCap]::Capture($h)
$w = $bmp.Width
$ht = $bmp.Height

# Liveness check: too few distinct colours means nothing was painted
# (WebView2 sometimes hands back a blank bitmap).
$colors = @{}
$nonBlack = 0
$tot = 0
for ($y = 0; $y -lt $ht; $y += 7) {
    for ($x = 0; $x -lt $w; $x += 7) {
        $p = $bmp.GetPixel($x, $y)
        $tot++
        if ($p.R -gt 12 -or $p.G -gt 12 -or $p.B -gt 12) { $nonBlack++ }
        $key = "$($p.R),$($p.G),$($p.B)"
        $colors[$key] = 1
    }
}
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()

Write-Host ("saved {0} ({1}x{2}), distinct colors={3}, non-black={4:P0}" -f $Out, $w, $ht, $colors.Count, ($nonBlack / [double]$tot))
if ($colors.Count -lt 8) { Write-Host "WARNING: capture looks blank"; exit 2 }
