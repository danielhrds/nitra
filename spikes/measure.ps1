#Requires -Version 5.1
<#
.SYNOPSIS
    Side-by-side measurements of the two UI prototypes,
    without the game. Leave the PC alone while it runs (about 35 minutes).

.DESCRIPTION
    Per scenario and round (order alternates between rounds), starts the
    prototype, lets it settle, then samples once per second:
      CPU (% of one core), context switches/s of all its threads (wake-ups),
      private bytes, working set, GPU engine utilization (sum over engines)
      and dedicated GPU memory of the process.
    Scenarios: idle-visible (member list), idle-minimized, watching (ImGui;
    GPUI embedded; GPUI pop-out, with PresentMon on every swap chain).
    Then warm start-up (10 runs each) and a robustness loop (resize and
    minimize/restore while watching; CPU during resizing; handles and
    private bytes before and after).
    Results: D:\Projects\live\lab\ui-spike\<date>\.

    Keep this file ASCII-only (PowerShell 5.1).
#>
[CmdletBinding()]
param(
    [int] $Rounds = 2,
    [int] $IdleSeconds = 120,
    [int] $WatchSeconds = 60,
    [int] $SettleSeconds = 20,
    [string] $PresentMon = 'D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe',
    [switch] $Quick,
    # Run only these scenarios (e.g. idle-minimized) and skip start-up and
    # robustness.
    [string[]] $Only = @()
)

$ErrorActionPreference = 'Stop'
if ($Quick) { $Rounds = 1; $IdleSeconds = 10; $WatchSeconds = 10; $SettleSeconds = 5 }
$spikes = $PSScriptRoot
$out = Join-Path 'D:\Projects\live\lab\ui-spike' (Get-Date -Format 'yyyy-MM-dd-HHmm')
New-Item -ItemType Directory -Force $out | Out-Null
$summary = Join-Path $out 'summary.csv'
$invariant = [Globalization.CultureInfo]::InvariantCulture

$apps = @{
    imgui = Join-Path $spikes 'target\release\ui-spike-imgui.exe'
    gpui  = Join-Path $spikes 'target\release\ui-spike-gpui.exe'
}

Add-Type @"
using System; using System.Runtime.InteropServices;
public static class SpikeWin {
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
}
"@

function Get-Quantile([double[]] $values, [double] $q) {
    if (-not $values -or $values.Count -eq 0) { return [double]::NaN }
    $sorted = $values | Sort-Object
    $sorted[[int][math]::Floor(($sorted.Count - 1) * $q)]
}

function Get-Mean([double[]] $values) {
    if (-not $values -or $values.Count -eq 0) { return [double]::NaN }
    ($values | Measure-Object -Average).Average
}

# Context switches of all the process's threads so far (cumulative).
function Get-ContextSwitches([int] $Id) {
    $threads = Get-CimInstance Win32_PerfRawData_PerfProc_Thread -Filter "IDProcess=$Id" -ErrorAction SilentlyContinue
    [double](($threads | Measure-Object ContextSwitchesPersec -Sum).Sum)
}

# One sample per second for $Seconds; returns per-metric arrays. Wake-ups:
# context switches over the whole window (thread query is slow).
function Measure-Process([System.Diagnostics.Process] $Process, [int] $Seconds) {
    $id = $Process.Id
    $cpu = New-Object System.Collections.Generic.List[double]
    $gpu = New-Object System.Collections.Generic.List[double]
    $vram = New-Object System.Collections.Generic.List[double]
    $private = New-Object System.Collections.Generic.List[double]
    $working = New-Object System.Collections.Generic.List[double]
    $switchesStart = Get-ContextSwitches $id
    $window = [Diagnostics.Stopwatch]::StartNew()
    $Process.Refresh()
    $lastCpu = $Process.TotalProcessorTime.TotalMilliseconds
    $tick = [Diagnostics.Stopwatch]::StartNew()
    for ($i = 0; $i -lt $Seconds; $i++) {
        $engines = Get-CimInstance Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine -Filter "Name LIKE 'pid_$($id)_%'" -ErrorAction SilentlyContinue
        $memory = Get-CimInstance Win32_PerfFormattedData_GPUPerformanceCounters_GPUProcessMemory -Filter "Name LIKE 'pid_$($id)_%'" -ErrorAction SilentlyContinue
        $gpu.Add([double](($engines | Measure-Object UtilizationPercentage -Sum).Sum))
        $vram.Add([double](($memory | Measure-Object DedicatedUsage -Sum).Sum) / 1MB)
        $wait = 1000 - $tick.ElapsedMilliseconds
        if ($wait -gt 0) { Start-Sleep -Milliseconds $wait }
        if ($Process.HasExited) { break }
        $Process.Refresh()
        $nowCpu = $Process.TotalProcessorTime.TotalMilliseconds
        $cpu.Add(100.0 * ($nowCpu - $lastCpu) / $tick.Elapsed.TotalMilliseconds)
        $tick.Restart()
        $lastCpu = $nowCpu
        $private.Add($Process.PrivateMemorySize64 / 1MB)
        $working.Add($Process.WorkingSet64 / 1MB)
    }
    $switches = (Get-ContextSwitches $id) - $switchesStart
    [pscustomobject]@{
        Cpu = $cpu.ToArray(); Switches = [double[]]@($switches / $window.Elapsed.TotalSeconds); Gpu = $gpu.ToArray()
        Vram = $vram.ToArray(); Private = $private.ToArray(); Working = $working.ToArray()
    }
}

