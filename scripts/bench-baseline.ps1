#Requires -Version 5.1
<#
.SYNOPSIS
    Captures a game's frame pacing with PresentMon, optionally comparing
    conditions (no capture, WGC or DXGI capture, capture + encode with Media
    Foundation or AMF, the host pipeline up to conversion or complete), and
    analyzes the result.

.DESCRIPTION
    Follows docs/benchmark-protocol.md: records the environment, then runs
    rounds of timed PresentMon captures of the game process (a high beep
    marks the start of each capture, two short beeps its end). With several
    -Conditions, each round runs every condition once, in an order that
    rotates from round to round; for 'wgc' and 'dxgi' a `bench capture` of
    the monitor runs during the PresentMon capture (started -CaptureLead
    seconds before it, stopped as long after); for 'mf' and 'amf' it is a
    `bench encode --source monitor` (WGC capture, NV12 conversion and
    H.264 encode at -EncodeKbps); for 'convert' and 'pipeline' it is a
    `bench pipeline --monitor` (automatic capture and encoder selection) run
    up to the NV12 conversion or complete (WGC asked for about one frame per
    period, the default); 'rate' passes -WgcIntervalFactor explicitly (e.g. to
    compare 0.5 with 'pipeline') and 'rateprio' adds below-normal thread priority. Files are named run-<round>-<condition>.csv
    and capture-<round>-<condition>.json (encode runs also write
    encode-<round>-<condition>.h264). At the end, `bench frames` (one
    condition) or `bench ab` (several) is run. Whether OBS is running is
    recorded in environment.json; open or close it before starting.

    PresentMon needs administrator rights (ETW). Without them, the capture
    step relaunches this script elevated (one UAC prompt) in a hidden
    window, so the game keeps its fullscreen focus. Nothing in the system,
    game or GPU configuration is changed; settings are only read.

    Keep this file ASCII-only: PowerShell 5.1 reads BOM-less files with the
    ANSI code page.

.EXAMPLE
    .\scripts\bench-baseline.ps1 -Label range-static -PresentMon D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe

.EXAMPLE
    .\scripts\bench-baseline.ps1 -Label range-capture-ab -Conditions none,wgc,dxgi -Rounds 5 -PresentMon D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe
#>
[CmdletBinding()]
param(
    # Short name of the scene; becomes part of the output folder name.
    [Parameter(Mandatory)] [ValidatePattern('^[A-Za-z0-9_-]+$')] [string] $Label,

    # PresentMon console executable (default: the NITRA_PRESENTMON environment variable).
    [string] $PresentMon = $env:NITRA_PRESENTMON,

    [string] $ProcessName = 'VALORANT-Win64-Shipping.exe',

    # Conditions to compare: none (baseline without capture), wgc, dxgi, mf, amf, convert, pipeline,
    # rate, rateprio, or a name from -Programs.
    # With several, include 'none': the others are compared with it.
    [string[]] $Conditions = @('none'),

    # Each round runs every condition once.
    [ValidateRange(1, 50)] [int] $Rounds = 5,
    [ValidateRange(5, 600)] [int] $Seconds = 60,

    # Monitor index (bench probe order) and frame rate for capture conditions.
    [ValidateRange(0, 16)] [int] $Monitor = 0,
    [ValidateRange(1, 1000)] [int] $CaptureFps = 60,
    # Target bitrate (kbit/s) of the mf, amf and pipeline conditions.
    [ValidateRange(100, 500000)] [int] $EncodeKbps = 10000,
    # WGC MinUpdateInterval as a fraction of the frame period for 'rate' and 'rateprio'.
    [ValidateRange(0.1, 1.0)] [double] $WgcIntervalFactor = 0.95,
    # Seconds a capture runs before and after its PresentMon capture.
    [ValidateRange(1, 60)] [int] $CaptureLead = 5,

    # Extra conditions that run any program during the capture, as
    # 'name=C:\path\program.exe|arguments' ('{seconds}' in the arguments is
    # replaced by the capture's length, leads included). Started without a
    # console window, so a program that opens its window without activating
    # it leaves the game in the foreground; stopped after the capture if it
    # has not exited by then. Use the names in -Conditions.
    [string[]] $Programs = @(),

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
    [switch] $CaptureOnly,
    [string] $Bench
)

