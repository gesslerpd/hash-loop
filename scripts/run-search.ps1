<#
.SYNOPSIS
  Run a GPU cycle search with auto-tuned settings for a given --bits width (48-160).

.DESCRIPTION
  Picks --trials/--gpu-batch-size/--gpu-block-size/--gpu-restarts/--timeout-secs based on
  the requested bit width and invokes the release hash-loop binary. The positional `max`
  is set to u64::MAX (effectively unbounded) since --timeout-secs is the real stop
  condition; Brent's step cap is not the practical bottleneck at any of these widths.
  Expected work scales as 2^(bits/2), so bit widths above ~100 are exploratory only -
  "no cycle found" within the timeout is the expected, honest outcome there.

.PARAMETER Bits
  Number of leading SHA-1 bits retained (48-160).

.PARAMETER TimeoutSecs
  Override the auto-selected wall-clock budget, in seconds.

.PARAMETER MaxCycleLength
  Optional strict cutoff passed through as --max-cycle-length, to search only for an
  improvement over an existing recorded witness.

.PARAMETER DryRun
  Print the command that would run instead of running it.

.EXAMPLE
  ./scripts/run-search.ps1 -Bits 64
.EXAMPLE
  ./scripts/run-search.ps1 -Bits 96 -TimeoutSecs 900
.EXAMPLE
  ./scripts/run-search.ps1 -Bits 52 -MaxCycleLength 18935
#>
param(
    [Parameter(Mandatory = $true)]
    [ValidateRange(16, 160)]
    [int]$Bits,

    [int]$TimeoutSecs,

    [long]$MaxCycleLength,

    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

# Cumulative tiers: first tier whose Max >= Bits wins. Higher bits get a larger time
# budget but fewer restarts, since each restart re-runs the full budget from scratch and
# more parallel trials (not more restarts) is what actually helps find shorter cycles.
$tiers = @(
    @{ Max = 56;  TimeoutSecs = 60;   Restarts = 64 }
    @{ Max = 64;  TimeoutSecs = 180;  Restarts = 32 }
    @{ Max = 72;  TimeoutSecs = 600;  Restarts = 16 }
    @{ Max = 80;  TimeoutSecs = 1200; Restarts = 8 }
    @{ Max = 92;  TimeoutSecs = 1800; Restarts = 8 }
    @{ Max = 104; TimeoutSecs = 3600; Restarts = 4 }
    @{ Max = 160; TimeoutSecs = 3600; Restarts = 4 }
)
$tier = $tiers | Where-Object { $Bits -le $_.Max } | Select-Object -First 1

$effectiveTimeout = if ($TimeoutSecs) { $TimeoutSecs } else { $tier.TimeoutSecs }
$restarts = $tier.Restarts

# Fixed across all widths: GPU wall time for a fixed step budget barely depends on trial
# count (all trials run concurrently), and 65536 trials / a single batch / block size 512
# was measured as the sustained throughput ceiling on an RTX 3070 (8 GiB).
$trials = 65536
$batchSize = 65536
$blockSize = 512
$stepsPerDispatch = 65536
$max = 18446744073709551615 # u64::MAX; --timeout-secs is the real stop condition

if ($Bits -gt 100) {
    Write-Warning "bits=${Bits}: expected work is on the order of 2^$([math]::Round($Bits / 2)) hash evaluations - many orders of magnitude beyond what's reachable in $effectiveTimeout seconds or any realistic budget. 'no cycle found' is the expected, honest outcome; treat this as an exploratory/benchmark sample only."
}

if ($env:CUDA_PATH -and (Test-Path $env:CUDA_PATH)) {
    $cudaBin = Join-Path $env:CUDA_PATH "bin"
} else {
    $cudaBin = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v13.3\bin"
}
$cudaBinX64 = Join-Path $cudaBin "x64"
if (Test-Path $cudaBin) {
    $env:Path = "$cudaBin;$cudaBinX64;$env:Path"
}

$exe = Join-Path $PSScriptRoot "..\target\x86_64-pc-windows-msvc\release\hash-loop.exe"
if (-not (Test-Path $exe)) {
    throw "Release binary not found at $exe. Build it first with: cargo +stable-x86_64-pc-windows-msvc build --release --target x86_64-pc-windows-msvc"
}

$argList = @(
    "-v",
    # for seed replay/verification (always use CPU for this)
    # "--seed", "0000000000000000000000000000000000000000",
    # "--trials", "1",
    # "--gpu-restarts", "1",

    "--gpu",
    "--trials", $trials,
    "--gpu-restarts", $restarts,

    "--bits", $Bits,
    "--gpu-batch-size", $batchSize,
    "--gpu-block-size", $blockSize,
    "--gpu-steps-per-dispatch", $stepsPerDispatch,
    "--timeout-secs", $effectiveTimeout
)
if ($MaxCycleLength) {
    $argList += @("--max-cycle-length", $MaxCycleLength)
}
$argList += $max

Write-Host "$exe $($argList -join ' ')"
if ($DryRun) {
    exit 0
}

& $exe @argList
Write-Host "EXIT_CODE=$LASTEXITCODE"