function Write-Summary([string] $App, [string] $Scenario, [int] $Round, $M, [string] $Extra = '') {
    $row = [pscustomobject]@{
        app = $App; scenario = $Scenario; round = $Round
        cpu_mean = '{0:F2}' -f (Get-Mean $M.Cpu)
        cpu_p95 = '{0:F2}' -f (Get-Quantile $M.Cpu 0.95)
        wakeups_per_s = '{0:F0}' -f (Get-Mean $M.Switches)
        private_mb = '{0:F1}' -f (Get-Quantile $M.Private 0.5)
        working_set_mb = '{0:F1}' -f (Get-Quantile $M.Working 0.5)
        gpu_util_mean = '{0:F2}' -f (Get-Mean $M.Gpu)
        vram_mb = '{0:F1}' -f (Get-Quantile $M.Vram 0.5)
        extra = $Extra
    }
    $row | Export-Csv $summary -Append -NoTypeInformation
    Write-Host ("  {0,-6} {1,-15} r{2}: cpu {3}% (p95 {4}), wakeups {5}/s, private {6} MB, ws {7} MB, gpu {8}%, vram {9} MB {10}" -f `
        $row.app, $row.scenario, $row.round, $row.cpu_mean, $row.cpu_p95, $row.wakeups_per_s, $row.private_mb,
        $row.working_set_mb, $row.gpu_util_mean, $row.vram_mb, $row.extra)
}

function Start-App([string] $App, [string[]] $ArgList) {
    $process = Start-Process $apps[$App] -ArgumentList $ArgList -PassThru `
        -RedirectStandardOutput (Join-Path $out "$App-stdout.txt") -WindowStyle Normal
    $process
}

function Stop-App([System.Diagnostics.Process] $Process) {
    if (-not $Process.WaitForExit(15000)) { $Process | Stop-Process -Force -Confirm:$false }
}

# Per swap chain: presents, displayed frames, display-change p50/p99.
function Get-PresentMonSummary([string] $Csv) {
    if (-not (Test-Path $Csv)) { return 'no presentmon' }
    $rows = Import-Csv $Csv
    ($rows | Group-Object SwapChainAddress | ForEach-Object {
        $shown = @($_.Group | Where-Object { $_.MsBetweenDisplayChange -ne 'NA' } |
            ForEach-Object { [double]::Parse($_.MsBetweenDisplayChange, $invariant) })
        '{0}: {1} presents, {2} shown, display p50 {3:F1} p99 {4:F1} ms' -f $_.Name, $_.Count, $shown.Count,
            (Get-Quantile $shown 0.5), (Get-Quantile $shown 0.99)
    }) -join '; '
}

Write-Host "Results in $out"
Get-Process obs64, VALORANT-Win64-Shipping -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Warning "$($_.ProcessName) is running; measurements should be taken with it closed."
}

# 1. Idle and watching.
$scenarios = @(
    @{ App = 'imgui'; Name = 'idle-visible';   Args = @();                      Seconds = $IdleSeconds },
    @{ App = 'gpui';  Name = 'idle-visible';   Args = @();                      Seconds = $IdleSeconds },
    @{ App = 'imgui'; Name = 'idle-minimized'; Args = @('--minimized');         Seconds = $IdleSeconds },
    @{ App = 'gpui';  Name = 'idle-minimized'; Args = @('--minimized');         Seconds = $IdleSeconds },
    @{ App = 'imgui'; Name = 'watch';          Args = @('--watch');             Seconds = $WatchSeconds },
    @{ App = 'gpui';  Name = 'watch-embedded'; Args = @('--watch');             Seconds = $WatchSeconds },
    @{ App = 'gpui';  Name = 'watch-popout';   Args = @('--watch', '--popout'); Seconds = $WatchSeconds }
)
if ($Only.Count -gt 0) { $scenarios = @($scenarios | Where-Object { $Only -contains $_.Name }) }
for ($round = 1; $round -le $Rounds; $round++) {
    $order = if ($round % 2 -eq 1) { $scenarios } else { [array]$scenarios.Clone(); }
    if ($round % 2 -eq 0) { [array]::Reverse($order) }
    foreach ($s in $order) {
        $total = $SettleSeconds + $s.Seconds + 3
        $process = Start-App $s.App ($s.Args + @('--seconds', "$total"))
        Start-Sleep -Seconds $SettleSeconds
        $extra = ''
        $pmCsv = $null
        if ($s.Name -like 'watch*' -and (Test-Path $PresentMon)) {
            $pmCsv = Join-Path $out ("presentmon-{0}-{1}-r{2}.csv" -f $s.App, $s.Name, $round)
            $pm = Start-Process $PresentMon -ArgumentList @('--process_id', "$($process.Id)", '--output_file', $pmCsv,
                '--timed', "$($s.Seconds)", '--terminate_after_timed', '--stop_existing_session',
                '--session_name', 'ziz_ui_spike') -PassThru -WindowStyle Hidden
        }
        $m = Measure-Process $process $s.Seconds
        if ($pmCsv) { $pm.WaitForExit(15000) | Out-Null; $extra = Get-PresentMonSummary $pmCsv }
        Write-Summary $s.App $s.Name $round $m $extra
        Stop-App $process
        Start-Sleep -Seconds 2
    }
}