# Native commands report failure through $LASTEXITCODE; their stderr must
# not be treated as a PowerShell error.
$ErrorActionPreference = 'Continue'

# Accept "-Conditions none,wgc,dxgi" also when passed as one string (elevated relaunch).
$Conditions = @($Conditions | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim().ToLowerInvariant() } | Where-Object { $_ })
# -Programs entries, also when passed as one string: ';;'-separated (elevated
# relaunch) or comma-separated ("-File" passes a list as one string).
$programTable = @{}
$entries = @($Programs | ForEach-Object { $_ -split ';;' } | ForEach-Object { $_ -split ',(?=\s*[A-Za-z0-9_-]+=)' } |
    ForEach-Object { $_.Trim() } | Where-Object { $_ })
$Programs = $entries
foreach ($entry in $entries) {
    $name, $command = $entry -split '=', 2
    $path, $programArguments = $command -split '\|', 2
    if (-not $name -or -not $path) {
        Write-Host "Bad -Programs entry '$entry'; use 'name=C:\path\program.exe|arguments'." -ForegroundColor Red
        exit 2
    }
    $programTable[$name.Trim().ToLowerInvariant()] = @{ Path = $path.Trim(); Arguments = "$programArguments".Trim() }
}
$builtIn = @('none', 'wgc', 'dxgi', 'mf', 'amf', 'convert', 'pipeline', 'rate', 'rateprio')
$unknown = @($Conditions | Where-Object { $_ -notin $builtIn -and -not $programTable.ContainsKey($_) })
if ($unknown.Count -gt 0 -or $Conditions.Count -eq 0) {
    Write-Host "Unknown conditions: $($unknown -join ', '); use none, wgc, dxgi, mf, amf, convert, pipeline, rate, rateprio and/or names from -Programs." -ForegroundColor Red
    exit 2
}
if ($Conditions.Count -gt 1 -and $Conditions -notcontains 'none') {
    Write-Host "With several conditions, include 'none' as the baseline." -ForegroundColor Red
    exit 2
}

