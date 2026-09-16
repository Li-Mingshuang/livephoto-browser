<#
Boot self-test, repeated N times.

Why: the in-app self-tests (viewer click, HEVC playback, live playback) were seen to
FAIL intermittently -- one run in four timed out at `canplay` while the identical
binary passed in the runs before and after it. A single green run therefore proves
nothing about flakiness; this harness boots the app several times, waits a fixed
number of seconds, kills it, and prints the same key fields every time so a pass
rate can be stated honestly.

Reads the JSON the frontend writes itself (m0-results/ui-*.json), so nothing here
depends on the screen being visible.

ASCII only (Windows PowerShell reads .ps1 as ANSI without a BOM).

Usage:
  pwsh tools/boot-selftest.ps1 -Runs 4 -Seconds 40
#>
param(
    [int]$Runs = 4,
    [int]$Seconds = 40,
    [string]$Folder = "F:\DCIM"
)

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "target\release\app.exe"
$results = Join-Path $root "m0-results"

$env:LIVEPHOTO_OPEN = $Folder
Remove-Item Env:\LIVEPHOTO_SWEEP -ErrorAction SilentlyContinue
Remove-Item Env:\LIVEPHOTO_SYNC_PROTOCOL -ErrorAction SilentlyContinue
Remove-Item Env:\LIVEPHOTO_MEDIA_CHUNK -ErrorAction SilentlyContinue

function Read-Json([string]$name) {
    $f = Join-Path $results "ui-$name.json"
    if (-not (Test-Path $f)) { return $null }
    try { return Get-Content $f -Raw -Encoding UTF8 | ConvertFrom-Json } catch { return $null }
}

$summary = @()
for ($i = 1; $i -le $Runs; $i++) {
    Get-Process -Name app -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Seconds 2
    foreach ($n in "media-selftest", "live-selftest", "viewer-selftest") {
        $f = Join-Path $results "ui-$n.json"
        if (Test-Path $f) { Remove-Item $f -Force }
    }

    $p = Start-Process -FilePath $exe -PassThru
    Start-Sleep -Seconds $Seconds
    $alive = -not $p.HasExited
    if ($alive) { Stop-Process -Id $p.Id -Force }

    $m = Read-Json "media-selftest"
    $l = Read-Json "live-selftest"
    $v = Read-Json "viewer-selftest"

    $row = [pscustomobject]@{
        run            = $i
        alive          = $alive
        mediaCanPlay   = if ($m) { $m.mediaTest.canPlay } else { $null }
        mediaTimedOut  = if ($m) { $m.mediaTest.timedOut } else { $null }
        mediaReady     = if ($m) { $m.mediaTest.readyState } else { $null }
        liveReady      = if ($l) { $l.liveTest.readyState } else { $null }
        liveTime       = if ($l) { $l.liveTest.currentTime } else { $null }
        clickPass      = if ($v) { $v.viewerTest.clickTest.pass } else { $null }
        thumbsReady    = if ($m) { $m.thumbsReady } else { $null }
    }
    $summary += $row
    Write-Host ("run {0}: canPlay={1} timedOut={2} mediaReady={3} liveReady={4} liveTime={5} clickPass={6} thumbs={7} alive={8}" -f `
        $row.run, $row.mediaCanPlay, $row.mediaTimedOut, $row.mediaReady, $row.liveReady, $row.liveTime, $row.clickPass, $row.thumbsReady, $row.alive)
}

Get-Process -Name app -ErrorAction SilentlyContinue | Stop-Process -Force
Write-Host ""
$ok = ($summary | Where-Object { $_.mediaCanPlay -eq $true -and $_.clickPass -eq $true }).Count
Write-Host ("summary: {0}/{1} runs had media canPlay + viewer click pass" -f $ok, $Runs)
$summary | Export-Csv (Join-Path $results "boot-selftest.csv") -NoTypeInformation -Encoding UTF8
Write-Host ("saved: {0}" -f (Join-Path $results "boot-selftest.csv"))
