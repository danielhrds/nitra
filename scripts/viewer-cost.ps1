#Requires -Version 5.1
<#
.SYNOPSIS
    What a viewer costs on its own: `bench present` playing H.264 files
    (hardware decode + presenter window), measured from outside.

.DESCRIPTION
    For each file and round (order rotates between rounds), starts
    `bench present <file> --repeat --background` (optionally placed with -At,
    e.g. on a second monitor), lets it settle, then samples once per second:
      CPU (% of one core), private bytes, working set, GPU utilization per
      engine type (3D, VideoDecode, Copy, other), dedicated GPU memory;
    and context switches per second of all its threads over the window
    (wake-ups). The presenter's own counters (presented, replaced, dropped)
    come from its output. Leave the PC alone while it runs.

    Results: <lab>\viewer\<yyyy-MM-dd-HHmm>\summary.csv and the console.
    Keep this file ASCII-only (PowerShell 5.1).

.EXAMPLE
    .\scripts\viewer-cost.ps1 -Files D:\Projects\live\lab\viewer\streams\1080p60.h264,D:\Projects\live\lab\viewer\streams\1080p30.h264 -At -1800,132
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string[]] $Files,
    # Window position "X,Y" (screen coordinates, physical pixels).
    [string] $At,
    [ValidateRange(1, 20)] [int] $Rounds = 2,
    [ValidateRange(5, 600)] [int] $Seconds = 60,
    [ValidateRange(0, 120)] [int] $SettleSeconds = 10,
    # Extra arguments for `bench present` (e.g. '--no-overlay').
    [string] $ExtraArgs = '',
    # Minimize the player once it is up (to measure --skip-hidden).
    [switch] $Minimize,
    [string] $OutDir
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$bench = Join-Path $repoRoot 'target\release\bench.exe'
if (-not (Test-Path $bench)) { throw "build bench first: cargo build --release -p bench" }
$Files = @($Files | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
foreach ($file in $Files) { if (-not (Test-Path $file)) { throw "missing $file" } }
if (-not $OutDir) {
    $lab = if ($env:ZIZ_LAB_DIR) { $env:ZIZ_LAB_DIR } else { Join-Path (Split-Path -Parent $repoRoot) 'lab' }
    $OutDir = Join-Path $lab ('viewer\{0:yyyy-MM-dd-HHmm}' -f (Get-Date))
}
New-Item -ItemType Directory -Force $OutDir | Out-Null
$summary = Join-Path $OutDir 'summary.csv'

function Get-Quantile([double[]] $values, [double] $q) {
    if (-not $values -or $values.Count -eq 0) { return [double]::NaN }
    $sorted = $values | Sort-Object
    $sorted[[int][math]::Floor(($sorted.Count - 1) * $q)]
}

function Get-Mean([double[]] $values) {
    if (-not $values -or $values.Count -eq 0) { return [double]::NaN }
    ($values | Measure-Object -Average).Average
}

# Context switches per thread so far (cumulative), by thread id.
function Get-ThreadSwitches([int] $Id) {
    $table = @{}
    foreach ($thread in (Get-CimInstance Win32_PerfRawData_PerfProc_Thread -Filter "IDProcess=$Id" -ErrorAction SilentlyContinue)) {
        $table["$($thread.IDThread)"] = [double]$thread.ContextSwitchesPersec
    }
    $table
}

# Raw GPU engine running time (100 ns units) per engine, with its timestamp.
function Get-EngineTimes([int] $Id) {
    $table = @{}
    foreach ($engine in (Get-CimInstance Win32_PerfRawData_GPUPerformanceCounters_GPUEngine -Filter "Name LIKE 'pid_$($Id)_%'" -ErrorAction SilentlyContinue)) {
        $table[$engine.Name] = @([double]$engine.UtilizationPercentage, [double]$engine.Timestamp_Sys100NS)
    }
    $table
}

# Engine type of a counter name: 3D, Decode (VideoDecode, AMD "Video Codec
# Engine"), Copy or Other.
function Get-EngineType([string] $Name) {
    $type = if ($Name -match 'engtype_(.+)$') { $Matches[1].Trim() } else { '' }
    switch -regex ($type) {
        '^3D$' { '3D' }
        'Video ?Decode|Video Codec' { 'Decode' }
        '^Copy$' { 'Copy' }
        default { 'Other' }
    }
}

function Measure-Process([System.Diagnostics.Process] $Process, [int] $Seconds) {
    $id = $Process.Id
    $m = @{ Cpu = @(); Vram = @(); Private = @(); Working = @() }
    $switchesStart = Get-ThreadSwitches $id
    $enginesStart = Get-EngineTimes $id
    # The compositor does the composition work: measure it too.
    $dwm = Get-Process dwm -ErrorAction SilentlyContinue | Select-Object -First 1
    $dwmStart = if ($dwm) { Get-EngineTimes $dwm.Id } else { @{} }
    # dwm.exe CPU time may be off limits without administrator rights.
    $dwmCpuStart = try { $dwm.TotalProcessorTime.TotalMilliseconds } catch { $null }
    # A new process's GPU counters can take a moment to appear.
    for ($try = 0; $enginesStart.Count -eq 0 -and $try -lt 10; $try++) {
        Start-Sleep -Milliseconds 500
        $enginesStart = Get-EngineTimes $id
    }
    $window = [Diagnostics.Stopwatch]::StartNew()
    $Process.Refresh()
    $lastCpu = $Process.TotalProcessorTime.TotalMilliseconds
    $tick = [Diagnostics.Stopwatch]::StartNew()
    for ($i = 0; $i -lt $Seconds; $i++) {
        $memory = Get-CimInstance Win32_PerfFormattedData_GPUPerformanceCounters_GPUProcessMemory `
            -Filter "Name LIKE 'pid_$($id)_%'" -ErrorAction SilentlyContinue
        $m.Vram += [double](($memory | Measure-Object DedicatedUsage -Sum).Sum) / 1MB
        $wait = 1000 - $tick.ElapsedMilliseconds
        if ($wait -gt 0) { Start-Sleep -Milliseconds $wait }
        if ($Process.HasExited) { break }
        $Process.Refresh()
        $nowCpu = $Process.TotalProcessorTime.TotalMilliseconds
        $m.Cpu += 100.0 * ($nowCpu - $lastCpu) / $tick.Elapsed.TotalMilliseconds
        $tick.Restart()
        $lastCpu = $nowCpu
        $m.Private += $Process.PrivateMemorySize64 / 1MB
        $m.Working += $Process.WorkingSet64 / 1MB
    }
    # Wake-ups: only threads alive at both ends (exited threads leave the sum).
    $switchesEnd = Get-ThreadSwitches $id
    $seconds = $window.Elapsed.TotalSeconds
    $switches = 0.0
    foreach ($key in $switchesEnd.Keys) {
        if ($switchesStart.ContainsKey($key)) { $switches += [math]::Max(0, $switchesEnd[$key] - $switchesStart[$key]) }
    }
    $m.Wakeups = $switches / $seconds
    if ($dwm) {
        $dwm.Refresh()
        if ($null -ne $dwmCpuStart) {
            $m.DwmCpu = 100.0 * ($dwm.TotalProcessorTime.TotalMilliseconds - $dwmCpuStart) / $window.Elapsed.TotalMilliseconds
        }
        $m.Dwm = Get-EngineUse $dwmStart (Get-EngineTimes $dwm.Id)
    }
    $m.Gpu = Get-EngineUse $enginesStart (Get-EngineTimes $id)
    $m
}

# Engine use: running time over elapsed time, summed per engine type.
function Get-EngineUse([hashtable] $Start, [hashtable] $End) {
    $use = @{ '3D' = 0.0; 'Decode' = 0.0; 'Copy' = 0.0; 'Other' = 0.0 }
    foreach ($name in $End.Keys) {
        if (-not $Start.ContainsKey($name)) { continue }
        $busy = $End[$name][0] - $Start[$name][0]
        $elapsed = $End[$name][1] - $Start[$name][1]
        if ($elapsed -gt 0) { $use[(Get-EngineType $name)] += 100.0 * $busy / $elapsed }
    }
    $use
}

# Keep the displays on while measuring (a sleeping display stops composition),
# as a video player does; released when the script ends. Changes no setting.
Add-Type -Namespace ZizViewer -Name Native -MemberDefinition @"
[DllImport("kernel32.dll")] public static extern uint SetThreadExecutionState(uint flags);
[DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr window, int command);
"@
# ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED
[void][ZizViewer.Native]::SetThreadExecutionState([uint32]2147483651)

Get-Process obs64, VALORANT-Win64-Shipping -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Warning "$($_.ProcessName) is running; measure with it closed."
}
Write-Host "viewer cost: $($Files.Count) file(s) x $Rounds round(s), $Seconds s each -> $OutDir"
for ($round = 1; $round -le $Rounds; $round++) {
    $order = @($Files)
    $shift = ($round - 1) % $order.Count
    $order = @(0..($order.Count - 1) | ForEach-Object { $order[($_ + $shift) % $order.Count] })
    foreach ($file in $order) {
        $name = [IO.Path]::GetFileNameWithoutExtension($file)
        $total = $SettleSeconds + $Seconds + 3
        # Play at the file's frame rate (from its name, e.g. 1080p30).
        $fps = if ($name -match 'p(\d+)') { $Matches[1] } else { '60' }
        $arguments = "present `"$file`" --fps $fps --repeat --background --seconds $total $ExtraArgs"
        if ($At) { $arguments += " --at=$At" }
        $startInfo = New-Object System.Diagnostics.ProcessStartInfo
        $startInfo.FileName = $bench
        $startInfo.Arguments = $arguments
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardOutput = $true
        $process = [Diagnostics.Process]::Start($startInfo)
        $outputTask = $process.StandardOutput.ReadToEndAsync()
        if ($Minimize) {
            # SW_SHOWMINNOACTIVE: minimized without activating another window.
            for ($try = 0; $process.MainWindowHandle -eq [IntPtr]::Zero -and $try -lt 50; $try++) {
                Start-Sleep -Milliseconds 100
                $process.Refresh()
            }
            [void][ZizViewer.Native]::ShowWindowAsync($process.MainWindowHandle, 7)
        }
        Start-Sleep -Seconds $SettleSeconds
        $m = Measure-Process $process $Seconds
        if (-not $process.WaitForExit(($total + 20) * 1000)) { $process | Stop-Process -Force }
        $output = $outputTask.Result
        $output | Out-File (Join-Path $OutDir "$name-r$round.txt") -Encoding utf8
        $presenter = [regex]::Match($output, 'presenter: (\d+) submitted, (\d+) presented \(([0-9.]+)/s\), (\d+) replaced, (\d+) dropped')
        $hidden = [regex]::Match($output, '(\d+) hidden')
        $row = [pscustomobject]@{
            file = $name; round = $round
            cpu_mean = '{0:F2}' -f (Get-Mean $m.Cpu); cpu_p95 = '{0:F2}' -f (Get-Quantile $m.Cpu 0.95)
            wakeups_per_s = '{0:F0}' -f $m.Wakeups
            gpu_3d = '{0:F2}' -f $m.Gpu['3D']; gpu_decode = '{0:F2}' -f $m.Gpu['Decode']
            gpu_copy = '{0:F2}' -f $m.Gpu['Copy']; gpu_other = '{0:F2}' -f $m.Gpu['Other']
            dwm_3d = if ($m.Dwm) { '{0:F2}' -f $m.Dwm['3D'] } else { 'NA' }
            dwm_cpu = if ($null -ne $m.DwmCpu) { '{0:F2}' -f $m.DwmCpu } else { 'NA' }
            vram_mb = '{0:F1}' -f (Get-Quantile $m.Vram 0.5)
            private_mb = '{0:F1}' -f (Get-Quantile $m.Private 0.5); working_set_mb = '{0:F1}' -f (Get-Quantile $m.Working 0.5)
            presented_per_s = if ($presenter.Success) { $presenter.Groups[3].Value } else { 'NA' }
            replaced = if ($presenter.Success) { $presenter.Groups[4].Value } else { 'NA' }
            dropped = if ($presenter.Success) { $presenter.Groups[5].Value } else { 'NA' }
            hidden = if ($hidden.Success) { $hidden.Groups[1].Value } else { 'NA' }
        }
        $row | Export-Csv $summary -Append -NoTypeInformation
        Write-Host ("  {0,-8} r{1}: cpu {2}% (p95 {3}), wakeups {4}/s, gpu 3D {5}% decode {6}% copy {7}% other {8}%, vram {9} MB, private {10} MB, {11} fps shown, {12} replaced, {13} dropped, {14} hidden; dwm 3D {15}% cpu {16}%" -f `
            $row.file, $row.round, $row.cpu_mean, $row.cpu_p95, $row.wakeups_per_s, $row.gpu_3d, $row.gpu_decode,
            $row.gpu_copy, $row.gpu_other, $row.vram_mb, $row.private_mb, $row.presented_per_s, $row.replaced, $row.dropped, $row.hidden, $row.dwm_3d, $row.dwm_cpu)
        Start-Sleep -Seconds 2
    }
}
# ES_CONTINUOUS alone: back to normal.
[void][ZizViewer.Native]::SetThreadExecutionState([uint32]2147483648)
Write-Host "Summary: $summary"
