#Requires -Version 5.1
<#
.SYNOPSIS
    Captures a game frame-pacing baseline with PresentMon and analyzes it.

.DESCRIPTION
    Follows docs/benchmark-protocol.md: records the environment, then runs
    PresentMon for N timed captures of the game process (a high beep marks the
    start of each capture, two short beeps its end), then runs `bench frames`
    on the CSV files.

    PresentMon needs administrator rights (ETW). Without them, the capture
    step relaunches this script elevated (one UAC prompt) in a hidden window,
    so the game keeps its fullscreen focus. Nothing in the system, game or GPU
    configuration is changed; settings are only read.

    Keep this file ASCII-only: PowerShell 5.1 reads BOM-less files with the
    ANSI code page.

.EXAMPLE
    .\scripts\bench-baseline.ps1 -Label range-static -PresentMon D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe -Notes 'fps limit off'
#>
[CmdletBinding()]
param(
    # Short name of the scene; becomes part of the output folder name.
    [Parameter(Mandatory)] [ValidatePattern('^[A-Za-z0-9_-]+$')] [string] $Label,

    # PresentMon console executable (default: the ZIZ_PRESENTMON environment variable).
    [string] $PresentMon = $env:ZIZ_PRESENTMON,

    [string] $ProcessName = 'VALORANT-Win64-Shipping.exe',
    [ValidateRange(1, 50)] [int] $Runs = 5,
    [ValidateRange(5, 600)] [int] $Seconds = 60,

    # Time to get back into the game after the UAC prompt.
    [ValidateRange(0, 300)] [int] $StartDelay = 20,

    # Pause between captures; used for the GapHint action.
    [ValidateRange(0, 300)] [int] $Gap = 10,

    [string] $GapHint = 'tap crouch once (avoids the inactivity kick in The Range); no other input',

    # Free text stored in environment.json (e.g. the in-game FPS limit).
    [string] $Notes = '',

    # Output folder [default: <lab>\baseline\<yyyy-MM-dd-HHmm>-<Label>].
    [string] $OutDir,

    # Internal: set when the script relaunches itself elevated to run the captures.
    [switch] $CaptureOnly
)

# Native commands report failure through $LASTEXITCODE; their stderr must
# not be treated as a PowerShell error.
$ErrorActionPreference = 'Continue'

function Test-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    ([Security.Principal.WindowsPrincipal] $identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Invoke-Captures {
    $log = Join-Path $OutDir 'capture-log.txt'
    function Write-Log([string] $message) {
        ('{0:yyyy-MM-dd HH:mm:ss} {1}' -f (Get-Date), $message) | Out-File -FilePath $log -Append -Encoding utf8
    }
    Write-Log "captures start in $StartDelay s"
    Start-Sleep -Seconds $StartDelay
    for ($run = 1; $run -le $Runs; $run++) {
        $csv = Join-Path $OutDir ('run-{0}.csv' -f $run)
        [console]::Beep(880, 250)
        Write-Log "run $run start"
        $output = & $PresentMon --process_name $ProcessName --output_file $csv --timed $Seconds `
            --terminate_after_timed --no_console_stats --qpc_time `
            --session_name ziz_baseline --stop_existing_session 2>&1
        $exitCode = $LASTEXITCODE
        foreach ($line in $output) { Write-Log "  presentmon: $line" }
        Write-Log "run $run end (exit $exitCode)"
        [console]::Beep(660, 150); Start-Sleep -Milliseconds 100; [console]::Beep(660, 150)
        if ($run -lt $Runs) { Start-Sleep -Seconds $Gap }
    }
    Write-Log 'captures done'
}

if ($CaptureOnly) {
    Invoke-Captures
    exit 0
}

$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $PresentMon -or -not (Test-Path -LiteralPath $PresentMon -PathType Leaf)) {
    Write-Host 'PresentMon not found: pass -PresentMon <path to PresentMon-x.y.z-x64.exe> or set ZIZ_PRESENTMON.' -ForegroundColor Red
    exit 2
}
$PresentMon = (Resolve-Path -LiteralPath $PresentMon).Path

$processBaseName = [IO.Path]::GetFileNameWithoutExtension($ProcessName)
if (-not (Get-Process -Name $processBaseName -ErrorAction SilentlyContinue)) {
    Write-Host "$ProcessName is not running; start the game and enter the scene first." -ForegroundColor Red
    exit 2
}