function Test-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    ([Security.Principal.WindowsPrincipal] $identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-RoundOrder([int] $round) {
    # Rotate the condition list so every condition takes every position.
    $count = $Conditions.Count
    $shift = ($round - 1) % $count
    @(0..($count - 1) | ForEach-Object { $Conditions[($_ + $shift) % $count] })
}

function Invoke-Captures {
    $log = Join-Path $OutDir 'capture-log.txt'
    function Write-Log([string] $message) {
        ('{0:yyyy-MM-dd HH:mm:ss} {1}' -f (Get-Date), $message) | Out-File -FilePath $log -Append -Encoding utf8
    }
    Write-Log "captures start in $StartDelay s; conditions: $($Conditions -join ', '); rounds: $Rounds"
    Start-Sleep -Seconds $StartDelay
    $total = $Rounds * $Conditions.Count
    $index = 0
    for ($round = 1; $round -le $Rounds; $round++) {
        foreach ($condition in (Get-RoundOrder $round)) {
            $index++
            $name = '{0}-{1}' -f $round, $condition
            $captureProcess = $null
            if ($programTable.ContainsKey($condition)) {
                $program = $programTable[$condition]
                $startInfo = New-Object System.Diagnostics.ProcessStartInfo
                $startInfo.FileName = $program.Path
                $startInfo.Arguments = $program.Arguments.Replace('{seconds}', "$($Seconds + 2 * $CaptureLead)")
                $startInfo.UseShellExecute = $false
                # No console window: it would take the focus from the game.
                $startInfo.CreateNoWindow = $true
                $captureProcess = [Diagnostics.Process]::Start($startInfo)
                $null = $captureProcess.Handle
                $arguments = @($condition)
                Write-Log "capture $index/$total ($name): program $($program.Path) $($startInfo.Arguments) started (pid $($captureProcess.Id))"
                Start-Sleep -Seconds $CaptureLead
            }
            elseif ($condition -ne 'none') {
                $captureSeconds = $Seconds + 2 * $CaptureLead
                $summary = '"{0}"' -f (Join-Path $OutDir "capture-$name.json")
                if ($condition -eq 'convert') {
                    $arguments = @('pipeline', '--monitor', $Monitor, '--until', 'convert', '--fps', $CaptureFps,
                        '--seconds', $captureSeconds, '--summary-json', $summary)
                }
                elseif ($condition -in @('pipeline', 'rate', 'rateprio')) {
                    $arguments = @('pipeline', '--monitor', $Monitor, '--fps', $CaptureFps, '--seconds', $captureSeconds,
                        '--kbps', $EncodeKbps, '--output', ('"{0}"' -f (Join-Path $OutDir "encode-$name.h264")),
                        '--summary-json', $summary)
                    if ($condition -ne 'pipeline') {
                        $arguments += @('--wgc-interval-factor', $WgcIntervalFactor.ToString([Globalization.CultureInfo]::InvariantCulture))
                    }
                    if ($condition -eq 'rateprio') { $arguments += '--low-priority' }
                }
                elseif ($condition -in @('mf', 'amf')) {
                    $arguments = @('encode', '--encoder', $condition, '--source', 'monitor', '--monitor', $Monitor,
                        '--fps', $CaptureFps, '--seconds', $captureSeconds, '--kbps', $EncodeKbps,
                        '--output', ('"{0}"' -f (Join-Path $OutDir "encode-$name.h264")), '--summary-json', $summary)
                }
                else {
                    $arguments = @('capture', '--backend', $condition, '--monitor', $Monitor, '--fps', $CaptureFps,
                        '--seconds', $captureSeconds, '--no-cursor', '--report-every', 3600, '--summary-json', $summary)
                }
                $captureProcess = Start-Process -FilePath $Bench -ArgumentList $arguments -WindowStyle Hidden -PassThru `
                    -RedirectStandardOutput (Join-Path $OutDir "capture-$name.log") `
                    -RedirectStandardError (Join-Path $OutDir "capture-$name.err.log")
                # Reading the handle now keeps ExitCode available after exit.
                $null = $captureProcess.Handle
                Write-Log "capture $index/$total ($name): bench $($arguments[0]) started (pid $($captureProcess.Id))"
                Start-Sleep -Seconds $CaptureLead
            }

            $csv = Join-Path $OutDir "run-$name.csv"
            [console]::Beep(880, 250)
            Write-Log "capture $index/$total ($name): presentmon start"
            $output = & $PresentMon --process_name $ProcessName --output_file $csv --timed $Seconds `
                --terminate_after_timed --no_console_stats --qpc_time `
                --session_name nitra_baseline --stop_existing_session 2>&1
            $exitCode = $LASTEXITCODE
            foreach ($line in $output) { Write-Log "  presentmon: $line" }
            Write-Log "capture $index/$total ($name): presentmon end (exit $exitCode)"
            [console]::Beep(660, 150); Start-Sleep -Milliseconds 100; [console]::Beep(660, 150)

            if ($captureProcess) {
                if (-not $captureProcess.WaitForExit(($CaptureLead + 30) * 1000)) {
                    Stop-Process -Id $captureProcess.Id -Force -ErrorAction SilentlyContinue
                    Write-Log "capture $index/$total ($name): bench $($arguments[0]) did not exit; killed"
                }
                else {
                    Write-Log "capture $index/$total ($name): bench $($arguments[0]) exit $($captureProcess.ExitCode)"
                }
            }
            if ($index -lt $total) { Start-Sleep -Seconds $Gap }
        }
    }
    Write-Log 'captures done'
}

if ($CaptureOnly) {
    Invoke-Captures
    exit 0
}

$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $PresentMon -or -not (Test-Path -LiteralPath $PresentMon -PathType Leaf)) {
    Write-Host 'PresentMon not found: pass -PresentMon <path to PresentMon-x.y.z-x64.exe> or set NITRA_PRESENTMON.' -ForegroundColor Red
    exit 2
}
$PresentMon = (Resolve-Path -LiteralPath $PresentMon).Path

$processBaseName = [IO.Path]::GetFileNameWithoutExtension($ProcessName)
if (-not (Get-Process -Name $processBaseName -ErrorAction SilentlyContinue)) {
    Write-Host "$ProcessName is not running; start the game and enter the scene first." -ForegroundColor Red
    exit 2
}

