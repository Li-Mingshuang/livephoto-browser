<#
Video sweep forensics: locate "some video files freeze the app when opened".

Why a script instead of clicking by hand:
  the developer cannot see the screen, so every conclusion must come from
  reproducible machine evidence. This script drives an in-app self-test
  (`LIVEPHOTO_SWEEP`) through environment variables. The frontend loads each
  video for real in a hidden `<video>` element and records:
    - did it reach canplay / what error / how long it took
    - the page main thread's worst stall during that load (50ms heartbeat drift)
    - how many range requests / bytes the protocol served, and the slowest disk read
  The result JSON is rewritten after EVERY file, so even if the app really
  freezes, the last line written names the culprit file.

Usage:
  ./tools/video-sweep.ps1                 # current implementation (async + 4MB chunks)
  ./tools/video-sweep.ps1 -Legacy         # old behaviour (sync + no chunk limit), for A/B
  ./tools/video-sweep.ps1 -Limit 40 -Timeout 8000
#>
param(
    [switch]$Legacy,
    [int]$Limit = 24,
    [int]$Timeout = 6000,
    [string]$Folder = "F:\DCIM",
    [int]$WaitSeconds = 420
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "target\release\app.exe"
$results = Join-Path $root "m0-results"
$json = Join-Path $results "ui-video-sweep.json"

if (-not (Test-Path $exe)) { throw "not found: $exe (build first: cargo build --release -p app --features custom-protocol)" }

# Clean start: a leftover process holds the exe and pollutes the evidence
Get-Process -Name app -ErrorAction SilentlyContinue | Stop-Process -Force
if (Test-Path $json) { Remove-Item $json -Force }

$env:LIVEPHOTO_OPEN = $Folder
$env:LIVEPHOTO_SWEEP = [string]$Limit
$env:LIVEPHOTO_SWEEP_TIMEOUT = [string]$Timeout
if ($Legacy) {
    $env:LIVEPHOTO_SYNC_PROTOCOL = "1"
    $env:LIVEPHOTO_MEDIA_CHUNK = "0"
} else {
    Remove-Item Env:\LIVEPHOTO_SYNC_PROTOCOL -ErrorAction SilentlyContinue
    Remove-Item Env:\LIVEPHOTO_MEDIA_CHUNK -ErrorAction SilentlyContinue
}

if ($Legacy) {
    Write-Host "mode: LEGACY (synchronous protocol + no chunk limit)"
} else {
    Write-Host "mode: CURRENT (asynchronous protocol + 4MB chunks)"
}
Write-Host "sample: $Limit files, per-file timeout ${Timeout}ms, folder $Folder"

$tag = "async"
if ($Legacy) { $tag = "legacy" }
$log = Join-Path $results "sweep-$tag.log"
if (Test-Path $log) { Remove-Item $log -Force }

$proc = Start-Process -FilePath $exe -PassThru -RedirectStandardOutput $log -RedirectStandardError "$log.err"
Write-Host "started pid=$($proc.Id), waiting for the self-test (max ${WaitSeconds}s)"

$deadline = (Get-Date).AddSeconds($WaitSeconds)
$last = $null
$crashed = $false
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 3
    if ($proc.HasExited) {
        # -536870904 (0xE0000008) 之类的退出码代表进程被异常终止（不是正常关窗）
        Write-Host "!! process exited, code=$($proc.ExitCode)"
        $crashed = $true
        break
    }
    if (-not (Test-Path $json)) { continue }
    try { $last = Get-Content $json -Raw -Encoding UTF8 | ConvertFrom-Json } catch { continue }
    if ($last.videoSweep.pickedCount -gt 0 -and $last.videoSweep.tested -ge $last.videoSweep.pickedCount) { break }
}

if (-not $last -or -not $last.videoSweep) {
    Write-Host "!! no result was produced (crashed=$crashed)"
    if (Test-Path $log) { Write-Host "---- app stdout (tail) ----"; Get-Content $log -Tail 40 }
    exit 1
}
$last = $last.videoSweep

Write-Host ""
Write-Host ("tested {0}/{1} (library: {2} video assets of {3} assets)" -f $last.tested, $last.pickedCount, $last.videoAssets, $last.total)
Write-Host ("worst main-thread stall: {0}ms (id={1})" -f $last.worstStall.stallMs, $last.worstStall.id)
Write-Host ""
$rows = $last.results | ForEach-Object {
    [pscustomobject]@{
        id        = $_.id
        kind      = $_.kind
        MB        = [math]::Round(($_.videoBytes / 1048576.0), 1)
        outcome   = $_.outcome
        totalMs   = $_.totalMs
        stallMs   = $_.mainThreadMaxStallMs
        stalls    = $_.mainThreadStalls
        reqs      = $_.protocol.requests
        srvMB     = [math]::Round(($_.protocol.bytes / 1048576.0), 1)
        readMaxMs = $_.protocol.readMsMax
        playedTo  = $_.playedTo
        errCode   = if ($_.error) { $_.error.code } else { "" }
    }
}
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host

Copy-Item $json (Join-Path $results "ui-video-sweep-$tag.json") -Force
Write-Host "result: $json (also saved as ui-video-sweep-$tag.json)"
Write-Host "app log: $log"

if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force; Write-Host "closed pid=$($proc.Id)" }