if (-not $OutDir) {
    $lab = if ($env:ZIZ_LAB_DIR) { $env:ZIZ_LAB_DIR } else { Join-Path (Split-Path -Parent $repoRoot) 'lab' }
    $OutDir = Join-Path $lab ('baseline\{0:yyyy-MM-dd-HHmm}-{1}' -f (Get-Date), $Label)
}
if ((Test-Path -LiteralPath $OutDir) -and (Get-ChildItem -LiteralPath $OutDir -Force | Select-Object -First 1)) {
    Write-Host "Output folder is not empty: $OutDir" -ForegroundColor Red
    exit 2
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

$bench = Join-Path $repoRoot 'target\release\bench.exe'
Push-Location $repoRoot
try {
    & cargo build --release -p bench
    if ($LASTEXITCODE -ne 0) { Write-Host 'cargo build failed' -ForegroundColor Red; exit $LASTEXITCODE }
}
finally {
    Pop-Location
}

# Environment, read only.
$hags = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\GraphicsDrivers' -Name HwSchMode -ErrorAction SilentlyContinue).HwSchMode
$gameMode = (Get-ItemProperty 'HKCU:\Software\Microsoft\GameBar' -Name AutoGameModeEnabled -ErrorAction SilentlyContinue).AutoGameModeEnabled
$environment = [ordered]@{
    captured_on        = (Get-Date).ToString('yyyy-MM-ddTHH:mm:ssK')
    label              = $Label
    process            = $ProcessName
    runs               = $Runs
    seconds_per_run    = $Seconds
    gap_seconds        = $Gap
    gap_action         = $GapHint
    # The release file name carries the version; the executable has no version resource.
    presentmon         = (Split-Path -Leaf $PresentMon)
    presentmon_sha256  = (Get-FileHash -LiteralPath $PresentMon -Algorithm SHA256).Hash
    power_scheme       = ((powercfg /getactivescheme) -join ' ').Trim()
    hags_hwschmode     = if ($null -ne $hags) { $hags } else { 'not set (default)' }
    game_mode_auto     = if ($null -ne $gameMode) { $gameMode } else { 'not set (default)' }
    notes              = $Notes
}
$environment | ConvertTo-Json | Out-File -FilePath (Join-Path $OutDir 'environment.json') -Encoding utf8
& $bench probe --json (Join-Path $OutDir 'probe.json') | Out-Null

$total = $StartDelay + $Runs * $Seconds + ($Runs - 1) * $Gap
Write-Host ''
Write-Host "Baseline '$Label': $Runs x $Seconds s of $ProcessName (about $([math]::Ceiling($total / 60)) min)." -ForegroundColor Cyan
Write-Host "  1. Accept the UAC prompt if it appears, then click back into the game ($StartDelay s)."
Write-Host '  2. High beep: capture started. Keep still, no input.'
Write-Host "  3. Two short beeps: capture ended. In the $Gap s pause: $GapHint."
Write-Host "  Output: $OutDir"
Write-Host ''

$captureArguments = @{
    OutDir = $OutDir; PresentMon = $PresentMon; ProcessName = $ProcessName
    Runs = $Runs; Seconds = $Seconds; StartDelay = $StartDelay; Gap = $Gap
}
if (Test-Administrator) {
    Invoke-Captures
}
else {
    $argumentList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', ('"{0}"' -f $PSCommandPath),
        '-Label', $Label, '-CaptureOnly')
    foreach ($name in $captureArguments.Keys) {
        $argumentList += @("-$name", ('"{0}"' -f $captureArguments[$name]))
    }
    try {
        Start-Process powershell -Verb RunAs -WindowStyle Hidden -Wait -ErrorAction Stop -ArgumentList $argumentList
    }
    catch {
        Write-Host "Elevation was declined or failed: $($_.Exception.Message)" -ForegroundColor Red
        exit 1
    }
}

$csvFiles = @(Get-ChildItem -LiteralPath $OutDir -Filter 'run-*.csv' | Where-Object Length -gt 0 | Sort-Object Name)
if ($csvFiles.Count -ne $Runs) {
    Write-Host "Expected $Runs non-empty captures, found $($csvFiles.Count); see capture-log.txt." -ForegroundColor Red
    exit 1
}
Write-Host ''
& $bench frames ($csvFiles | ForEach-Object FullName) --process $ProcessName
exit $LASTEXITCODE