if (-not $OutDir) {
    $lab = if ($env:NITRA_LAB_DIR) { $env:NITRA_LAB_DIR } else { Join-Path (Split-Path -Parent $repoRoot) 'lab' }
    $OutDir = Join-Path $lab ('baseline\{0:yyyy-MM-dd-HHmm}-{1}' -f (Get-Date), $Label)
}
if ((Test-Path -LiteralPath $OutDir) -and (Get-ChildItem -LiteralPath $OutDir -Force | Select-Object -First 1)) {
    Write-Host "Output folder is not empty: $OutDir" -ForegroundColor Red
    exit 2
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

$Bench = Join-Path $repoRoot 'target\release\bench.exe'
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
    captured_on       = (Get-Date).ToString('yyyy-MM-ddTHH:mm:ssK')
    label             = $Label
    process           = $ProcessName
    conditions        = $Conditions
    rounds            = $Rounds
    seconds_per_run   = $Seconds
    capture_monitor   = $Monitor
    capture_fps       = $CaptureFps
    encode_kbps       = $EncodeKbps
    wgc_interval_factor = $WgcIntervalFactor
    programs          = $Programs
    obs_running       = [bool] (Get-Process -Name obs64 -ErrorAction SilentlyContinue)
    capture_lead_s    = $CaptureLead
    gap_seconds       = $Gap
    gap_action        = $GapHint
    # The release file name carries the version; the executable has no version resource.
    presentmon        = (Split-Path -Leaf $PresentMon)
    presentmon_sha256 = (Get-FileHash -LiteralPath $PresentMon -Algorithm SHA256).Hash
    power_scheme      = ((powercfg /getactivescheme) -join ' ').Trim()
    hags_hwschmode    = if ($null -ne $hags) { $hags } else { 'not set (default)' }
    game_mode_auto    = if ($null -ne $gameMode) { $gameMode } else { 'not set (default)' }
    notes             = $Notes
}
$environment | ConvertTo-Json | Out-File -FilePath (Join-Path $OutDir 'environment.json') -Encoding utf8
& $Bench probe --json (Join-Path $OutDir 'probe.json') | Out-Null

$captures = $Rounds * $Conditions.Count
$extra = if (@($Conditions | Where-Object { $_ -ne 'none' }).Count -gt 0) { 2 * $CaptureLead } else { 0 }
$total = $StartDelay + $captures * ($Seconds + $extra) + ($captures - 1) * $Gap
Write-Host ''
Write-Host "Session '$Label': $Rounds round(s) of [$($Conditions -join ', ')], $captures x $Seconds s of $ProcessName (about $([math]::Ceiling($total / 60)) min)." -ForegroundColor Cyan
Write-Host "  1. Accept the UAC prompt if it appears, then click back into the game ($StartDelay s)."
Write-Host '  2. High beep: capture started. Keep still, no input.'
Write-Host "  3. Two short beeps: capture ended. In the pause: $GapHint."
Write-Host "  Output: $OutDir"
Write-Host ''

$captureArguments = [ordered]@{
    OutDir = $OutDir; PresentMon = $PresentMon; ProcessName = $ProcessName; Bench = $Bench
    Conditions = ($Conditions -join ','); Rounds = $Rounds; Seconds = $Seconds; Monitor = $Monitor
    CaptureFps = $CaptureFps; EncodeKbps = $EncodeKbps; WgcIntervalFactor = $WgcIntervalFactor.ToString([Globalization.CultureInfo]::InvariantCulture); CaptureLead = $CaptureLead; StartDelay = $StartDelay; Gap = $Gap
}
if ($Programs.Count -gt 0) { $captureArguments['Programs'] = ($Programs -join ';;') }
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
if ($csvFiles.Count -ne $captures) {
    Write-Host "Expected $captures non-empty captures, found $($csvFiles.Count); see capture-log.txt." -ForegroundColor Red
    exit 1
}
Write-Host ''
if ($Conditions.Count -eq 1) {
    & $Bench frames ($csvFiles | ForEach-Object FullName) --process $ProcessName
}
else {
    & $Bench ab $OutDir --process $ProcessName
}
exit $LASTEXITCODE
