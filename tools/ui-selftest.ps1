# UI self-test: verify scroll and click end-to-end with synthesized input.
#
# Why: the developer cannot see the screen, so "does the wheel scroll the grid"
# must be answered by injecting real input and reading back the counters the page
# writes into m0-results/ui-*.json.
#
# Uses SendInput (not the legacy mouse_event) -- mouse_event wheel events were not
# being delivered to the WebView2 in an earlier run.
#
# ASCII only (Windows PowerShell reads .ps1 as ANSI without a BOM).
#
# Usage: powershell -File tools\ui-selftest.ps1

$ErrorActionPreference = "Continue"
$root = "C:\myFiles\codes\deepseek\livephoto-windows"
$results = "$root\m0-results"

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
[StructLayout(LayoutKind.Sequential)]
public struct MOUSEINPUT {
  public int dx; public int dy;
  public uint mouseData; public uint dwFlags; public uint time;
  public IntPtr dwExtraInfo;
}
[StructLayout(LayoutKind.Sequential)]
public struct INPUT { public uint type; public MOUSEINPUT mi; }

public class U {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int ht, bool repaint);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }

  public const uint INPUT_MOUSE = 0;
  public const uint WHEEL = 0x0800, LEFTDOWN = 0x0002, LEFTUP = 0x0004;

  public static void Wheel(int delta) {
    INPUT[] a = new INPUT[1];
    a[0].type = INPUT_MOUSE;
    a[0].mi.dwFlags = WHEEL;
    a[0].mi.mouseData = unchecked((uint)delta);
    SendInput(1, a, Marshal.SizeOf(typeof(INPUT)));
  }
  public static void Click() {
    INPUT[] a = new INPUT[2];
    a[0].type = INPUT_MOUSE; a[0].mi.dwFlags = LEFTDOWN;
    a[1].type = INPUT_MOUSE; a[1].mi.dwFlags = LEFTUP;
    SendInput(2, a, Marshal.SizeOf(typeof(INPUT)));
  }
  public static int InputSize() { return Marshal.SizeOf(typeof(INPUT)); }
}
"@ -ErrorAction SilentlyContinue

Write-Host "INPUT struct size = $([U]::InputSize()) (expect 40 on x64)"

$proc = Get-Process app -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $proc) { Write-Host "NO APP WINDOW"; exit 1 }
$h = $proc.MainWindowHandle
[U]::ShowWindow($h, 9) | Out-Null
[U]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 900

$r = New-Object U+RECT
[U]::GetWindowRect($h, [ref]$r) | Out-Null
$barH = 56
$gx = [int](($r.L + $r.R) / 2)
$gy = [int](($r.T + $barH + $r.B) / 2)
Write-Host "window=$($r.L),$($r.T) size=$($r.R - $r.L)x$($r.B - $r.T)  grid center=$gx,$gy"

function Read-Diag([string]$tag) {
    $f = "$results\ui-$tag.json"
    if (-not (Test-Path $f)) { Write-Host "  (no $f)"; return }
    $py = @"
import json,sys
sys.stdout.reconfigure(encoding='utf-8')
j=json.load(open(r'$f',encoding='utf-8'))
c=j['counters']; g=j['grid']
print(f"  grid {g['clientWidth']}x{g['clientHeight']} scrollH={g['scrollHeight']} scrollTop={g['scrollTop']} mounted={j['mounted']}")
print(f"  wheel={c['wheel']} scroll={c['scroll']} click={c['cellClick']} resize={c['resize']} maxScrollTop={c['maxScrollTop']} selected={j['selectedId']}")
"@
    $py | Out-File -Encoding utf8 "$env:TEMP\diag.py"
    python "$env:TEMP\diag.py" 2>&1
}

Write-Host "`n--- BEFORE ---"
Read-Diag "tick"

[U]::SetCursorPos($gx, $gy) | Out-Null
Start-Sleep -Milliseconds 400

Write-Host "`n--- 10x wheel down (-120 each) ---"
for ($i = 0; $i -lt 10; $i++) { [U]::Wheel(-120); Start-Sleep -Milliseconds 100 }
Start-Sleep -Seconds 5
Write-Host "--- AFTER WHEEL ---"
Read-Diag "tick"

Write-Host "`n--- click ---"
[U]::Click()
Start-Sleep -Seconds 5
Write-Host "--- AFTER CLICK ---"
Read-Diag "tick"

Write-Host "`n--- resize window (narrower by 320px) ---"
[U]::MoveWindow($h, $r.L, $r.T, ($r.R - $r.L - 320), ($r.B - $r.T), $true) | Out-Null
Start-Sleep -Seconds 5
Write-Host "--- AFTER RESIZE ---"
Read-Diag "tick"