if ($Only.Count -gt 0) { Write-Host "Done. Summary: $summary"; return }

# 2. Warm start-up: process creation -> first drawn frame.
$startCsv = Join-Path $out 'startup-warm.csv'
foreach ($i in 1..10) {
    foreach ($app in @('imgui', 'gpui')) {
        $output = & $apps[$app] --seconds 2 2>&1 | Out-String
        $match = [regex]::Match($output, 'first frame ([0-9.]+) ms')
        [pscustomobject]@{ app = $app; run = $i; first_frame_ms = if ($match.Success) { $match.Groups[1].Value } else { 'NA' } } |
            Export-Csv $startCsv -Append -NoTypeInformation
        Start-Sleep -Milliseconds 500
    }
}
Import-Csv $startCsv | Group-Object app | ForEach-Object {
    $v = [double[]]($_.Group | Where-Object { $_.first_frame_ms -ne 'NA' } | ForEach-Object { [double]::Parse($_.first_frame_ms, $invariant) })
    Write-Host ("  {0,-6} warm start-up: median {1:F1} ms, p95 {2:F1} ms ({3} runs)" -f $_.Name, (Get-Quantile $v 0.5), (Get-Quantile $v 0.95), $v.Count)
}

# 3. Robustness while watching: 2 batches of 40 resizes + 10 minimize/restore.
$sizes = @(@(1280, 800), @(700, 900), @(1600, 600), @(1000, 1000), @(900, 560))
foreach ($s in @(@{ App = 'imgui'; Args = @('--watch') }, @{ App = 'gpui'; Args = @('--watch') })) {
    $process = Start-App $s.App ($s.Args + @('--seconds', '120'))
    Start-Sleep -Seconds 5
    $process.Refresh()
    $window = $process.MainWindowHandle
    $readings = @()
    foreach ($batch in 1..2) {
        $process.Refresh()
        $before = @{ Handles = $process.HandleCount; Private = $process.PrivateMemorySize64 / 1MB; Cpu = $process.TotalProcessorTime.TotalMilliseconds }
        $clock = [Diagnostics.Stopwatch]::StartNew()
        foreach ($i in 0..39) {
            $size = $sizes[$i % $sizes.Count]
            [SpikeWin]::SetWindowPos($window, [IntPtr]::Zero, 80, 80, $size[0], $size[1], 0x0014) | Out-Null
            Start-Sleep -Milliseconds 50
        }
        $resizeCpu = 100.0 * ($process.TotalProcessorTime.TotalMilliseconds - $before.Cpu) / $clock.Elapsed.TotalMilliseconds
        foreach ($i in 1..10) {
            [SpikeWin]::ShowWindow($window, 6) | Out-Null   # SW_MINIMIZE
            Start-Sleep -Milliseconds 300
            [SpikeWin]::ShowWindow($window, 9) | Out-Null   # SW_RESTORE
            Start-Sleep -Milliseconds 300
        }
        Start-Sleep -Seconds 2
        $process.Refresh()
        $readings += '{0}: batch {1}: cpu while resizing {2:F1}%, handles {3} -> {4}, private {5:F1} -> {6:F1} MB' -f `
            $s.App, $batch, $resizeCpu, $before.Handles, $process.HandleCount, $before.Private, ($process.PrivateMemorySize64 / 1MB)
    }
    $process | Stop-Process -Force -Confirm:$false
    $readings | ForEach-Object { Write-Host "  $_" }
    $readings | Add-Content (Join-Path $out 'robustness.txt')
    Start-Sleep -Seconds 2
}
Write-Host "Done. Summary: $summary"
