# Degradation comparison: thumbnail throughput vs progress, across worker counts.
# Usage: powershell -File tools\run-degradation.ps1 [sourceRoot]
#
# Starts from an empty cache each time so runs are comparable.
# NOTE: ASCII only -- Windows PowerShell reads .ps1 as ANSI without a BOM,
#       and non-ASCII characters break string parsing.

$ErrorActionPreference = "Continue"
$root = "C:\myFiles\codes\deepseek\livephoto-windows"
$exe = "$root\target\release\bench_thumbs.exe"
$dir = "$root\m0-results"
$cacheDir = "$dir\thumb-cache"
$srcRoot = if ($args.Count -ge 1) { $args[0] } else { "F:\DCIM" }

$configs = @(
    @{ w = 1; n = 800 },
    @{ w = 2; n = 800 },
    @{ w = 1; n = 1500 }
)

foreach ($cfg in $configs) {
    $w = $cfg.w
    $n = $cfg.n
    $tag = "$n-$($w)w"

    if (Test-Path $cacheDir) { Remove-Item -Recurse -Force $cacheDir -ErrorAction SilentlyContinue }

    Write-Host ""
    Write-Host "===== $n thumbs / $w workers ====="
    $p = Start-Process -FilePath $exe -ArgumentList $srcRoot, "$n", "$w" -PassThru `
        -RedirectStandardOutput "$dir\deg-$tag.out.txt" `
        -RedirectStandardError "$dir\deg-$tag.err.txt"
    $p | Wait-Process -Timeout 1200 -ErrorAction SilentlyContinue
    if (-not $p.HasExited) { $p.Kill(); Write-Host "  >>> TIMEOUT, killed" }

    $jsonPath = "$dir\thumbs-dcim-$n-$($w)w.json"
    if (Test-Path $jsonPath) {
        $j = Get-Content $jsonPath -Raw | ConvertFrom-Json
        Write-Host ("  throughput {0}/s  avg {1}ms  max {2}ms  max/avg {3}  err {4}" -f `
                $j.pass1.throughput_per_sec, $j.pass1.avg_decode_ms, $j.pass1.max_decode_ms, `
                $j.pass1.max_over_avg, $j.pass1.err)
        Write-Host ("  first100 {0}/s -> last100 {1}/s  degradation {2}" -f `
                $j.instability_check.first_100_per_sec, $j.instability_check.last_100_per_sec, `
                $j.instability_check.degradation_ratio)
        $b = ($j.throughput_buckets | ForEach-Object { "$($_.items)=$($_.per_sec)" }) -join "  "
        Write-Host "  buckets: $b"
        Write-Host ("  cache {0} files / {1} MB   hot-cache {2}/s" -f `
                $j.cache.files, [math]::Round($j.cache.bytes / 1MB, 1), $j.pass2_all_cached.throughput_per_sec)
    }
    else {
        Write-Host "  NO RESULT: $jsonPath"
        Get-Content "$dir\deg-$tag.err.txt" -ErrorAction SilentlyContinue | Select-Object -First 8
    }
}
